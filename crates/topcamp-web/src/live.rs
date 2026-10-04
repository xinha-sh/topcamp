//! Topcoat-native live bus (UI-05r): typed room/user events.
//!
//! HTTP mutation handlers publish here post-commit; `live!` regions on
//! connected pages subscribe and re-emit. There is no wire protocol:
//! this is an in-process bus (one `serve` process, like the cable
//! broker). The browser only ever speaks Topcoat's runtime protocol.
//!
//! Procedures (client-callable): posting from the composer, typing
//! start/stop, and presence present/absent for hidden tabs. Explicit
//! `/live/*` paths: stable across builds.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use topcamp_db::PgDb;
use topcamp_db::repositories::{
    MembershipRepository, MessageRepository, NewMessage, RoomRepository,
};
use topcoat::{
    Result,
    context::{Cx, app_context},
    runtime::procedure,
};

use crate::state::{AppState, http_error};

/// Room-scoped event: the message tail, per-message regions, and the
/// typing indicator subscribe to these.
#[derive(Debug, Clone)]
pub enum RoomEvent {
    MessageCreated {
        message_id: i64,
    },
    MessageUpdated {
        message_id: i64,
    },
    MessageRemoved {
        client_message_id: String,
    },
    BoostsChanged {
        message_id: i64,
    },
    Typing {
        user_id: i64,
        user_name: String,
        start: bool,
    },
}

/// User-scoped event: the live sidebar subscribes to these (plus the
/// global channel for open-room mutations, which every sidebar shows).
#[derive(Debug, Clone)]
pub enum UserEvent {
    MessageInRoom { room_id: i64 },
    RoomRead { room_id: i64 },
    RoomList,
}

/// Typing indicator TTL (`TYPING_TIMEOUT_MILLISECONDS`).
const TYPING_TTL: Duration = Duration::from_secs(5);

/// Fanout capacity per room/user channel (bursts absorb + `Lagged`
/// resubscribes at the live edge; regions refetch on lag).
const CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, Default)]
struct BusInner {
    rooms: Mutex<HashMap<i64, broadcast::Sender<RoomEvent>>>,
    users: Mutex<HashMap<i64, broadcast::Sender<UserEvent>>>,
    global: Mutex<Option<broadcast::Sender<UserEvent>>>,
    typing: Mutex<HashMap<(i64, i64), (String, Instant)>>,
}

/// Cheap-clone handle to the live bus.
#[derive(Debug, Clone, Default)]
pub struct LiveBus {
    inner: Arc<BusInner>,
}

impl LiveBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get-or-create a room's channel.
    pub fn room(&self, room_id: i64) -> broadcast::Sender<RoomEvent> {
        let mut rooms = self.inner.rooms.lock().expect("bus lock");
        rooms
            .entry(room_id)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .clone()
    }

    /// Get-or-create a user's channel.
    pub fn user(&self, user_id: i64) -> broadcast::Sender<UserEvent> {
        let mut users = self.inner.users.lock().expect("bus lock");
        users
            .entry(user_id)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .clone()
    }

    /// The global channel (open-room list mutations).
    pub fn global(&self) -> broadcast::Sender<UserEvent> {
        let mut global = self.inner.global.lock().expect("bus lock");
        global
            .get_or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .clone()
    }

    /// Record typing; true when newly added (callers publish on true).
    pub fn typing_start(&self, room_id: i64, user_id: i64, name: String) -> bool {
        let mut typing = self.inner.typing.lock().expect("bus lock");
        Self::purge_locked(&mut typing);
        typing
            .insert((room_id, user_id), (name, Instant::now()))
            .is_none()
    }

    /// Clear typing; true when something was cleared.
    pub fn typing_stop(&self, room_id: i64, user_id: i64) -> bool {
        let mut typing = self.inner.typing.lock().expect("bus lock");
        typing.remove(&(room_id, user_id)).is_some()
    }

    /// Current typists in a room (id, name), stale entries purged.
    pub fn typing_now(&self, room_id: i64) -> Vec<(i64, String)> {
        let mut typing = self.inner.typing.lock().expect("bus lock");
        Self::purge_locked(&mut typing);
        let mut now: Vec<(i64, String)> = typing
            .iter()
            .filter(|((room, _), _)| *room == room_id)
            .map(|((_, user), (name, _))| (*user, name.clone()))
            .collect();
        now.sort_by(|a, b| a.1.cmp(&b.1));
        now
    }

    fn purge_locked(typing: &mut HashMap<(i64, i64), (String, Instant)>) {
        typing.retain(|_, (_, at)| at.elapsed() < TYPING_TTL);
    }
}

// --- publishes (post-commit, from mutation handlers) ---------------------

