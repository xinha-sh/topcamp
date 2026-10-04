//! `PurgeBlob` workflow (replaces the `PurgeBlob` job).
//!
//! Evidence: MIGRATION_NOTES.md "DBOS workflow detailed design".
//! Ordering is load-bearing: delete ROWS first so a retry can never
//! resurrect; object delete is idempotent, so repeating it is safe.

use serde::{Deserialize, Serialize};
use topcamp_db::repositories::AttachmentRepository;
use topcamp_db::PgDb;
use topcamp_storage::{BlobStore, S3BlobStore};

use crate::worker;

pub use topcamp_storage::variant_key;

/// Workflow input: the blob to purge (with its derived rows).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurgeBlob {
    pub blob_id: i64,
}

/// Durable failure for purge steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurgeFailed(pub String);

impl std::fmt::Display for PurgeFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "blob purge failed: {}", self.0)
    }
}

impl std::error::Error for PurgeFailed {}

type Fallible<T> = Result<T, dbos::Error<PurgeFailed>>;

/// Retry policy for row deletes: repeat deletes are no-ops, so repeats
/// are safe (and a retry can never resurrect — rows stay gone).
fn db_retry() -> dbos::StepOptions<PurgeFailed> {
    dbos::StepOptions {
        max_attempts: 5,
        interval: std::time::Duration::from_secs(1),
        backoff_rate: 2.0,
        max_interval: std::time::Duration::from_secs(30),
        ..Default::default()
    }
}

/// Step 0: collect every object key (blob + variants) BEFORE the rows
/// go away. Read-only and checkpointed: resume reuses the recorded keys
/// rather than re-reading rows that are already gone.
pub async fn collect_keys(db: &PgDb, blob_id: i64) -> Fallible<Vec<String>> {
    dbos::step_with("collect_keys", db_retry(), || async move {
        let keys = match db.find_blob(blob_id).await.map_err(|err| {
            dbos::Error::Application(PurgeFailed(format!("find blob {blob_id}: {err:?}")))
        })? {
            None => Vec::new(),
            Some(blob) => {
                let mut keys = vec![blob.key.clone()];
                let digests = db.variant_digests(blob_id).await.map_err(|err| {
                    dbos::Error::Application(PurgeFailed(format!(
                        "variant digests {blob_id}: {err:?}"
                    )))
                })?;
                keys.extend(digests.iter().map(|d| variant_key(&blob.key, d)));
                keys
            }
        };
        Ok::<_, dbos::Error<PurgeFailed>>(keys)
    })
    .await
}

/// Step 1: delete attachment + variant/preview rows. Rows first: if the
/// workflow dies after this step, resume finds no rows and only re-runs the
/// idempotent object deletes below — never a resurrection.
pub async fn delete_rows(db: &PgDb, blob_id: i64) -> Fallible<u64> {
    dbos::step_with("delete_rows", db_retry(), || async move {
        db.delete_blob(blob_id).await.map_err(|err| {
            dbos::Error::Application(PurgeFailed(format!("delete rows {blob_id}: {err:?}")))
        })
    })
    .await
}

/// Step 2: delete the bytes from the blob store. Idempotent by key
/// (missing keys are a no-op), so repeats and resumes are safe.
pub async fn delete_objects(store: &S3BlobStore, keys: Vec<String>) -> Fallible<()> {
    // `keys` is borrowed, never moved: the step body is `FnMut` (retries
    // re-call it), so each attempt clones afresh from the borrow.
    dbos::step_with("delete_objects", db_retry(), || async {
        for key in keys.clone() {
            store.delete(&key).await.map_err(|err| {
                dbos::Error::Application(PurgeFailed(format!("delete object {key}: {err}")))
            })?;
        }
        Ok::<_, dbos::Error<PurgeFailed>>(())
    })
    .await
}

/// Workflow body: keys, then rows, then objects. Never any other order.
#[tracing::instrument(name = "purge_blob", skip(input), fields(blob_id = input.blob_id))]
pub async fn purge_blob(input: PurgeBlob) -> Fallible<()> {
    let Some(db) = worker::db() else {
        return Err(dbos::Error::Application(PurgeFailed(
            "worker database not initialized".to_string(),
        )));
    };
    let Some(store) = worker::store() else {
        return Err(dbos::Error::Application(PurgeFailed(
            "worker blob store not initialized".to_string(),
        )));
    };
    let keys = collect_keys(&db, input.blob_id).await?;
    delete_rows(&db, input.blob_id).await?;
    delete_objects(&store, keys).await
}

/// Register purge workflows on a [`dbos::DBOS`] instance. Returns the
/// ref so the relay can start executions by id.
pub fn register(dbos: &dbos::DBOS) -> dbos::Result<dbos::WorkflowRef<PurgeBlob, (), PurgeFailed>> {
    let registered = dbos.register_workflow("purge_blob", purge_blob)?;
    debug_assert_eq!(registered.name(), "purge_blob");
    Ok(registered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_registers_under_stable_name() {
        let dbos = dbos::DBOS::new(dbos::Config::new("topcamp-test", "postgres://unused"));
        let registered = dbos
            .register_workflow("purge_blob", purge_blob)
            .expect("registration succeeds");
        assert_eq!(registered.name(), "purge_blob");
    }

    #[test]
    fn variant_keys_nest_under_blob_key() {
        assert_eq!(variant_key("abc", "d1"), "abc/variants/d1");
    }
}
