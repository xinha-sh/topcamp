//! `DeliverWebhook` workflow (replaces `Bot::WebhookJob`).
//!
//! Evidence: MIGRATION_NOTES.md "DBOS workflow detailed design".
//! Webhook POST policies from the integrations trace: 7s connect/read
//! timeouts, 60s total deadline, ≤100MB reply; timeouts become a
//! "Failed to respond within N seconds" text reply (a BRANCH, not a
//! failure); bad URL/connection errors/unparseable MIME fail the delivery.
//! CRITICAL: reply creation carries idempotency key
//! `webhook-delivery:{message_id}:{webhook_id}` (`webhook_deliveries`,
//! `migrations/0004_workflows.sql`) — a retried delivery must NOT
//! double-post the bot's reply.

use serde::{Deserialize, Serialize};
use topcamp_db::repositories::{MessageRepository, NewMessage, WebhookRepository};
use topcamp_db::PgDb;

use crate::worker;

/// Durable application failure: delivery failed outright (bad URL,
/// connection error, unparsable MIME). Recorded as a job error upstream;
/// here it fails the workflow so the retry policy engages instead of
/// posting a phantom reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryFailed(pub String);

impl std::fmt::Display for DeliveryFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "webhook delivery failed: {}", self.0)
    }
}

impl std::error::Error for DeliveryFailed {}

/// Workflow input: the webhook (integration) and the triggering message.
/// `bot_id` is the `webhooks.id` row; the reply is authored by that row's
/// `user_id` (the bot user).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliverWebhook {
    pub bot_id: i64,
    pub message_id: i64,
}

/// What the bot endpoint returned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WebhookReply {
    /// Text reply → bot message with canonicalized body.
    Text(String),
    /// The endpoint timed out: post the timeout notice as the reply text.
    TimedOut { seconds: u64 },
    /// Delivery failed outright (bad URL, connection error, unparsable
    /// MIME): recorded as a job error, no reply posted, workflow fails so
    /// the retry policy applies.
    Failed(String),
}

/// Maximum accepted reply body (integrations trace: 100MB).
pub const MAX_REPLY_BYTES: usize = 100 * 1024 * 1024;

/// Idempotency key for a delivery: `webhook-delivery:{message_id}:{bot_id}`.
pub fn delivery_key(input: &DeliverWebhook) -> String {
    format!("webhook-delivery:{}:{}", input.message_id, input.bot_id)
}

