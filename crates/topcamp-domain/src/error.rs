//! Layered error model (§31 of the migration spec).
//!
//! Four layers; only the web boundary maps them to responses. Infrastructure
//! details (SQL, S3, DBOS internals) never leak past [`Error::Infrastructure`].

/// Errors in pure business rules (authorization, validation, state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    /// `User::can_administer?` failed: administrator OR creator OR new record.
    NotAuthorized,
    /// Record referenced by id does not exist or is out of scope.
    NotFound,
    /// Input rejected by a domain invariant (blank name, bad involvement, …).
    InvalidInput(&'static str),
    /// A unique constraint rejected the write (`RecordNotUnique`).
    /// Callers that rescue it (join's duplicate-email redirect) match
    /// on this; unrescued it renders 500, like upstream.
    Conflict,
}

/// Errors in use-case orchestration (repos, workflows, broadcasts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplicationError {
    Domain(DomainError),
    /// Upstream dependency failed in a way the use case cannot repair
    /// (message kept abstract: no SQL/S3/DBOS detail crosses this line).
    DependencyUnavailable(&'static str),
    /// Deliberate internal failures preserved from upstream
    /// (nil search record, JSON message update, …).
    Internal(&'static str),
}

/// Errors from infrastructure adapters (sqlx, S3 client, DBOS).
/// The inner message is logged, never rendered.
#[derive(Debug)]
pub struct InfrastructureError {
    pub context: &'static str,
}

impl InfrastructureError {
    pub fn new(context: &'static str) -> Self {
        Self { context }
    }
}

impl std::fmt::Display for InfrastructureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "infrastructure failure ({})", self.context)
    }
}

impl std::error::Error for InfrastructureError {}

/// Top-level error: exactly one of the three layers.
#[derive(Debug)]
pub enum Error {
    Domain(DomainError),
    Application(ApplicationError),
    Infrastructure(InfrastructureError),
}

impl From<DomainError> for Error {
    fn from(e: DomainError) -> Self {
        Self::Domain(e)
    }
}

impl From<ApplicationError> for Error {
    fn from(e: ApplicationError) -> Self {
        Self::Application(e)
    }
}

impl From<InfrastructureError> for Error {
    fn from(e: InfrastructureError) -> Self {
        Self::Infrastructure(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infra_error_hides_detail() {
        // The Display impl must not interpolate any inner detail string.
        let e = InfrastructureError::new("secret-conn-string");
        assert_eq!(e.to_string(), "infrastructure failure (secret-conn-string)");
        assert!(!e.to_string().contains("password"));
    }
}
