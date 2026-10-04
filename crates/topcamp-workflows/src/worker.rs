//! Worker-process database access for workflow bodies.
//!
//! Registered workflow bodies receive only their deserialized input, so step
//! functions cannot take a pool through the signature. The worker binary
//! calls [`init`] once at startup; bodies resolve it via [`db`]. Failing
//! closed when uninitialized keeps local misuse loud rather than silently
//! running steps against nothing.

use std::sync::OnceLock;

use topcamp_db::PgDb;
use topcamp_storage::S3BlobStore;

static WORKER_DB: OnceLock<PgDb> = OnceLock::new();
static WORKER_STORE: OnceLock<S3BlobStore> = OnceLock::new();
static WORKER_HTTP: OnceLock<reqwest::Client> = OnceLock::new();

/// Install the worker's database handle. Returns `Err` if already set.
pub fn init(db: PgDb) -> Result<(), PgDb> {
    WORKER_DB.set(db).map_err(|_| {
        WORKER_DB
            .get()
            .expect("just failed to set; a value is present")
            .clone()
    })
}

/// The worker's database handle, if [`init`] ran.
pub fn db() -> Option<PgDb> {
    WORKER_DB.get().cloned()
}

/// Install the worker's blob store (`S3BlobStore` is concrete: the
/// `BlobStore` trait has `async fn` and is not object-safe, and every
/// deployment here is RustFS-backed anyway).
pub fn init_store(store: S3BlobStore) -> Result<(), S3BlobStore> {
    WORKER_STORE.set(store).map_err(|_| {
        WORKER_STORE
            .get()
            .expect("just failed to set; a value is present")
            .clone()
    })
}

/// The worker's blob store, if [`init_store`] ran.
pub fn store() -> Option<S3BlobStore> {
    WORKER_STORE.get().cloned()
}

/// Install the shared outbound HTTP client (webhook/push delivery).
pub fn init_http(client: reqwest::Client) -> Result<(), reqwest::Client> {
    WORKER_HTTP.set(client).map_err(|_| {
        WORKER_HTTP
            .get()
            .expect("just failed to set; a value is present")
            .clone()
    })
}

/// The shared outbound HTTP client, if [`init_http`] ran.
pub fn http() -> Option<reqwest::Client> {
    WORKER_HTTP.get().cloned()
}

/// JSON content type for hand-serialized POST bodies. Bodies go through
/// `serde_json::to_vec` + this header so the crate needs no `json` reqwest
/// feature (which would rebuild the whole reqwest subtree).
pub const JSON_CONTENT_TYPE: &str = "application/json";

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn init_then_get_then_reinit_fails() {
        // `connect_lazy` opens no connections, so this runs offline. No
        // other suite test touches worker state, so owning the OnceLock here
        // is safe.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused")
            .expect("lazy pool builds offline");
        super::init(PgDb::new(pool)).expect("first init wins");
        assert!(super::db().is_some());
        let pool2 = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused")
            .expect("lazy pool builds offline");
        assert!(super::init(PgDb::new(pool2)).is_err());
    }
}
