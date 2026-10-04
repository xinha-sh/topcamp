//! `/cable` mount: Action Cable over Topcoat's websocket boundary (§24).
//!
//! Contract (MIGRATION_NOTES.md §2.6 trace): `actioncable-v1-json`
//! subprotocol with CLIENT-first negotiation, `welcome` on connect, `ping`
//! every 3s, per-identifier `confirm_subscription`/`reject_subscription`,
//! `disconnect` with reasons, broadcast frames wrapping per-subscription
//! identifiers, session-cookie auth (`reject_unauthorized_connection` →
//! 403), and the eight Ruby-named channels with membership guards.
//!
//! Flow per connection: auth → register → welcome → select-loop over
//! socket reads, subscription fan-in, revocation inbox, and the beat
//! ticker. Every exit path runs unsubscribe effects (presence `absent`)
//! and unregisters the connection.

use std::collections::HashMap;

use topcamp_cable::{
    BEAT_INTERVAL_SECS, Cable, Channel, ConnectionCommand, SUBPROTOCOLS, ServerFrame,
    decode_room_stream, decode_user_rooms_stream, parse_action, parse_identifier,
    room_channel_stream, user_stream, verify_stream,
};
use topcamp_db::repositories::{MembershipRepository, MembershipRow, RoomRepository, UserRow};
use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        content::websocket::{Message, WebSocket, WebSocketUpgrade},
        error::forbidden,
        request::headers,
        response::Response,
        route,
    },
};

use crate::state::AppState;

/// Client-first subprotocol negotiation: the client's first listed
/// protocol that is in [`SUBPROTOCOLS`] wins (websocket-driver hybi walk
/// upstream). Topcoat's own picker is server-ordered, so the route
/// computes the winner from the raw header and offers only that.
pub fn negotiate_client_first(requested: Option<&str>) -> Option<&'static str> {
    let requested = requested?;
    requested.split(',').map(str::trim).find_map(|candidate| {
        SUBPROTOCOLS
            .iter()
            .find(|supported| candidate == **supported)
            .copied()
    })
}

#[route(GET "/cable")]
pub async fn cable_mount(cx: &Cx, upgrade: WebSocketUpgrade) -> Result<Response> {
    let app = app_context::<AppState>(cx).clone();
    // Auth BEFORE the upgrade: no session/active user, no socket.
    // `current_user` re-reads the session row from the database on every
    // call (never memoized), which is the traced cable-side re-check: a
    // just-revoked session fails here rather than missing an in-flight
    // disconnect.
    let Some(user) = crate::auth::current_user(cx).await? else {
        tracing::warn!(
            request_id = crate::layers::try_request_id(cx),
            "cable upgrade rejected: no session"
        );
        return Err(forbidden().into());
    };
    let requested = headers(cx)
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok());
    let winner = negotiate_client_first(requested);
    let upgrade = match winner {
        // Offer only the winner: Topcoat then selects exactly it.
        Some(protocol) => upgrade.protocols([protocol]),
        // Lenient: no shared protocol still upgrades (frames are JSON
        // either way); the socket just carries no negotiated protocol.
        None => upgrade,
    };
    tracing::info!(
        request_id = crate::layers::try_request_id(cx),
        user_id = user.id,
        protocol = winner,
        "cable upgrade"
    );
    upgrade.on_upgrade(move |socket| async move {
        run_socket(app, user, socket).await;
    })
}

/// One live subscription: its parsed channel plus the fan-in task pumping
/// one broker stream into the connection's inbox.
struct Subscription {
    channel: Channel,
    membership_id: Option<i64>,
    forwarders: Vec<tokio::task::JoinHandle<()>>,
}

/// A broadcast payload arriving from a subscribed stream.
struct FanIn {
    identifier: String,
    payload: String,
}

