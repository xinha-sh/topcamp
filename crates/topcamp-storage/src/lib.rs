//! Blob storage behind a small object-store abstraction (§20).
//!
//! Bytes live in RustFS (S3-compatible); metadata rows stay in PostgreSQL
//! (`AttachmentRepository`). The [`BlobStore`] trait is the only surface
//! workflows touch: `put/get/head/delete` plus presigned-URL purposes
//! (`presign_get` for downloads, `presign_put` for direct browser uploads).
//! Signed-URL expiry is 5 minutes per the migration spec; long-lived cache
//! headers are served at the HTTP layer, not stored here.

pub mod error;
pub mod s3;
pub mod store;

pub use error::StorageError;
pub use s3::{S3BlobStore, S3Config};
pub use store::{BlobStore, ObjectMeta, PRESIGN_EXPIRY};

/// Object key for a derived variant. Convention (documented here because
/// the schema stores digests, not keys): variant bytes live under the
/// blob's key. Purge collection, attachment derivation, and the web
/// logo/avatar variants all use this helper so they can never disagree.
pub fn variant_key(blob_key: &str, digest: &str) -> String {
    format!("{blob_key}/variants/{digest}")
}
