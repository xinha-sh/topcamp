//! Session-backed current user + login/logout (§8–§9).
//!
//! Token issuance and cookies are owned by Topcoat (`topcoat-session`); this
//! module persists/looks-up the SHA-256 token hash via [`SessionRepository`].
//! Passwords verify with bcrypt against `users.password_digest`. Unknown
//! emails, missing digests, and bad passwords all re-render the sign-in form
//! with 401 — never a distinguishing error.

use std::sync::OnceLock;

use serde::Deserialize;
use topcamp_db::PgDb;
use topcamp_db::pg::session_key;
use topcamp_db::repositories::{
    CredentialsRow, PushSubscriptionRepository, SessionRepository, UserRepository, UserRow,
};
use topcamp_domain::auth::UserStatus;
use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        content::Form,
        error::see_other,
        request::{client_ip, headers},
        response::{IntoResponse, Response},
        route,
    },
    session,
};

use crate::state::{AppState, http_error};

/// Rejection copy shared by bad credentials (401) and rate limiting
/// (429): deliberately non-distinguishing, matching upstream.
pub const REJECTION: &str = "Too many requests or unauthorized.";

/// Resolve the request's user: token hash → live session → active user.
/// Returns `None` without a session, with an expired/unknown session, or
/// when the user is not active. Inactive users fail closed.
/// Sessions resume at most hourly
/// ([`session_resume_due`](topcamp_domain::auth::session_resume_due)): the
/// row's activity/expiry refresh and the cookie re-sign happen together.
pub async fn current_user(cx: &Cx) -> Result<Option<UserRow>> {
    let Some(hash) = session::token_hash(cx).await? else {
        return Ok(None);
    };
    let db = &app_context::<AppState>(cx).db;
    let Some(record) = db
        .find_by_token(&session_key(&hash))
        .await
        .map_err(http_error)?
    else {
        return Ok(None);
    };
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if topcamp_domain::auth::session_resume_due(record.last_active_at_unix, now_unix)
        && let Some(refreshed) = session::refresh(cx).await?
    {
        let user_agent = headers(cx).get("user-agent").and_then(|v| v.to_str().ok());
        let ip = client_ip(cx).map(|addr| addr.to_string());
        let expires_in = refreshed
            .expires_at
            .duration_since(std::time::SystemTime::now())
            .unwrap_or_default();
        SessionRepository::resume(db, record.id, user_agent, ip.as_deref(), expires_in)
            .await
            .map_err(http_error)?;
    }
    let user = UserRepository::find_by_id(db, record.user_id)
        .await
        .map_err(http_error)?;
    Ok(user.filter(|u| u.status == UserStatus::Active.value()))
}

/// [`current_user`] plus `deny_bots`: session holders pass; without
/// a session, a valid `?bot_key=` is 403 (bots only speak the bot
/// API); anything else is anonymous. Interactive routes use this.
pub async fn current_user_or_deny_bot(cx: &Cx) -> Result<Option<UserRow>> {
    if let Some(user) = current_user(cx).await? {
        return Ok(Some(user));
    }
    crate::bots::deny_bots(cx).await?;
    Ok(None)
}

/// Sign-in/out form body (`application/x-www-form-urlencoded`).
/// Credentials are optional so a missing field rejects exactly like a
/// wrong one (never a distinguishing error). `_method=delete` turns the
/// POST into a sign-out: HTML forms cannot send DELETE and Topcoat has
/// no method-override layer, so the override dispatches here.
#[derive(Debug, Deserialize)]
struct SessionForm {
    email_address: Option<String>,
    password: Option<String>,
    authenticity_token: String,
    #[serde(rename = "_method", default)]
    method_override: Option<String>,
    #[serde(default)]
    push_subscription_endpoint: Option<String>,
}

/// Dummy digest so unknown emails cost a bcrypt verification too (no
/// timing oracle for account enumeration). Computed once; never matches.
fn dummy_digest() -> &'static str {
    static DIGEST: OnceLock<String> = OnceLock::new();
    DIGEST.get_or_init(|| {
        bcrypt::hash("topcamp-login-dummy", bcrypt::DEFAULT_COST)
            .expect("bcrypt hashes the dummy password")
    })
}

/// Verify `password` against `digest`, failing closed on malformed hashes.
fn password_matches(password: &str, digest: &str) -> bool {
    bcrypt::verify(password, digest).unwrap_or(false)
}

/// `rate_limit to: 10, within: 3.minutes, only: :create`, keyed by
/// client IP in a fixed window. Unknowable IPs (in-process requests in
/// tests) skip limiting rather than sharing one global bucket.
fn rate_limited(ip: Option<String>) -> bool {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static LIMITS: OnceLock<Mutex<HashMap<String, (u64, std::time::Instant)>>> = OnceLock::new();
    let Some(ip) = ip else { return false };
    let key = format!("rate-limit:sessions:{ip}");
    let now = std::time::Instant::now();
    let window = std::time::Duration::from_secs(3 * 60);
    let mut limits = LIMITS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    limits.retain(|_, (_, expires_at)| *expires_at > now);
    let entry = limits.entry(key).or_insert((0, now + window));
    entry.0 += 1;
    entry.0 > 10
}

#[route([POST, DELETE] "/session")]
pub async fn sessions(cx: &Cx, Form(input): Form<SessionForm>) -> Result<Response> {
    if !crate::csrf::verify(cx, &input.authenticity_token) {
        return Err(topcoat::router::error::forbidden().into());
    }
    if topcoat::router::request::method(cx) == http::Method::DELETE
        || input.method_override.as_deref() == Some("delete")
    {
        return destroy(cx, input.push_subscription_endpoint.as_deref()).await;
    }
    create(cx, input).await
}

