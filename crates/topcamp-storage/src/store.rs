//! The object-store contract (§20). One trait, six operations.

use std::time::Duration;

use crate::error::StorageError;

/// Object facts from [`BlobStore::head`], without fetching bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    /// Byte length (`None` when the backend omits it).
    pub size: Option<u64>,
    /// Stored content type, if any.
    pub content_type: Option<String>,
    /// Entity tag for conditional reads, if any.
    pub etag: Option<String>,
}

/// Five-minute signed-URL expiry (§20: service URLs).
pub const PRESIGN_EXPIRY: Duration = Duration::from_secs(300);

#[allow(async_fn_in_trait)]
pub trait BlobStore {
    /// Store `body` at `key`, replacing any existing object.
    async fn put(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<(), StorageError>;
    /// Fetch the object's bytes, or `None` when the key is missing.
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError>;
    /// Fetch the object's facts, or `None` when the key is missing.
    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError>;
    /// Remove the object. Missing keys are a no-op (S3 semantics).
    async fn delete(&self, key: &str) -> Result<(), StorageError>;
    /// Presigned download URL, valid for `expires_in`.
    async fn presign_get(&self, key: &str, expires_in: Duration) -> Result<String, StorageError>;
    /// Presigned upload URL, valid for `expires_in`.
    async fn presign_put(&self, key: &str, expires_in: Duration) -> Result<String, StorageError>;
}
