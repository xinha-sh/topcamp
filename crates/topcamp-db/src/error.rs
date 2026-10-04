//! Error mapping: sqlx failures become [`topcamp_domain`] errors.
//!
//! Layer rule (§31): infrastructure detail never crosses the boundary.
//! `RowNotFound` surfaces as `Domain(NotFound)`; every other driver failure
//! becomes `Infrastructure` with a static context string (no SQL text, no
//! connection detail).

use topcamp_domain::error::{DomainError, Error as DomainErrorRoot, InfrastructureError};

/// Failures from this crate's PostgreSQL adapters.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// Any query/executor failure.
    #[error("database operation failed")]
    Sqlx(#[from] sqlx::Error),
}

impl DbError {
    /// `ActiveRecord::RecordNotUnique`: Postgres `23505`. Callers that
    /// rescue it (join's duplicate-email redirect) match on this.
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, DbError::Sqlx(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23505"))
    }
}

impl From<DbError> for DomainErrorRoot {
    fn from(err: DbError) -> Self {
        if err.is_unique_violation() {
            return Self::Domain(DomainError::Conflict);
        }
        match err {
            DbError::Sqlx(sqlx::Error::RowNotFound) => Self::Domain(DomainError::NotFound),
            DbError::Sqlx(_) => Self::Infrastructure(InfrastructureError::new("postgres")),
        }
    }
}
