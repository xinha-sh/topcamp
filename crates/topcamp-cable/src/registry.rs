//! Connection registry for realtime revocation (traced §2.6).
//!
//! Upstream `remote_connections.where(current_user:)` finds every socket of
//! a user for `DisconnectUser`: per-connection `disconnect` with `reason:
//! remote`, `reconnect: true` on sign-out/membership loss (the client
//! replays subscriptions; channels turn away lost rooms) vs `false` on
//! deactivate/ban. Each connection owns an inbox; revocation never blocks
//! the revoker (full inbox ⇒ the socket is already gone).
//!
//! Connection ids are process-local counters, not GIDs: the traced
//! `connection_identifier` (`gid://topcamp/User/<id>`) keys the lookup by
//! user, which is this map's outer key.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

/// A command delivered to one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionCommand {
    /// Send the `disconnect` frame (reason `remote`) and close.
    Disconnect { reconnect: bool },
}

/// Inbox depth per connection. Commands are rare; a full inbox means a
/// dead socket whose cleanup is already queued.
pub const INBOX_CAPACITY: usize = 8;

/// Process-local connection id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(u64);

/// Cloneable registry handle. Clone shares every connection (cheap).
#[derive(Debug, Clone, Default)]
pub struct Registry {
    inner: Arc<Mutex<RegistryInner>>,
    next_id: Arc<AtomicU64>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    by_user: HashMap<i64, HashMap<ConnectionId, mpsc::Sender<ConnectionCommand>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Track a new connection for `user_id`. Returns its id + inbox.
    /// The caller MUST call [`unregister`](Self::unregister) on close.
    pub fn register(&self, user_id: i64) -> (ConnectionId, mpsc::Receiver<ConnectionCommand>) {
        let id = ConnectionId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = mpsc::channel(INBOX_CAPACITY);
        self.inner
            .lock()
            .expect("registry mutex")
            .by_user
            .entry(user_id)
            .or_default()
            .insert(id, tx);
        (id, rx)
    }

    /// Forget a closed connection. Missing entries are a no-op
    /// (double-close is normal on revocation races).
    pub fn unregister(&self, user_id: i64, id: ConnectionId) {
        let mut inner = self.inner.lock().expect("registry mutex");
        if let Some(connections) = inner.by_user.get_mut(&user_id) {
            connections.remove(&id);
            if connections.is_empty() {
                inner.by_user.remove(&user_id);
            }
        }
    }

    /// Disconnect every connection of `user_id`. Returns the connections
    /// signalled. `reconnect: true` on sign-out/membership loss, `false`
    /// on deactivate/ban.
    pub fn disconnect_user(&self, user_id: i64, reconnect: bool) -> usize {
        let inner = self.inner.lock().expect("registry mutex");
        let Some(connections) = inner.by_user.get(&user_id) else {
            return 0;
        };
        let mut signalled = 0;
        for sender in connections.values() {
            // `try_send`: a full/closed inbox is a dead socket; the
            // revoker never blocks.
            if sender
                .try_send(ConnectionCommand::Disconnect { reconnect })
                .is_ok()
            {
                signalled += 1;
            }
        }
        signalled
    }

    /// Live connection count (observability).
    pub fn connection_count(&self) -> usize {
        self.inner
            .lock()
            .expect("registry mutex")
            .by_user
            .values()
            .map(HashMap::len)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revocation_reaches_all_user_connections() {
        let registry = Registry::new();
        let (_, mut a) = registry.register(7);
        let (_, mut b) = registry.register(7);
        let (_, mut other) = registry.register(9);
        assert_eq!(registry.disconnect_user(7, true), 2);
        assert_eq!(
            a.try_recv(),
            Ok(ConnectionCommand::Disconnect { reconnect: true })
        );
        assert_eq!(
            b.try_recv(),
            Ok(ConnectionCommand::Disconnect { reconnect: true })
        );
        assert!(other.try_recv().is_err());
        assert_eq!(registry.disconnect_user(404, false), 0);
    }

    #[test]
    fn unregister_forgets() {
        let registry = Registry::new();
        let (id, _) = registry.register(7);
        assert_eq!(registry.connection_count(), 1);
        registry.unregister(7, id);
        assert_eq!(registry.connection_count(), 0);
        registry.unregister(7, id);
    }
}
