//! Topcamp durable-work worker: DBOS executor + outbox relay.
//!
//! Owns nothing synchronous (§19): HTTP commits first, the outbox carries
//! the intent, this process turns rows into workflow executions. One
//! process runs one DBOS executor (recovery included) and the relay loop;
//! run more processes for more throughput (claims are `SKIP LOCKED`).
//!
//! Env: `DATABASE_URL` (also the DBOS system database), `RUSTFS_*` (see
//! `S3Config::dev`), `RELAY_POLL_MS` (default 1000), `RUST_LOG`.

use std::time::Duration;

use topcamp_db::logging::init_stderr;
use topcamp_db::pool;
use topcamp_db::PgDb;
use topcamp_storage::{S3BlobStore, S3Config};
use topcamp_workflows::relay;
use topcamp_workflows::worker;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_stderr()?;

    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://topcamp:topcamp@localhost:5432/topcamp".to_string());
    let db = PgDb::new(pool::connect(&database_url).await?);
    worker::init(db.clone()).map_err(|_| "worker db already initialized")?;

    let store_config = S3Config::dev();
    let store = S3BlobStore::new(&store_config)?;
    store.ensure_bucket(&store_config).await?;
    worker::init_store(store).map_err(|_| "worker store already initialized")?;

    // Integrations trace policies: 7s connect/read, 60s total.
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(7))
        .read_timeout(Duration::from_secs(7))
        .timeout(Duration::from_secs(60))
        .build()?;
    worker::init_http(http).map_err(|_| "worker http already initialized")?;

    let mut config = dbos::Config::new("topcamp", database_url.as_str());
    // Pinned per deploy: recovery keys workflows to the version that wrote
    // them, and a Rust rebuild is not reproducible enough to compute one.
    config.app_version =
        Some(std::env::var("DBOS_APP_VERSION").unwrap_or_else(|_| "dev".to_string()));
    let dbos = dbos::DBOS::new(config);
    let handles = relay::register_all(&dbos)?;
    dbos.launch().await?;
    tracing::info!("worker launched; relay polling");

    let poll = std::env::var("RELAY_POLL_MS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(1));
    // No signal handling by design: termination (SIGTERM/SIGINT default)
    // is a crash, and crashes are the recovered case — DBOS replays
    // interrupted workflows on the next launch, and claimed-but-unfinished
    // outbox rows are reclaimed by `claimed_at` expiry. This also keeps
    // `tokio/signal` out of the feature set (a full tokio-subtree rebuild).
    loop {
        tokio::time::sleep(poll).await;
        if let Err(err) = relay::run_once(&db, &handles, 25).await {
            tracing::warn!(error = format!("{err:?}"), "relay pass failed");
        }
    }
}
