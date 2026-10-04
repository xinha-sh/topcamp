//! Shared request state + domain-to-HTTP error mapping (§7, §31).
//!
//! Handlers read [`AppState`] from the Topcoat app context
//! (`app_context::<AppState>(cx)`); it must be registered before serving.
//! [`http_error`] converts domain failures to responses without leaking
//! infrastructure detail: `NotFound` → 404, `NotAuthorized` → 403,
//! `InvalidInput` → 400, everything else → a detail-free 500.

use topcamp_cable::{Cable, Registry};
use topcamp_db::PgDb;
use topcamp_domain::error::{ApplicationError, DomainError, Error as DomainErrorRoot};
use topcoat::Error;
use topcoat::router::error::{bad_request, forbidden, not_found};

/// Cloneable app state: a [`PgDb`] handle (pool clone, cheap) plus the
/// realtime fanout and connection registry. All clones are cheap handle
/// clones.
#[derive(Debug, Clone)]
pub struct AppState {
    pub db: PgDb,
    pub cable: Cable,
    pub registry: Registry,
    /// Signs Turbo `signed_stream_name`s (see `topcamp_cable::signed`).
    pub stream_key: StreamKey,
    /// In-process live bus for Topcoat `live!` regions (UI-05r).
    pub bus: crate::live::LiveBus,
}

impl AppState {
    pub fn new(db: PgDb, cable: Cable) -> Self {
        Self {
            db,
            cable,
            registry: Registry::new(),
            stream_key: StreamKey::ephemeral(),
            bus: crate::live::LiveBus::new(),
        }
    }

    pub fn with_stream_key(mut self, key: StreamKey) -> Self {
        self.stream_key = key;
        self
    }
}

/// HMAC key for Turbo stream names. Debug redacts the bytes.
#[derive(Clone)]
pub struct StreamKey(Vec<u8>);

impl StreamKey {
    /// `SECRET_KEY_BASE` (Rails' name) when set, else 32 random bytes.
    /// Ephemeral keys invalidate signed stream names on restart, which
    /// only costs clients a page reload (names are minted per render).
    pub fn from_env_or_ephemeral() -> Self {
        match std::env::var("SECRET_KEY_BASE") {
            Ok(secret) if !secret.trim().is_empty() => Self(secret.into_bytes()),
            _ => Self::ephemeral(),
        }
    }

    pub fn ephemeral() -> Self {
        use rand::RngCore as _;
        let mut bytes = vec![0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for StreamKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StreamKey([redacted])")
    }
}

/// Map a domain error to an HTTP error. Application/infrastructure failures
/// become a static 500 message — never SQL or connection detail.
pub fn http_error(err: DomainErrorRoot) -> Error {
    match err {
        DomainErrorRoot::Domain(DomainError::NotFound) => not_found().into(),
        DomainErrorRoot::Domain(DomainError::NotAuthorized) => forbidden().into(),
        DomainErrorRoot::Domain(DomainError::InvalidInput(message)) => bad_request(message).into(),
        // Unrescued `RecordNotUnique` renders 500, like upstream;
        // callers that rescue it (join) match before this mapping.
        DomainErrorRoot::Domain(DomainError::Conflict) => Error::msg("unique constraint violated"),
        DomainErrorRoot::Application(_) | DomainErrorRoot::Infrastructure(_) => {
            Error::msg("dependency unavailable")
        }
    }
}

/// Drop the unused import warning when application errors gain structure.
#[allow(dead_code)]
fn _application_is_opaque(err: &ApplicationError) -> &'static str {
    match err {
        ApplicationError::DependencyUnavailable(which) => which,
        ApplicationError::Internal(what) => what,
        ApplicationError::Domain(_) => "domain",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use topcoat::router::error::{BadRequestError, ForbiddenError, NotFoundError};

    #[test]
    fn not_found_maps_to_404() {
        let err = http_error(DomainErrorRoot::Domain(DomainError::NotFound));
        assert!(err.is::<NotFoundError>());
    }

    #[test]
    fn not_authorized_maps_to_403() {
        let err = http_error(DomainErrorRoot::Domain(DomainError::NotAuthorized));
        assert!(err.is::<ForbiddenError>());
    }

    #[test]
    fn invalid_input_maps_to_400() {
        let err = http_error(DomainErrorRoot::Domain(DomainError::InvalidInput("blank")));
        assert!(err.is::<BadRequestError>());
    }

    #[test]
    fn infra_maps_to_opaque_500() {
        use topcamp_domain::error::InfrastructureError;
        let err = http_error(DomainErrorRoot::Infrastructure(InfrastructureError::new(
            "postgres",
        )));
        assert!(!err.is::<NotFoundError>());
        assert!(!err.is::<ForbiddenError>());
        assert!(!err.is::<BadRequestError>());
        assert_eq!(err.to_string(), "dependency unavailable");
    }
}
