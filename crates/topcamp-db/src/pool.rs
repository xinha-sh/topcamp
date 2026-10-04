//! PostgreSQL connection pool ("DB concurrency → PG pooling").
//!
//! Upstream ran ONE writer thread + N reader connections (default 8,
//! `db_readers`). The writer queue is SQLite-specific and goes away:
//! repositories take transactions from this pool and PostgreSQL MVCC handles
//! concurrent writers. Pool size mirrors the old reader count.

use sqlx::postgres::{PgPool, PgPoolOptions};

/// Default max connections, mirroring upstream `db_readers` (default 8).
pub const DEFAULT_MAX_CONNECTIONS: u32 = 8;

/// Connect with the default (reader-sized) pool.
pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    connect_with_max_connections(database_url, DEFAULT_MAX_CONNECTIONS).await
}

/// Connect with an explicit pool size.
pub async fn connect_with_max_connections(
    database_url: &str,
    max_connections: u32,
) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(database_url)
        .await
}

/// Build a pool without opening any connection. Requests that answer before
/// any query runs (health, redirects, the sign-in form) serve without a live
/// database; the first query fails closed until the database is reachable.
pub fn connect_lazy(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(DEFAULT_MAX_CONNECTIONS)
        .connect_lazy(database_url)
}
