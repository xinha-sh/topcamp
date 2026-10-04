//! Transactional outbox relay (`migrations/0002_outbox.sql`, §14).
//!
//! The message-creation transaction inserts domain rows AND outbox rows
//! atomically; a relay claims rows after commit and dispatches them, then
//! marks them done. Topics mirror the upstream `Event` enum
//! (`push_message`, `deliver_webhook`, `remove_banned_content`,
//! `purge_blob`); `disconnect_user` is a synchronous cable broadcast and
//! never enters the outbox.

/// Claim shape (`0002_outbox.sql`, expiry added in `0004_workflows.sql`):
/// pending rows only, oldest first, locked without blocking concurrent
/// relays. Claims expire after 5 minutes: a relay killed mid-dispatch
/// leaves its rows claimed, and expiry hands them to the next pass —
/// re-dispatch is safe because workflow ids are deterministic
/// (`outbox:{row_id}` rejoins the recorded execution). `payload::text`
/// keeps the row mappable without extra JSON dependencies.
pub const CLAIM_QUERY: &str = "SELECT id, topic, payload::text AS payload \
    FROM outbox \
    WHERE done_at IS NULL AND (claimed_at IS NULL OR claimed_at < now() - interval '5 minutes') \
    ORDER BY id LIMIT $1 \
    FOR UPDATE SKIP LOCKED";

/// One pending outbox row.
pub struct OutboxEntry {
    pub id: i64,
    pub topic: String,
    pub payload: String,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for OutboxEntry {
    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row as _;
        Ok(Self {
            id: row.try_get("id")?,
            topic: row.try_get("topic")?,
            payload: row.try_get("payload")?,
        })
    }
}

/// Claim up to `limit` pending rows inside the caller's transaction and stamp
/// them claimed (with an attempt bump) so concurrent relays skip them.
pub async fn claim_batch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    limit: i64,
) -> Result<Vec<OutboxEntry>, sqlx::Error> {
    let entries = sqlx::query_as::<_, OutboxEntry>(CLAIM_QUERY)
        .bind(limit)
        .fetch_all(&mut **tx)
        .await?;
    if !entries.is_empty() {
        let ids: Vec<i64> = entries.iter().map(|entry| entry.id).collect();
        sqlx::query(
            "UPDATE outbox SET claimed_at = now(), attempts = attempts + 1 WHERE id = ANY($1)",
        )
        .bind(ids)
        .execute(&mut **tx)
        .await?;
    }
    Ok(entries)
}

/// Publish an event inside the caller's transaction (`migrations/0002_outbox.sql`).
/// `payload_json` must be a JSON document (callers serialize ids only, so
/// plain `format!` interpolation of integers is injection-safe); the
/// `outbox_topic_check` constraint rejects unknown topics.
pub async fn publish(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    topic: &str,
    payload_json: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO outbox (topic, payload) VALUES ($1, $2::jsonb)")
        .bind(topic)
        .bind(payload_json)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Mark a dispatched row done inside the caller's transaction.
pub async fn mark_done(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE outbox SET done_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Release a claimed row back to pending (failed dispatch). The attempt
/// bump from [`claim_batch`] stays, so poison rows are visible via
/// `attempts` while a later relay pass retries them.
pub async fn release(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE outbox SET claimed_at = NULL WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