/// A message was stored: the room's tail appends it, every member's
/// sidebar pips (except windows viewing the room, which never pip).
pub(crate) async fn publish_message_create(
    db: &PgDb,
    bus: &LiveBus,
    room_id: i64,
    message_id: i64,
) -> Result<()> {
    bus.room(room_id)
        .send(RoomEvent::MessageCreated { message_id })
        .ok();
    let members = MembershipRepository::member_user_ids(db, room_id)
        .await
        .map_err(http_error)?;
    for member in members {
        bus.user(member)
            .send(UserEvent::MessageInRoom { room_id })
            .ok();
    }
    Ok(())
}

/// A message was edited: its live region re-emits.
pub(crate) fn publish_message_update(bus: &LiveBus, room_id: i64, message_id: i64) {
    bus.room(room_id)
        .send(RoomEvent::MessageUpdated { message_id })
        .ok();
}

/// A boost was added/removed: the message region re-renders (its
/// boosts come from the same `message_views` read as the body).
pub(crate) fn publish_boosts_changed(bus: &LiveBus, room_id: i64, message_id: i64) {
    bus.room(room_id)
        .send(RoomEvent::BoostsChanged { message_id })
        .ok();
}

/// A message was destroyed: its live region emits empty.
pub(crate) fn publish_message_remove(bus: &LiveBus, room_id: i64, client_message_id: &str) {
    bus.room(room_id)
        .send(RoomEvent::MessageRemoved {
            client_message_id: client_message_id.to_string(),
        })
        .ok();
}

/// An open room was created/updated/destroyed: every sidebar rebuilds.
pub(crate) fn publish_open_rooms(bus: &LiveBus) {
    bus.global().send(UserEvent::RoomList).ok();
}

/// A closed/direct room changed: each remaining member's sidebar rebuilds.
pub(crate) async fn publish_member_rooms(db: &PgDb, bus: &LiveBus, room_id: i64) -> Result<()> {
    let members = MembershipRepository::member_user_ids(db, room_id)
        .await
        .map_err(http_error)?;
    for member in members {
        bus.user(member).send(UserEvent::RoomList).ok();
    }
    Ok(())
}

/// A membership was marked read: its sidebar unpips the room.
pub(crate) fn publish_room_read(bus: &LiveBus, user_id: i64, room_id: i64) {
    bus.user(user_id).send(UserEvent::RoomRead { room_id }).ok();
}

// --- procedures ------------------------------------------------------------

/// Composer submit: same store path as the form create, then publish.
/// The room's tail appends the message on every window, including the
/// sender's (no echo problem: the sender holds no local copy).
#[procedure("/live/messages/post")]
pub async fn post_message(cx: &Cx, room_id: i64, body: String) -> Result<Result<String, String>> {
    let app = app_context::<AppState>(cx);
    let Some(user) = crate::auth::current_user(cx).await? else {
        return Ok(Err("You are signed out.".to_string()));
    };
    let room = RoomRepository::find_for_user(&app.db, user.id, room_id)
        .await
        .map_err(http_error)?;
    let Some(room) = room else {
        return Ok(Err("This room was deleted.".to_string()));
    };
    let stored = crate::richtext::canonicalize_plain(&body);
    if stored.trim().is_empty() {
        return Ok(Err("Write a message first.".to_string()));
    }
    let row = app
        .db
        .post_message(NewMessage {
            room_id: room.id,
            creator_id: user.id,
            client_message_id: crate::messages::uuid_v4(),
            body: stored,
        })
        .await
        .map_err(http_error)?;
    publish_message_create(&app.db, &app.bus, room.id, row.id).await?;
    // Sending stops typing (the composer clears).
    if app.bus.typing_stop(room.id, user.id) {
        app.bus
            .room(room.id)
            .send(RoomEvent::Typing {
                user_id: user.id,
                user_name: user.name.clone(),
                start: false,
            })
            .ok();
    }
    Ok(Ok(row.client_message_id))
}

/// Quick-boost submit: same store path as the form create, then
/// publish. Every window's message region re-renders its boosts.
#[procedure("/live/messages/boost")]
pub async fn boost_message(
    cx: &Cx,
    message_id: i64,
    content: String,
) -> Result<Result<i64, String>> {
    let app = app_context::<AppState>(cx);
    let Some(user) = crate::auth::current_user(cx).await? else {
        return Ok(Err("You are signed out.".to_string()));
    };
    let content = content.trim().to_string();
    if content.is_empty() || content.chars().count() > 16 {
        return Ok(Err("Pick an emoji first.".to_string()));
    }
    let message = MessageRepository::find_by_id(&app.db, message_id)
        .await
        .map_err(http_error)?;
    let Some(message) = message else {
        return Ok(Err("This message was deleted.".to_string()));
    };
    let room = RoomRepository::find_for_user(&app.db, user.id, message.room_id)
        .await
        .map_err(http_error)?;
    if room.is_none() {
        return Ok(Err("This message was deleted.".to_string()));
    }
    let id = MessageRepository::create_boost(&app.db, message.id, user.id, &content)
        .await
        .map_err(http_error)?;
    publish_boosts_changed(&app.bus, message.room_id, message.id);
    Ok(Ok(id))
}

