//! Outbox relay: post-commit rows become DBOS executions (§14, §19).
//!
//! The relay claims pending [`outbox`](topcamp_db::outbox) rows, starts one
//! workflow per row with the deterministic id `outbox:{row_id}` (DBOS treats
//! the id as an idempotency key, so a crash between completion and
//! `mark_done` rejoins the same execution instead of duplicating it), awaits
//! the result, then marks the row done. Failed dispatches release the claim
//! so a later pass retries; the attempt bump from the claim stays, keeping
//! poison rows visible.
//!
//! Topic → workflow map (payloads are integer-only JSON):
//! `push_message` → `send_notification`, `deliver_webhook` →
//! `deliver_webhook`, `remove_banned_content` → `remove_banned_content`,
//! `purge_blob` → `purge_blob`, `process_attachment` →
//! `process_attachment`. `cable_fanout` rows and resolved
//! `attachment_ready` events become `NOTIFY cable` (`migrations/0005_*`):
//! the worker and the web server are separate processes, and fanout lives
//! in the web process, which LISTENs and publishes into the broker.

use topcamp_db::outbox::OutboxEntry;
use topcamp_db::PgDb;

use crate::attachments::{AttachmentFailed, ProcessAttachment};
use crate::moderation::{ModerationFailed, RemoveBannedContent};
use crate::notifications::{DeliveryOutcome, NotificationFailed, SendNotification};
use crate::purge::{PurgeBlob, PurgeFailed};
use crate::webhooks::{DeliverWebhook, DeliveryFailed};

/// Typed start handles, one per workflow. Built once at worker startup
/// (registration freezes at `launch`, so refs must be kept, not re-made).
pub struct Handles {
    pub send_notification:
        dbos::WorkflowRef<SendNotification, Vec<DeliveryOutcome>, NotificationFailed>,
    pub deliver_webhook: dbos::WorkflowRef<DeliverWebhook, i64, DeliveryFailed>,
    pub remove_banned_content: dbos::WorkflowRef<RemoveBannedContent, u64, ModerationFailed>,
    pub purge_blob: dbos::WorkflowRef<PurgeBlob, (), PurgeFailed>,
    pub process_attachment: dbos::WorkflowRef<ProcessAttachment, (), AttachmentFailed>,
}

/// Register every workflow and keep the start handles.
pub fn register_all(dbos: &dbos::DBOS) -> dbos::Result<Handles> {
    Ok(Handles {
        send_notification: crate::notifications::register(dbos)?,
        deliver_webhook: crate::webhooks::register(dbos)?,
        remove_banned_content: crate::moderation::register(dbos)?,
        purge_blob: crate::purge::register(dbos)?,
        process_attachment: crate::attachments::register(dbos)?,
    })
}

/// Why a dispatch failed. Carries the outbox row id for log correlation.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("outbox {0}: bad payload: {1}")]
    Payload(i64, String),
    #[error("outbox {0}: unknown topic {1:?}")]
    Topic(i64, String),
    #[error("outbox {0}: workflow failed: {1}")]
    Workflow(i64, String),
    #[error("outbox {0}: database error: {1:?}")]
    Db(i64, topcamp_domain::error::Error),
}

fn field(payload: &serde_json::Value, id: i64, name: &str) -> Result<i64, RelayError> {
    payload
        .get(name)
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| RelayError::Payload(id, format!("missing integer {name:?}")))
}

/// Deliver one `NOTIFY cable` carrying a `{"stream", "message"}` JSON
/// document. The web process LISTENs and publishes into the broker.
async fn pg_notify(db: &PgDb, outbox_id: i64, notify_payload: &str) -> Result<(), RelayError> {
    sqlx::query("SELECT pg_notify('cable', $1)")
        .bind(notify_payload)
        .execute(db.pool())
        .await
        .map_err(|err| {
            RelayError::Db(
                outbox_id,
                topcamp_domain::error::Error::from(topcamp_db::DbError::from(err)),
            )
        })?;
    Ok(())
}

/// Resolve `attachment_ready` to its room stream and notify. Blobs with no
/// message attachment have no audience: success with no notify (the
/// durable `processed` fact already committed in `finalize`).
async fn notify_attachment_ready(
    db: &PgDb,
    outbox_id: i64,
    payload: &serde_json::Value,
) -> Result<(), RelayError> {
    use topcamp_db::repositories::{AttachmentRepository, MessageRepository, RoomRepository};
    let blob_id = field(payload, outbox_id, "blob_id")?;
    let attachments = AttachmentRepository::attachments_for_blob(db, blob_id)
        .await
        .map_err(|err| RelayError::Db(outbox_id, err))?;
    for (record_type, record_id) in attachments {
        if record_type != "Message" {
            continue;
        }
        let message = MessageRepository::find_by_id(db, record_id)
            .await
            .map_err(|err| RelayError::Db(outbox_id, err))?;
        let Some(message) = message else { continue };
        let room = RoomRepository::find_by_id(db, message.room_id)
            .await
            .map_err(|err| RelayError::Db(outbox_id, err))?;
        let Some(room) = room else { continue };
        let notify = serde_json::json!({
            "stream": topcamp_cable::room_stream(&room.kind, room.id),
            "message": {"action": "attachment_ready", "blob_id": blob_id, "message_id": message.id},
        })
        .to_string();
        pg_notify(db, outbox_id, &notify).await?;
    }
    Ok(())
}

