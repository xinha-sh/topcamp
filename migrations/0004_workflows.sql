-- 0004_workflows.sql: durable-workflow support tables + topics.
--
-- `webhook_deliveries` carries the DeliverWebhook idempotency key
-- (`webhook-delivery:{message_id}:{bot_id}`): `messages.client_message_id`
-- has NO unique constraint upstream and must not gain one, so the
-- exactly-once reply guard lives here instead (first insert wins, retries
-- reuse the recorded reply id).
--
-- The outbox gains the two attachment topics: `process_attachment`
-- schedules the §22 workflow, `attachment_ready` notifies (broadcast
-- relay) after finalize commits.

CREATE TABLE webhook_deliveries (
    id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    delivery_key     TEXT NOT NULL,
    reply_message_id BIGINT REFERENCES messages (id),
    CONSTRAINT webhook_deliveries_key_unique UNIQUE (delivery_key)
);

ALTER TABLE outbox DROP CONSTRAINT outbox_topic_check;
ALTER TABLE outbox ADD CONSTRAINT outbox_topic_check CHECK (
    topic IN (
        'push_message',
        'deliver_webhook',
        'remove_banned_content',
        'purge_blob',
        'process_attachment',
        'attachment_ready'
    )
);

-- Claims expire (`claimed_at < now() - 5 minutes` re-enters the claim
-- query), so the pending index keys on `done_at` alone: a partial
-- predicate cannot call `now()` (not immutable), and the old
-- `claimed_at IS NULL` predicate would hide expired claims from the
-- index. Small table; the wider predicate stays cheap.
DROP INDEX index_outbox_pending;
CREATE INDEX index_outbox_pending ON outbox (id) WHERE done_at IS NULL;
