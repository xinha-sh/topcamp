//! In-process pubsub fanout (traced §2.6 `Broadcasts`, §25–§26).
//!
//! One Tokio broadcast channel per stream. Publishers never block: a
//! lagging socket observes `Lagged` and resubscribes at the live edge,
//! matching the drop-and-log pool-overflow semantics upstream. The `/cable` socket
//! mount (pending) subscribes one receiver per socket per stream;
//! [`Cable::publish`] is called synchronously after commit (§26), while
//! durable-then-notify flows go through DBOS + the outbox instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

/// Per-stream channel depth. A local bound (not traced): deep enough for
/// message bursts, shallow enough that a stuck socket is cut off quickly.
pub const CHANNEL_CAPACITY: usize = 256;

/// A broadcast payload: raw JSON for the frame's `message` field. The
/// socket loop wraps it per subscription
/// (`{"identifier":<its-own>,"message":<payload>}`) because subscribers of
/// one stream carry different identifiers (room vs Turbo vs per-user
/// channels) — a pre-encoded frame would stamp the wrong identifier on
/// most receivers.
pub type Frame = Arc<str>;

/// Cloneable fanout handle. Clone shares every stream (cheap).
#[derive(Debug, Clone, Default)]
pub struct Cable {
    inner: Arc<Mutex<BrokerInner>>,
}

#[derive(Debug, Default)]
struct BrokerInner {
    channels: HashMap<String, broadcast::Sender<Frame>>,
}

impl Cable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a raw-JSON `payload` to `stream`. Returns the number of
    /// live receivers (0 when nobody is subscribed — the payload is then
    /// dropped). Each receiving socket wraps the payload with its own
    /// subscription identifier (see [`Frame`]).
    pub fn publish(&self, stream: &str, payload: &str) -> usize {
        // Payloads must be JSON documents: receivers splice them raw into
        // the broadcast frame, so a non-JSON payload would corrupt the
        // wire. Validated once here, at the single choke point.
        if serde_json::from_str::<serde_json::Value>(payload).is_err() {
            return 0;
        }
        let mut inner = self.inner.lock().expect("cable mutex");
        let sender = inner.channels.entry(stream.to_string()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
            tx
        });
        sender.send(payload.into()).unwrap_or(0)
    }

    /// Subscribe to `stream`. Missed frames are never replayed; a lagging
    /// receiver observes `Lagged` and resubscribes at the live edge.
    pub fn subscribe(&self, stream: &str) -> broadcast::Receiver<Frame> {
        let mut inner = self.inner.lock().expect("cable mutex");
        inner
            .channels
            .entry(stream.to_string())
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
                tx
            })
            .subscribe()
    }

    /// Number of known streams (observability).
    pub fn stream_count(&self) -> usize {
        self.inner.lock().expect("cable mutex").channels.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_reaches_all_subscribers() {
        let cable = Cable::new();
        let mut a = cable.subscribe("room:1");
        let mut b = cable.subscribe("room:1");
        assert_eq!(cable.publish("room:1", r#"{"type":"ping"}"#), 2);
        for rx in [&mut a, &mut b] {
            assert_eq!(rx.recv().await.unwrap().as_ref(), r#"{"type":"ping"}"#);
        }
    }

    #[test]
    fn publish_without_subscribers_drops() {
        let cable = Cable::new();
        assert_eq!(cable.publish("room:9", "{}"), 0);
        assert_eq!(cable.stream_count(), 1);
    }

    #[test]
    fn non_json_payloads_are_refused() {
        let cable = Cable::new();
        let _rx = cable.subscribe("room:9");
        assert_eq!(cable.publish("room:9", "x"), 0);
    }

    #[tokio::test]
    async fn lagging_receiver_is_cut_off_not_stuck() {
        let cable = Cable::new();
        let mut rx = cable.subscribe("room:2");
        for _ in 0..(CHANNEL_CAPACITY + 10) {
            cable.publish("room:2", "{}");
        }
        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
    }
}
