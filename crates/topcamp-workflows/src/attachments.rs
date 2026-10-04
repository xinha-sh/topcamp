//! `ProcessAttachment` workflow (replaces `ActiveStorage::AnalyzeJob` +
//! variant/preview processing, §22).
//!
//! Evidence: MIGRATION_NOTES.md "DBOS workflow detailed design" + the
//! storage trace (staged-blob protocol, 4-slot media semaphore).
//! Invariants: the ORIGINAL bytes are never deleted on derived-asset
//! failure; variant/preview work is idempotent by content digest; the media
//! semaphore becomes workflow concurrency control (queue options at
//! schedule time, not a semaphore in code).
//!
//! Pipeline (§22): validation → metadata extraction → content processing
//! (PNG/JPEG thumbnail) → `attachment_ready` outbox event for the
//! broadcast relay. GIF/WebP bytes are analyzed (type recorded) but get no
//! derived assets — no decoder is wired for them, and a skipped variant is
//! honest where a fake one would corrupt the store.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use topcamp_db::outbox;
use topcamp_db::repositories::AttachmentRepository;
use topcamp_db::PgDb;
use topcamp_storage::{BlobStore, S3BlobStore};

use crate::purge::variant_key;
use crate::worker;

/// Workflow input: the staged blob row awaiting processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessAttachment {
    pub blob_id: i64,
}

/// Durable failure for attachment steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentFailed(pub String);

impl std::fmt::Display for AttachmentFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "attachment processing failed: {}", self.0)
    }
}

impl std::error::Error for AttachmentFailed {}

type Fallible<T> = Result<T, dbos::Error<AttachmentFailed>>;

/// What analysis found. `analyzed: true` merges into blob metadata, as
/// upstream does after `AnalyzeJob`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Analysis {
    pub content_type: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_secs: Option<u64>,
}

/// Thumbnail bounds: 1200x800 max, aspect preserved (per the storage
/// trace). Anything already smaller is stored as-is (no upscale).
pub const THUMB_MAX_WIDTH: u32 = 1200;
pub const THUMB_MAX_HEIGHT: u32 = 800;

/// Variation digest for the thumbnail: hex sha256 of the transform spec,
/// mirroring how Rails digests variation transforms. Stable across
/// retries, so `record_variant` reuses the row instead of duplicating.
pub fn thumb_digest() -> String {
    format!("{:x}", Sha256::digest("resize_to_limit:1200x800,png"))
}

/// Content sniffing without a decoder farm: PNG/JPEG go through `image`
/// (dims + validation); GIF/WebP are recognized by magic only.
fn sniff(bytes: &[u8]) -> (String, Option<(u32, u32)>) {
    if let Ok(img) = image::load_from_memory(bytes) {
        let format = image::guess_format(bytes).ok();
        let content_type = match format {
            Some(image::ImageFormat::Png) => "image/png",
            Some(image::ImageFormat::Jpeg) => "image/jpeg",
            _ => "application/octet-stream",
        }
        .to_string();
        return (content_type, Some((img.width(), img.height())));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return ("image/gif".to_string(), None);
    }
    if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return ("image/webp".to_string(), None);
    }
    ("application/octet-stream".to_string(), None)
}

/// Retry policy for row/store IO. Safe: reads, idempotent puts, and
/// `ON CONFLICT`-guarded variant records.
fn io_retry() -> dbos::StepOptions<AttachmentFailed> {
    dbos::StepOptions {
        max_attempts: 5,
        interval: std::time::Duration::from_secs(1),
        backoff_rate: 2.0,
        max_interval: std::time::Duration::from_secs(30),
        ..Default::default()
    }
}

