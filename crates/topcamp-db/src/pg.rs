//! Concrete PostgreSQL repositories (`migrations/0001_init.sql`, §11–§15).
//!
//! [`PgDb`] is a cheap-clone handle over [`sqlx::PgPool`] implementing the
//! focused traits in [`crate::repositories`]. Error mapping goes through
//! [`crate::DbError`] so no SQL text ever crosses into the domain (§31).
//! `search_vector` is maintained here on write (no trigger); reachability
//! for search is membership in the room.

use sqlx::postgres::{PgPool, PgRow};
use sqlx::{FromRow, Row, Transaction};

use crate::error::DbError;
use crate::repositories::{
    AccountRepository, AccountRow, AttachmentRepository, AvatarUser, BlobRow, BoostDetail, BotRow,
    CredentialsRow, FormUser, MembershipRepository, MembershipRow, MessageDetail,
    MessageRepository, MessageRow, NewBlob, NewMessage, NewMessageAttachment, ProfileMembership,
    ProfileUpdate, ProfileUser, PushSubscriptionRepository, PushSubscriptionRow, RecordAttachment,
    RepoResult, RoomRepository, RoomRow, SearchRepository, SearchRow, SessionRepository,
    SessionRow, SidebarMembership, SidebarUser, UserRepository, UserRow, WebhookRepository,
    WebhookRow,
};
use topcamp_domain::auth::{UserRole, UserStatus};
use topcamp_domain::search::match_terms;

/// Shared PostgreSQL handle. `Clone` is a pool-handle clone (cheap).
#[derive(Debug, Clone)]
pub struct PgDb {
    pool: PgPool,
}

impl PgDb {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Push endpoints for every member of a room that has any subscription.
    /// Cross-aggregate read for the notification workflow (not a message
    /// aggregate concern, hence inherent rather than on a repo trait).
    pub async fn push_endpoints_for_room(&self, room_id: i64) -> RepoResult<Vec<String>> {
        let endpoints = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT ps.endpoint FROM push_subscriptions ps JOIN memberships mb ON mb.user_id = ps.user_id WHERE mb.room_id = $1 AND ps.endpoint IS NOT NULL",
        )
        .bind(room_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(endpoints)
    }

    /// Post a message atomically (§13–§14): message row + body RichText +
    /// room touch + `push_message` outbox row + unread stamps commit together.
    /// Durable side effects (push, webhooks) run after commit via the outbox
    /// relay — never inside this transaction (§19).
    #[tracing::instrument(
        name = "db.post_message",
        skip(self, input),
        fields(room_id = input.room_id, creator_id = input.creator_id)
    )]
    pub async fn post_message(&self, input: NewMessage) -> RepoResult<MessageRow> {
        self.post_message_with_attachment(input, None).await
    }

    /// Post a message with its attachment rows in one commit. The
    /// caller puts the bytes first (outside the transaction): a failed
    /// commit orphans unreachable bytes rather than serving half rows.
    pub async fn post_message_with_attachment(
        &self,
        input: NewMessage,
        attachment: Option<NewMessageAttachment>,
    ) -> RepoResult<MessageRow> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let row = MessageRepository::create(self, &mut tx, input).await?;
        // Integer-only payload: no escaping concerns.
        let payload = format!("{{\"message_id\":{},\"room_id\":{}}}", row.id, row.room_id);
        crate::outbox::publish(&mut tx, "push_message", &payload)
            .await
            .map_err(db)?;
        RoomRepository::mark_received(self, &mut tx, row.room_id, row.id).await?;
        if let Some(attachment) = attachment {
            let content_type = attachment.blob.content_type.clone();
            let blob = Self::insert_blob_in(&mut tx, &attachment.blob).await?;
            Self::attach_in(&mut tx, "Message", row.id, "attachment", blob.id).await?;
            Self::merge_metadata_in(
                &mut tx,
                blob.id,
                content_type.as_deref(),
                &attachment.metadata_json,
            )
            .await?;
        }
        tx.commit().await.map_err(db)?;
        Ok(row)
    }

    async fn insert_blob_in(
        tx: &mut Transaction<'_, sqlx::Postgres>,
        blob: &NewBlob,
    ) -> RepoResult<BlobRow> {
        let row = sqlx::query_as::<_, BlobRow>(
            "INSERT INTO active_storage_blobs (created_at, key, filename, content_type, byte_size, checksum, service_name) VALUES (now(), $1, $2, $3, $4, $5, $6) RETURNING id, key, filename, content_type, byte_size, checksum, service_name",
        )
        .bind(&blob.key)
        .bind(&blob.filename)
        .bind(&blob.content_type)
        .bind(blob.byte_size)
        .bind(&blob.checksum)
        .bind(&blob.service_name)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn attach_in(
        tx: &mut Transaction<'_, sqlx::Postgres>,
        record_type: &str,
        record_id: i64,
        name: &str,
        blob_id: i64,
    ) -> RepoResult<()> {
        // Strict insert: double-attach violates `attachments_uniqueness`
        // and must surface, not silently pass.
        sqlx::query(
            "INSERT INTO active_storage_attachments (created_at, blob_id, name, record_id, record_type) VALUES (now(), $1, $2, $3, $4)",
        )
        .bind(blob_id)
        .bind(name)
        .bind(record_id)
        .bind(record_type)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn merge_metadata_in(
        tx: &mut Transaction<'_, sqlx::Postgres>,
        blob_id: i64,
        content_type: Option<&str>,
        metadata_json: &str,
    ) -> RepoResult<()> {
        // `metadata` is TEXT holding a JSON object (or NULL): the merge
        // happens in SQL via `jsonb ||` so no JSON dependency leaks in here.
        sqlx::query(
            "UPDATE active_storage_blobs SET content_type = COALESCE($2, content_type), metadata = (COALESCE(NULLIF(metadata, ''), '{}')::jsonb || $3::jsonb)::text WHERE id = $1",
        )
        .bind(blob_id)
        .bind(content_type)
        .bind(metadata_json)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        Ok(())
    }
}

fn db(err: sqlx::Error) -> topcamp_domain::error::Error {
    topcamp_domain::error::Error::from(DbError::from(err))
}

const ROOM_INSERT: &str = "INSERT INTO rooms (created_at, updated_at, creator_id, name, \"type\") VALUES (now(), now(), $1, $2, $3) RETURNING id, creator_id, name, \"type\" AS kind";
const MESSAGE_INSERT: &str = "INSERT INTO messages (created_at, updated_at, client_message_id, creator_id, room_id, search_vector) VALUES (now(), now(), $1, $2, $3, to_tsvector('english', regexp_replace($4, '<[^>]*>', ' ', 'g'))) RETURNING id, room_id, creator_id, client_message_id";

/// Storage key for a Topcoat session: lowercase hex of the SHA-256 token
/// hash. Raw tokens never touch the database.
pub fn session_key(hash: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in hash {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

impl<'r> FromRow<'r, PgRow> for RoomRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            creator_id: row.try_get("creator_id")?,
            name: row.try_get("name")?,
            kind: row.try_get("kind")?,
        })
    }
}

impl<'r> FromRow<'r, PgRow> for MembershipRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            room_id: row.try_get("room_id")?,
            user_id: row.try_get("user_id")?,
            involvement: row.try_get("involvement")?,
        })
    }
}