async fn run_socket(app: AppState, user: UserRow, socket: WebSocket) {
    let (conn_id, mut inbox) = app.registry.register(user.id);
    tracing::info!(user_id = user.id, connection_id = ?conn_id, "cable connected");
    let mut socket = Some(socket);
    let mut subscriptions: HashMap<String, Subscription> = HashMap::new();
    let (fan_tx, mut fan_rx) = tokio::sync::mpsc::channel::<FanIn>(topcamp_cable::CHANNEL_CAPACITY);
    let mut beat = tokio::time::interval(std::time::Duration::from_secs(BEAT_INTERVAL_SECS));

    // Welcome first, always.
    let hello = ServerFrame::welcome().encode();
    if send_text(socket.as_mut().expect("socket present"), &hello)
        .await
        .is_err()
    {
        app.registry.unregister(user.id, conn_id);
        return;
    }

    loop {
        let sock = socket.as_mut().expect("socket present until close");
        tokio::select! {
            incoming = sock.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        handle_command(&app, &user, sock, &fan_tx, &mut subscriptions, text.as_str()).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    // Binary/ping/pong: pings are auto-answered; anything
                    // else is ignored (Action Cable speaks text only).
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
            fanin = fan_rx.recv() => {
                let Some(fanin) = fanin else { break };
                let frame = encode_broadcast(&fanin.identifier, &fanin.payload);
                if send_text(sock, &frame).await.is_err() {
                    break;
                }
            }
            command = inbox.recv() => {
                match command {
                    Some(ConnectionCommand::Disconnect { reconnect }) => {
                        let frame = ServerFrame::disconnect(
                            Some(topcamp_cable::DisconnectReason::Remote),
                            reconnect,
                        ).encode();
                        let _ = send_text(sock, &frame).await;
                        break;
                    }
                    None => break,
                }
            }
            _ = beat.tick() => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                if send_text(sock, &ServerFrame::ping(now).encode()).await.is_err() {
                    break;
                }
            }
        }
    }

    // Every exit runs unsubscribe effects (presence `absent`) and drops
    // the registration — then the close handshake.
    let live = subscriptions.len();
    close_connection(&app, &user, &mut subscriptions).await;
    app.registry.unregister(user.id, conn_id);
    tracing::info!(
        user_id = user.id,
        connection_id = ?conn_id,
        subscriptions = live,
        "cable disconnected"
    );
    if let Some(sock) = socket.take() {
        let _ = sock.close().await;
    }
}

async fn send_text(socket: &mut WebSocket, text: &str) -> topcoat::Result<()> {
    socket.send(Message::text(text)).await
}

async fn send_reject(socket: &mut WebSocket, identifier: &str) {
    // Identifiers carry channel params (room ids, stream names) — safe to
    // log; never bodies or tokens.
    tracing::info!(identifier, "cable subscription rejected");
    let frame = ServerFrame::reject(identifier).encode();
    let _ = send_text(socket, &frame).await;
}

/// Splice a broadcast frame: the identifier is JSON-encoded, the payload
/// spliced raw (the broker guarantees valid JSON documents).
fn encode_broadcast(identifier: &str, payload: &str) -> String {
    let id = serde_json::to_string(identifier).unwrap_or_else(|_| "\"?\"".to_string());
    format!("{{\"identifier\":{id},\"message\":{payload}}}")
}

async fn handle_command(
    app: &AppState,
    user: &UserRow,
    socket: &mut WebSocket,
    fan_tx: &tokio::sync::mpsc::Sender<FanIn>,
    subscriptions: &mut HashMap<String, Subscription>,
    raw: &str,
) {
    let Ok(command) = ServerFrame::decode_command(raw) else {
        // Undecodable frames are ignored upstream; log at debug so a
        // silent client stays diagnosable (truncated: commands are small).
        let preview: String = raw.chars().take(80).collect();
        tracing::debug!(preview, "cable undecodable command");
        return;
    };
    match command {
        topcamp_cable::ClientCommand::Subscribe { identifier } => {
            subscribe(app, user, socket, fan_tx, subscriptions, &identifier).await;
        }
        topcamp_cable::ClientCommand::Unsubscribe { identifier } => {
            unsubscribe(app, subscriptions, &identifier).await;
        }
        topcamp_cable::ClientCommand::Message { identifier, data } => {
            perform(app, user, subscriptions, &identifier, &data).await;
        }
    }
}

