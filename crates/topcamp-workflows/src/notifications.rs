//! `SendNotification` workflow (replaces `Room::PushMessageJob`, §28).
//!
//! Evidence: MIGRATION_NOTES.md "DBOS workflow detailed design".
//! Only `dbos` 0.5.0 APIs used (§17): `register_workflow`, `step`.
//! Skipped when VAPID is unconfigured — decided at schedule time (§19).
//!
//! LIMITATION (tracked, not silent): delivery POSTs a plaintext JSON
//! payload. Upstream encrypts per RFC 8291 (`aes128gcm` + VAPID); real
//! browser push endpoints will reject plaintext with 4xx, which this step
//! records as `delivered: false` rather than pretending. Encryption is the
//! remaining transport gap — the DBOS half (durable, retried, per-endpoint
//! outcomes) is complete.

use serde::{Deserialize, Serialize};
use topcamp_db::repositories::MessageRepository;
use topcamp_db::PgDb;

use crate::worker;

/// Durable failure for notification steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationFailed(pub String);

impl std::fmt::Display for NotificationFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "notification failed: {}", self.0)
    }
}

impl std::error::Error for NotificationFailed {}

type Fallible<T> = Result<T, dbos::Error<NotificationFailed>>;

/// Retry policy for the recipient load: a pure read, so repeats are safe.
fn db_retry() -> dbos::StepOptions<NotificationFailed> {
    dbos::StepOptions {
        max_attempts: 5,
        interval: std::time::Duration::from_secs(1),
        backoff_rate: 2.0,
        max_interval: std::time::Duration::from_secs(30),
        ..Default::default()
    }
}

/// Workflow input: the message to notify about. Idempotency key for the
/// whole workflow is `send-notification:{message_id}` (decided by the
/// scheduler via explicit workflow IDs at `start` time).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendNotification {
    pub message_id: i64,
}

/// Durable output: per-endpoint delivery outcomes (for observability, not
/// control flow — a failed endpoint is dropped + logged, matching the
/// upstream pool-overflow semantics).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryOutcome {
    pub endpoint: String,
    pub delivered: bool,
}

/// Load step: resolve subscription endpoints for the message's room
/// members. Pure data gathering so it is trivially retry-safe; the actual
/// HTTP sends happen in `deliver_batch` (separate step so a send failure
/// never re-runs the load).
pub async fn load_recipients(db: &PgDb, message_id: i64) -> Fallible<Vec<String>> {
    dbos::step_with("load_recipients", db_retry(), || async move {
        let message = MessageRepository::find_by_id(db, message_id)
            .await
            .map_err(|err| {
                dbos::Error::Application(NotificationFailed(format!(
                    "load message {message_id}: {err:?}"
                )))
            })?;
        let Some(message) = message else {
            return Ok::<_, dbos::Error<NotificationFailed>>(Vec::new());
        };
        db.push_endpoints_for_room(message.room_id)
            .await
            .map_err(|err| {
                dbos::Error::Application(NotificationFailed(format!("load endpoints: {err:?}")))
            })
    })
    .await
}

/// Deliver step: POST the payload to every endpoint. One step for the
/// batch (not one per endpoint): a failed endpoint is dropped + logged and
/// recorded as undelivered, matching the upstream pool-overflow semantics —
/// one dead endpoint never fails the workflow. Repeat sends carry the same
/// payload, so retries are idempotent.
pub async fn deliver_batch(
    http: &reqwest::Client,
    message_id: i64,
    room_id: i64,
    endpoints: Vec<String>,
) -> Fallible<Vec<DeliveryOutcome>> {
    // Borrowed, not moved: the step body is `FnMut`, so each attempt
    // clones afresh from the borrow.
    dbos::step_with("deliver_batch", db_retry(), || async {
        let mut outcomes = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints.clone() {
            let body = serde_json::to_vec(
                &serde_json::json!({"message_id": message_id, "room_id": room_id}),
            )
            .map_err(|err| {
                dbos::Error::Application(NotificationFailed(format!("encode payload: {err}")))
            })?;
            let outcome = match http
                .post(&endpoint)
                .header(reqwest::header::CONTENT_TYPE, worker::JSON_CONTENT_TYPE)
                .body(body)
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    tracing::info!(message_id, endpoint = endpoint.as_str(), "push delivered");
                    true
                }
                Ok(response) => {
                    tracing::warn!(
                        message_id,
                        endpoint = endpoint.as_str(),
                        status = response.status().as_u16(),
                        "push rejected; endpoint dropped"
                    );
                    false
                }
                Err(err) => {
                    tracing::warn!(
                        message_id,
                        endpoint = endpoint.as_str(),
                        error = err.to_string(),
                        "push send failed; endpoint dropped"
                    );
                    false
                }
            };
            outcomes.push(DeliveryOutcome {
                endpoint,
                delivered: outcome,
            });
        }
        Ok::<_, dbos::Error<NotificationFailed>>(outcomes)
    })
    .await
}

/// Workflow body: load, then deliver. Registered under `send_notification`.
#[tracing::instrument(name = "send_notification", skip(input), fields(message_id = input.message_id))]
pub async fn send_notification(input: SendNotification) -> Fallible<Vec<DeliveryOutcome>> {
    let Some(db) = worker::db() else {
        return Err(dbos::Error::Application(NotificationFailed(
            "worker database not initialized".to_string(),
        )));
    };
    let Some(http) = worker::http() else {
        return Err(dbos::Error::Application(NotificationFailed(
            "worker http client not initialized".to_string(),
        )));
    };
    let endpoints = load_recipients(&db, input.message_id).await?;
    if endpoints.is_empty() {
        return Ok(Vec::new());
    }
    let room_id = MessageRepository::find_by_id(&db, input.message_id)
        .await
        .map_err(|err| {
            dbos::Error::Application(NotificationFailed(format!(
                "reload message {}: {err:?}",
                input.message_id
            )))
        })?
        .map(|message| message.room_id)
        .unwrap_or(0);
    deliver_batch(&http, input.message_id, room_id, endpoints).await
}

/// Register all notification workflows on a [`dbos::DBOS`] instance.
/// Returns the ref so the relay can start executions by id.
pub fn register(
    dbos: &dbos::DBOS,
) -> dbos::Result<dbos::WorkflowRef<SendNotification, Vec<DeliveryOutcome>, NotificationFailed>> {
    let notifications = dbos.register_workflow("send_notification", send_notification)?;
    debug_assert_eq!(notifications.name(), "send_notification");
    Ok(notifications)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_registers_under_stable_name() {
        let dbos = dbos::DBOS::new(dbos::Config::new("topcamp-test", "postgres://unused"));
        // Registration infers input/output types and must not fail; the
        // returned ref carries the stable name (replay identity).
        let registered = dbos
            .register_workflow("send_notification", send_notification)
            .expect("registration succeeds");
        assert_eq!(registered.name(), "send_notification");
    }
}