/// Step 1: analyze the staged bytes (identify, size-verify, probe media).
/// Read-only against the original: never mutates or deletes it. A size
/// mismatch fails DURABLY (retry cannot fix corrupt staged bytes).
pub async fn analyze(db: &PgDb, store: &S3BlobStore, blob_id: i64) -> Fallible<Analysis> {
    dbos::step_with("analyze", io_retry(), || async move {
        let blob = db.find_blob(blob_id).await.map_err(|err| {
            dbos::Error::Application(AttachmentFailed(format!("find blob {blob_id}: {err:?}")))
        })?;
        let Some(blob) = blob else {
            return Err(dbos::Error::Application(AttachmentFailed(format!(
                "blob {blob_id} has no row"
            ))));
        };
        let bytes = store.get(&blob.key).await.map_err(|err| {
            dbos::Error::Application(AttachmentFailed(format!("fetch {}: {err}", blob.key)))
        })?;
        let Some(bytes) = bytes else {
            return Err(dbos::Error::Application(AttachmentFailed(format!(
                "blob {blob_id} has no staged bytes"
            ))));
        };
        if bytes.len() as i64 != blob.byte_size {
            return Err(dbos::Error::Application(AttachmentFailed(format!(
                "blob {blob_id} size mismatch: row says {}, store has {}",
                blob.byte_size,
                bytes.len()
            ))));
        }
        let (content_type, dims) = sniff(&bytes);
        let (width, height) = dims.unzip();
        let fragment = serde_json::json!({
            "analyzed": true,
            "width": width,
            "height": height,
        })
        .to_string();
        db.update_blob_metadata(blob_id, Some(&content_type), &fragment)
            .await
            .map_err(|err| {
                dbos::Error::Application(AttachmentFailed(format!(
                    "merge metadata {blob_id}: {err:?}"
                )))
            })?;
        Ok::<_, dbos::Error<AttachmentFailed>>(Analysis {
            content_type,
            width,
            height,
            duration_secs: None,
        })
    })
    .await
}

/// Step 2: generate the thumbnail for raster images (PNG/JPEG with dims).
/// Idempotent by variation digest: object put FIRST, row record second, so
/// a crash between them leaves an orphan object (re-put, never a lost
/// row). Non-raster content yields no variants — the original still
/// finalizes. A failure here FAILS ONLY this step: the original row and
/// staged bytes survive untouched.
pub async fn derive_assets(
    db: &PgDb,
    store: &S3BlobStore,
    blob_id: i64,
    analysis: &Analysis,
) -> Fallible<Vec<String>> {
    // Borrowed, not moved: the step body is `FnMut`, so each attempt
    // clones afresh from the borrow.
    dbos::step_with("derive_assets", io_retry(), || async {
        let raster = matches!(analysis.content_type.as_str(), "image/png" | "image/jpeg")
            && analysis.width.is_some()
            && analysis.height.is_some();
        if !raster {
            return Ok::<_, dbos::Error<AttachmentFailed>>(Vec::new());
        }
        let digest = thumb_digest();
        if db
            .find_variant(blob_id, &digest)
            .await
            .map_err(|err| {
                dbos::Error::Application(AttachmentFailed(format!(
                    "find variant {blob_id}: {err:?}"
                )))
            })?
            .is_some()
        {
            // Row exists ⇒ the put below already landed (put precedes
            // record), so there is nothing to redo.
            return Ok::<_, dbos::Error<AttachmentFailed>>(vec![digest]);
        }
        let blob = db.find_blob(blob_id).await.map_err(|err| {
            dbos::Error::Application(AttachmentFailed(format!("find blob {blob_id}: {err:?}")))
        })?;
        let Some(blob) = blob else {
            return Err(dbos::Error::Application(AttachmentFailed(format!(
                "blob {blob_id} vanished mid-processing"
            ))));
        };
        let bytes = store
            .get(&blob.key)
            .await
            .map_err(|err| {
                dbos::Error::Application(AttachmentFailed(format!("fetch {}: {err}", blob.key)))
            })?
            .ok_or_else(|| {
                dbos::Error::Application(AttachmentFailed(format!(
                    "blob {blob_id} lost its staged bytes"
                )))
            })?;
        let img = image::load_from_memory(&bytes).map_err(|err| {
            dbos::Error::Application(AttachmentFailed(format!("decode {blob_id}: {err}")))
        })?;
        let thumb = img.thumbnail(THUMB_MAX_WIDTH, THUMB_MAX_HEIGHT);
        let mut encoded = Vec::new();
        thumb
            .write_to(
                &mut std::io::Cursor::new(&mut encoded),
                image::ImageFormat::Png,
            )
            .map_err(|err| {
                dbos::Error::Application(AttachmentFailed(format!("encode {blob_id}: {err}")))
            })?;
        let key = variant_key(&blob.key, &digest);
        store
            .put(&key, encoded, Some("image/png"))
            .await
            .map_err(|err| {
                dbos::Error::Application(AttachmentFailed(format!("put {key}: {err}")))
            })?;
        db.record_variant(blob_id, &digest).await.map_err(|err| {
            dbos::Error::Application(AttachmentFailed(format!(
                "record variant {blob_id}: {err:?}"
            )))
        })?;
        Ok::<_, dbos::Error<AttachmentFailed>>(vec![digest])
    })
    .await
}