/// Subscribe: parse → authorize → effects → stream fan-in → confirm.
/// Anything unrecognized or unauthorized is rejected, never left hanging.
async fn subscribe(
    app: &AppState,
    user: &UserRow,
    socket: &mut WebSocket,
    fan_tx: &tokio::sync::mpsc::Sender<FanIn>,
    subscriptions: &mut HashMap<String, Subscription>,
    identifier: &str,
) {
    let Some(channel) = parse_identifier(identifier) else {
        send_reject(socket, identifier).await;
        return;
    };
    // Re-subscribe replaces the old one (idempotent by identifier).
    unsubscribe(app, subscriptions, identifier).await;

    let streams: Vec<String>;
    let mut membership_id = None;
    match &channel {
        Channel::Heartbeat => {
            streams = Vec::new();
        }
        Channel::ReadRooms => {
            streams = vec![user_stream(user.id, "reads")];
        }
        Channel::UnreadRooms => {
            streams = vec![user_stream(user.id, "unreads")];
        }
        Channel::Presence { room_id } | Channel::Room { room_id } | Channel::Typing { room_id } => {
            let room_id = *room_id;
            // Membership guard ("channels turn away lost rooms").
            let membership = MembershipRepository::find(&app.db, room_id, user.id)
                .await
                .unwrap_or_default();
            let Some(membership): Option<MembershipRow> = membership else {
                send_reject(socket, identifier).await;
                return;
            };
            let room = RoomRepository::find_by_id(&app.db, room_id)
                .await
                .unwrap_or_default();
            let Some(room) = room else {
                send_reject(socket, identifier).await;
                return;
            };
            membership_id = Some(membership.id);
            // `stream_for @room`: JSON payloads (typing), never Turbo tags.
            streams = vec![room_channel_stream(&room.kind, room.id)];
            if matches!(channel, Channel::Presence { .. }) {
                // Presence subscribe marks the membership connected.
                let _ = MembershipRepository::mark_connected(&app.db, membership.id).await;
            }
        }
        Channel::RoomMessages { stream } | Channel::TurboStreams { stream } => {
            // `RoomMessagesChannel` serves room message streams only; the
            // guard on `Turbo::StreamsChannel` turns those away (upstream
            // `RoomStreamsAreAuthorized`).
            let wants_messages = matches!(channel, Channel::RoomMessages { .. });
            let Some(verified) = verify_stream(app.stream_key.bytes(), stream) else {
                send_reject(socket, identifier).await;
                return;
            };
            if verified.ends_with(":messages") != wants_messages {
                send_reject(socket, identifier).await;
                return;
            }
            if !turbo_authorized(app, user, &verified).await {
                send_reject(socket, identifier).await;
                return;
            }
            streams = vec![verified];
        }
    }

    let mut forwarders = Vec::with_capacity(streams.len());
    for stream in streams {
        forwarders.push(spawn_forwarder(
            app.cable.clone(),
            stream,
            identifier.to_string(),
            fan_tx.clone(),
        ));
    }
    subscriptions.insert(
        identifier.to_string(),
        Subscription {
            channel,
            membership_id,
            forwarders,
        },
    );
    tracing::info!(user_id = user.id, identifier, "cable subscribed");
    let frame = ServerFrame::confirm(identifier).encode();
    let _ = send_text(socket, &frame).await;
}

/// Turbo authorization without Rails bytes (see `channels.rs`): the
/// global `rooms` stream (any signed-in user), own `user_<id>_*`
/// streams, the user's own `<gid>:rooms` stream, or room streams for
/// member rooms.
async fn turbo_authorized(app: &AppState, user: &UserRow, stream: &str) -> bool {
    if stream == "rooms" {
        return true;
    }
    if let Some(rest) = stream.strip_prefix("user_") {
        return rest == format!("{}_reads", user.id) || rest == format!("{}_unreads", user.id);
    }
    // A user's own rooms stream (`<user gid>:rooms`).
    if let Some(owner) = decode_user_rooms_stream(stream) {
        return owner == user.id;
    }
    let Some((_, room_id)) = decode_room_stream(stream) else {
        return false;
    };
    matches!(
        MembershipRepository::find(&app.db, room_id, user.id).await,
        Ok(Some(_))
    )
}