impl MembershipRepository for PgDb {
    async fn find(&self, room_id: i64, user_id: i64) -> RepoResult<Option<MembershipRow>> {
        let row = sqlx::query_as::<_, MembershipRow>(
            "SELECT id, room_id, user_id, involvement FROM memberships WHERE room_id = $1 AND user_id = $2",
        )
        .bind(room_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn create(&self, room_id: i64, user_id: i64) -> RepoResult<MembershipRow> {
        let row = sqlx::query_as::<_, MembershipRow>(
            "INSERT INTO memberships (created_at, updated_at, room_id, user_id) VALUES (now(), now(), $1, $2) RETURNING id, room_id, user_id, involvement",
        )
        .bind(room_id)
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn mark_connected(&self, id: i64) -> RepoResult<()> {
        // `connected`: stale memberships restart at 1 (CONNECTION_TTL).
        sqlx::query(
            "UPDATE memberships SET connections = CASE WHEN connected_at >= now() - interval '60 seconds' THEN connections + 1 ELSE 1 END, connected_at = now(), updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn mark_present(&self, id: i64) -> RepoResult<()> {
        // `present`: connect + viewing clears the unread stamp.
        sqlx::query(
            "UPDATE memberships SET connections = CASE WHEN connected_at >= now() - interval '60 seconds' THEN connections + 1 ELSE 1 END, connected_at = now(), unread_at = NULL, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn mark_refreshed(&self, id: i64) -> RepoResult<()> {
        // `refresh_connection`: only stale memberships change (revived
        // to 1); fresh ones are untouched, never incremented.
        sqlx::query(
            "UPDATE memberships SET connections = 1, updated_at = now() WHERE id = $1 AND (connected_at IS NULL OR connected_at < now() - interval '60 seconds')",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn mark_disconnected(&self, id: i64) -> RepoResult<()> {
        // `disconnected`: stale memberships zero out; a drained
        // presence clears its timestamp.
        sqlx::query(
            "UPDATE memberships SET connections = CASE WHEN connected_at >= now() - interval '60 seconds' THEN GREATEST(connections - 1, 0) ELSE 0 END, connected_at = CASE WHEN CASE WHEN connected_at >= now() - interval '60 seconds' THEN GREATEST(connections - 1, 0) ELSE 0 END < 1 THEN NULL ELSE connected_at END, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn disconnect_all(&self) -> RepoResult<()> {
        sqlx::query("UPDATE memberships SET connections = 0, connected_at = NULL")
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn set_involvement(&self, id: i64, involvement: &str) -> RepoResult<()> {
        // Invalid levels fail the `memberships_involvement_check` CHECK.
        sqlx::query("UPDATE memberships SET involvement = $1, updated_at = now() WHERE id = $2")
            .bind(involvement)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn destroy(&self, id: i64) -> RepoResult<()> {
        sqlx::query("DELETE FROM memberships WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn delete_for_room(&self, room_id: i64) -> RepoResult<()> {
        sqlx::query("DELETE FROM memberships WHERE room_id = $1")
            .bind(room_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn visible_with_rooms(&self, user_id: i64) -> RepoResult<Vec<SidebarMembership>> {
        let rows = sqlx::query_as::<_, SidebarMembership>(
            "SELECT m.id AS membership_id, r.id AS room_id, r.name AS room_name, r.\"type\" AS room_type, \
             (EXTRACT(EPOCH FROM r.updated_at) * 1000)::bigint AS room_updated_epoch, (m.unread_at IS NOT NULL) AS unread \
             FROM memberships m JOIN rooms r ON r.id = m.room_id \
             WHERE m.user_id = $1 AND m.involvement != 'invisible' ORDER BY LOWER(r.name)",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn member_user_ids(&self, room_id: i64) -> RepoResult<Vec<i64>> {
        let ids =
            sqlx::query_scalar::<_, i64>("SELECT user_id FROM memberships WHERE room_id = $1")
                .bind(room_id)
                .fetch_all(&self.pool)
                .await
                .map_err(db)?;
        Ok(ids)
    }

    async fn is_unread(&self, room_id: i64, user_id: i64) -> RepoResult<bool> {
        let unread = sqlx::query_scalar::<_, bool>(
            "SELECT unread_at IS NOT NULL FROM memberships WHERE room_id = $1 AND user_id = $2",
        )
        .bind(room_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(unread.unwrap_or(false))
    }

    async fn grant_to(&self, room_id: i64, involvement: &str, user_ids: &[i64]) -> RepoResult<()> {
        sqlx::query(
            "INSERT INTO memberships (created_at, updated_at, involvement, room_id, user_id) \
             SELECT now(), now(), $1, $2, unnest($3::bigint[]) \
             ON CONFLICT (room_id, user_id) DO NOTHING",
        )
        .bind(involvement)
        .bind(room_id)
        .bind(user_ids)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn revoke_from(&self, room_id: i64, user_ids: &[i64]) -> RepoResult<()> {
        sqlx::query("DELETE FROM memberships WHERE room_id = $1 AND user_id = ANY($2)")
            .bind(room_id)
            .bind(user_ids)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }
}

impl<'r> FromRow<'r, PgRow> for MessageRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            room_id: row.try_get("room_id")?,
            creator_id: row.try_get("creator_id")?,
            client_message_id: row.try_get("client_message_id")?,
        })
    }
}

impl RoomRepository for PgDb {
    async fn find_by_id(&self, id: i64) -> RepoResult<Option<RoomRow>> {
        let row = sqlx::query_as::<_, RoomRow>(
            "SELECT id, creator_id, name, \"type\" AS kind FROM rooms WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn open_ids(&self) -> RepoResult<Vec<i64>> {
        let ids =
            sqlx::query_scalar::<_, i64>("SELECT id FROM rooms WHERE \"type\" = 'Rooms::Open'")
                .fetch_all(&self.pool)
                .await
                .map_err(db)?;
        Ok(ids)
    }

    async fn create(&self, creator_id: i64, name: Option<&str>, kind: &str) -> RepoResult<RoomRow> {
        let row = sqlx::query_as::<_, RoomRow>(ROOM_INSERT)
            .bind(creator_id)
            .bind(name)
            .bind(kind)
            .fetch_one(&self.pool)
            .await
            .map_err(db)?;
        Ok(row)
    }

    async fn list_for_user(&self, user_id: i64) -> RepoResult<Vec<RoomRow>> {
        let rows = sqlx::query_as::<_, RoomRow>(
            "SELECT r.id, r.creator_id, r.name, r.\"type\" AS kind FROM rooms r \
             JOIN memberships m ON m.room_id = r.id \
             WHERE m.user_id = $1 ORDER BY r.name NULLS LAST, r.id",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn original_for_user(&self, user_id: i64) -> RepoResult<Option<RoomRow>> {
        let row = sqlx::query_as::<_, RoomRow>(
            "SELECT r.id, r.creator_id, r.name, r.\"type\" AS kind FROM rooms r \
             JOIN memberships m ON m.room_id = r.id \
             WHERE m.user_id = $1 ORDER BY r.created_at ASC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn original(&self) -> RepoResult<Option<RoomRow>> {
        let row = sqlx::query_as::<_, RoomRow>(
            "SELECT id, creator_id, name, \"type\" AS kind FROM rooms ORDER BY created_at ASC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn updated_ms(&self, id: i64) -> RepoResult<Option<i64>> {
        let ms: Option<i64> = sqlx::query_scalar(
            "SELECT (EXTRACT(EPOCH FROM updated_at) * 1000)::bigint FROM rooms WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(ms)
    }

    async fn last_for_user(&self, user_id: i64) -> RepoResult<Option<RoomRow>> {
        let row = sqlx::query_as::<_, RoomRow>(
            "SELECT r.id, r.creator_id, r.name, r.\"type\" AS kind FROM rooms r \
             JOIN memberships m ON m.room_id = r.id \
             WHERE m.user_id = $1 ORDER BY r.id DESC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn find_for_user(&self, user_id: i64, room_id: i64) -> RepoResult<Option<RoomRow>> {
        let row = sqlx::query_as::<_, RoomRow>(
            "SELECT r.id, r.creator_id, r.name, r.\"type\" AS kind FROM rooms r \
             JOIN memberships m ON m.room_id = r.id \
             WHERE m.user_id = $1 AND r.id = $2 LIMIT 1",
        )
        .bind(user_id)
        .bind(room_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn destroy(&self, id: i64) -> RepoResult<()> {
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn update(&self, id: i64, name: Option<String>, kind: Option<&str>) -> RepoResult<()> {
        sqlx::query(
            "UPDATE rooms SET name = COALESCE($1, name), \"type\" = COALESCE($2, \"type\"), \
             updated_at = now() WHERE id = $3",
        )
        .bind(name)
        .bind(kind)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn find_direct_for(&self, user_ids: &[i64]) -> RepoResult<Option<i64>> {
        let mut wanted: Vec<i64> = user_ids.to_vec();
        wanted.sort_unstable();
        wanted.dedup();
        let rows: Vec<(i64, Vec<i64>)> = sqlx::query_as(
            "SELECT r.id, array_agg(m.user_id ORDER BY m.user_id) \
             FROM rooms r JOIN memberships m ON m.room_id = r.id \
             WHERE r.\"type\" = 'Rooms::Direct' GROUP BY r.id ORDER BY r.id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows
            .into_iter()
            .find(|(_, members)| *members == wanted)
            .map(|(id, _)| id))
    }

    async fn mark_received(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        room_id: i64,
        message_id: i64,
    ) -> RepoResult<()> {
        // Unread stamps for every member except the message author. A missing
        // message yields NULL from the subquery, matching no rows.
        sqlx::query(
            "UPDATE memberships SET unread_at = now(), updated_at = now() \
             WHERE room_id = $1 \
             AND user_id <> (SELECT creator_id FROM messages WHERE id = $2)",
        )
        .bind(room_id)
        .bind(message_id)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        Ok(())
    }
}

impl MessageRepository for PgDb {
    async fn create(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        input: NewMessage,
    ) -> RepoResult<MessageRow> {
        let row = sqlx::query_as::<_, MessageRow>(MESSAGE_INSERT)
            .bind(&input.client_message_id)
            .bind(input.creator_id)
            .bind(input.room_id)
            .bind(&input.body)
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
        sqlx::query(
            "INSERT INTO action_text_rich_texts \
             (created_at, updated_at, body, name, record_id, record_type) \
             VALUES (now(), now(), $1, 'body', $2, 'Message')",
        )
        .bind(&input.body)
        .bind(row.id)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        sqlx::query("UPDATE rooms SET updated_at = now() WHERE id = $1")
            .bind(input.room_id)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
        Ok(row)
    }

    async fn find_by_id(&self, id: i64) -> RepoResult<Option<MessageRow>> {
        let row = sqlx::query_as::<_, MessageRow>(
            "SELECT id, room_id, creator_id, client_message_id FROM messages WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn find_in_room(
        &self,
        room_id: i64,
        before: Option<i64>,
        after: Option<i64>,
        limit: i64,
    ) -> RepoResult<Vec<MessageRow>> {
        let rows = sqlx::query_as::<_, MessageRow>(
            "SELECT id, room_id, creator_id, client_message_id FROM messages WHERE room_id = $1 AND ($2 IS NULL OR id < $2) AND ($3 IS NULL OR id > $3) ORDER BY id DESC LIMIT $4",
        )
        .bind(room_id)
        .bind(before)
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn bot_last_page(&self, room_id: i64) -> RepoResult<Vec<MessageRow>> {
        let mut rows = sqlx::query_as::<_, MessageRow>(
            "SELECT id, room_id, creator_id, client_message_id FROM messages WHERE room_id = $1 ORDER BY created_at DESC LIMIT 40",
        )
        .bind(room_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.reverse();
        Ok(rows)
    }

    async fn bot_page_before(&self, room_id: i64, cursor_id: i64) -> RepoResult<Vec<MessageRow>> {
        let mut rows = sqlx::query_as::<_, MessageRow>(
            "SELECT id, room_id, creator_id, client_message_id FROM messages WHERE room_id = $1 AND created_at < (SELECT created_at FROM messages WHERE id = $2 AND room_id = $1) ORDER BY created_at DESC LIMIT 40",
        )
        .bind(room_id)
        .bind(cursor_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.reverse();
        Ok(rows)
    }

    async fn bot_page_after(&self, room_id: i64, cursor_id: i64) -> RepoResult<Vec<MessageRow>> {
        let rows = sqlx::query_as::<_, MessageRow>(
            "SELECT id, room_id, creator_id, client_message_id FROM messages WHERE room_id = $1 AND created_at > (SELECT created_at FROM messages WHERE id = $2 AND room_id = $1) ORDER BY created_at ASC LIMIT 40",
        )
        .bind(room_id)
        .bind(cursor_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn find_in_room_by_id(&self, room_id: i64, id: i64) -> RepoResult<Option<MessageRow>> {
        let row = sqlx::query_as::<_, MessageRow>(
            "SELECT id, room_id, creator_id, client_message_id FROM messages WHERE room_id = $1 AND id = $2 LIMIT 1",
        )
        .bind(room_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn count_in_room(&self, room_id: i64) -> RepoResult<i64> {
        let count =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages WHERE room_id = $1")
                .bind(room_id)
                .fetch_one(&self.pool)
                .await
                .map_err(db)?;
        Ok(count)
    }

    async fn exists_before(&self, room_id: i64, cursor_id: i64) -> RepoResult<bool> {
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE room_id = $1 AND created_at < (SELECT created_at FROM messages WHERE id = $2 AND room_id = $1))",
        )
        .bind(room_id)
        .bind(cursor_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(exists)
    }

    async fn exists_after(&self, room_id: i64, cursor_id: i64) -> RepoResult<bool> {
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE room_id = $1 AND created_at > (SELECT created_at FROM messages WHERE id = $2 AND room_id = $1))",
        )
        .bind(room_id)
        .bind(cursor_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(exists)
    }

    async fn find_detail_in_room(
        &self,
        room_id: i64,
        id: i64,
    ) -> RepoResult<Option<MessageDetail>> {
        let row = sqlx::query_as::<_, MessageDetail>(
            "SELECT m.id, m.room_id, m.creator_id, m.client_message_id, \
             (EXTRACT(EPOCH FROM m.created_at) * 1000)::bigint AS created_ms, \
             (EXTRACT(EPOCH FROM m.updated_at) * 1000)::bigint AS updated_ms, \
             to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_iso, \
             r.body AS body \
             FROM messages m LEFT JOIN action_text_rich_texts r \
             ON r.record_type = 'Message' AND r.record_id = m.id AND r.name = 'body' \
             WHERE m.room_id = $1 AND m.id = $2 LIMIT 1",
        )
        .bind(room_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn last_page(&self, room_id: i64) -> RepoResult<Vec<MessageDetail>> {
        let mut rows = sqlx::query_as::<_, MessageDetail>(
            "SELECT m.id, m.room_id, m.creator_id, m.client_message_id, \
             (EXTRACT(EPOCH FROM m.created_at) * 1000)::bigint AS created_ms, \
             (EXTRACT(EPOCH FROM m.updated_at) * 1000)::bigint AS updated_ms, \
             to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_iso, \
             r.body AS body \
             FROM messages m LEFT JOIN action_text_rich_texts r \
             ON r.record_type = 'Message' AND r.record_id = m.id AND r.name = 'body' \
             WHERE m.room_id = $1 ORDER BY m.created_at DESC LIMIT 40",
        )
        .bind(room_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.reverse();
        Ok(rows)
    }

    async fn page_before(&self, room_id: i64, anchor_id: i64) -> RepoResult<Vec<MessageDetail>> {
        let mut rows = sqlx::query_as::<_, MessageDetail>(
            "SELECT m.id, m.room_id, m.creator_id, m.client_message_id, \
             (EXTRACT(EPOCH FROM m.created_at) * 1000)::bigint AS created_ms, \
             (EXTRACT(EPOCH FROM m.updated_at) * 1000)::bigint AS updated_ms, \
             to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_iso, \
             r.body AS body \
             FROM messages m LEFT JOIN action_text_rich_texts r \
             ON r.record_type = 'Message' AND r.record_id = m.id AND r.name = 'body' \
             WHERE m.room_id = $1 AND m.created_at < (SELECT created_at FROM messages WHERE id = $2) \
             ORDER BY m.created_at DESC LIMIT 40",
        )
        .bind(room_id)
        .bind(anchor_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.reverse();
        Ok(rows)
    }

    async fn page_after(&self, room_id: i64, anchor_id: i64) -> RepoResult<Vec<MessageDetail>> {
        let rows = sqlx::query_as::<_, MessageDetail>(
            "SELECT m.id, m.room_id, m.creator_id, m.client_message_id, \
             (EXTRACT(EPOCH FROM m.created_at) * 1000)::bigint AS created_ms, \
             (EXTRACT(EPOCH FROM m.updated_at) * 1000)::bigint AS updated_ms, \
             to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_iso, \
             r.body AS body \
             FROM messages m LEFT JOIN action_text_rich_texts r \
             ON r.record_type = 'Message' AND r.record_id = m.id AND r.name = 'body' \
             WHERE m.room_id = $1 AND m.created_at > (SELECT created_at FROM messages WHERE id = $2) \
             ORDER BY m.created_at ASC LIMIT 40",
        )
        .bind(room_id)
        .bind(anchor_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn page_after_id(&self, room_id: i64, after_id: i64) -> RepoResult<Vec<MessageDetail>> {
        let rows = sqlx::query_as::<_, MessageDetail>(
            "SELECT m.id, m.room_id, m.creator_id, m.client_message_id, \
             (EXTRACT(EPOCH FROM m.created_at) * 1000)::bigint AS created_ms, \
             (EXTRACT(EPOCH FROM m.updated_at) * 1000)::bigint AS updated_ms, \
             to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_iso, \
             r.body AS body \
             FROM messages m LEFT JOIN action_text_rich_texts r \
             ON r.record_type = 'Message' AND r.record_id = m.id AND r.name = 'body' \
             WHERE m.room_id = $1 AND m.id > $2 \
             ORDER BY m.created_at ASC LIMIT 40",
        )
        .bind(room_id)
        .bind(after_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn paged(&self, room_id: i64) -> RepoResult<bool> {
        let more: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM messages WHERE room_id = $1 LIMIT 1 OFFSET 40)",
        )
        .bind(room_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(more)
    }

    async fn update_body(&self, id: i64, body: &str) -> RepoResult<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let current: Option<Option<String>> = sqlx::query_scalar(
            "SELECT body FROM action_text_rich_texts \
             WHERE record_type = 'Message' AND record_id = $1 AND name = 'body'",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        if current.flatten().as_deref() == Some(body) {
            tx.rollback().await.map_err(db)?;
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO action_text_rich_texts \
             (created_at, updated_at, body, name, record_id, record_type) \
             VALUES (now(), now(), $1, 'body', $2, 'Message') \
             ON CONFLICT (record_type, record_id, name) \
             DO UPDATE SET body = $1, updated_at = now()",
        )
        .bind(body)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query(
            "UPDATE messages SET updated_at = now(), search_vector = to_tsvector('english', regexp_replace($2, '<[^>]*>', ' ', 'g')) \
             WHERE id = $1",
        )
        .bind(id)
        .bind(body)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query(
            "UPDATE rooms SET updated_at = now() WHERE id = (SELECT room_id FROM messages WHERE id = $1)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn boosts_for_message(&self, message_id: i64) -> RepoResult<Vec<BoostDetail>> {
        let rows = sqlx::query_as::<_, BoostDetail>(
            "SELECT b.id, b.message_id, b.booster_id, u.name AS booster_name, b.content, \
             (EXTRACT(EPOCH FROM b.updated_at) * 1000)::bigint AS updated_ms \
             FROM boosts b JOIN users u ON u.id = b.booster_id \
             WHERE b.message_id = $1 ORDER BY b.created_at ASC",
        )
        .bind(message_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn create_boost(
        &self,
        message_id: i64,
        booster_id: i64,
        content: &str,
    ) -> RepoResult<i64> {
        let id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO boosts (message_id, booster_id, content, created_at, updated_at) \
             VALUES ($1, $2, $3, now(), now()) RETURNING id",
        )
        .bind(message_id)
        .bind(booster_id)
        .bind(content)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(id)
    }

    async fn find_boost(
        &self,
        message_id: i64,
        id: i64,
        booster_id: i64,
    ) -> RepoResult<Option<BoostDetail>> {
        let row = sqlx::query_as::<_, BoostDetail>(
            "SELECT b.id, b.message_id, b.booster_id, u.name AS booster_name, b.content, \
             (EXTRACT(EPOCH FROM b.updated_at) * 1000)::bigint AS updated_ms \
             FROM boosts b JOIN users u ON u.id = b.booster_id \
             WHERE b.message_id = $1 AND b.id = $2 AND b.booster_id = $3",
        )
        .bind(message_id)
        .bind(id)
        .bind(booster_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn delete_boost(&self, id: i64) -> RepoResult<()> {
        sqlx::query("DELETE FROM boosts WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn search_reachable(
        &self,
        user_id: i64,
        query: &str,
        limit: i64,
    ) -> RepoResult<Vec<MessageRow>> {
        // `match_terms` neutralizes tsquery operators so malformed input can
        // never become a driver error; empty terms match nothing, not an error.
        let terms = match_terms(query);
        let rows = sqlx::query_as::<_, MessageRow>(
            "SELECT m.id, m.room_id, m.creator_id, m.client_message_id FROM messages m WHERE m.search_vector @@ plainto_tsquery('english', $2) AND EXISTS (SELECT 1 FROM memberships mb WHERE mb.room_id = m.room_id AND mb.user_id = $1) ORDER BY ts_rank(m.search_vector, plainto_tsquery('english', $2)) DESC LIMIT $3",
        )
        .bind(user_id)
        .bind(terms)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn search_details(
        &self,
        user_id: i64,
        query: &str,
        limit: i64,
    ) -> RepoResult<Vec<MessageDetail>> {
        let terms = match_terms(query);
        let mut rows = sqlx::query_as::<_, MessageDetail>(
            "SELECT m.id, m.room_id, m.creator_id, m.client_message_id, \
             (EXTRACT(EPOCH FROM m.created_at) * 1000)::bigint AS created_ms, \
             (EXTRACT(EPOCH FROM m.updated_at) * 1000)::bigint AS updated_ms, \
             to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_iso, \
             r.body AS body \
             FROM messages m LEFT JOIN action_text_rich_texts r \
             ON r.record_type = 'Message' AND r.record_id = m.id AND r.name = 'body' \
             WHERE m.search_vector @@ plainto_tsquery('english', $2) \
             AND EXISTS (SELECT 1 FROM memberships mb WHERE mb.room_id = m.room_id AND mb.user_id = $1) \
             ORDER BY m.created_at DESC, m.id DESC LIMIT $3",
        )
        .bind(user_id)
        .bind(terms)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.reverse();
        Ok(rows)
    }

    async fn ids_for_room(&self, room_id: i64) -> RepoResult<Vec<i64>> {
        let ids =
            sqlx::query_scalar::<_, i64>("SELECT id FROM messages WHERE room_id = $1 ORDER BY id")
                .bind(room_id)
                .fetch_all(&self.pool)
                .await
                .map_err(db)?;
        Ok(ids)
    }

    async fn destroy(&self, id: i64) -> RepoResult<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let room_id: Option<i64> = sqlx::query_scalar("SELECT room_id FROM messages WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
        let blob_ids: Vec<i64> = sqlx::query_scalar(
            "SELECT blob_id FROM active_storage_attachments \
             WHERE record_type = 'Message' AND record_id = $1 AND name = 'attachment'",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query(
            "DELETE FROM active_storage_attachments \
             WHERE record_type = 'Message' AND record_id = $1 AND name = 'attachment'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        // Boosts first: `boosts.message_id` has no cascade.
        sqlx::query("DELETE FROM boosts WHERE message_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query(
            "DELETE FROM action_text_rich_texts \
             WHERE record_type = 'Message' AND record_id = $1 AND name = 'body'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        if let Some(room_id) = room_id {
            sqlx::query("UPDATE rooms SET updated_at = now() WHERE id = $1")
                .bind(room_id)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
        for blob_id in blob_ids {
            crate::outbox::publish(&mut tx, "purge_blob", &format!("{{\"blob_id\":{blob_id}}}"))
                .await
                .map_err(db)?;
        }
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn ids_for_creator(&self, creator_id: i64) -> RepoResult<Vec<i64>> {
        let ids = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM messages WHERE creator_id = $1 ORDER BY id",
        )
        .bind(creator_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(ids)
    }

    async fn removal_keys(&self, ids: &[i64]) -> RepoResult<Vec<(i64, i64, String)>> {
        let rows = sqlx::query_as::<_, (i64, i64, String)>(
            "SELECT id, room_id, client_message_id FROM messages WHERE id = ANY($1)",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }
}

impl<'r> FromRow<'r, PgRow> for UserRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            email_address: row.try_get("email_address")?,
            role: row.try_get("role")?,
            status: row.try_get("status")?,
        })
    }
}

impl<'r> FromRow<'r, PgRow> for BotRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            bot_token: row.try_get("bot_token")?,
            updated_number: row.try_get("updated_number")?,
        })
    }
}

/// `User.generate_bot_token`: 12 alphanumerics.
fn generate_bot_token() -> String {
    use rand::Rng as _;
    rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(12)
        .map(char::from)
        .collect()
}

impl<'r> FromRow<'r, PgRow> for CredentialsRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            user_id: row.try_get("id")?,
            password_digest: row.try_get("password_digest")?,
        })
    }
}

impl<'r> FromRow<'r, PgRow> for AccountRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            updated_number: row.try_get("updated_number")?,
            join_code: row.try_get("join_code")?,
        })
    }
}

impl<'r> FromRow<'r, PgRow> for MessageDetail {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            room_id: row.try_get("room_id")?,
            creator_id: row.try_get("creator_id")?,
            client_message_id: row.try_get("client_message_id")?,
            created_ms: row.try_get("created_ms")?,
            updated_ms: row.try_get("updated_ms")?,
            created_iso: row.try_get("created_iso")?,
            body: row.try_get("body")?,
        })
    }
}

impl<'r> FromRow<'r, PgRow> for BoostDetail {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            message_id: row.try_get("message_id")?,
            booster_id: row.try_get("booster_id")?,
            booster_name: row.try_get("booster_name")?,
            content: row.try_get("content")?,
            updated_ms: row.try_get("updated_ms")?,
        })
    }
}

impl<'r> FromRow<'r, PgRow> for SidebarUser {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            updated_number: row.try_get("updated_number")?,
        })
    }
}

impl<'r> FromRow<'r, PgRow> for AvatarUser {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            role: row.try_get("role")?,
            updated_number: row.try_get("updated_number")?,
        })
    }
}

impl<'r> FromRow<'r, PgRow> for FormUser {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            bio: row.try_get("bio")?,
            updated_number: row.try_get("updated_number")?,
        })
    }
}

impl<'r> FromRow<'r, PgRow> for SearchRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            query: row.try_get("query")?,
        })
    }
}

impl<'r> FromRow<'r, PgRow> for ProfileUser {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            email_address: row.try_get("email_address")?,
            bio: row.try_get("bio")?,
            role: row.try_get("role")?,
            status: row.try_get("status")?,
            updated_number: row.try_get("updated_number")?,
            avatar_attached: row.try_get("avatar_attached")?,
        })
    }
}

impl<'r> FromRow<'r, PgRow> for ProfileMembership {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            room_id: row.try_get("room_id")?,
            kind: row.try_get("kind")?,
            name: row.try_get("name")?,
            involvement: row.try_get("involvement")?,
        })
    }
}

impl<'r> FromRow<'r, PgRow> for SidebarMembership {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            membership_id: row.try_get("membership_id")?,
            room_id: row.try_get("room_id")?,
            room_name: row.try_get("room_name")?,
            room_type: row.try_get("room_type")?,
            room_updated_epoch: row.try_get("room_updated_epoch")?,
            unread: row.try_get("unread")?,
        })
    }
}

impl SearchRepository for PgDb {
    async fn record(&self, user_id: i64, query: &str) -> RepoResult<()> {
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT id FROM searches WHERE user_id = $1 AND query = $2 LIMIT 1")
                .bind(user_id)
                .bind(query)
                .fetch_optional(&self.pool)
                .await
                .map_err(db)?;
        match existing {
            Some(id) => {
                sqlx::query("UPDATE searches SET updated_at = now() WHERE id = $1")
                    .bind(id)
                    .execute(&self.pool)
                    .await
                    .map_err(db)?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO searches (created_at, updated_at, query, user_id) \
                     VALUES (now(), now(), $2, $1)",
                )
                .bind(user_id)
                .bind(query)
                .execute(&self.pool)
                .await
                .map_err(db)?;
            }
        }
        // `trim_recent_searches`: keep the 10 newest touches.
        sqlx::query(
            "DELETE FROM searches WHERE user_id = $1 AND id NOT IN \
             (SELECT id FROM searches WHERE user_id = $1 ORDER BY updated_at DESC LIMIT 10)",
        )
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn ordered(&self, user_id: i64) -> RepoResult<Vec<SearchRow>> {
        let rows = sqlx::query_as::<_, SearchRow>(
            "SELECT id, query FROM searches WHERE user_id = $1 ORDER BY updated_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn clear(&self, user_id: i64) -> RepoResult<()> {
        sqlx::query("DELETE FROM searches WHERE user_id = $1")
            .bind(user_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }
}

impl UserRepository for PgDb {
    async fn find_by_id(&self, id: i64) -> RepoResult<Option<UserRow>> {
        let row = sqlx::query_as::<_, UserRow>(
            "SELECT id, name, email_address, role, status FROM users WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn find_active_by_email(&self, email: &str) -> RepoResult<Option<UserRow>> {
        let active = UserStatus::Active.value();
        let row = sqlx::query_as::<_, UserRow>(
            "SELECT id, name, email_address, role, status FROM users WHERE email_address = $1 AND status = $2",
        )
        .bind(email)
        .bind(active)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn find_active_credentials_by_email(
        &self,
        email: &str,
    ) -> RepoResult<Option<CredentialsRow>> {
        let active = UserStatus::Active.value();
        let row = sqlx::query_as::<_, CredentialsRow>(
            "SELECT id, password_digest FROM users WHERE email_address = $1 AND status = $2",
        )
        .bind(email)
        .bind(active)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn create(
        &self,
        name: &str,
        email: &str,
        password_digest: Option<&str>,
    ) -> RepoResult<UserRow> {
        let row = sqlx::query_as::<_, UserRow>(
            "INSERT INTO users (created_at, updated_at, name, email_address, password_digest) VALUES (now(), now(), $1, $2, $3) RETURNING id, name, email_address, role, status",
        )
        .bind(name)
        .bind(email)
        .bind(password_digest)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn set_role(&self, id: i64, role: i32) -> RepoResult<()> {
        // Upstream allowlist: member/administrator only, anything else falls
        // back to member (`UserRole::from_value_or_member`).
        let role = UserRole::from_value_or_member(role).value();
        sqlx::query("UPDATE users SET role = $1, updated_at = now() WHERE id = $2")
            .bind(role)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn mark_bot(&self, id: i64) -> RepoResult<()> {
        sqlx::query("UPDATE users SET role = 2, updated_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn active_bots_ordered(&self) -> RepoResult<Vec<BotRow>> {
        let rows = sqlx::query_as::<_, BotRow>(
            "SELECT id, name, bot_token, to_char(updated_at, 'YYYYMMDDHHMMSS') AS updated_number FROM users WHERE status = 0 AND role = 2 ORDER BY LOWER(name)",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn find_active_bot(&self, id: i64) -> RepoResult<Option<BotRow>> {
        let row = sqlx::query_as::<_, BotRow>(
            "SELECT id, name, bot_token, to_char(updated_at, 'YYYYMMDDHHMMSS') AS updated_number FROM users WHERE status = 0 AND role = 2 AND id = $1 LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn authenticate_bot(&self, bot_key: &str) -> RepoResult<Option<BotRow>> {
        let mut parts = bot_key.split('-');
        let (Some(id), Some(token)) = (parts.next(), parts.next()) else {
            return Ok(None);
        };
        let Ok(id) = id.parse::<i64>() else {
            return Ok(None);
        };
        let row = sqlx::query_as::<_, BotRow>(
            "SELECT id, name, bot_token, to_char(updated_at, 'YYYYMMDDHHMMSS') AS updated_number FROM users WHERE status = 0 AND role = 2 AND id = $1 AND bot_token = $2 LIMIT 1",
        )
        .bind(id)
        .bind(token)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn create_bot(&self, name: &str) -> RepoResult<BotRow> {
        let row = sqlx::query_as::<_, BotRow>(
            "INSERT INTO users (created_at, updated_at, name, bot_token, role) VALUES (now(), now(), $1, $2, 2) RETURNING id, name, bot_token, to_char(updated_at, 'YYYYMMDDHHMMSS') AS updated_number",
        )
        .bind(name)
        .bind(generate_bot_token())
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn rename_bot(&self, id: i64, name: &str) -> RepoResult<()> {
        sqlx::query("UPDATE users SET name = $1, updated_at = now() WHERE id = $2")
            .bind(name)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn reset_bot_key(&self, id: i64) -> RepoResult<BotRow> {
        let row = sqlx::query_as::<_, BotRow>(
            "UPDATE users SET bot_token = $1, updated_at = now() WHERE id = $2 RETURNING id, name, bot_token, to_char(updated_at, 'YYYYMMDDHHMMSS') AS updated_number",
        )
        .bind(generate_bot_token())
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn active_bots_in_room(&self, room_id: i64) -> RepoResult<Vec<BotRow>> {
        let rows = sqlx::query_as::<_, BotRow>(
            "SELECT u.id, u.name, u.bot_token, to_char(u.updated_at, 'YYYYMMDDHHMMSS') AS updated_number \
             FROM users u INNER JOIN memberships m ON m.user_id = u.id \
             WHERE m.room_id = $1 AND u.status = 0 AND u.role = 2 ORDER BY LOWER(u.name)",
        )
        .bind(room_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn deactivate(&self, id: i64) -> RepoResult<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(
            "DELETE FROM memberships WHERE user_id = $1 AND room_id NOT IN \
             (SELECT id FROM rooms WHERE \"type\" = 'Rooms::Direct')",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        // Static SQL per table (sqlx 0.9 rejects dynamic strings).
        for sql in [
            "DELETE FROM push_subscriptions WHERE user_id = $1",
            "DELETE FROM searches WHERE user_id = $1",
            "DELETE FROM sessions WHERE user_id = $1",
        ] {
            sqlx::query(sql)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
        // Mangle the email (`-deactivated-<uuid>@`) so the address frees
        // for reuse; uuid v4 from `gen_random_uuid()`. NULL stays NULL
        // (bots have no address) — mangling it to `''` would collide on
        // the second bot deactivation.
        sqlx::query(
            "UPDATE users SET status = $1, updated_at = now(), \
             email_address = CASE WHEN email_address IS NULL THEN NULL \
             ELSE regexp_replace(email_address, '@', '-deactivated-' || gen_random_uuid()::text || '@') END \
             WHERE id = $2",
        )
        .bind(UserStatus::Deactivated.value())
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn ban(&self, id: i64) -> RepoResult<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(
            "INSERT INTO bans (created_at, updated_at, ip_address, user_id) \
             SELECT now(), now(), ip_address, $1 FROM sessions \
             WHERE user_id = $1 AND NULLIF(ip_address, '') IS NOT NULL \
             GROUP BY ip_address",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("UPDATE users SET status = $1, updated_at = now() WHERE id = $2")
            .bind(UserStatus::Banned.value())
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn unban(&self, id: i64) -> RepoResult<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("DELETE FROM bans WHERE user_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("UPDATE users SET status = $1, updated_at = now() WHERE id = $2")
            .bind(UserStatus::Active.value())
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn update_profile(&self, id: i64, update: ProfileUpdate) -> RepoResult<()> {
        sqlx::query(
            "UPDATE users SET updated_at = now(), name = COALESCE($2, name), \
             email_address = COALESCE($3, email_address), bio = COALESCE($4, bio), \
             password_digest = COALESCE($5, password_digest) WHERE id = $1",
        )
        .bind(id)
        .bind(update.name)
        .bind(update.email_address)
        .bind(update.bio)
        .bind(update.password_digest)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn find_profile(&self, id: i64) -> RepoResult<Option<ProfileUser>> {
        let row = sqlx::query_as::<_, ProfileUser>(
            "SELECT u.id, u.name, u.email_address, u.bio, u.role, u.status, \
             to_char(u.updated_at, 'YYYYMMDDHHMMSS') AS updated_number, \
             EXISTS (SELECT 1 FROM active_storage_attachments a \
                     WHERE a.record_type = 'User' AND a.record_id = u.id AND a.name = 'avatar') \
             AS avatar_attached \
             FROM users u WHERE u.id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn memberships_for_profile(&self, user_id: i64) -> RepoResult<Vec<ProfileMembership>> {
        let rows = sqlx::query_as::<_, ProfileMembership>(
            "SELECT m.room_id, r.\"type\" AS kind, r.name, m.involvement \
             FROM memberships m JOIN rooms r ON r.id = m.room_id \
             WHERE m.user_id = $1 ORDER BY LOWER(r.name)",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn set_involvement(
        &self,
        user_id: i64,
        room_id: i64,
        involvement: &str,
    ) -> RepoResult<()> {
        sqlx::query(
            "UPDATE memberships SET involvement = $3, updated_at = now() \
             WHERE user_id = $1 AND room_id = $2",
        )
        .bind(user_id)
        .bind(room_id)
        .bind(involvement)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn members_of_room(&self, room_id: i64) -> RepoResult<Vec<SidebarUser>> {
        let rows = sqlx::query_as::<_, SidebarUser>(
            "SELECT u.id, u.name, to_char(u.updated_at, 'YYYYMMDDHHMMSS') AS updated_number FROM users u \
             JOIN memberships m ON m.user_id = u.id WHERE m.room_id = $1 ORDER BY u.id",
        )
        .bind(room_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn direct_placeholders(
        &self,
        exclude: &[i64],
        limit: i64,
    ) -> RepoResult<Vec<SidebarUser>> {
        let active = UserStatus::Active.value();
        // `<> ALL('{}')` is vacuously true: an empty exclusion still lists.
        let rows = sqlx::query_as::<_, SidebarUser>(
            "SELECT id, name, to_char(updated_at, 'YYYYMMDDHHMMSS') AS updated_number FROM users \
             WHERE status = $1 AND id <> ALL($2) ORDER BY created_at ASC LIMIT $3",
        )
        .bind(active)
        .bind(exclude)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn find_avatar_user(&self, id: i64) -> RepoResult<Option<AvatarUser>> {
        let row = sqlx::query_as::<_, AvatarUser>(
            "SELECT id, name, role, to_char(updated_at, 'YYYYMMDDHHMMSS') AS updated_number FROM users WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn active_ordered(&self) -> RepoResult<Vec<FormUser>> {
        let rows = sqlx::query_as::<_, FormUser>(
            "SELECT id, name, bio, to_char(updated_at, 'YYYYMMDDHHMMSS') AS updated_number \
             FROM users WHERE status = 0 ORDER BY LOWER(name)",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn account_users(&self, include_banned: bool) -> RepoResult<Vec<UserRow>> {
        // Static SQL per branch (sqlx 0.9 rejects dynamic strings).
        let sql = if include_banned {
            "SELECT id, name, email_address, role, status FROM users \
             WHERE status IN (0, 2) AND role != 2 ORDER BY LOWER(name)"
        } else {
            "SELECT id, name, email_address, role, status FROM users \
             WHERE status = 0 AND role != 2 ORDER BY LOWER(name)"
        };
        let rows = sqlx::query_as::<_, UserRow>(sql)
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        Ok(rows)
    }

    async fn where_ids(&self, ids: &[i64]) -> RepoResult<Vec<i64>> {
        let rows =
            sqlx::query_scalar::<_, i64>("SELECT id FROM users WHERE id = ANY($1) ORDER BY id")
                .bind(ids)
                .fetch_all(&self.pool)
                .await
                .map_err(db)?;
        Ok(rows)
    }

    async fn form_users(&self, ids: &[i64]) -> RepoResult<Vec<FormUser>> {
        let rows = sqlx::query_as::<_, FormUser>(
            "SELECT id, name, bio, to_char(updated_at, 'YYYYMMDDHHMMSS') AS updated_number \
             FROM users WHERE id = ANY($1)",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn autocompletable(
        &self,
        room_id: Option<i64>,
        filter: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> RepoResult<Vec<FormUser>> {
        let active = UserStatus::Active.value();
        // `filtered_by`: `%name%`, LIKE metacharacters escaped.
        let pattern = filter.map(|text| {
            format!(
                "%{}%",
                text.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )
        });
        let scope = room_id.unwrap_or(0);
        let rows = sqlx::query_as::<_, FormUser>(
            "SELECT u.id, u.name, u.bio, to_char(u.updated_at, 'YYYYMMDDHHMMSS') AS updated_number \
             FROM users u \
             WHERE u.status = $1 \
             AND ($2 = 0 OR EXISTS (SELECT 1 FROM memberships mb WHERE mb.room_id = $2 AND mb.user_id = u.id)) \
             AND ($3 IS NULL OR u.name ILIKE $3) \
             ORDER BY LOWER(u.name) LIMIT $4 OFFSET $5",
        )
        .bind(active)
        .bind(scope)
        .bind(pattern)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn first_administrator(&self) -> RepoResult<Option<UserRow>> {
        let admin = UserRole::Administrator.value();
        let row = sqlx::query_as::<_, UserRow>(
            "SELECT id, name, email_address, role, status FROM users WHERE role = $1 ORDER BY id LIMIT 1",
        )
        .bind(admin)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn count(&self) -> RepoResult<i64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await
            .map_err(db)?;
        Ok(count)
    }
}

impl AccountRepository for PgDb {
    async fn first(&self) -> RepoResult<Option<AccountRow>> {
        let row = sqlx::query_as::<_, AccountRow>(
            "SELECT id, name, join_code, to_char(updated_at, 'YYYYMMDDHHMMSS') AS updated_number FROM accounts ORDER BY id LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn count(&self) -> RepoResult<i64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
            .fetch_one(&self.pool)
            .await
            .map_err(db)?;
        Ok(count)
    }

    async fn create(&self, name: &str, join_code: &str) -> RepoResult<AccountRow> {
        let row = sqlx::query_as::<_, AccountRow>(
            "INSERT INTO accounts (created_at, updated_at, join_code, name, singleton_guard) VALUES (now(), now(), $1, $2, 0) RETURNING id, name, join_code, to_char(updated_at, 'YYYYMMDDHHMMSS') AS updated_number",
        )
        .bind(join_code)
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn destroy(&self, id: i64) -> RepoResult<()> {
        sqlx::query("DELETE FROM accounts WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn logo_attached(&self, account_id: i64) -> RepoResult<bool> {
        let attached: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM active_storage_attachments WHERE record_type = 'Account' AND record_id = $1 AND name = 'logo')",
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(attached)
    }

    async fn update_name_settings(
        &self,
        id: i64,
        name: Option<&str>,
        settings_json: &str,
    ) -> RepoResult<()> {
        sqlx::query(
            "UPDATE accounts SET name = COALESCE($2, name), \
             settings = (COALESCE(settings, '{}'::jsonb) || $3::jsonb), updated_at = now() \
             WHERE id = $1",
        )
        .bind(id)
        .bind(name)
        .bind(settings_json)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn reset_join_code(&self, id: i64, join_code: &str) -> RepoResult<()> {
        sqlx::query("UPDATE accounts SET join_code = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(join_code)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn custom_styles(&self) -> RepoResult<Option<String>> {
        // `Option<Option<..>>`: no row at all vs a NULL column.
        let css: Option<Option<String>> =
            sqlx::query_scalar("SELECT custom_styles FROM accounts ORDER BY id LIMIT 1")
                .fetch_optional(&self.pool)
                .await
                .map_err(db)?;
        Ok(css.flatten())
    }

    async fn update_custom_styles(&self, id: i64, css: &str) -> RepoResult<()> {
        sqlx::query("UPDATE accounts SET custom_styles = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(css)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn room_creation_restricted(&self) -> RepoResult<bool> {
        let restricted: bool = sqlx::query_scalar(
            "SELECT COALESCE(settings->>'restrict_room_creation_to_administrators' = 'true', false) FROM accounts ORDER BY id LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?
        .unwrap_or(false);
        Ok(restricted)
    }
}

impl<'r> FromRow<'r, PgRow> for SessionRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            user_id: row.try_get("user_id")?,
            token: row.try_get("token")?,
            last_active_at_unix: row.try_get("last_active_at_unix")?,
        })
    }
}

impl SessionRepository for PgDb {
    async fn start(
        &self,
        user_id: i64,
        user_agent: Option<&str>,
        ip_address: Option<&str>,
        expires_in: std::time::Duration,
        token: &str,
    ) -> RepoResult<SessionRow> {
        let row = sqlx::query_as::<_, SessionRow>(
            "INSERT INTO sessions (created_at, updated_at, ip_address, last_active_at, token, user_agent, user_id, expires_at) VALUES (now(), now(), $3, now(), $4, $2, $1, now() + make_interval(secs => $5)) RETURNING id, user_id, token, extract(epoch FROM last_active_at)::bigint AS last_active_at_unix",
        )
        .bind(user_id)
        .bind(user_agent)
        .bind(ip_address)
        .bind(token)
        .bind(expires_in.as_secs_f64())
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn find_by_token(&self, token: &str) -> RepoResult<Option<SessionRow>> {
        let row = sqlx::query_as::<_, SessionRow>(
            "SELECT id, user_id, token, extract(epoch FROM last_active_at)::bigint AS last_active_at_unix FROM sessions WHERE token = $1 AND expires_at > now()",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn resume(
        &self,
        id: i64,
        user_agent: Option<&str>,
        ip_address: Option<&str>,
        expires_in: std::time::Duration,
    ) -> RepoResult<()> {
        sqlx::query(
            "UPDATE sessions SET last_active_at = now(), updated_at = now(), user_agent = COALESCE($2, user_agent), ip_address = COALESCE($3, ip_address), expires_at = now() + make_interval(secs => $4) WHERE id = $1",
        )
        .bind(id)
        .bind(user_agent)
        .bind(ip_address)
        .bind(expires_in.as_secs_f64())
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn destroy(&self, id: i64) -> RepoResult<()> {
        sqlx::query("DELETE FROM sessions WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }
}

impl<'r> FromRow<'r, PgRow> for BlobRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            key: row.try_get("key")?,
            filename: row.try_get("filename")?,
            content_type: row.try_get("content_type")?,
            byte_size: row.try_get("byte_size")?,
            checksum: row.try_get("checksum")?,
            service_name: row.try_get("service_name")?,
        })
    }
}

impl AttachmentRepository for PgDb {
    async fn insert_blob(&self, blob: NewBlob) -> RepoResult<BlobRow> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let row = Self::insert_blob_in(&mut tx, &blob).await?;
        tx.commit().await.map_err(db)?;
        Ok(row)
    }

    async fn find_blob_by_key(&self, key: &str) -> RepoResult<Option<BlobRow>> {
        let row = sqlx::query_as::<_, BlobRow>(
            "SELECT id, key, filename, content_type, byte_size, checksum, service_name FROM active_storage_blobs WHERE key = $1",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn find_blob(&self, id: i64) -> RepoResult<Option<BlobRow>> {
        let row = sqlx::query_as::<_, BlobRow>(
            "SELECT id, key, filename, content_type, byte_size, checksum, service_name FROM active_storage_blobs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn avatar_for_user(&self, user_id: i64) -> RepoResult<Option<BlobRow>> {
        let row = sqlx::query_as::<_, BlobRow>(
            "SELECT b.id, b.key, b.filename, b.content_type, b.byte_size, b.checksum, b.service_name \
             FROM active_storage_blobs b JOIN active_storage_attachments a ON a.blob_id = b.id \
             WHERE a.record_type = 'User' AND a.record_id = $1 AND a.name = 'avatar'",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn blob_for_record(
        &self,
        record_type: &str,
        record_id: i64,
        name: &str,
    ) -> RepoResult<Option<BlobRow>> {
        let row = sqlx::query_as::<_, BlobRow>(
            "SELECT b.id, b.key, b.filename, b.content_type, b.byte_size, b.checksum, b.service_name \
             FROM active_storage_blobs b JOIN active_storage_attachments a ON a.blob_id = b.id \
             WHERE a.record_type = $1 AND a.record_id = $2 AND a.name = $3",
        )
        .bind(record_type)
        .bind(record_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn attachments_for_records(
        &self,
        record_type: &str,
        name: &str,
        record_ids: &[i64],
    ) -> RepoResult<Vec<RecordAttachment>> {
        let rows = sqlx::query(
            "SELECT a.record_id, b.id, b.key, b.filename, b.content_type, b.byte_size, b.checksum, b.service_name, \
             (COALESCE(NULLIF(b.metadata, ''), '{}')::jsonb ->> 'width')::bigint AS width, \
             (COALESCE(NULLIF(b.metadata, ''), '{}')::jsonb ->> 'height')::bigint AS height \
             FROM active_storage_blobs b JOIN active_storage_attachments a ON a.blob_id = b.id \
             WHERE a.record_type = $1 AND a.name = $2 AND a.record_id = ANY($3)",
        )
        .bind(record_type)
        .bind(name)
        .bind(record_ids)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(RecordAttachment {
                record_id: row.try_get("record_id").map_err(db)?,
                blob: BlobRow::from_row(row).map_err(db)?,
                width: row.try_get("width").map_err(db)?,
                height: row.try_get("height").map_err(db)?,
            });
        }
        Ok(out)
    }

    async fn detach_from_record(
        &self,
        record_type: &str,
        record_id: i64,
        name: &str,
    ) -> RepoResult<Option<i64>> {
        let blob_id: Option<i64> = sqlx::query_scalar(
            "DELETE FROM active_storage_attachments WHERE record_type = $1 AND record_id = $2 AND name = $3 RETURNING blob_id",
        )
        .bind(record_type)
        .bind(record_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(blob_id)
    }

    async fn attachments_for_blob(&self, blob_id: i64) -> RepoResult<Vec<(String, i64)>> {
        let rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT record_type, record_id FROM active_storage_attachments WHERE blob_id = $1 ORDER BY id",
        )
        .bind(blob_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn attach_to_record(
        &self,
        record_type: &str,
        record_id: i64,
        name: &str,
        blob_id: i64,
    ) -> RepoResult<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        Self::attach_in(&mut tx, record_type, record_id, name, blob_id).await?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn find_variant(&self, blob_id: i64, digest: &str) -> RepoResult<Option<i64>> {
        let id = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM active_storage_variant_records WHERE blob_id = $1 AND variation_digest = $2",
        )
        .bind(blob_id)
        .bind(digest)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(id)
    }

    async fn record_variant(&self, blob_id: i64, digest: &str) -> RepoResult<i64> {
        let id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO active_storage_variant_records (blob_id, variation_digest) VALUES ($1, $2) ON CONFLICT (blob_id, variation_digest) DO UPDATE SET blob_id = EXCLUDED.blob_id RETURNING id",
        )
        .bind(blob_id)
        .bind(digest)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(id)
    }

    async fn variant_digests(&self, blob_id: i64) -> RepoResult<Vec<String>> {
        let digests = sqlx::query_scalar::<_, String>(
            "SELECT variation_digest FROM active_storage_variant_records WHERE blob_id = $1 ORDER BY id",
        )
        .bind(blob_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(digests)
    }

    async fn update_blob_metadata(
        &self,
        blob_id: i64,
        content_type: Option<&str>,
        metadata_json: &str,
    ) -> RepoResult<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        Self::merge_metadata_in(&mut tx, blob_id, content_type, metadata_json).await?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn delete_blob(&self, blob_id: i64) -> RepoResult<u64> {
        let variants = sqlx::query("DELETE FROM active_storage_variant_records WHERE blob_id = $1")
            .bind(blob_id)
            .execute(&self.pool)
            .await
            .map_err(db)?
            .rows_affected();
        let attachments = sqlx::query("DELETE FROM active_storage_attachments WHERE blob_id = $1")
            .bind(blob_id)
            .execute(&self.pool)
            .await
            .map_err(db)?
            .rows_affected();
        let blob = sqlx::query("DELETE FROM active_storage_blobs WHERE id = $1")
            .bind(blob_id)
            .execute(&self.pool)
            .await
            .map_err(db)?
            .rows_affected();
        Ok(variants + attachments + blob)
    }
}

impl<'r> FromRow<'r, PgRow> for WebhookRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            url: row.try_get("url")?,
            user_id: row.try_get("user_id")?,
        })
    }
}

impl WebhookRepository for PgDb {
    async fn find_webhook(&self, id: i64) -> RepoResult<Option<WebhookRow>> {
        let row =
            sqlx::query_as::<_, WebhookRow>("SELECT id, url, user_id FROM webhooks WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(db)?;
        Ok(row)
    }

    async fn find_by_user(&self, user_id: i64) -> RepoResult<Option<WebhookRow>> {
        let row = sqlx::query_as::<_, WebhookRow>(
            "SELECT id, url, user_id FROM webhooks WHERE user_id = $1 LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn create_webhook(&self, user_id: i64, url: Option<&str>) -> RepoResult<WebhookRow> {
        let row = sqlx::query_as::<_, WebhookRow>(
            "INSERT INTO webhooks (created_at, updated_at, url, user_id) VALUES (now(), now(), $1, $2) RETURNING id, url, user_id",
        )
        .bind(url)
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn set_webhook_url(&self, id: i64, url: &str) -> RepoResult<()> {
        sqlx::query("UPDATE webhooks SET url = $1, updated_at = now() WHERE id = $2")
            .bind(url)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn destroy_webhook(&self, id: i64) -> RepoResult<()> {
        sqlx::query("DELETE FROM webhooks WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn enqueue_bot_deliveries(
        &self,
        room_id: i64,
        direct: bool,
        mentioned_ids: &[i64],
        creator_id: i64,
        message_id: i64,
    ) -> RepoResult<usize> {
        // Eligible bots (active, in the room, webhooked, not the
        // creator), narrowed to mentions outside direct rooms.
        let bots = self.active_bots_in_room(room_id).await?;
        let mut queued = 0usize;
        let mut tx = self.pool.begin().await.map_err(db)?;
        for bot in bots {
            if bot.id == creator_id {
                continue;
            }
            if !direct && !mentioned_ids.contains(&bot.id) {
                continue;
            }
            let webhook: Option<i64> =
                sqlx::query_scalar("SELECT id FROM webhooks WHERE user_id = $1 LIMIT 1")
                    .bind(bot.id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db)?;
            if webhook.is_none() {
                continue;
            }
            let payload = format!("{{\"bot_id\":{},\"message_id\":{message_id}}}", bot.id);
            crate::outbox::publish(&mut tx, "deliver_webhook", &payload)
                .await
                .map_err(db)?;
            queued += 1;
        }
        tx.commit().await.map_err(db)?;
        Ok(queued)
    }

    async fn find_delivery_reply(&self, delivery_key: &str) -> RepoResult<Option<i64>> {
        let reply = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT reply_message_id FROM webhook_deliveries WHERE delivery_key = $1",
        )
        .bind(delivery_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?
        .flatten();
        Ok(reply)
    }

    async fn claim_delivery(&self, delivery_key: &str) -> RepoResult<bool> {
        let won = sqlx::query_scalar::<_, i64>(
            "INSERT INTO webhook_deliveries (delivery_key) VALUES ($1) ON CONFLICT (delivery_key) DO NOTHING RETURNING id",
        )
        .bind(delivery_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(won.is_some())
    }

    async fn set_delivery_reply(&self, delivery_key: &str, reply_id: i64) -> RepoResult<()> {
        sqlx::query("UPDATE webhook_deliveries SET reply_message_id = $2 WHERE delivery_key = $1")
            .bind(delivery_key)
            .bind(reply_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }
}

impl<'r> FromRow<'r, PgRow> for PushSubscriptionRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            user_id: row.try_get("user_id")?,
            endpoint: row.try_get("endpoint")?,
            p256dh_key: row.try_get("p256dh_key")?,
            auth_key: row.try_get("auth_key")?,
            user_agent: row.try_get("user_agent")?,
        })
    }
}

impl PushSubscriptionRepository for PgDb {
    async fn push_subscriptions_for_user(
        &self,
        user_id: i64,
    ) -> RepoResult<Vec<PushSubscriptionRow>> {
        let rows = sqlx::query_as::<_, PushSubscriptionRow>(
            "SELECT id, user_id, endpoint, p256dh_key, auth_key, user_agent FROM push_subscriptions WHERE user_id = $1 ORDER BY id",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }

    async fn find_push_subscription(
        &self,
        user_id: i64,
        id: i64,
    ) -> RepoResult<Option<PushSubscriptionRow>> {
        let row = sqlx::query_as::<_, PushSubscriptionRow>(
            "SELECT id, user_id, endpoint, p256dh_key, auth_key, user_agent FROM push_subscriptions WHERE user_id = $1 AND id = $2",
        )
        .bind(user_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn find_push_subscription_by_params(
        &self,
        user_id: i64,
        endpoint: &str,
        p256dh_key: Option<&str>,
        auth_key: Option<&str>,
    ) -> RepoResult<Option<PushSubscriptionRow>> {
        let row = sqlx::query_as::<_, PushSubscriptionRow>(
            "SELECT id, user_id, endpoint, p256dh_key, auth_key, user_agent FROM push_subscriptions WHERE user_id = $1 AND endpoint = $2 AND p256dh_key IS NOT DISTINCT FROM $3 AND auth_key IS NOT DISTINCT FROM $4 LIMIT 1",
        )
        .bind(user_id)
        .bind(endpoint)
        .bind(p256dh_key)
        .bind(auth_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn create_push_subscription(
        &self,
        user_id: i64,
        endpoint: &str,
        p256dh_key: Option<&str>,
        auth_key: Option<&str>,
        user_agent: Option<&str>,
    ) -> RepoResult<PushSubscriptionRow> {
        let row = sqlx::query_as::<_, PushSubscriptionRow>(
            "INSERT INTO push_subscriptions (created_at, updated_at, user_id, endpoint, p256dh_key, auth_key, user_agent) VALUES (now(), now(), $1, $2, $3, $4, $5) RETURNING id, user_id, endpoint, p256dh_key, auth_key, user_agent",
        )
        .bind(user_id)
        .bind(endpoint)
        .bind(p256dh_key)
        .bind(auth_key)
        .bind(user_agent)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    async fn touch_push_subscription(&self, id: i64) -> RepoResult<()> {
        sqlx::query("UPDATE push_subscriptions SET updated_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn destroy_push_subscription(&self, user_id: i64, id: i64) -> RepoResult<bool> {
        let removed = sqlx::query("DELETE FROM push_subscriptions WHERE user_id = $1 AND id = $2")
            .bind(user_id)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(removed.rows_affected() > 0)
    }

    async fn destroy_push_subscriptions_by_endpoint(
        &self,
        user_id: i64,
        endpoint: &str,
    ) -> RepoResult<u64> {
        let removed =
            sqlx::query("DELETE FROM push_subscriptions WHERE user_id = $1 AND endpoint = $2")
                .bind(user_id)
                .bind(endpoint)
                .execute(&self.pool)
                .await
                .map_err(db)?;
        Ok(removed.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_is_lowercase_hex() {
        let key = session_key(&[0xABu8; 32]);
        assert_eq!(key.len(), 64);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(&key[..4], "abab");
        assert_eq!(session_key(&[0u8; 32]).len(), 64);
    }
}
