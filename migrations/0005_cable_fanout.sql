-- 0005_cable_fanout.sql: worker→web realtime bridge topic.
--
-- The DBOS worker and the web server are separate processes, but Cable
-- fanout lives in the web process. `cable_fanout` rows carry
-- `{"stream": ..., "message": ...}`; the relay translates them to
-- `NOTIFY cable` (never the reverse — the bridge is one-directional),
-- and the web process LISTENs and publishes into the broker. Producers:
-- moderation `broadcast_remove` (one row per removed message) and any
-- future durable-then-notify flow. `attachment_ready` keeps its own
-- topic; the relay resolves it to a `cable_fanout`-shaped NOTIFY.

ALTER TABLE outbox DROP CONSTRAINT outbox_topic_check;
ALTER TABLE outbox ADD CONSTRAINT outbox_topic_check CHECK (
    topic IN (
        'push_message',
        'deliver_webhook',
        'remove_banned_content',
        'purge_blob',
        'process_attachment',
        'attachment_ready',
        'cable_fanout'
    )
);
