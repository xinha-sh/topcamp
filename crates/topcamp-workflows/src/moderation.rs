//! `RemoveBannedContent` workflow (replaces `RemoveBannedContentJob`).
//!
//! Evidence: MIGRATION_NOTES.md "DBOS workflow detailed design" + the jobs
//! trace (destroys each user message in its own tx + `broadcast_remove`).
//! Per-message steps so resume CONTINUES past already-removed messages
//! instead of restarting the whole ban.

use serde::{Deserialize, Serialize};
use topcamp_db::repositories::MessageRepository;
use topcamp_db::PgDb;

use crate::worker;

/// Workflow input: the banned user whose content must go.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveBannedContent {
    pub user_id: i64,
}

/// Durable failure for moderation steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModerationFailed(pub String);

impl std::fmt::Display for ModerationFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ban-content removal failed: {}", self.0)
    }
}

impl std::error::Error for ModerationFailed {}

type Fallible<T> = Result<T, dbos::Error<ModerationFailed>>;

/// Retry policy for the wired steps below. Safe because every body is
/// idempotent: reads (`list_messages`) and row deletes (`destroy_chunk`,
/// where a repeat delete is a no-op and the reported count is computed
/// outside the body).
fn db_retry() -> dbos::StepOptions<ModerationFailed> {
    dbos::StepOptions {
        max_attempts: 5,
        interval: std::time::Duration::from_secs(1),
        backoff_rate: 2.0,
        max_interval: std::time::Duration::from_secs(30),
        ..Default::default()
    }
}

/// Step 1: list the user's message ids. Deterministic snapshot: resume
/// reuses the recorded list rather than re-scanning.
pub async fn list_messages(db: &PgDb, user_id: i64) -> Fallible<Vec<i64>> {
    let ids = dbos::step_with("list_messages", db_retry(), || async move {
        db.ids_for_creator(user_id).await.map_err(|err| {
            dbos::Error::Application(ModerationFailed(format!("list messages: {err:?}")))
        })
    })
    .await?;
    Ok(ids)
}

/// Step 2: destroy a chunk of messages, one row per statement (each its own
/// implicit tx). One step PER MESSAGE would explode the checkpoint table
/// for huge bans; chunks keep resume at chunk granularity. Afterwards one
/// `cable_fanout` row per destroyed message carries the realtime
/// `broadcast_remove` (target `message_<client_message_id>`, per the
/// `Broadcasts` trace). Duplicate rows on step retry are harmless:
/// remove×2 is the same visual state.
pub async fn destroy_chunk(db: &PgDb, message_ids: Vec<i64>) -> Fallible<u64> {
    // Counted OUTSIDE the step body: step bodies are `FnMut` (retries call
    // them again), so moving `message_ids` out inside would not compile —
    // which is precisely where the mistake should be reported.
    let expected = message_ids.len() as u64;
    dbos::step_with("destroy_chunk", db_retry(), || {
        // Cloned per attempt: the closure stays `FnMut` (retry-safe), and
        // each attempt iterates a fresh list.
        let message_ids = message_ids.clone();
        async move {
            // Resolve BEFORE destroying: destroyed rows can't be read back
            // for their room/client ids.
            let mut doomed = Vec::with_capacity(message_ids.len());
            for id in message_ids {
                match MessageRepository::find_by_id(db, id).await {
                    Ok(Some(message)) => doomed.push(message),
                    Ok(None) => {}
                    Err(err) => {
                        return Err(dbos::Error::Application(ModerationFailed(format!(
                            "resolve {id}: {err:?}"
                        ))));
                    }
                }
            }
            for message in &doomed {
                MessageRepository::destroy(db, message.id)
                    .await
                    .map_err(|err| {
                        dbos::Error::Application(ModerationFailed(format!(
                            "destroy {}: {err:?}",
                            message.id
                        )))
                    })?;
            }
            if !doomed.is_empty() {
                use topcamp_db::repositories::RoomRepository;
                let mut tx = db.pool().begin().await.map_err(|err| {
                    dbos::Error::Application(ModerationFailed(format!("begin tx: {err}")))
                })?;
                for message in &doomed {
                    let room = RoomRepository::find_by_id(db, message.room_id)
                        .await
                        .map_err(|err| {
                            dbos::Error::Application(ModerationFailed(format!(
                                "find room {}: {err:?}",
                                message.room_id
                            )))
                        })?;
                    // Room already gone ⇒ no stream to notify.
                    let Some(room) = room else { continue };
                    let stream = topcamp_cable::room_stream(&room.kind, room.id);
                    let payload = serde_json::json!({
                        "stream": stream,
                        "message": {
                            "action": "remove",
                            "target": format!("message_{}", message.client_message_id),
                            "message_id": message.id,
                        },
                    })
                    .to_string();
                    topcamp_db::outbox::publish(&mut tx, "cable_fanout", &payload)
                        .await
                        .map_err(|err| {
                            dbos::Error::Application(ModerationFailed(format!(
                                "publish remove {}: {err}",
                                message.id
                            )))
                        })?;
                }
                tx.commit().await.map_err(|err| {
                    dbos::Error::Application(ModerationFailed(format!("commit removes: {err}")))
                })?;
            }
            Ok::<_, dbos::Error<ModerationFailed>>(expected)
        }
    })
    .await
}

/// Resolve the worker database or fail the workflow loudly when the worker
/// binary forgot [`worker::init`].
fn worker_db() -> Fallible<PgDb> {
    worker::db().ok_or_else(|| {
        dbos::Error::Application(ModerationFailed(
            "worker database not initialized".to_string(),
        ))
    })
}

/// Workflow body: snapshot the list, then destroy in chunks.
#[tracing::instrument(name = "remove_banned_content", skip(input), fields(user_id = input.user_id))]
pub async fn remove_banned_content(input: RemoveBannedContent) -> Fallible<u64> {
    let db = worker_db()?;
    let messages = list_messages(&db, input.user_id).await?;
    let mut removed = 0u64;
    for chunk in messages.chunks(100) {
        removed += destroy_chunk(&db, chunk.to_vec()).await?;
    }
    Ok(removed)
}

/// Register moderation workflows on a [`dbos::DBOS`] instance. Returns
/// the ref so the relay can start executions by id.
pub fn register(
    dbos: &dbos::DBOS,
) -> dbos::Result<dbos::WorkflowRef<RemoveBannedContent, u64, ModerationFailed>> {
    let registered = dbos.register_workflow("remove_banned_content", remove_banned_content)?;
    debug_assert_eq!(registered.name(), "remove_banned_content");
    Ok(registered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_registers_under_stable_name() {
        let dbos = dbos::DBOS::new(dbos::Config::new("topcamp-test", "postgres://unused"));
        let registered = dbos
            .register_workflow("remove_banned_content", remove_banned_content)
            .expect("registration succeeds");
        assert_eq!(registered.name(), "remove_banned_content");
    }
}