/// Demo sign-in form (`POST /session/demo`).
#[derive(Debug, Deserialize)]
struct DemoForm {
    authenticity_token: String,
}

/// One-click demo login (debug builds only): signs in as the first
/// administrator — the same account the sign-in help contact names.
/// Release builds answer 404, as if the route didn't exist:
/// passwordless admin login must never ship.
#[route(POST "/session/demo")]
pub async fn demo_create(cx: &Cx, Form(input): Form<DemoForm>) -> Result<Response> {
    if !cfg!(debug_assertions) {
        return crate::not_found::response(cx);
    }
    if !crate::csrf::verify(cx, &input.authenticity_token) {
        return Err(topcoat::router::error::forbidden().into());
    }
    let db = &app_context::<AppState>(cx).db;
    let Some(admin) = UserRepository::first_administrator(db)
        .await
        .map_err(http_error)?
    else {
        return see_other("/session/new").into_response(cx);
    };
    start_session(cx, db, admin.id).await?;
    see_other("/").into_response(cx)
}

async fn create(cx: &Cx, input: SessionForm) -> Result<Response> {
    if rate_limited(client_ip(cx).map(|addr| addr.to_string())) {
        return crate::pages::login_failed(
            cx,
            input.email_address,
            http::StatusCode::TOO_MANY_REQUESTS,
        )
        .await;
    }
    let db = &app_context::<AppState>(cx).db;
    let credentials: Option<CredentialsRow> = match input.email_address.clone() {
        Some(email) => db
            .find_active_credentials_by_email(&email)
            .await
            .map_err(http_error)?,
        None => None,
    };
    let (user_id, digest) = match credentials {
        Some(row) => (Some(row.user_id), row.password_digest),
        None => (None, None),
    };
    let digest = digest.unwrap_or_else(|| dummy_digest().to_string());
    let authenticated = match (user_id, input.password) {
        (Some(_), Some(password)) => password_matches(&password, &digest),
        _ => {
            // Still pay for one verification so missing fields cost what
            // a wrong password costs (no timing oracle).
            password_matches("topcamp-login-dummy-miss", &digest);
            false
        }
    };
    if !authenticated {
        return crate::pages::login_failed(cx, input.email_address, http::StatusCode::UNAUTHORIZED)
            .await;
    }
    let user_id = user_id.unwrap_or_default();
    start_session(cx, db, user_id).await?;
    see_other("/").into_response(cx)
}

/// Start a Topcoat session and persist its row (`start_new_session_for`).
/// Shared by password login and first-run setup.
pub(crate) async fn start_session(cx: &Cx, db: &PgDb, user_id: i64) -> Result<()> {
    let started = session::start(cx).await?;
    let user_agent = headers(cx).get("user-agent").and_then(|v| v.to_str().ok());
    let ip = client_ip(cx).map(|addr| addr.to_string());
    let expires_in = started
        .expires_at
        .duration_since(std::time::SystemTime::now())
        .unwrap_or_default();
    SessionRepository::start(
        db,
        user_id,
        user_agent,
        ip.as_deref(),
        expires_in,
        &session_key(&started.token_hash),
    )
    .await
    .map_err(http_error)?;
    Ok(())
}

async fn destroy(cx: &Cx, push_endpoint: Option<&str>) -> Result<Response> {
    // `remove_push_subscription` runs before the session dies, while
    // the user still resolves. A failed lookup never blocks sign-out.
    if let Some(endpoint) = push_endpoint
        && let Ok(Some(user)) = current_user_or_deny_bot(cx).await
    {
        let db = &app_context::<AppState>(cx).db;
        PushSubscriptionRepository::destroy_push_subscriptions_by_endpoint(db, user.id, endpoint)
            .await
            .map_err(http_error)?;
    }
    if let Some(hash) = session::stop(cx).await? {
        let app = app_context::<AppState>(cx);
        if let Some(record) = app
            .db
            .find_by_token(&session_key(&hash))
            .await
            .map_err(http_error)?
        {
            SessionRepository::destroy(&app.db, record.id)
                .await
                .map_err(http_error)?;
            // Synchronous revocation (§26): the client's sockets drop with
            // `reconnect: true` and replay subscriptions; channels turn
            // away rooms the signed-out user can no longer see.
            crate::cable::disconnect_user(app, record.user_id, true);
        }
    }
    see_other("/").into_response(cx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Low-cost digest: fast to build, exercises the real verify path.
    fn test_digest(password: &str) -> String {
        bcrypt::hash(password, 4).expect("cost-4 hash builds")
    }

    #[test]
    fn correct_password_verifies() {
        assert!(password_matches("s3cret", &test_digest("s3cret")));
    }

    #[test]
    fn wrong_password_rejected() {
        assert!(!password_matches("wrong", &test_digest("s3cret")));
    }

    #[test]
    fn malformed_digest_fails_closed() {
        assert!(!password_matches("s3cret", "not-a-bcrypt-hash"));
        assert!(!password_matches("s3cret", ""));
    }

    #[test]
    fn dummy_digest_rejects_attacker_passwords() {
        for guess in ["password", "s3cret", "", "topcamp"] {
            assert!(!password_matches(guess, dummy_digest()), "guess {guess:?}");
        }
        // Computed once: the timing decoy is stable across calls.
        assert_eq!(dummy_digest(), dummy_digest());
    }

    #[test]
    fn rate_limit_allows_ten_then_trips() {
        let ip = format!("198.51.100.{}", std::process::id() % 250 + 1);
        for _ in 0..10 {
            assert!(!rate_limited(Some(ip.clone())));
        }
        assert!(rate_limited(Some(ip.clone())));
        // Other IPs and unknowable IPs are unaffected.
        assert!(!rate_limited(Some("203.0.113.9".to_string())));
        assert!(!rate_limited(None));
    }
}
