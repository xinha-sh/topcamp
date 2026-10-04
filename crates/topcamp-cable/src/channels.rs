//! App channel dispatch (traced §2.6 `channels/*`).
//!
//! Identifiers are Action Cable JSON: `{"channel":"RoomChannel",
//! "room_id":1}`. Parsing is pure; authorization (membership, Turbo stream
//! ownership) runs against the repositories in the web socket loop.
//!
//! Signed-stream compat note: upstream guards `RoomMessagesChannel` and
//! `Turbo::StreamsChannel` with Rails `MessageVerifier` signatures. The
//! audit explicitly scoped those formats out (TRACES_SUPPLEMENT.md §4:
//! "cross-compat ... requires re-porting these exact formats"), so the
//! guard here is authorization-equivalent instead: the identifier must
//! carry a `signed_stream_name` the server minted (see [`crate::signed`]),
//! and the verified stream must name a room the user belongs to (GID
//! decoded from the stream name), the user's own `user_<id>_*` stream,
//! or the global `rooms` stream. Same access decision, no Rails bytes.

use serde::Deserialize;

/// A subscription identifier, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Channel {
    Heartbeat,
    Presence { room_id: i64 },
    ReadRooms,
    UnreadRooms,
    Room { room_id: i64 },
    RoomMessages { stream: String },
    Typing { room_id: i64 },
    TurboStreams { stream: String },
}

#[derive(Debug, Deserialize)]
struct Identifier {
    channel: String,
    room_id: Option<i64>,
    signed_stream_name: Option<String>,
}

/// Parse a subscription identifier. `None` → `reject_subscription`.
pub fn parse_identifier(raw: &str) -> Option<Channel> {
    let parsed: Identifier = serde_json::from_str(raw).ok()?;
    match parsed.channel.as_str() {
        "HeartbeatChannel" => Some(Channel::Heartbeat),
        "PresenceChannel" => Some(Channel::Presence {
            room_id: parsed.room_id?,
        }),
        "ReadRoomsChannel" => Some(Channel::ReadRooms),
        "UnreadRoomsChannel" => Some(Channel::UnreadRooms),
        "RoomChannel" => Some(Channel::Room {
            room_id: parsed.room_id?,
        }),
        "RoomMessagesChannel" => Some(Channel::RoomMessages {
            stream: parsed.signed_stream_name?,
        }),
        "TypingNotificationsChannel" => Some(Channel::Typing {
            room_id: parsed.room_id?,
        }),
        "Turbo::StreamsChannel" => Some(Channel::TurboStreams {
            stream: parsed.signed_stream_name?,
        }),
        _ => None,
    }
}

/// Decode a room message stream back to `(room_kind, room_id)`.
/// Inverse of [`crate::room_stream`] for the Turbo authorization guard.
pub fn decode_room_stream(stream: &str) -> Option<(String, i64)> {
    use base64::Engine as _;
    let param = stream.strip_suffix(":messages")?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(param)
        .ok()?;
    let gid = String::from_utf8(decoded).ok()?;
    let rest = gid.strip_prefix("gid://topcamp/")?;
    let (model, id) = rest.rsplit_once('/')?;
    Some((model.to_string(), id.parse().ok()?))
}

/// Decode a user's rooms stream back to the user id.
/// Inverse of [`crate::user_rooms_stream`].
pub fn decode_user_rooms_stream(stream: &str) -> Option<i64> {
    use base64::Engine as _;
    let param = stream.strip_suffix(":rooms")?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(param)
        .ok()?;
    let gid = String::from_utf8(decoded).ok()?;
    let rest = gid.strip_prefix("gid://topcamp/User/")?;
    rest.parse().ok()
}

/// A performed channel action: `data` is itself a JSON-encoded string
/// upstream (`{"action":"start",...}`).
#[derive(Debug, Deserialize)]
pub struct ChannelAction {
    pub action: String,
}

pub fn parse_action(data: &str) -> Option<ChannelAction> {
    serde_json::from_str(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_channels() {
        assert_eq!(
            parse_identifier(r#"{"channel":"HeartbeatChannel"}"#),
            Some(Channel::Heartbeat)
        );
        assert_eq!(
            parse_identifier(r#"{"channel":"PresenceChannel","room_id":3}"#),
            Some(Channel::Presence { room_id: 3 })
        );
        assert_eq!(
            parse_identifier(r#"{"channel":"ReadRoomsChannel"}"#),
            Some(Channel::ReadRooms)
        );
        assert_eq!(
            parse_identifier(r#"{"channel":"RoomChannel","room_id":9}"#),
            Some(Channel::Room { room_id: 9 })
        );
        assert_eq!(
            parse_identifier(r#"{"channel":"Turbo::StreamsChannel","signed_stream_name":"s"}"#),
            Some(Channel::TurboStreams {
                stream: "s".to_string()
            })
        );
        assert_eq!(
            parse_identifier(r#"{"channel":"RoomMessagesChannel","signed_stream_name":"s"}"#),
            Some(Channel::RoomMessages {
                stream: "s".to_string()
            })
        );
        assert_eq!(parse_identifier(r#"{"channel":"Nope"}"#), None);
        assert_eq!(parse_identifier(r#"{"channel":"RoomChannel"}"#), None);
        assert_eq!(
            parse_identifier(r#"{"channel":"RoomMessagesChannel","room_id":9}"#),
            None
        );
        assert_eq!(parse_identifier("junk"), None);
    }

    #[test]
    fn room_stream_round_trips() {
        let stream = crate::room_stream("Rooms::Open", 7);
        assert_eq!(
            decode_room_stream(&stream).as_ref(),
            Some(&("Rooms::Open".to_string(), 7))
        );
        assert_eq!(decode_room_stream("user_7_reads"), None);
        assert_eq!(decode_room_stream("!!!:messages"), None);
    }

    #[test]
    fn actions_parse() {
        let action = parse_action(r#"{"action":"start"}"#).unwrap();
        assert_eq!(action.action, "start");
        assert!(parse_action("junk").is_none());
    }
}
