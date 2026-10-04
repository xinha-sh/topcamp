//! Broadcast/stream naming (traced §2.6 + `naming.rs` supplement).
//!
//! - `gid_param`: unpadded URL-safe Base64 of `gid://topcamp/{Model}/{id}`
//!   (STI names preserved, e.g. `Rooms::Open`).
//! - Room message stream: `<room-gid-param>:messages`.
//! - Per-user streams: `user_<id>_reads`, `user_<id>_unreads`,
//!   `user_<id>_rooms`.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;

/// Unpadded URL-safe Base64 of `gid://topcamp/{model}/{id}`.
pub fn gid_param(model: &str, id: i64) -> String {
    URL_SAFE_NO_PAD.encode(format!("gid://topcamp/{model}/{id}"))
}

/// Message stream for a room, from its STI kind + id:
/// `<gid-param>:messages`. Turbo tags only.
pub fn room_stream(room_kind: &str, room_id: i64) -> String {
    format!("{}:messages", gid_param(room_kind, room_id))
}

/// Room channel stream (`stream_for @room`): `room:<gid-param>`.
/// JSON payloads for `RoomChannel`/`PresenceChannel`/
/// `TypingNotificationsChannel` (typing notifications); Turbo tags
/// never land here.
pub fn room_channel_stream(room_kind: &str, room_id: i64) -> String {
    format!("room:{}", gid_param(room_kind, room_id))
}

/// Per-user stream: `user_<id>_{suffix}` where suffix is one of
/// `reads` / `unreads`.
pub fn user_stream(user_id: i64, suffix: &str) -> String {
    format!("user_{user_id}_{suffix}")
}

/// A user's rooms stream (`turbo_stream_from Current.user, :rooms`):
/// `<user gid param>:rooms`.
pub fn user_rooms_stream(user_id: i64) -> String {
    format!("{}:rooms", gid_param("User", user_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gid_param_is_url_safe_unpadded_base64() {
        let param = gid_param("Rooms::Open", 1);
        assert!(!param.is_empty());
        assert!(
            param
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "param {param}"
        );
        let decoded = URL_SAFE_NO_PAD.decode(&param).unwrap();
        assert_eq!(decoded, b"gid://topcamp/Rooms::Open/1");
    }

    #[test]
    fn room_stream_joins_param_and_messages() {
        let stream = room_stream("Rooms::Open", 1);
        assert!(stream.ends_with(":messages"), "stream {stream}");
        assert_eq!(stream, format!("{}:messages", gid_param("Rooms::Open", 1)));
    }

    #[test]
    fn user_streams_follow_convention() {
        assert_eq!(user_stream(7, "reads"), "user_7_reads");
        assert_eq!(user_stream(7, "unreads"), "user_7_unreads");
        assert_eq!(
            user_rooms_stream(7),
            format!("{}:rooms", gid_param("User", 7))
        );
    }
}
