//! Action Cable JSON frame codec (traced §2.6).
//!
//! Client→server commands carry `command` + `identifier`; `message` commands
//! additionally carry `data` (itself a JSON-encoded string upstream).
//! Server→client frames: `welcome` on connect, `ping` every 3s
//! (`BEAT_INTERVAL`), `confirm_subscription` / `reject_subscription` per
//! identifier, `disconnect` with reason + reconnect flag, and `broadcast`
//! frames with pre-encoded payloads.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A command sent by the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "lowercase")]
pub enum ClientCommand {
    /// `{"command":"subscribe","identifier":"..."}`
    Subscribe { identifier: String },
    /// `{"command":"unsubscribe","identifier":"..."}`
    Unsubscribe { identifier: String },
    /// `{"command":"message","identifier":"...","data":"..."}`
    Message { identifier: String, data: String },
}

/// Disconnect reason. `reconnect` passes through unvalidated; Rails may
/// close bare (reason null) — represented as [`DisconnectReason::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisconnectReason {
    Unauthorized,
    InvalidRequest,
    ServerRestart,
    Remote,
    /// Bare close or an unrecognized reason: reconnect decided by client.
    #[serde(other)]
    Unknown,
}

/// A frame sent to the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerFrame {
    /// `{"type":"welcome"}` — first frame on connect.
    Welcome {
        #[serde(rename = "type")]
        kind: WelcomeKind,
    },
    /// `{"type":"ping","message":<unix sec>}` — every 3s.
    Ping {
        #[serde(rename = "type")]
        kind: PingKind,
        message: i64,
    },
    /// `{"identifier":"...","type":"confirm_subscription"}`
    Confirm {
        identifier: String,
        #[serde(rename = "type")]
        kind: ConfirmKind,
    },
    /// `{"identifier":"...","type":"reject_subscription"}`
    Reject {
        identifier: String,
        #[serde(rename = "type")]
        kind: RejectKind,
    },
    /// `{"type":"disconnect","reason":…,"reconnect":…}`
    Disconnect {
        #[serde(rename = "type")]
        kind: DisconnectKind,
        reason: Option<DisconnectReason>,
        reconnect: bool,
    },
    /// `{"identifier":"...","message":…}` — pre-encoded broadcast payload.
    Broadcast { identifier: String, message: Value },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WelcomeKind {
    Welcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PingKind {
    Ping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmKind {
    ConfirmSubscription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectKind {
    RejectSubscription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DisconnectKind {
    Disconnect,
}

impl ServerFrame {
    pub fn welcome() -> Self {
        Self::Welcome {
            kind: WelcomeKind::Welcome,
        }
    }

    pub fn ping(now_unix_secs: i64) -> Self {
        Self::Ping {
            kind: PingKind::Ping,
            message: now_unix_secs,
        }
    }

    pub fn confirm(identifier: impl Into<String>) -> Self {
        Self::Confirm {
            identifier: identifier.into(),
            kind: ConfirmKind::ConfirmSubscription,
        }
    }

    pub fn reject(identifier: impl Into<String>) -> Self {
        Self::Reject {
            identifier: identifier.into(),
            kind: RejectKind::RejectSubscription,
        }
    }

    pub fn disconnect(reason: Option<DisconnectReason>, reconnect: bool) -> Self {
        Self::Disconnect {
            kind: DisconnectKind::Disconnect,
            reason,
            reconnect,
        }
    }

    pub fn broadcast(identifier: impl Into<String>, message: Value) -> Self {
        Self::Broadcast {
            identifier: identifier.into(),
            message,
        }
    }

    /// Serialize one frame to its wire bytes.
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("frames are JSON-serializable")
    }

    /// Parse one client command from wire bytes.
    pub fn decode_command(raw: &str) -> Result<ClientCommand, serde_json::Error> {
        serde_json::from_str(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn welcome_wire_shape() {
        assert_eq!(ServerFrame::welcome().encode(), r#"{"type":"welcome"}"#);
    }

    #[test]
    fn ping_wire_shape() {
        assert_eq!(
            ServerFrame::ping(1_700_000_000).encode(),
            r#"{"type":"ping","message":1700000000}"#
        );
    }

    #[test]
    fn confirm_and_reject_wire_shapes() {
        assert_eq!(
            ServerFrame::confirm("room:1").encode(),
            r#"{"identifier":"room:1","type":"confirm_subscription"}"#
        );
        assert_eq!(
            ServerFrame::reject("room:1").encode(),
            r#"{"identifier":"room:1","type":"reject_subscription"}"#
        );
    }

    #[test]
    fn disconnect_wire_shapes() {
        assert_eq!(
            ServerFrame::disconnect(Some(DisconnectReason::ServerRestart), true).encode(),
            r#"{"type":"disconnect","reason":"server_restart","reconnect":true}"#
        );
        // Bare close: null reason passes through.
        assert_eq!(
            ServerFrame::disconnect(None, false).encode(),
            r#"{"type":"disconnect","reason":null,"reconnect":false}"#
        );
    }

    #[test]
    fn broadcast_preserves_payload() {
        let frame = ServerFrame::broadcast("room:1", json!({"action": "append", "n": 1}));
        assert_eq!(
            frame.encode(),
            r#"{"identifier":"room:1","message":{"action":"append","n":1}}"#
        );
    }

    #[test]
    fn client_commands_decode() {
        assert_eq!(
            ServerFrame::decode_command(r#"{"command":"subscribe","identifier":"room:1"}"#)
                .unwrap(),
            ClientCommand::Subscribe {
                identifier: "room:1".to_string()
            }
        );
        assert_eq!(
            ServerFrame::decode_command(
                r#"{"command":"message","identifier":"room:1","data":"{\"a\":1}"}"#
            )
            .unwrap(),
            ClientCommand::Message {
                identifier: "room:1".to_string(),
                data: r#"{"a":1}"#.to_string(),
            }
        );
    }
}
