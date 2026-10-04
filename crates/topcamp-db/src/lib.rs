//! PostgreSQL application layer for Topcamp.
//!
//! Design source: `MIGRATION_NOTES.md` ("PostgreSQL schema plan",
//! "DB concurrency → PG pooling", "Repositories"). Schema itself lives in
//! `migrations/0001_init.sql` (+ transactional outbox `0002_outbox.sql`).
//!
//! Rules honored here:
//! - ids are `i64` (`BIGINT IDENTITY`); datetimes are set via SQL `now()`.
//! - Single-writer serialization is gone (SQLite-specific); PostgreSQL MVCC
//!   handles concurrent writers through a shared pool.
//! - `after_commit` hooks become outbox rows inserted in-tx ([`outbox`]).
//! - No `DatabaseService` god-type: one trait per aggregate ([`repositories`]).

pub mod error;
pub mod logging;
pub mod outbox;
pub mod pg;
pub mod pool;
pub mod repositories;

pub use error::DbError;
pub use pg::PgDb;
pub use pool::{connect, connect_lazy, connect_with_max_connections, DEFAULT_MAX_CONNECTIONS};
