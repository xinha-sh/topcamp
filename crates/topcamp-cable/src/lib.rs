//! Topcamp realtime protocol: Action Cable JSON frames + stream naming.
//!
//! Wire behavior preserved from the `topcamp_cable` trace (§2.6,
//! MIGRATION_NOTES.md): `/cable` mount, `actioncable-v1-json`
//! subprotocol, lifecycle frames, per-identifier confirm/reject, broadcast
//! frames with pre-encoded payloads, GID-param stream names. Transport
//! (WebSocket upgrade, socket, pubsub fanout) lands on top of this codec.

pub mod broker;
pub mod channels;
pub mod naming;
pub mod protocol;
pub mod registry;
pub mod signed;

pub use broker::{Cable, CHANNEL_CAPACITY};
pub use channels::{
    decode_room_stream, decode_user_rooms_stream, parse_action, parse_identifier, Channel,
};
pub use naming::{gid_param, room_channel_stream, room_stream, user_rooms_stream, user_stream};
pub use protocol::{ClientCommand, DisconnectReason, ServerFrame};
pub use registry::{ConnectionCommand, ConnectionId, Registry};
pub use signed::{sign_stream, verify_stream};

/// Seconds between `ping` frames (`BEAT_INTERVAL` upstream).
pub const BEAT_INTERVAL_SECS: u64 = 3;

/// Subprotocols the mount speaks, in preference order. Negotiation itself
/// walks the CLIENT's list first (see the `/cable` route): the client's
/// first listed protocol that is in this set wins.
pub const SUBPROTOCOLS: [&str; 2] = ["actioncable-v1-json", "actioncable-unsupported"];