/// Read the reply body with a hard cap: the stream stops past
/// `MAX_REPLY_BYTES + 1`, so an unbounded bot cannot OOM the worker.
async fn read_capped(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, dbos::Error<DeliveryFailed>> {
    let mut body = Vec::new();
    loop {
        let chunk = response.chunk().await.map_err(|err| {
            dbos::Error::Application(DeliveryFailed(format!("read reply body: {err}")))
        })?;
        let Some(chunk) = chunk else { break };
        if body.len() + chunk.len() > MAX_REPLY_BYTES {
            return Err(dbos::Error::Application(DeliveryFailed(format!(
                "reply exceeds {MAX_REPLY_BYTES} bytes"
            ))));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Step 1: POST the payload. Timeout → `TimedOut` branch (success path),
/// transport failure → `Failed` (retry path).
pub async fn post_payload(
    http: &reqwest::Client,
    db: &PgDb,
    input: &DeliverWebhook,
) -> Result<WebhookReply, dbos::Error<DeliveryFailed>> {
    // Borrowed, not moved: the step body is `FnMut`, so each attempt
    // re-reads from the borrow.
    dbos::step("post_payload", || async move {
        let webhook = db.find_webhook(input.bot_id).await.map_err(|err| {
            dbos::Error::Application(DeliveryFailed(format!(
                "find webhook {}: {err:?}",
                input.bot_id
            )))
        })?;
        let Some(webhook) = webhook else {
            tracing::warn!(bot_id = input.bot_id, "webhook has no row; delivery failed");
            return Ok::<_, dbos::Error<DeliveryFailed>>(WebhookReply::Failed(format!(
                "webhook {} has no row",
                input.bot_id
            )));
        };
        let Some(url) = webhook.url else {
            tracing::warn!(bot_id = input.bot_id, "webhook has no url; delivery failed");
            return Ok::<_, dbos::Error<DeliveryFailed>>(WebhookReply::Failed(format!(
                "webhook {} has no url",
                input.bot_id
            )));
        };
        let payload = serde_json::to_vec(
            &serde_json::json!({"message_id": input.message_id, "bot_id": input.bot_id}),
        )
        .map_err(|err| {
            dbos::Error::Application(DeliveryFailed(format!("encode payload: {err}")))
        })?;
        let response = http
            .post(url.as_str())
            .header(reqwest::header::CONTENT_TYPE, worker::JSON_CONTENT_TYPE)
            .body(payload)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(err) if err.is_timeout() => {
                tracing::warn!(
                    bot_id = input.bot_id,
                    message_id = input.message_id,
                    "webhook POST timed out; recording timeout reply"
                );
                return Ok::<_, dbos::Error<DeliveryFailed>>(WebhookReply::TimedOut {
                    seconds: 60,
                });
            }
            Err(err) => {
                tracing::warn!(
                    bot_id = input.bot_id,
                    message_id = input.message_id,
                    error = err.to_string(),
                    "webhook POST failed"
                );
                return Ok::<_, dbos::Error<DeliveryFailed>>(WebhookReply::Failed(format!(
                    "post {url}: {err}"
                )));
            }
        };
        let status = response.status().as_u16();
        if response.status() == reqwest::StatusCode::REQUEST_TIMEOUT
            || response.status() == reqwest::StatusCode::GATEWAY_TIMEOUT
        {
            tracing::warn!(
                bot_id = input.bot_id,
                message_id = input.message_id,
                status,
                "webhook timed out upstream; recording timeout reply"
            );
            return Ok::<_, dbos::Error<DeliveryFailed>>(WebhookReply::TimedOut { seconds: 60 });
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text_like = content_type.starts_with("text/")
            || content_type.contains("json")
            || content_type.is_empty();
        if !text_like {
            tracing::warn!(
                bot_id = input.bot_id,
                message_id = input.message_id,
                status,
                content_type = content_type.as_str(),
                "webhook reply unparsable; delivery failed"
            );
            return Ok::<_, dbos::Error<DeliveryFailed>>(WebhookReply::Failed(format!(
                "unparsable reply MIME: {content_type}"
            )));
        }
        let body = read_capped(response).await?;
        let text = String::from_utf8(body).map_err(|_| {
            dbos::Error::Application(DeliveryFailed("reply body is not UTF-8".to_string()))
        })?;
        // Metadata only (status + byte count) — never the reply text (§39).
        tracing::info!(
            bot_id = input.bot_id,
            message_id = input.message_id,
            status,
            bytes = text.len(),
            "webhook delivered"
        );
        Ok::<_, dbos::Error<DeliveryFailed>>(WebhookReply::Text(text))
    })
    .await
}

/// Step 2: create the bot's reply message. Guarded by idempotency key
/// `webhook-delivery:{message_id}:{bot_id}`: the claim row is inserted
/// BEFORE the message, so a retry (or a racing duplicate) reuses the
/// recorded reply instead of double-posting. A lost race against a
/// still-running winner fails retryably — the relay resets the claim and a
/// later pass reaps the winner's reply.
pub async fn create_reply(
    db: &PgDb,
    input: &DeliverWebhook,
    reply: &WebhookReply,
) -> Result<i64, dbos::Error<DeliveryFailed>> {
    let preview = match reply {
        WebhookReply::Text(t) => t.clone(),
        WebhookReply::TimedOut { seconds } => {
            format!("Failed to respond within {seconds} seconds")
        }
        WebhookReply::Failed(detail) => {
            // Recorded as an error upstream; surface a durable failure so
            // retries engage rather than posting a phantom reply.
            return Err(dbos::Error::Application(DeliveryFailed(detail.clone())));
        }
    };
    // Borrowed, not moved: the step body is `FnMut`, so attempts share
    // the borrows and clone what they must own.
    dbos::step("create_reply", || async {
        let key = delivery_key(input);
        if let Some(existing) = db.find_delivery_reply(&key).await.map_err(|err| {
            dbos::Error::Application(DeliveryFailed(format!("find delivery {key}: {err:?}")))
        })? {
            return Ok::<_, dbos::Error<DeliveryFailed>>(existing);
        }
        if !db.claim_delivery(&key).await.map_err(|err| {
            dbos::Error::Application(DeliveryFailed(format!("claim delivery {key}: {err:?}")))
        })? {
            // Lost the race: the winner's reply may already be recorded.
            if let Some(existing) = db.find_delivery_reply(&key).await.map_err(|err| {
                dbos::Error::Application(DeliveryFailed(format!("reread delivery {key}: {err:?}")))
            })? {
                return Ok::<_, dbos::Error<DeliveryFailed>>(existing);
            }
            return Err(dbos::Error::Application(DeliveryFailed(format!(
                "delivery {key} claimed by a running winner; retry later"
            ))));
        }
        let webhook = db.find_webhook(input.bot_id).await.map_err(|err| {
            dbos::Error::Application(DeliveryFailed(format!(
                "find webhook {}: {err:?}",
                input.bot_id
            )))
        })?;
        let Some(webhook) = webhook else {
            return Err(dbos::Error::Application(DeliveryFailed(format!(
                "webhook {} vanished mid-delivery",
                input.bot_id
            ))));
        };
        if preview.trim().is_empty() {
            // An empty bot reply posts no message (upstream parity: nothing
            // to say). The claim row stays, so a retry re-lands here and
            // again posts nothing — still exactly-once (zero).
            return Ok::<_, dbos::Error<DeliveryFailed>>(0);
        }
        let trigger = MessageRepository::find_by_id(db, input.message_id)
            .await
            .map_err(|err| {
                dbos::Error::Application(DeliveryFailed(format!(
                    "find trigger {}: {err:?}",
                    input.message_id
                )))
            })?;
        let Some(trigger) = trigger else {
            return Err(dbos::Error::Application(DeliveryFailed(format!(
                "trigger message {} is gone",
                input.message_id
            ))));
        };
        let mut tx =
            db.pool().begin().await.map_err(|err| {
                dbos::Error::Application(DeliveryFailed(format!("begin tx: {err}")))
            })?;
        let created = MessageRepository::create(
            db,
            &mut tx,
            NewMessage {
                room_id: trigger.room_id,
                creator_id: webhook.user_id,
                client_message_id: key.clone(),
                body: preview.clone(),
            },
        )
        .await
        .map_err(|err| {
            dbos::Error::Application(DeliveryFailed(format!("create reply: {err:?}")))
        })?;
        tx.commit().await.map_err(|err| {
            dbos::Error::Application(DeliveryFailed(format!("commit reply: {err}")))
        })?;
        db.set_delivery_reply(&key, created.id)
            .await
            .map_err(|err| {
                dbos::Error::Application(DeliveryFailed(format!("record reply {key}: {err:?}")))
            })?;
        Ok::<_, dbos::Error<DeliveryFailed>>(created.id)
    })
    .await
}

/// Workflow body: post, then create the reply (which no-ops on retry via
/// the idempotency key).
#[tracing::instrument(name = "deliver_webhook", skip(input), fields(bot_id = input.bot_id, message_id = input.message_id))]
pub async fn deliver_webhook(input: DeliverWebhook) -> Result<i64, dbos::Error<DeliveryFailed>> {
    let Some(db) = worker::db() else {
        return Err(dbos::Error::Application(DeliveryFailed(
            "worker database not initialized".to_string(),
        )));
    };
    let Some(http) = worker::http() else {
        return Err(dbos::Error::Application(DeliveryFailed(
            "worker http client not initialized".to_string(),
        )));
    };
    let reply = post_payload(&http, &db, &input).await?;
    create_reply(&db, &input, &reply).await
}

/// Register webhook workflows on a [`dbos::DBOS`] instance. Returns the
/// ref so the relay can start executions by id.
pub fn register(
    dbos: &dbos::DBOS,
) -> dbos::Result<dbos::WorkflowRef<DeliverWebhook, i64, DeliveryFailed>> {
    let registered = dbos.register_workflow("deliver_webhook", deliver_webhook)?;
    debug_assert_eq!(registered.name(), "deliver_webhook");
    Ok(registered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_registers_under_stable_name() {
        let dbos = dbos::DBOS::new(dbos::Config::new("topcamp-test", "postgres://unused"));
        let registered = dbos
            .register_workflow("deliver_webhook", deliver_webhook)
            .expect("registration succeeds");
        assert_eq!(registered.name(), "deliver_webhook");
    }

    #[test]
    fn delivery_key_shape() {
        let input = DeliverWebhook {
            bot_id: 7,
            message_id: 42,
        };
        assert_eq!(delivery_key(&input), "webhook-delivery:42:7");
    }
}