/// One forwarder per (subscription, stream): pumps broker frames into the
/// connection inbox. A lagged receiver resubscribes at the live edge;
/// a full inbox drops (same overflow semantics as the broker).
fn spawn_forwarder(
    cable: Cable,
    stream: String,
    identifier: String,
    fan_tx: tokio::sync::mpsc::Sender<FanIn>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = cable.subscribe(&stream);
        loop {
            match rx.recv().await {
                Ok(payload) => {
                    let fanin = FanIn {
                        identifier: identifier.clone(),
                        payload: payload.as_ref().to_string(),
                    };
                    if fan_tx.try_send(fanin).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    rx = cable.subscribe(&stream);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

async fn unsubscribe(
    app: &AppState,
    subscriptions: &mut HashMap<String, Subscription>,
    identifier: &str,
) {
    let Some(subscription) = subscriptions.remove(identifier) else {
        return;
    };
    for forwarder in subscription.forwarders {
        forwarder.abort();
    }
    // Presence unsubscribe marks the membership disconnected (`absent`).
    if matches!(subscription.channel, Channel::Presence { .. })
        && let Some(membership_id) = subscription.membership_id
    {
        let _ = MembershipRepository::mark_disconnected(&app.db, membership_id).await;
    }
}

async fn close_connection(
    app: &AppState,
    _user: &UserRow,
    subscriptions: &mut HashMap<String, Subscription>,
) {
    let identifiers: Vec<String> = subscriptions.keys().cloned().collect();
    for identifier in identifiers {
        unsubscribe(app, subscriptions, &identifier).await;
    }
}

/// Perform a channel action (`message` command).
async fn perform(
    app: &AppState,
    user: &UserRow,
    subscriptions: &HashMap<String, Subscription>,
    identifier: &str,
    data: &str,
) {
    let Some(subscription) = subscriptions.get(identifier) else {
        return;
    };
    let Some(action) = parse_action(data) else {
        return;
    };
    match &subscription.channel {
        Channel::Presence { room_id } => match action.action.as_str() {
            // `present`: stay connected, mark the room read, tell the
            // user's OTHER windows (`user_<id>_reads`, `{room_id}`).
            "present" => {
                if let Some(membership_id) = subscription.membership_id {
                    let _ = MembershipRepository::mark_present(&app.db, membership_id).await;
                }
                let payload = serde_json::json!({"room_id": room_id}).to_string();
                let stream = user_stream(user.id, "reads");
                let receivers = app.cable.publish(&stream, &payload);
                tracing::info!(
                    user_id = user.id,
                    room_id = *room_id,
                    stream = stream.as_str(),
                    receivers,
                    "cable read receipt"
                );
            }
            // `refresh`: revive a stale connection, no read ping.
            "refresh" => {
                if let Some(membership_id) = subscription.membership_id {
                    let _ = MembershipRepository::mark_refreshed(&app.db, membership_id).await;
                }
            }
            // Explicit `absent` (hidden tab): same as unsubscribe.
            "absent" => {
                if let Some(membership_id) = subscription.membership_id {
                    let _ = MembershipRepository::mark_disconnected(&app.db, membership_id).await;
                }
            }
            _ => {}
        },
        Channel::Typing { room_id } => match action.action.as_str() {
            "start" | "stop" => {
                let room_id = *room_id;
                let Ok(Some(room)) = RoomRepository::find_by_id(&app.db, room_id).await else {
                    return;
                };
                let payload = serde_json::json!({
                    "action": action.action,
                    "user": {"id": user.id, "name": user.name},
                })
                .to_string();
                // `broadcast_to @room`: the `room:<gid>` channel stream.
                let stream = room_channel_stream(&room.kind, room.id);
                let receivers = app.cable.publish(&stream, &payload);
                tracing::info!(
                    user_id = user.id,
                    room_id,
                    action = action.action.as_str(),
                    receivers,
                    "cable typing"
                );
            }
            _ => {}
        },
        // No performable actions on the other channels.
        _ => {}
    }
}

/// Realtime revocation for routes: sign-out and membership loss pass
/// `reconnect: true` (the client replays subscriptions); deactivate/ban
/// pass `false`.
pub fn disconnect_user(app: &AppState, user_id: i64, reconnect: bool) -> usize {
    let signalled = app.registry.disconnect_user(user_id, reconnect);
    tracing::info!(user_id, reconnect, signalled, "cable revocation");
    signalled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_prefers_client_order() {
        // Client lists unsupported first: it wins even though our
        // preference order puts v1 first.
        assert_eq!(
            negotiate_client_first(Some("actioncable-unsupported, actioncable-v1-json")),
            Some("actioncable-unsupported")
        );
        assert_eq!(
            negotiate_client_first(Some("actioncable-v1-json")),
            Some("actioncable-v1-json")
        );
        assert_eq!(negotiate_client_first(Some("soap")), None);
        assert_eq!(negotiate_client_first(None), None);
    }

    #[test]
    fn broadcast_splice_escapes_identifier() {
        assert_eq!(
            encode_broadcast("room:1", r#"{"a":1}"#),
            r#"{"identifier":"room:1","message":{"a":1}}"#
        );
    }
}