/// Start the workflow for one claimed row and await its result.
pub async fn dispatch(db: &PgDb, handles: &Handles, entry: &OutboxEntry) -> Result<(), RelayError> {
    let payload: serde_json::Value = serde_json::from_str(&entry.payload)
        .map_err(|err| RelayError::Payload(entry.id, err.to_string()))?;
    // Deterministic id ⇒ re-dispatch after a crash rejoins the recorded
    // execution instead of running the effects twice.
    let workflow_id = format!("outbox:{}", entry.id);
    let options = dbos::RunOptions {
        workflow_id: Some(workflow_id.as_str()),
        ..Default::default()
    };
    match entry.topic.as_str() {
        "push_message" => {
            let input = SendNotification {
                message_id: field(&payload, entry.id, "message_id")?,
            };
            handles
                .send_notification
                .run_with(input, options)
                .await
                .map_err(|err| RelayError::Workflow(entry.id, err.to_string()))?;
        }
        "deliver_webhook" => {
            let input = DeliverWebhook {
                bot_id: field(&payload, entry.id, "bot_id")?,
                message_id: field(&payload, entry.id, "message_id")?,
            };
            handles
                .deliver_webhook
                .run_with(input, options)
                .await
                .map_err(|err| RelayError::Workflow(entry.id, err.to_string()))?;
        }
        "remove_banned_content" => {
            let input = RemoveBannedContent {
                user_id: field(&payload, entry.id, "user_id")?,
            };
            handles
                .remove_banned_content
                .run_with(input, options)
                .await
                .map_err(|err| RelayError::Workflow(entry.id, err.to_string()))?;
        }
        "purge_blob" => {
            let input = PurgeBlob {
                blob_id: field(&payload, entry.id, "blob_id")?,
            };
            handles
                .purge_blob
                .run_with(input, options)
                .await
                .map_err(|err| RelayError::Workflow(entry.id, err.to_string()))?;
        }
        "process_attachment" => {
            let input = ProcessAttachment {
                blob_id: field(&payload, entry.id, "blob_id")?,
            };
            handles
                .process_attachment
                .run_with(input, options)
                .await
                .map_err(|err| RelayError::Workflow(entry.id, err.to_string()))?;
        }
        "cable_fanout" => {
            let stream = payload
                .get("stream")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    RelayError::Payload(entry.id, "missing string \"stream\"".to_string())
                })?;
            let message = payload
                .get("message")
                .ok_or_else(|| RelayError::Payload(entry.id, "missing \"message\"".to_string()))?;
            let notify = serde_json::json!({"stream": stream, "message": message}).to_string();
            pg_notify(db, entry.id, &notify).await?;
        }
        "attachment_ready" => {
            notify_attachment_ready(db, entry.id, &payload).await?;
        }
        other => return Err(RelayError::Topic(entry.id, other.to_string())),
    }
    Ok(())
}

/// One relay pass: claim up to `limit` rows, dispatch each, mark done on
/// success or release on failure. Returns rows successfully dispatched.
pub async fn run_once(
    db: &PgDb,
    handles: &Handles,
    limit: i64,
) -> Result<usize, topcamp_domain::error::Error> {
    use topcamp_db::outbox;
    fn dberr(err: sqlx::Error) -> topcamp_domain::error::Error {
        topcamp_domain::error::Error::from(topcamp_db::DbError::from(err))
    }
    let mut tx = db.pool().begin().await.map_err(dberr)?;
    let entries = outbox::claim_batch(&mut tx, limit).await.map_err(dberr)?;
    tx.commit().await.map_err(dberr)?;
    let mut done = 0;
    for entry in entries {
        match dispatch(db, handles, &entry).await {
            Ok(()) => {
                let mut tx = db.pool().begin().await.map_err(dberr)?;
                outbox::mark_done(&mut tx, entry.id).await.map_err(dberr)?;
                tx.commit().await.map_err(dberr)?;
                done += 1;
            }
            Err(err) => {
                tracing::warn!(
                    outbox_id = entry.id,
                    topic = entry.topic.as_str(),
                    error = err.to_string(),
                    "relay dispatch failed; claim released"
                );
                let mut tx = db.pool().begin().await.map_err(dberr)?;
                outbox::release(&mut tx, entry.id).await.map_err(dberr)?;
                tx.commit().await.map_err(dberr)?;
            }
        }
    }
    Ok(done)
}
