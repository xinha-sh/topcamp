//! Dev server for browser smoke tests: serves the Topcamp router on
//! `HOST`/`PORT` (default `127.0.0.1:3000`).
//!
//! The pool is lazy, so pages that answer before any query runs (`/`,
//! `/login`, `/up`) serve without a live PostgreSQL. DB-backed routes fail
//! closed until `DATABASE_URL` points at a migrated database.
//!
//! Two boot tasks need the database and degrade loudly without it:
//! presence reset (`Membership::disconnect_all` — stale `connected_at`
//! from a previous boot) and the `LISTEN cable` bridge (worker `NOTIFY`s
//! fanned out into the broker). Both log a warning and continue when the
//! database is unreachable, so the smoke server still serves.
//!
//! After changing markup, rebuild plus re-bundle so the stylesheet serves:
//! `cargo build -p topcamp-web --bin serve && topcoat asset bundle
//! --bin serve -p topcamp-web`. Then run:
//! `PORT=3000 cargo run -p topcamp-web --bin serve`

use topcamp_cable::Cable;
use topcamp_db::logging::init_stderr;
use topcamp_db::repositories::MembershipRepository;
use topcamp_db::{PgDb, connect_lazy};
use topcamp_web::{
    router,
    state::{AppState, StreamKey},
};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Without this every request/cable log below is dropped (§39).
    init_stderr().map_err(std::io::Error::other)?;
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://unused".to_string());
    let pool = connect_lazy(&url).expect("lazy pool builds without a live database");
    let db = PgDb::new(pool);
    let cable = Cable::new();
    let state = AppState::new(db.clone(), cable.clone())
        .with_stream_key(StreamKey::from_env_or_ephemeral());

    // Avatar blob store (first-run uploads). Handle construction is I/O-
    // free; bucket bootstrap runs in the background and degrades loudly
    // (avatar uploads 500 until RustFS answers, everything else serves).
    let store_config = topcamp_storage::S3Config::dev();
    match topcamp_storage::S3BlobStore::new(&store_config) {
        Ok(store) => {
            let _ = topcamp_web::first_run::init_store(store.clone());
            tokio::spawn(async move {
                if let Err(err) = store.ensure_bucket(&store_config).await {
                    tracing::warn!(
                        error = err.to_string(),
                        "avatar bucket bootstrap skipped (no RustFS?)"
                    );
                }
            });
        }
        Err(err) => tracing::warn!(
            error = err.to_string(),
            "avatar store disabled (bad RustFS credentials?)"
        ),
    }

    // Boot presence reset: connections from a previous boot are gone.
    let boot_db = db.clone();
    tokio::spawn(async move {
        if let Err(err) = MembershipRepository::disconnect_all(&boot_db).await {
            tracing::warn!(
                error = format!("{err:?}"),
                "presence reset skipped (no database?)"
            );
        }
    });

    // Worker→web bridge: `NOTIFY cable, '{"stream","message"}'` rows
    // become broker fanout. Own connection (LISTEN blocks its session).
    let bridge_cable = cable.clone();
    let bridge_url = url.clone();
    tokio::spawn(async move {
        run_bridge(&bridge_url, &bridge_cable).await;
    });

    topcoat::start(router(state)).await
}

async fn run_bridge(database_url: &str, cable: &Cable) {
    let mut listener = match sqlx::postgres::PgListener::connect(database_url).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::warn!(
                error = err.to_string(),
                "cable bridge disabled (no database?)"
            );
            return;
        }
    };
    if let Err(err) = listener.listen("cable").await {
        tracing::warn!(error = err.to_string(), "cable bridge LISTEN failed");
        return;
    }
    loop {
        match listener.recv().await {
            Ok(notification) => {
                let payload = notification.payload();
                match serde_json::from_str::<serde_json::Value>(payload) {
                    Ok(json) => {
                        let stream = json.get("stream").and_then(|v| v.as_str());
                        let message = json.get("message");
                        match (stream, message) {
                            (Some(stream), Some(message)) => {
                                cable.publish(stream, &message.to_string());
                            }
                            _ => {
                                tracing::warn!("cable bridge: malformed notify (no stream/message)")
                            }
                        }
                    }
                    Err(_) => tracing::warn!("cable bridge: malformed notify (not JSON)"),
                }
            }
            Err(err) => {
                tracing::warn!(
                    error = err.to_string(),
                    "cable bridge recv failed, retrying"
                );
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}
