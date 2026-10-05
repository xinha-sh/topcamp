//! Repository traits, one per aggregate ("Repositories", §12).
//!
//! The `DatabaseService` god-type is explicitly rejected: callers depend only
//! on the trait(s) they need. Timestamps are written via SQL `now()`, so no
//! clock/time dependency leaks into these signatures. Implementors map
//! `sqlx::Error` to [`topcamp_domain::error::Error`] via [`crate::DbError`].

use topcamp_domain::error::Error;

pub type RepoResult<T> = Result<T, Error>;

// --- users ---------------------------------------------------------------

/// Mirrors `db/models/user.rs`: email lookup for `authenticate_by`, role
/// allowlist changes, deactivate-on-destroy (rows are never deleted).
pub struct UserRow {
    pub id: i64,
    pub name: String,
    pub email_address: Option<String>,
    pub role: i32,
    pub status: i32,
}

/// A bot with its API token (`users` with `role = Bot`). Separate
/// from [`UserRow`] so token lookups never widen session/user reads.
pub struct BotRow {
    pub id: i64,
    pub name: String,
    pub bot_token: Option<String>,
    pub updated_number: String,
}

impl BotRow {
    /// `user.bot_key`: `"#{id}-#{bot_token}"`, `None` untokened.
    pub fn bot_key(&self) -> Option<String> {
        self.bot_token
            .as_deref()
            .map(|token| format!("{}-{token}", self.id))
    }
}

/// Credentials for password login. Separate from [`UserRow`] so password
/// digests never ride along on session/user lookups.
pub struct CredentialsRow {
    pub user_id: i64,
    pub password_digest: Option<String>,
}

/// Sidebar identity: id, name, and the `v=` cache key for avatar URLs.
#[derive(Clone, Debug, PartialEq)]
pub struct SidebarUser {
    pub id: i64,
    pub name: String,
    pub updated_number: String,
}

/// Avatar lookup: identity plus role (bot fallback) and cache key.
#[derive(Clone, Debug)]
pub struct AvatarUser {
    pub id: i64,
    pub name: String,
    pub role: i32,
    pub updated_number: String,
}

/// Profile form submission: `None` keeps the stored value.
pub struct ProfileUpdate {
    pub name: Option<String>,
    pub email_address: Option<String>,
    pub bio: Option<String>,
    pub password_digest: Option<String>,
}

/// Profile display row: identity, contact, bio, avatar presence.
pub struct ProfileUser {
    pub id: i64,
    pub name: String,
    pub email_address: Option<String>,
    pub bio: Option<String>,
    pub role: i32,
    pub status: i32,
    pub updated_number: String,
    pub avatar_attached: bool,
}

/// One profile memberships-menu row.
pub struct ProfileMembership {
    pub room_id: i64,
    pub kind: String,
    pub name: Option<String>,
    pub involvement: String,
}

/// Room-form user: identity plus bio (the `title` tooltip) and the
/// avatar `v=` cache key.
#[derive(Clone, Debug)]
pub struct FormUser {
    pub id: i64,
    pub name: String,
    pub bio: Option<String>,
    pub updated_number: String,
}

