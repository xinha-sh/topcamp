//! Topcamp domain layer: pure business rules, no I/O, no framework.
//!
//! Ported from the traced behavior of `basecamp/once-campfire-rust`
//! (see MIGRATION_NOTES.md). Every rule here has an evidence pointer.

pub mod auth;
pub mod error;
pub mod rooms;
pub mod search;
