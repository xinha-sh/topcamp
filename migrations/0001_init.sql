-- 0001_init.sql: Topcamp PostgreSQL schema, derived from upstream `schema.sql`.
-- Conventions (see MIGRATION_NOTES.md "PostgreSQL schema plan"):
--   * `id` is BIGINT identity everywhere (Rust code uses i64).
--   * datetimes are TIMESTAMPTZ (UTC).
--   * FKs are plain REFERENCES with no ON DELETE action, exactly as upstream
--     (destruction cascades in application code). Notably absent, as upstream:
--     boosts.booster_id, memberships room/user, polymorphic record pairs.
--   * messages.client_message_id is NOT unique upstream: no constraint added.
--   * FTS5 message_search_index is replaced by messages.search_vector + GIN.

CREATE TABLE accounts (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL,
    custom_styles   TEXT,
    join_code       TEXT NOT NULL,
    name            TEXT NOT NULL,
    settings        JSONB,
    singleton_guard INTEGER NOT NULL DEFAULT 0,
    CONSTRAINT accounts_singleton_guard_unique UNIQUE (singleton_guard)
);

CREATE TABLE users (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL,
    bio             TEXT,
    bot_token       TEXT,
    email_address   TEXT,
    name            TEXT NOT NULL,
    password_digest TEXT,
    role            INTEGER NOT NULL DEFAULT 0,
    status          INTEGER NOT NULL DEFAULT 0,
    CONSTRAINT users_bot_token_unique UNIQUE (bot_token),
    CONSTRAINT users_email_address_unique UNIQUE (email_address)
);

CREATE TABLE rooms (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL,
    creator_id  BIGINT NOT NULL,
    name        TEXT,
    type        TEXT NOT NULL
);

CREATE TABLE memberships (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL,
    connected_at TIMESTAMPTZ,
    connections INTEGER NOT NULL DEFAULT 0,
    involvement TEXT NOT NULL DEFAULT 'mentions',
    room_id     BIGINT NOT NULL,
    unread_at   TIMESTAMPTZ,
    user_id     BIGINT NOT NULL,
    CONSTRAINT memberships_room_user_unique UNIQUE (room_id, user_id),
    CONSTRAINT memberships_involvement_check CHECK (involvement IN ('mentions', 'everything', 'invisible', 'nothing'))
);
CREATE INDEX index_memberships_on_room_id ON memberships (room_id);
CREATE INDEX index_memberships_on_user_id ON memberships (user_id);
CREATE INDEX index_memberships_on_room_id_and_created_at ON memberships (room_id, created_at);

CREATE TABLE messages (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at        TIMESTAMPTZ NOT NULL,
    updated_at        TIMESTAMPTZ NOT NULL,
    client_message_id TEXT NOT NULL,
    creator_id        BIGINT NOT NULL REFERENCES users (id),
    room_id           BIGINT NOT NULL REFERENCES rooms (id),
    search_vector     TSVECTOR NOT NULL DEFAULT ''::tsvector
);
CREATE INDEX index_messages_on_creator_id ON messages (creator_id);
CREATE INDEX index_messages_on_room_id ON messages (room_id);
-- App-added paging index: 60ms -> 0.02ms at 236k rows. Load-bearing.
CREATE INDEX index_messages_on_room_id_and_created_at ON messages (room_id, created_at);
CREATE INDEX index_messages_search_vector ON messages USING GIN (search_vector);

CREATE TABLE action_text_rich_texts (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL,
    body        TEXT,
    name        TEXT NOT NULL,
    record_id   BIGINT NOT NULL,
    record_type TEXT NOT NULL,
    CONSTRAINT rich_texts_uniqueness UNIQUE (record_type, record_id, name)
);

CREATE TABLE active_storage_blobs (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at   TIMESTAMPTZ NOT NULL,
    byte_size    BIGINT NOT NULL,
    checksum     TEXT,
    content_type TEXT,
    filename     TEXT NOT NULL,
    key          TEXT NOT NULL,
    metadata     TEXT,
    service_name TEXT NOT NULL,
    CONSTRAINT blobs_key_unique UNIQUE (key)
);

CREATE TABLE active_storage_attachments (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL,
    blob_id     BIGINT NOT NULL REFERENCES active_storage_blobs (id),
    name        TEXT NOT NULL,
    record_id   BIGINT NOT NULL,
    record_type TEXT NOT NULL,
    CONSTRAINT attachments_uniqueness UNIQUE (record_type, record_id, name, blob_id)
);
CREATE INDEX index_active_storage_attachments_on_blob_id ON active_storage_attachments (blob_id);

CREATE TABLE active_storage_variant_records (
    id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    blob_id          BIGINT NOT NULL REFERENCES active_storage_blobs (id),
    variation_digest TEXT NOT NULL,
    CONSTRAINT variant_records_uniqueness UNIQUE (blob_id, variation_digest)
);

CREATE TABLE bans (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    ip_address TEXT NOT NULL,
    user_id    BIGINT NOT NULL REFERENCES users (id)
);
CREATE INDEX index_bans_on_ip_address ON bans (ip_address);
CREATE INDEX index_bans_on_user_id ON bans (user_id);

CREATE TABLE boosts (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    booster_id BIGINT NOT NULL,
    content    VARCHAR(16) NOT NULL,
    message_id BIGINT NOT NULL REFERENCES messages (id)
);
CREATE INDEX index_boosts_on_booster_id ON boosts (booster_id);
CREATE INDEX index_boosts_on_message_id ON boosts (message_id);

CREATE TABLE push_subscriptions (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    auth_key   TEXT,
    endpoint   TEXT,
    p256dh_key TEXT,
    user_agent TEXT,
    user_id    BIGINT NOT NULL REFERENCES users (id)
);
CREATE INDEX idx_push_subscriptions_on_endpoint_keys ON push_subscriptions (endpoint, p256dh_key, auth_key);
CREATE INDEX index_push_subscriptions_on_user_id ON push_subscriptions (user_id);

CREATE TABLE searches (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    query      TEXT NOT NULL,
    user_id    BIGINT NOT NULL REFERENCES users (id)
);
CREATE INDEX index_searches_on_user_id ON searches (user_id);

CREATE TABLE sessions (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at     TIMESTAMPTZ NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL,
    ip_address     TEXT,
    last_active_at TIMESTAMPTZ NOT NULL,
    token          TEXT NOT NULL,
    user_agent     TEXT,
    user_id        BIGINT NOT NULL REFERENCES users (id),
    CONSTRAINT sessions_token_unique UNIQUE (token)
);
CREATE INDEX index_sessions_on_user_id ON sessions (user_id);

CREATE TABLE webhooks (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    url        TEXT,
    user_id    BIGINT NOT NULL REFERENCES users (id)
);
CREATE INDEX index_webhooks_on_user_id ON webhooks (user_id);