#[allow(async_fn_in_trait)]
pub trait UserRepository {
    async fn find_by_id(&self, id: i64) -> RepoResult<Option<UserRow>>;
    async fn find_active_by_email(&self, email: &str) -> RepoResult<Option<UserRow>>;
    /// Active user id + digest for `POST /session`; `None` digest means the
    /// account has no password (OAuth/bot) and can never password-login.
    async fn find_active_credentials_by_email(
        &self,
        email: &str,
    ) -> RepoResult<Option<CredentialsRow>>;
    /// `User.administrator.first`, for the sign-in help contact. Any
    /// status: a deactivated install still shows whom to email.
    async fn first_administrator(&self) -> RepoResult<Option<UserRow>>;
    /// Total user rows, for the sign-in page's first-run redirect.
    async fn count(&self) -> RepoResult<i64>;
    async fn create(
        &self,
        name: &str,
        email: &str,
        password_digest: Option<&str>,
    ) -> RepoResult<UserRow>;
    async fn set_role(&self, id: i64, role: i32) -> RepoResult<()>;
    /// Upstream `User#bot!`: role flip only, no token reset.
    async fn mark_bot(&self, id: i64) -> RepoResult<()>;
    /// `User.active_bots.ordered`: active bots by `LOWER(name)`.
    async fn active_bots_ordered(&self) -> RepoResult<Vec<BotRow>>;
    /// `User.active_bots.find(id)`.
    async fn find_active_bot(&self, id: i64) -> RepoResult<Option<BotRow>>;
    /// `User.authenticate_bot(bot_key)`: `"#{id}-#{bot_token}"`
    /// (only the first two dash segments count). Non-integer ids
    /// find nothing — Postgres would raise where SQLite shrugs.
    async fn authenticate_bot(&self, bot_key: &str) -> RepoResult<Option<BotRow>>;
    /// `User.create_bot!(name)`: bot row with a fresh token (the
    /// webhook and room memberships are the caller's, like
    /// upstream's separate steps).
    async fn create_bot(&self, name: &str) -> RepoResult<BotRow>;
    /// Bot rename (`update_bot!` without the webhook half).
    async fn rename_bot(&self, id: i64, name: &str) -> RepoResult<()>;
    /// `bot.reset_bot_key`: fresh token, returned with the bot.
    async fn reset_bot_key(&self, id: i64) -> RepoResult<BotRow>;
    /// `room.active_bots`: active bots holding a membership, by
    /// `LOWER(name)` (the direct-room webhook candidates).
    async fn active_bots_in_room(&self, room_id: i64) -> RepoResult<Vec<BotRow>>;
    /// Full `deactivate`: drop non-direct memberships, push
    /// subscriptions, searches and sessions, then mark deactivated
    /// with a mangled email (frees the address for reuse).
    async fn deactivate(&self, id: i64) -> RepoResult<()>;
    /// `ban`: record the session IPs, drop sessions, mark banned.
    /// Message destruction + remove broadcasts happen in the caller
    /// (upstream's `RemoveBannedContentJob`, run inline).
    async fn ban(&self, id: i64) -> RepoResult<()>;
    /// `unban`: drop IP bans, mark active.
    async fn unban(&self, id: i64) -> RepoResult<()>;
    /// Profile update: `None` fields keep their values (upstream's
    /// `.compact`); blank passwords never reach here (kept).
    async fn update_profile(&self, id: i64, update: ProfileUpdate) -> RepoResult<()>;
    /// One user's profile display row (show + profile pages).
    async fn find_profile(&self, id: i64) -> RepoResult<Option<ProfileUser>>;
    /// `memberships.with_ordered_room`: rooms by `LOWER(name)` with
    /// involvement, for the profile memberships menu.
    async fn memberships_for_profile(&self, user_id: i64) -> RepoResult<Vec<ProfileMembership>>;
    /// Set a membership's involvement level.
    async fn set_involvement(
        &self,
        user_id: i64,
        room_id: i64,
        involvement: &str,
    ) -> RepoResult<()>;
    /// `room.users`: members by id, for direct-room sidebar entries.
    async fn members_of_room(&self, room_id: i64) -> RepoResult<Vec<SidebarUser>>;
    /// `find_direct_placeholder_users`: active users outside `exclude`,
    /// oldest first, capped at `limit`.
    async fn direct_placeholders(
        &self,
        exclude: &[i64],
        limit: i64,
    ) -> RepoResult<Vec<SidebarUser>>;
    /// Avatar route lookup by id.
    async fn find_avatar_user(&self, id: i64) -> RepoResult<Option<AvatarUser>>;
    /// `User.active.ordered`: active users by `LOWER(name)`, for the
    /// room access forms.
    async fn active_ordered(&self) -> RepoResult<Vec<FormUser>>;
    /// `account_users`: active users (+banned when admins look), no
    /// bots, by `LOWER(name)` — the account page people list.
    async fn account_users(&self, include_banned: bool) -> RepoResult<Vec<UserRow>>;
    /// `User.where(id:)`: the ids that exist, in id order (unknown ids
    /// in a `user_ids[]` submission silently drop).
    async fn where_ids(&self, ids: &[i64]) -> RepoResult<Vec<i64>>;
    /// `User.where(id:)` rows with bio + avatar key, for message
    /// creators and boosters (unknown ids drop, like `where_ids`).
    async fn form_users(&self, ids: &[i64]) -> RepoResult<Vec<FormUser>>;
    /// Autocompletable users: active, `LOWER(name)` order, optional
    /// room scope + `%filter%` name match (LIKE wildcards escaped).
    async fn autocompletable(
        &self,
        room_id: Option<i64>,
        filter: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> RepoResult<Vec<FormUser>>;
}

// --- accounts ------------------------------------------------------------

/// Singleton row: install name + logo cache key for the sign-in page.
/// `updated_number` is `updated_at` as `YYYYMMDDHHMMSS` (Rails
/// `to_fs(:number)`), formatted in SQL so no clock dependency leaks in.
pub struct AccountRow {
    pub id: i64,
    pub name: String,
    pub updated_number: String,
    pub join_code: String,
}

#[allow(async_fn_in_trait)]
pub trait AccountRepository {
    async fn first(&self) -> RepoResult<Option<AccountRow>>;
    /// Singleton presence, for the first-run guard (`prevent_repeats`).
    async fn count(&self) -> RepoResult<i64>;
    /// Create the singleton (`FirstRun.create`): `join_code` is caller-
    /// generated (`XXXX-XXXX-XXXX`); the unique `singleton_guard` rejects
    /// a rival first-run with a unique violation.
    async fn create(&self, name: &str, join_code: &str) -> RepoResult<AccountRow>;
    /// `settings.restrict_room_creation_to_administrators?`: only the
    /// literal `"true"` counts (missing settings or key → false).
    async fn room_creation_restricted(&self) -> RepoResult<bool>;
    /// Delete by id. Compensation only: a first-run that created the
    /// account but failed a later step removes it so the install stays
    /// fresh (no account-without-users redirect loop).
    async fn destroy(&self, id: i64) -> RepoResult<()>;
    /// `accounts#update`: rename (`None` keeps) + merge `settings_json`
    /// (a JSON object document) into `settings`, touching `updated_at`
    /// (the logo `?v=` cache-buster rides it).
    async fn update_name_settings(
        &self,
        id: i64,
        name: Option<&str>,
        settings_json: &str,
    ) -> RepoResult<()>;
    /// `reset_join_code`: replace the invite code (caller-generated).
    async fn reset_join_code(&self, id: i64, join_code: &str) -> RepoResult<()>;
    /// Whether the account has an attached `logo` blob (nav +
    /// invitation render the logo only then).
    async fn logo_attached(&self, account_id: i64) -> RepoResult<bool>;
    /// The singleton's `custom_styles` CSS (`UI-17`), if any.
    async fn custom_styles(&self) -> RepoResult<Option<String>>;
    /// `custom_styles#update`: replace the CSS, touching `updated_at`.
    async fn update_custom_styles(&self, id: i64, css: &str) -> RepoResult<()>;
}

// --- rooms ---------------------------------------------------------------

/// STI `type` column carries the 3 room class names as plain text.
pub struct RoomRow {
    pub id: i64,
    pub creator_id: i64,
    pub name: Option<String>,
    pub kind: String,
}

#[allow(async_fn_in_trait)]
pub trait RoomRepository {
    async fn find_by_id(&self, id: i64) -> RepoResult<Option<RoomRow>>;
    async fn create(&self, creator_id: i64, name: Option<&str>, kind: &str) -> RepoResult<RoomRow>;
    /// `Rooms::Open` ids, for granting new bots their memberships.
    async fn open_ids(&self) -> RepoResult<Vec<i64>>;
    /// Rooms the user belongs to, name-ordered, for the landing page.
    async fn list_for_user(&self, user_id: i64) -> RepoResult<Vec<RoomRow>>;
    /// `user.rooms.original`: oldest membership by room creation, the
    /// welcome redirect's fallback when no room was visited yet.
    async fn original_for_user(&self, user_id: i64) -> RepoResult<Option<RoomRow>>;
    /// `Room.original`: the oldest room overall (the invitation room).
    async fn original(&self) -> RepoResult<Option<RoomRow>>;
    /// `room.updated_at` as epoch milliseconds (the refresh
    /// controller's `loaded_at`).
    async fn updated_ms(&self, id: i64) -> RepoResult<Option<i64>>;
    /// `user.rooms.last`: newest membership by room id, for `GET /rooms`.
    async fn last_for_user(&self, user_id: i64) -> RepoResult<Option<RoomRow>>;
    /// `user.rooms.find_by(id:)`: membership-scoped lookup backing
    /// `set_room` (unknown or inaccessible rooms redirect home).
    async fn find_for_user(&self, user_id: i64, room_id: i64) -> RepoResult<Option<RoomRow>>;
    /// Delete the room row. Callers destroy memberships + messages first
    /// (upstream `Room#destroy` order); the row goes last.
    async fn destroy(&self, id: i64) -> RepoResult<()>;
    /// `Room#update!`: set name and/or type, touching `updated_at`.
    /// Callers compare first and skip the call when nothing changed
    /// (upstream only writes on change). The direct-type guard lives
    /// in the caller too: typed `set_room` scopes reject directs
    /// before updates ever see them.
    async fn update(&self, id: i64, name: Option<String>, kind: Option<&str>) -> RepoResult<()>;
    /// `Rooms::Direct.find_for`: the direct room whose member set is
    /// exactly `user_ids`, if any.
    async fn find_direct_for(&self, user_ids: &[i64]) -> RepoResult<Option<i64>>;
    /// `Room::receive`: runs on message create (unread memberships + push
    /// scheduling via the outbox). Takes the caller's tx so the unread
    /// stamps commit atomically with the message + outbox rows.
    async fn mark_received(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        room_id: i64,
        message_id: i64,
    ) -> RepoResult<()>;
}

// --- memberships ---------------------------------------------------------

pub struct MembershipRow {
    pub id: i64,
    pub room_id: i64,
    pub user_id: i64,
    pub involvement: String,
}

/// Sidebar entry: membership + room display fields. `room_updated_epoch`
/// feeds the directs strip's freshness attribute.
pub struct SidebarMembership {
    pub membership_id: i64,
    pub room_id: i64,
    pub room_name: Option<String>,
    pub room_type: String,
    pub room_updated_epoch: i64,
    pub unread: bool,
}

#[allow(async_fn_in_trait)]
pub trait MembershipRepository {
    async fn find(&self, room_id: i64, user_id: i64) -> RepoResult<Option<MembershipRow>>;
    async fn create(&self, room_id: i64, user_id: i64) -> RepoResult<MembershipRow>;
    /// Presence subscribe (`connected`): revive-or-bump the connection
    /// count, stamp `connected_at`. A stale membership (no `connected_at`
    /// within the 60s TTL) restarts at 1 instead of accumulating.
    async fn mark_connected(&self, id: i64) -> RepoResult<()>;
    /// Presence `present`: `mark_connected` plus clears `unread_at`
    /// (viewing the room marks it read).
    async fn mark_present(&self, id: i64) -> RepoResult<()>;
    /// Presence `refresh`: revive a stale membership's count to 1;
    /// a fresh membership is untouched.
    async fn mark_refreshed(&self, id: i64) -> RepoResult<()>;
    /// Presence unsubscribe / `absent` (`disconnected`): decrement a
    /// fresh membership, zero a stale one; a drained membership clears
    /// its `connected_at`.
    async fn mark_disconnected(&self, id: i64) -> RepoResult<()>;
    /// Boot reset (`Membership.disconnect_all`): clears every connection.
    async fn disconnect_all(&self) -> RepoResult<()>;
    async fn set_involvement(&self, id: i64, involvement: &str) -> RepoResult<()>;
    async fn destroy(&self, id: i64) -> RepoResult<()>;
    /// Delete every membership of a room (room destroy's first step).
    async fn delete_for_room(&self, room_id: i64) -> RepoResult<()>;
    /// All member user ids of a room, for realtime fanout.
    async fn member_user_ids(&self, room_id: i64) -> RepoResult<Vec<i64>>;
    /// `membership.unread?`: an `unread_at` stamp is set.
    async fn is_unread(&self, room_id: i64, user_id: i64) -> RepoResult<bool>;
    /// `visible_with_ordered_room`: non-invisible memberships with room
    /// fields, `LOWER(name)`-ordered, for the sidebar.
    async fn visible_with_rooms(&self, user_id: i64) -> RepoResult<Vec<SidebarMembership>>;
    /// `memberships.grant_to(users)`: insert with `involvement`,
    /// skipping existing members (`ON CONFLICT DO NOTHING`).
    async fn grant_to(&self, room_id: i64, involvement: &str, user_ids: &[i64]) -> RepoResult<()>;
    /// `memberships.revoke_from(users)`: delete by room + user ids.
    async fn revoke_from(&self, room_id: i64, user_ids: &[i64]) -> RepoResult<()>;
}

// --- messages ------------------------------------------------------------

pub struct NewMessage {
    pub room_id: i64,
    pub creator_id: i64,
    pub client_message_id: String,
    /// Stored body (plain text from the API, block HTML from the
    /// composer): written to the `body` RichTextRecord and indexed into
    /// `search_vector` with tags stripped (repositories maintain the
    /// index, not a trigger — see `migrations/0002_outbox.sql`).
    pub body: String,
}

pub struct MessageRow {
    pub id: i64,
    pub room_id: i64,
    pub creator_id: i64,
    pub client_message_id: String,
}

/// A message with its `body` rich text and timestamps, for page
/// rendering. `created_iso` is UTC `YYYY-MM-DDTHH:MM:SSZ` (second
/// precision, matching upstream `iso8601`); the `*_ms` fields are
/// epoch milliseconds for `data-message-timestamp` and ETags.
#[derive(Clone, Debug)]
pub struct MessageDetail {
    pub id: i64,
    pub room_id: i64,
    pub creator_id: i64,
    pub client_message_id: String,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub created_iso: String,
    pub body: Option<String>,
}

/// One saved recent search (`searches.ordered`).
#[derive(Clone, Debug)]
pub struct SearchRow {
    pub id: i64,
    pub query: String,
}

/// `Search`: recent-search recording + listing + clearing.
#[allow(async_fn_in_trait)]
pub trait SearchRepository {
    /// `Search.record`: find-or-create by (user, query), touch it,
    /// trim the user's recents to 10.
    async fn record(&self, user_id: i64, query: &str) -> RepoResult<()>;
    /// Recents, most-recently-touched first.
    async fn ordered(&self, user_id: i64) -> RepoResult<Vec<SearchRow>>;
    /// Destroy all of the user's searches.
    async fn clear(&self, user_id: i64) -> RepoResult<()>;
}

/// A boost with its booster's display fields (`boosts.ordered`).
pub struct BoostDetail {
    pub id: i64,
    pub message_id: i64,
    pub booster_id: i64,
    pub booster_name: String,
    pub content: String,
    pub updated_ms: i64,
}

#[allow(async_fn_in_trait)]
pub trait MessageRepository {
    /// Insert row + body `RichTextRecord` + attachment + touches inside the
    /// caller's tx (which also inserts the outbox rows); `client_message_id`
    /// has NO unique constraint upstream — do not enforce one.
    async fn create(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        input: NewMessage,
    ) -> RepoResult<MessageRow>;
    async fn find_by_id(&self, id: i64) -> RepoResult<Option<MessageRow>>;
    /// `find_in_room`-scoped pagination (`before`/`after`/last-page).
    async fn find_in_room(
        &self,
        room_id: i64,
        before: Option<i64>,
        after: Option<i64>,
        limit: i64,
    ) -> RepoResult<Vec<MessageRow>>;
    /// `Message::Pagination` for the bot API: the newest 40 oldest
    /// first / 40 before / 40 after a cursor, `created_at`-ordered
    /// (ties excluded both ways, like Rails; cursors are message
    /// ids, their timestamps compared in-query).
    async fn bot_last_page(&self, room_id: i64) -> RepoResult<Vec<MessageRow>>;
    async fn bot_page_before(&self, room_id: i64, cursor_id: i64) -> RepoResult<Vec<MessageRow>>;
    async fn bot_page_after(&self, room_id: i64, cursor_id: i64) -> RepoResult<Vec<MessageRow>>;
    /// `room.messages.find(id)`: the cursor lookup (missing → 404).
    async fn find_in_room_by_id(&self, room_id: i64, id: i64) -> RepoResult<Option<MessageRow>>;
    /// `Message::Pagination` header queries: room count + whether
    /// older/newer messages exist past a cursor.
    async fn count_in_room(&self, room_id: i64) -> RepoResult<i64>;
    async fn exists_before(&self, room_id: i64, cursor_id: i64) -> RepoResult<bool>;
    async fn exists_after(&self, room_id: i64, cursor_id: i64) -> RepoResult<bool>;
    /// All message ids created by a user, oldest first. Snapshot read for
    /// the moderation workflow (`RemoveBannedContent` reuses the recorded
    /// list on resume instead of re-scanning).
    async fn ids_for_creator(&self, creator_id: i64) -> RepoResult<Vec<i64>>;
    /// `(id, room_id, client_message_id)` for message ids: live-bus
    /// remove keys (the ban handler emits removes inline while the
    /// workflow destroys durable-side).
    async fn removal_keys(&self, ids: &[i64]) -> RepoResult<Vec<(i64, i64, String)>>;
    /// Reachable-scope full-text search (`plainto_tsquery` AND semantics,
    /// `ts_rank` ordering, caller-enforced 100-row cap).
    async fn search_reachable(
        &self,
        user_id: i64,
        query: &str,
        limit: i64,
    ) -> RepoResult<Vec<MessageRow>>;
    /// Newest-`limit` reachable full-text hits with bodies, oldest
    /// first (upstream `reachable_messages.search(q).last(100)`).
    async fn search_details(
        &self,
        user_id: i64,
        query: &str,
        limit: i64,
    ) -> RepoResult<Vec<MessageDetail>>;
    /// All message ids in a room, oldest first (room destroy's sweep).
    async fn ids_for_room(&self, room_id: i64) -> RepoResult<Vec<i64>>;
    /// `Message#destroy`: boosts, body rich text, and the attachment row
    /// go with the row; detached blobs publish `purge_blob` for the
    /// worker relay, and the room is touched. Boosts must go first:
    /// `boosts.message_id` has no cascade.
    async fn destroy(&self, id: i64) -> RepoResult<()>;
    /// `@room.messages.find(id)` with body + timestamps, for pages.
    async fn find_detail_in_room(&self, room_id: i64, id: i64)
        -> RepoResult<Option<MessageDetail>>;
    /// `room.messages.last_page`: newest 40, oldest first.
    async fn last_page(&self, room_id: i64) -> RepoResult<Vec<MessageDetail>>;
    /// `room.messages.page_before(message)`: 40 older, oldest first.
    /// The anchor timestamp is resolved inside SQL so sub-millisecond
    /// precision survives (epoch-ms round-trips would re-include the
    /// anchor); unknown anchor → empty page.
    async fn page_before(&self, room_id: i64, anchor_id: i64) -> RepoResult<Vec<MessageDetail>>;
    /// `room.messages.page_after(message)`: 40 newer, oldest first.
    async fn page_after(&self, room_id: i64, anchor_id: i64) -> RepoResult<Vec<MessageDetail>>;
    /// Id-cursor variant for the live tail: 40 messages with
    /// `id > after_id`, oldest first. Unlike [`page_after`](Self::page_after),
    /// it survives the anchor's deletion (empty rooms pass 0).
    async fn page_after_id(&self, room_id: i64, after_id: i64) -> RepoResult<Vec<MessageDetail>>;
    /// `room.messages.paged?`: more than one page exists.
    async fn paged(&self, room_id: i64) -> RepoResult<bool>;
    /// `message.update!(body:)`: upsert the body rich text, refresh the
    /// search vector, touch message + room. No-op when unchanged.
    async fn update_body(&self, id: i64, body: &str) -> RepoResult<()>;
    /// `message.boosts.ordered` with booster display fields.
    async fn boosts_for_message(&self, message_id: i64) -> RepoResult<Vec<BoostDetail>>;
    /// `@message.boosts.create!(content:)`, boosted by `booster_id`.
    /// Returns the new boost id.
    async fn create_boost(
        &self,
        message_id: i64,
        booster_id: i64,
        content: &str,
    ) -> RepoResult<i64>;
    /// `@message.boosts.find_by!(id:, booster:)` — destroy scoping.
    async fn find_boost(
        &self,
        message_id: i64,
        id: i64,
        booster_id: i64,
    ) -> RepoResult<Option<BoostDetail>>;
    /// `@boost.destroy!`.
    async fn delete_boost(&self, id: i64) -> RepoResult<()>;
}

// --- sessions ------------------------------------------------------------

/// `SessionRepository: start/find_by_token/resume/destroy` (§8–§9).
///
/// Token issuance and cookies are owned by Topcoat (`topcoat-session`); the
/// stored `token` is the hex SHA-256 token hash, never the raw token.
/// `find_by_token` rejects expired rows; resume refreshes
/// `last_active_at`/UA/IP plus the Topcoat-assigned expiry (callers
/// rate-limit to the hourly schedule). `expires_in` is the remaining session
/// lifetime (from `Session::expires_at`); sqlx has no `SystemTime` binding,
/// so the repo computes `now() + make_interval` in SQL.
pub struct SessionRow {
    pub id: i64,
    pub user_id: i64,
    pub token: String,
    /// `last_active_at` as unix seconds; feeds
    /// [`session_resume_due`](topcamp_domain::auth::session_resume_due).
    pub last_active_at_unix: i64,
}

#[allow(async_fn_in_trait)]
pub trait SessionRepository {
    async fn start(
        &self,
        user_id: i64,
        user_agent: Option<&str>,
        ip_address: Option<&str>,
        expires_in: std::time::Duration,
        token: &str,
    ) -> RepoResult<SessionRow>;
    async fn find_by_token(&self, token: &str) -> RepoResult<Option<SessionRow>>;
    async fn resume(
        &self,
        id: i64,
        user_agent: Option<&str>,
        ip_address: Option<&str>,
        expires_in: std::time::Duration,
    ) -> RepoResult<()>;
    async fn destroy(&self, id: i64) -> RepoResult<()>;
}

// --- attachments ---------------------------------------------------------

/// Blob + variant records (`active_storage_blobs`,
/// `active_storage_attachments`, `active_storage_variant_records`).
pub struct NewBlob {
    pub key: String,
    pub filename: String,
    pub content_type: Option<String>,
    pub byte_size: i64,
    pub checksum: Option<String>,
    pub service_name: String,
}

pub struct BlobRow {
    pub id: i64,
    pub key: String,
    pub filename: String,
    pub content_type: Option<String>,
    pub byte_size: i64,
    pub checksum: Option<String>,
    pub service_name: String,
}

/// Blob rows committed atomically with a message post (single commit,
/// so the live tail never renders a message mid-attach).
pub struct NewMessageAttachment {
    pub blob: NewBlob,
    pub metadata_json: String,
}

/// A named attachment plus the blob's analyzed dimensions (from
/// the `metadata` JSON `width`/`height`; `None` until analyzed).
pub struct RecordAttachment {
    pub record_id: i64,
    pub blob: BlobRow,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

#[allow(async_fn_in_trait)]
pub trait AttachmentRepository {
    async fn insert_blob(&self, blob: NewBlob) -> RepoResult<BlobRow>;
    async fn find_blob_by_key(&self, key: &str) -> RepoResult<Option<BlobRow>>;
    async fn attach_to_record(
        &self,
        record_type: &str,
        record_id: i64,
        name: &str,
        blob_id: i64,
    ) -> RepoResult<()>;
    async fn find_blob(&self, id: i64) -> RepoResult<Option<BlobRow>>;
    /// `(record_type, record_id)` pairs a blob is attached to (relay
    /// resolution for `attachment_ready` broadcasts).
    async fn attachments_for_blob(&self, blob_id: i64) -> RepoResult<Vec<(String, i64)>>;
    /// A user's attached avatar blob, if any.
    async fn avatar_for_user(&self, user_id: i64) -> RepoResult<Option<BlobRow>>;
    /// The blob attached as `name` on any record (account logos,
    /// message attachments), if any.
    async fn blob_for_record(
        &self,
        record_type: &str,
        record_id: i64,
        name: &str,
    ) -> RepoResult<Option<BlobRow>>;
    /// The `name` attachments for many records at once (message
    /// pages), each with its blob and analyzed dimensions.
    async fn attachments_for_records(
        &self,
        record_type: &str,
        name: &str,
        record_ids: &[i64],
    ) -> RepoResult<Vec<RecordAttachment>>;
    /// Delete the named attachment row, returning the detached blob id
    /// (callers publish `purge_blob` for it). `None` when unattached.
    async fn detach_from_record(
        &self,
        record_type: &str,
        record_id: i64,
        name: &str,
    ) -> RepoResult<Option<i64>>;
    async fn find_variant(&self, blob_id: i64, digest: &str) -> RepoResult<Option<i64>>;
    async fn record_variant(&self, blob_id: i64, digest: &str) -> RepoResult<i64>;
    /// All variation digests for a blob (purge key collection).
    async fn variant_digests(&self, blob_id: i64) -> RepoResult<Vec<String>>;
    /// Merge `metadata_json` (a JSON object document) into the blob's
    /// `metadata` via `jsonb ||`; sets `content_type` when `Some`.
    async fn update_blob_metadata(
        &self,
        blob_id: i64,
        content_type: Option<&str>,
        metadata_json: &str,
    ) -> RepoResult<()>;
    /// Delete variant records + attachments + the blob row, in that order.
    /// Rows-first: a retry after this step only re-runs the idempotent
    /// object deletes — never a resurrection. Returns total rows removed.
    async fn delete_blob(&self, blob_id: i64) -> RepoResult<u64>;
}

// --- webhooks ------------------------------------------------------------

pub struct WebhookRow {
    pub id: i64,
    pub url: Option<String>,
    pub user_id: i64,
}

/// Bot webhooks (`webhooks`) + delivery guards (`webhook_deliveries`,
/// `migrations/0004_workflows.sql`).
#[allow(async_fn_in_trait)]
pub trait WebhookRepository {
    async fn find_webhook(&self, id: i64) -> RepoResult<Option<WebhookRow>>;
    /// `bot.webhook` (`webhooks` by `user_id`).
    async fn find_by_user(&self, user_id: i64) -> RepoResult<Option<WebhookRow>>;
    /// `create_webhook!(url:)` (any `Some`, `""` included).
    async fn create_webhook(&self, user_id: i64, url: Option<&str>) -> RepoResult<WebhookRow>;
    /// `webhook.update!(url:)`.
    async fn set_webhook_url(&self, id: i64, url: &str) -> RepoResult<()>;
    /// `webhook.destroy`.
    async fn destroy_webhook(&self, id: i64) -> RepoResult<()>;
    /// `deliver_webhooks_to_bots`: enqueue one `deliver_webhook`
    /// outbox row per eligible bot (active bot in the room with a
    /// webhook, not the creator; non-direct rooms further narrow to
    /// `mentioned_ids`), like upstream's post-create write.
    async fn enqueue_bot_deliveries(
        &self,
        room_id: i64,
        direct: bool,
        mentioned_ids: &[i64],
        creator_id: i64,
        message_id: i64,
    ) -> RepoResult<usize>;
    /// The recorded reply for a delivery key, if this delivery (or a
    /// previous attempt of it) already posted one.
    async fn find_delivery_reply(&self, delivery_key: &str) -> RepoResult<Option<i64>>;
    /// Claim a delivery key with a NULL reply. First insert wins
    /// (`ON CONFLICT DO NOTHING`); returns `true` when this call won.
    /// The winner creates the reply message, then calls
    /// [`WebhookRepository::set_delivery_reply`].
    async fn claim_delivery(&self, delivery_key: &str) -> RepoResult<bool>;
    /// Fill in the winner's reply id.
    async fn set_delivery_reply(&self, delivery_key: &str, reply_id: i64) -> RepoResult<()>;
}

// --- push subscriptions --------------------------------------------------

/// `Push::Subscription` row: a browser's push endpoint + keys.
pub struct PushSubscriptionRow {
    pub id: i64,
    pub user_id: i64,
    pub endpoint: Option<String>,
    pub p256dh_key: Option<String>,
    pub auth_key: Option<String>,
    pub user_agent: Option<String>,
}

/// `Push::Subscription` (`push_subscriptions`). Every lookup is
/// user-scoped, like upstream's `Current.user.push_subscriptions`.
#[allow(async_fn_in_trait)]
pub trait PushSubscriptionRepository {
    /// The user's subscriptions, oldest first (the dev-mode index).
    async fn push_subscriptions_for_user(
        &self,
        user_id: i64,
    ) -> RepoResult<Vec<PushSubscriptionRow>>;
    /// One subscription (`find`, user-scoped; `None` is upstream's
    /// `RecordNotFound` → the 404 page).
    async fn find_push_subscription(
        &self,
        user_id: i64,
        id: i64,
    ) -> RepoResult<Option<PushSubscriptionRow>>;
    /// `find_by(endpoint:, p256dh_key:, auth_key:)` for create's
    /// touch-or-insert (NULL keys match NULL, like ActiveRecord).
    async fn find_push_subscription_by_params(
        &self,
        user_id: i64,
        endpoint: &str,
        p256dh_key: Option<&str>,
        auth_key: Option<&str>,
    ) -> RepoResult<Option<PushSubscriptionRow>>;
    /// `create` with the request's user agent.
    async fn create_push_subscription(
        &self,
        user_id: i64,
        endpoint: &str,
        p256dh_key: Option<&str>,
        auth_key: Option<&str>,
        user_agent: Option<&str>,
    ) -> RepoResult<PushSubscriptionRow>;
    /// `touch` (re-registration bumps `updated_at`).
    async fn touch_push_subscription(&self, id: i64) -> RepoResult<()>;
    /// `destroy_by(id:)` (user-scoped; `true` when a row died).
    async fn destroy_push_subscription(&self, user_id: i64, id: i64) -> RepoResult<bool>;
    /// `destroy_by(endpoint:, user_id:)` for sign-out. Returns rows
    /// removed.
    async fn destroy_push_subscriptions_by_endpoint(
        &self,
        user_id: i64,
        endpoint: &str,
    ) -> RepoResult<u64>;
}
