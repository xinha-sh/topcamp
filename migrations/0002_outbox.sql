-- 0002_outbox.sql: transactional outbox for post-commit durable scheduling.
-- Pattern (§14): the message-creation transaction inserts domain rows AND
-- outbox rows atomically; a relay (DBOS scheduler/worker) claims and
-- dispatches them after commit, then marks them done. No irreversible side
-- effect ever happens before commit. This replaces the in-process EventSink
-- queue (capacity 1024, drop-and-log, lost-on-crash) with durable delivery.
--
-- Search (§15) deliberately needs NO schema here: messages.search_vector
-- (from 0001) is maintained by the repositories, not a trigger — plain-text
-- extraction must go through the richtext library (same canonicalization as
-- the app renders), and upstream likewise indexes explicitly after commit
-- (create_in_index/update_in_index). Ranking uses ts_rank at query time.

CREATE TABLE outbox (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    topic       TEXT NOT NULL,
    -- Topics mirror the upstream Event enum: push_message, deliver_webhook,
    -- remove_banned_content, purge_blob. (disconnect_user is a synchronous
    -- cable broadcast and never enters the outbox.)
    payload     JSONB NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    claimed_at  TIMESTAMPTZ,
    done_at     TIMESTAMPTZ,
    CONSTRAINT outbox_topic_check CHECK (
        topic IN ('push_message', 'deliver_webhook', 'remove_banned_content', 'purge_blob')
    )
);
-- Claim query shape: SELECT ... WHERE done_at IS NULL AND claimed_at IS NULL
-- ORDER BY id LIMIT n FOR UPDATE SKIP LOCKED.
CREATE INDEX index_outbox_pending ON outbox (id)
    WHERE done_at IS NULL AND claimed_at IS NULL;
