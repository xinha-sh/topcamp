//! Storage failures: missing objects vs backend/transport problems.

use thiserror::Error;

/// Blob-store failure. `NotFound` is routine (conditional reads);
/// everything else is infrastructure.
#[derive(Debug, Error)]
pub enum StorageError {
    /// No object at the key.
    #[error("object not found: {0}")]
    NotFound(String),
    /// Backend, credentials, or transport failure. The message carries the
    /// SDK error; keys are safe to log, never secret material.
    #[error("storage backend error: {0}")]
    Backend(String),
}