/// Typing start (client-throttled by keystroke count); the bus dedupes
/// repeats, so only transitions publish.
#[procedure("/live/typing/start")]
pub async fn typing_start(cx: &Cx, room_id: i64) -> Result<bool> {
    let app = app_context::<AppState>(cx);
    let Ok(Some(user)) = crate::auth::current_user(cx).await else {
        return Ok(false);
    };
    if MembershipRepository::find(&app.db, room_id, user.id)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return Ok(false);
    }
    if app.bus.typing_start(room_id, user.id, user.name.clone()) {
        app.bus
            .room(room_id)
            .send(RoomEvent::Typing {
                user_id: user.id,
                user_name: user.name,
                start: true,
            })
            .ok();
    }
    Ok(true)
}

/// Typing stop (composer emptied/submitted, or tab hidden).
#[procedure("/live/typing/stop")]
pub async fn typing_stop(cx: &Cx, room_id: i64) -> Result<bool> {
    let app = app_context::<AppState>(cx);
    let Ok(Some(user)) = crate::auth::current_user(cx).await else {
        return Ok(false);
    };
    if app.bus.typing_stop(room_id, user.id) {
        app.bus
            .room(room_id)
            .send(RoomEvent::Typing {
                user_id: user.id,
                user_name: user.name,
                start: false,
            })
            .ok();
    }
    Ok(true)
}

/// Tab visible again: mark present (viewing reads the room) + unpip.
#[procedure("/live/presence/present")]
pub async fn presence_present(cx: &Cx, room_id: i64) -> Result<bool> {
    let app = app_context::<AppState>(cx);
    let Ok(Some(user)) = crate::auth::current_user(cx).await else {
        return Ok(false);
    };
    let Ok(Some(membership)) = MembershipRepository::find(&app.db, room_id, user.id).await else {
        return Ok(false);
    };
    MembershipRepository::mark_present(&app.db, membership.id)
        .await
        .ok();
    publish_room_read(&app.bus, user.id, room_id);
    Ok(true)
}

/// Tab hidden: mark absent (the TTL + disconnect cover the rest).
#[procedure("/live/presence/absent")]
pub async fn presence_absent(cx: &Cx, room_id: i64) -> Result<bool> {
    let app = app_context::<AppState>(cx);
    let Ok(Some(user)) = crate::auth::current_user(cx).await else {
        return Ok(false);
    };
    let Ok(Some(membership)) = MembershipRepository::find(&app.db, room_id, user.id).await else {
        return Ok(false);
    };
    MembershipRepository::mark_disconnected(&app.db, membership.id)
        .await
        .ok();
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_channel_fans_out() {
        let bus = LiveBus::new();
        let mut a = bus.room(1).subscribe();
        let mut b = bus.room(1).subscribe();
        bus.room(1)
            .send(RoomEvent::MessageCreated { message_id: 7 })
            .unwrap();
        assert!(matches!(
            a.blocking_recv().unwrap(),
            RoomEvent::MessageCreated { message_id: 7 }
        ));
        assert!(matches!(
            b.blocking_recv().unwrap(),
            RoomEvent::MessageCreated { message_id: 7 }
        ));
        // Other rooms hear nothing.
        let mut other = bus.room(2).subscribe();
        assert!(other.try_recv().is_err());
    }

    #[test]
    fn typing_dedupes_and_purges() {
        let bus = LiveBus::new();
        assert!(bus.typing_start(1, 9, "Kim".to_string()));
        assert!(!bus.typing_start(1, 9, "Kim".to_string()));
        assert_eq!(bus.typing_now(1), vec![(9, "Kim".to_string())]);
        assert!(bus.typing_stop(1, 9));
        assert!(!bus.typing_stop(1, 9));
        assert!(bus.typing_now(1).is_empty());
    }

    #[test]
    fn user_channels_are_independent() {
        let bus = LiveBus::new();
        let mut rx1 = bus.user(1).subscribe();
        let mut rx2 = bus.user(2).subscribe();
        bus.user(1).send(UserEvent::RoomList).unwrap();
        assert!(matches!(rx1.blocking_recv().unwrap(), UserEvent::RoomList));
        assert!(rx2.try_recv().is_err());
        bus.user(2)
            .send(UserEvent::RoomRead { room_id: 3 })
            .unwrap();
        assert!(matches!(
            rx2.blocking_recv().unwrap(),
            UserEvent::RoomRead { room_id: 3 }
        ));
        assert!(rx1.try_recv().is_err());
    }
}