/// Step 3: flip the blob to processed + schedule the ready notification.
/// The `attachment_ready` outbox row commits in its own tx; the broadcast
/// relay delivers it after commit (never inside the workflow tx, §19).
pub async fn finalize(db: &PgDb, blob_id: i64) -> Fallible<()> {
    dbos::step_with("finalize", io_retry(), || async move {
        db.update_blob_metadata(blob_id, None, r#"{"processed":true}"#)
            .await
            .map_err(|err| {
                dbos::Error::Application(AttachmentFailed(format!(
                    "mark processed {blob_id}: {err:?}"
                )))
            })?;
        let mut tx = db.pool().begin().await.map_err(|err| {
            dbos::Error::Application(AttachmentFailed(format!("begin tx: {err}")))
        })?;
        outbox::publish(
            &mut tx,
            "attachment_ready",
            &format!("{{\"blob_id\":{blob_id}}}"),
        )
        .await
        .map_err(|err| {
            dbos::Error::Application(AttachmentFailed(format!("publish ready: {err}")))
        })?;
        tx.commit().await.map_err(|err| {
            dbos::Error::Application(AttachmentFailed(format!("commit ready: {err}")))
        })?;
        Ok::<_, dbos::Error<AttachmentFailed>>(())
    })
    .await
}

/// Workflow body: analyze → derive → finalize. Originals outlive every
/// failure mode below them.
#[tracing::instrument(name = "process_attachment", skip(input), fields(blob_id = input.blob_id))]
pub async fn process_attachment(input: ProcessAttachment) -> Fallible<()> {
    let Some(db) = worker::db() else {
        return Err(dbos::Error::Application(AttachmentFailed(
            "worker database not initialized".to_string(),
        )));
    };
    let Some(store) = worker::store() else {
        return Err(dbos::Error::Application(AttachmentFailed(
            "worker blob store not initialized".to_string(),
        )));
    };
    let analysis = analyze(&db, &store, input.blob_id).await?;
    derive_assets(&db, &store, input.blob_id, &analysis).await?;
    finalize(&db, input.blob_id).await
}

/// Register attachment workflows on a [`dbos::DBOS`] instance. Returns
/// the ref so the relay can start executions by id.
pub fn register(
    dbos: &dbos::DBOS,
) -> dbos::Result<dbos::WorkflowRef<ProcessAttachment, (), AttachmentFailed>> {
    let registered = dbos.register_workflow("process_attachment", process_attachment)?;
    debug_assert_eq!(registered.name(), "process_attachment");
    Ok(registered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_registers_under_stable_name() {
        let dbos = dbos::DBOS::new(dbos::Config::new("topcamp-test", "postgres://unused"));
        let registered = dbos
            .register_workflow("process_attachment", process_attachment)
            .expect("registration succeeds");
        assert_eq!(registered.name(), "process_attachment");
    }

    #[test]
    fn thumb_digest_is_stable_hex() {
        let digest = thumb_digest();
        assert_eq!(digest, thumb_digest());
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sniff_recognizes_raster_magic() {
        // 1x1 transparent PNG (valid CRCs).
        const PNG: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x60, 0x00, 0x02, 0x00, 0x00, 0x05, 0x00, 0x01, 0x7A, 0x5E, 0xAB, 0x3F,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let (content_type, dims) = sniff(PNG);
        assert_eq!(content_type, "image/png");
        assert_eq!(dims, Some((1, 1)));
        assert_eq!(sniff(b"GIF89a...."), ("image/gif".to_string(), None));
        assert_eq!(
            sniff(b"junk"),
            ("application/octet-stream".to_string(), None)
        );
    }
}
