//! Session transfer + QR + browser gate tests against PostgreSQL
//! and the real router.
//!
//! Requires `DATABASE_URL` (default: the isolated
//! `topcamp_test_transfer` database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_transfer;"` then apply
//! `migrations/*.sql` in order. Tests run in parallel with unique
//! emails; nothing here empties shared tables.

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::UserRepository;
use topcamp_web::router;
use topcamp_web::signed_id;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_transfer".to_string()
    });
    PgPool::connect(&url).await.expect("connect DATABASE_URL")
}

/// One state per test: the ephemeral stream key signs the transfer
/// tokens the test mints, so minting and verifying share it.
async fn state(pool: &PgPool) -> AppState {
    AppState::new(PgDb::new(pool.clone()), Cable::new())
}

fn app(state: &AppState) -> Router {
    router(state.clone())
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock runs forward")
        .as_millis() as u64
}

fn csrf_token() -> String {
    "cd".repeat(32)
}

async fn body_text(response: topcoat::router::response::Response) -> String {
    let bytes = topcoat::router::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body reads");
    String::from_utf8(bytes.to_vec()).expect("response is UTF-8")
}

fn redirect_location(response: &topcoat::router::response::Response) -> String {
    response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn header(response: &topcoat::router::response::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// Set-Cookie pairs (name=value only) for replay as a Cookie header.
fn cookies(response: &topcoat::router::response::Response) -> String {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|pair| pair.split(';').next())
        .collect::<Vec<_>>()
        .join("; ")
}

fn unique(tag: &str) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock runs forward")
        .as_nanos();
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("{tag}-{}-{nanos}-{seq}", std::process::id())
}

async fn seed_account(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO accounts (created_at, updated_at, join_code, name) \
         VALUES (now(), now(), 'JOIN-CODE-1234', 'Topcamp') ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_user(pool: &PgPool, tag: &str) -> (i64, String) {
    let email = format!("{}@example.com", unique(tag));
    let digest = bcrypt::hash("s3cret", 4).expect("cost-4 digest builds");
    let db = PgDb::new(pool.clone());
    let id = UserRepository::create(&db, tag, &email, Some(&digest))
        .await
        .expect("seed user")
        .id;
    (id, email)
}

async fn seed_admin(pool: &PgPool, tag: &str) -> (i64, String) {
    let (id, email) = seed_user(pool, tag).await;
    sqlx::query("UPDATE users SET role = 1 WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    (id, email)
}

async fn login(state: &AppState, email: &str) -> String {
    let token = csrf_token();
    let body = format!(
        "email_address={}&password={}&authenticity_token={}",
        email.replace('@', "%40"),
        "s3cret",
        token
    );
    let request = http::Request::builder()
        .method("POST")
        .uri("/session")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("csrf_token={token}"))
        .body(Body::from(body))
        .expect("login request builds");
    let response = app(state).handle(request).await;
    assert_eq!(response.status(), 303);
    cookies(&response)
}

fn get_request(
    uri: &str,
    jar: &str,
    user_agent: Option<&str>,
    host: Option<&str>,
) -> http::Request<Body> {
    let mut builder = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", jar)
        .header("accept", "text/html,application/xhtml+xml");
    if let Some(ua) = user_agent {
        builder = builder.header("user-agent", ua);
    }
    if let Some(host) = host {
        builder = builder.header("host", host);
    }
    builder.body(Body::empty()).expect("request builds")
}

fn form_request(
    method: &str,
    uri: &str,
    jar: &str,
    fields: &[(&str, &str)],
    with_token: bool,
) -> http::Request<Body> {
    let token = csrf_token();
    let mut pairs: Vec<(String, String)> = fields
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    if with_token {
        pairs.push(("authenticity_token".to_string(), token.clone()));
    }
    let body = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    // The jar may already carry a session cookie; the CSRF cookie
    // rides alongside it.
    let jar = if jar.is_empty() {
        format!("csrf_token={token}")
    } else {
        format!("{jar}; csrf_token={token}")
    };
    http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", jar)
        .body(Body::from(body))
        .expect("request builds")
}

fn mint(state: &AppState, user_id: i64) -> String {
    signed_id::transfer_id(state.stream_key.bytes(), user_id, now_millis())
}

fn urlsafe_encode64(input: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE.encode(input.as_bytes())
}

const OLD_CHROME: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/100.0.0.0 Safari/537.36";
const NEW_CHROME: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

#[tokio::test]
async fn show_renders_form_when_signed_out() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    // The show page never verifies the token (upstream renders
    // regardless), so any id renders.
    let response = app(&state)
        .handle(get_request("/session/transfers/some-token", "", None, None))
        .await;
    assert_eq!(response.status(), 200);
    let body = body_text(response).await;
    assert!(
        body.contains("<form action=\"/session/transfers/some-token\""),
        "{body}"
    );
    assert!(body.contains("name=\"_method\" value=\"put\""), "{body}");
    assert!(body.contains("name=\"authenticity_token\""), "{body}");
    // No-JS experiment: the auto-submit hook is gone, so the form
    // carries a plain submit button instead.
    assert!(body.contains("type=\"submit\""), "{body}");
    assert!(!body.contains("data-controller="), "{body}");
}

#[tokio::test]
async fn show_renders_when_signed_in() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    let (_id, email) = seed_user(&pool, "Transfer Show Signed In").await;
    let jar = login(&state, &email).await;
    let token = mint(&state, _id);
    let response = app(&state)
        .handle(get_request(
            &format!("/session/transfers/{token}"),
            &jar,
            None,
            None,
        ))
        .await;
    assert_eq!(response.status(), 200);
    let body = body_text(response).await;
    assert!(body.contains("type=\"submit\""), "{body}");
    assert!(!body.contains("data-controller="), "{body}");
}

#[tokio::test]
async fn update_valid_token_logs_in() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    let (id, _email) = seed_user(&pool, "Transfer Login").await;
    let token = mint(&state, id);
    let uri = format!("/session/transfers/{token}");
    let response = app(&state)
        .handle(form_request("PUT", &uri, "", &[], true))
        .await;
    assert_eq!(response.status(), 303);
    assert_eq!(redirect_location(&response), "/");
    let jar = cookies(&response);
    // The new session belongs to the transfer's user.
    let profile = app(&state)
        .handle(get_request("/users/me/profile", &jar, None, None))
        .await;
    assert_eq!(profile.status(), 200);
    let body = body_text(profile).await;
    assert!(body.contains("Transfer Login"), "{body}");
}

#[tokio::test]
async fn update_via_post_method_override() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    let (id, _) = seed_user(&pool, "Transfer Override").await;
    let token = mint(&state, id);
    let uri = format!("/session/transfers/{token}");
    for (method, fields) in [
        ("POST", vec![("_method", "put")]),
        ("POST", vec![("_method", "patch")]),
        ("PATCH", vec![]),
    ] {
        let response = app(&state)
            .handle(form_request(method, &uri, "", &fields, true))
            .await;
        assert_eq!((method, response.status().as_u16()), (method, 303));
    }
}

#[tokio::test]
async fn update_rejects_forgeries() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    let (id, _) = seed_user(&pool, "Transfer Forgery").await;
    let valid = mint(&state, id);
    let mut tampered = valid.clone();
    tampered.replace_range(10..11, "A");
    let expired = signed_id::transfer_id(
        state.stream_key.bytes(),
        id,
        now_millis() - 5 * 60 * 60 * 1000,
    );
    let foreign = signed_id::transfer_id(b"another-secret-key-base", id, now_millis());
    for token in [
        &valid[..valid.len() - 4],
        "junk",
        &tampered,
        &expired,
        &foreign,
    ] {
        let uri = format!("/session/transfers/{token}");
        let response = app(&state)
            .handle(form_request("PUT", &uri, "", &[], true))
            .await;
        assert_eq!(response.status(), 400, "token {token}");
        let body = body_text(response).await;
        assert!(body.is_empty(), "token {token}: {body:?}");
    }
}

#[tokio::test]
async fn update_rejects_inactive_user() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    let (id, _) = seed_user(&pool, "Transfer Inactive").await;
    let token = mint(&state, id);
    sqlx::query("UPDATE users SET status = 1 WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let uri = format!("/session/transfers/{token}");
    let response = app(&state)
        .handle(form_request("PUT", &uri, "", &[], true))
        .await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn update_rejects_bad_csrf() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    let (id, _) = seed_user(&pool, "Transfer CSRF").await;
    let token = mint(&state, id);
    let uri = format!("/session/transfers/{token}");
    // Missing token field.
    let response = app(&state)
        .handle(form_request("PUT", &uri, "", &[], false))
        .await;
    assert_eq!(response.status(), 403);
    // Mismatched token.
    let bad = http::Request::builder()
        .method("PUT")
        .uri(&uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("csrf_token={}", csrf_token()))
        .body(Body::from(format!(
            "authenticity_token={}",
            "ab".repeat(32)
        )))
        .expect("request builds");
    let response = app(&state).handle(bad).await;
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn modify_without_override_404s() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    let (id, _) = seed_user(&pool, "Transfer No Override").await;
    let token = mint(&state, id);
    let uri = format!("/session/transfers/{token}");
    for fields in [
        vec![],
        vec![("_method", "delete")],
        vec![("_method", "get")],
    ] {
        let response = app(&state)
            .handle(form_request("POST", &uri, "", &fields, true))
            .await;
        assert_eq!(response.status(), 404);
    }
}

#[tokio::test]
async fn qr_renders_svg() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    let id = urlsafe_encode64("http://topcamp.test/session/transfers/abc");
    let response = app(&state)
        .handle(get_request(&format!("/qr_code/{id}"), "", None, None))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        header(&response, "content-type"),
        "image/svg+xml; charset=utf-8"
    );
    assert_eq!(
        header(&response, "cache-control"),
        "max-age=31556952, public"
    );
    let body = body_text(response).await;
    assert!(
        body.starts_with("<?xml version=\"1.0\" standalone=\"yes\"?>"),
        "{body:.120}"
    );
    assert!(body.contains("<svg "), "{body:.120}");
    assert!(body.ends_with("</svg>"));
    assert!(body.contains("fill=\"black\""), "{body:.200}");
}

#[tokio::test]
async fn qr_accepts_format_suffix() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    let id = urlsafe_encode64("http://topcamp.test/session/transfers/abc");
    let plain = app(&state)
        .handle(get_request(&format!("/qr_code/{id}"), "", None, None))
        .await;
    let suffixed = app(&state)
        .handle(get_request(&format!("/qr_code/{id}.png"), "", None, None))
        .await;
    assert_eq!(suffixed.status(), 200);
    assert_eq!(body_text(plain).await, body_text(suffixed).await);
}

#[tokio::test]
async fn qr_rejects_malformed_and_too_long() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    for id in ["a", "a*bc", "!!!"] {
        let response = app(&state)
            .handle(get_request(&format!("/qr_code/{id}"), "", None, None))
            .await;
        assert_eq!(response.status(), 500, "id {id}");
    }
    // 3000 bytes exceed version 40 at level H.
    let long = urlsafe_encode64(&"a".repeat(3000));
    let response = app(&state)
        .handle(get_request(&format!("/qr_code/{long}"), "", None, None))
        .await;
    assert_eq!(response.status(), 422);
}

/// The `session_transfer_url` input's value on a profile page.
fn transfer_input(body: &str) -> Option<String> {
    let marker = "id=\"session_transfer_url\"";
    let at = body.find(marker)?;
    let window = &body[..at];
    let start = window.rfind("value=\"")? + "value=\"".len();
    let end = window[start..].find('"')? + start;
    Some(window[start..end].to_string())
}

fn transfer_token(url: &str) -> &str {
    url.split("/session/transfers/")
        .nth(1)
        .expect("transfer URL carries a token")
}

#[tokio::test]
async fn profile_shows_fresh_transfer_link() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    let (_id, email) = seed_user(&pool, "Transfer Profile").await;
    let jar = login(&state, &email).await;
    let first = app(&state)
        .handle(get_request(
            "/users/me/profile",
            &jar,
            None,
            Some("topcamp.test"),
        ))
        .await;
    assert_eq!(first.status(), 200);
    let body = body_text(first).await;
    let url = transfer_input(&body).expect("transfer input present");
    assert!(
        url.starts_with("http://topcamp.test/session/transfers/"),
        "{url}"
    );
    assert!(
        body.contains("Use this link to login automatically on another device"),
        "{body}"
    );
    // No-JS experiment: the QR anchor is a plain link and the
    // Stimulus copy/lightbox hooks are gone.
    assert!(body.contains("href=\"/qr_code/"), "{body}");
    assert!(!body.contains("data-lightbox-"), "{body}");
    assert!(!body.contains("data-controller="), "{body}");
    // Each render mints its own token.
    let second = app(&state)
        .handle(get_request("/users/me/profile", &jar, None, None))
        .await;
    let other = transfer_input(&body_text(second).await).expect("transfer input present");
    assert!(other.starts_with("/session/transfers/"), "{other}");
    assert_ne!(transfer_token(&url), transfer_token(&other));
}

#[tokio::test]
async fn users_show_gates_transfer_by_role() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    let (admin_id, admin_email) = seed_admin(&pool, "Transfer Admin").await;
    let (user_id, user_email) = seed_user(&pool, "Transfer Member").await;
    let admin_jar = login(&state, &admin_email).await;
    let user_jar = login(&state, &user_email).await;

    // Administrators get the recovery label on someone else's page.
    let response = app(&state)
        .handle(get_request(
            &format!("/users/{user_id}"),
            &admin_jar,
            None,
            None,
        ))
        .await;
    assert_eq!(response.status(), 200);
    let body = body_text(response).await;
    assert!(
        body.contains("Share to get them back into their account"),
        "{body}"
    );
    // ... and the screen-reader label on their own.
    let response = app(&state)
        .handle(get_request(
            &format!("/users/{admin_id}"),
            &admin_jar,
            None,
            None,
        ))
        .await;
    let body = body_text(response).await;
    assert!(
        body.contains("Use this link to login automatically on another device"),
        "{body}"
    );
    // Non-administrators see no transfer link anywhere.
    let response = app(&state)
        .handle(get_request(
            &format!("/users/{admin_id}"),
            &user_jar,
            None,
            None,
        ))
        .await;
    let body = body_text(response).await;
    assert!(!body.contains("session_transfer_url"), "{body}");

    // Deactivated users lose the fieldset even for administrators.
    sqlx::query("UPDATE users SET status = 1 WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let response = app(&state)
        .handle(get_request(
            &format!("/users/{user_id}"),
            &admin_jar,
            None,
            None,
        ))
        .await;
    let body = body_text(response).await;
    assert!(!body.contains("session_transfer_url"), "{body}");
}

#[tokio::test]
async fn transfer_link_logs_in_on_another_device() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    let (_id, email) = seed_user(&pool, "Transfer Device").await;
    let jar = login(&state, &email).await;
    // Device one: copy the link off the profile page.
    let profile = app(&state)
        .handle(get_request("/users/me/profile", &jar, None, None))
        .await;
    let url = transfer_input(&body_text(profile).await).expect("transfer input");
    // Device two (no session): open the link, submit the form.
    let token = transfer_token(&url).to_string();
    let show = app(&state)
        .handle(get_request(
            &format!("/session/transfers/{token}"),
            "",
            None,
            None,
        ))
        .await;
    assert_eq!(show.status(), 200);
    let submit = app(&state)
        .handle(form_request(
            "POST",
            &format!("/session/transfers/{token}"),
            "",
            &[("_method", "put")],
            true,
        ))
        .await;
    assert_eq!(submit.status(), 303);
    let jar = cookies(&submit);
    let profile = app(&state)
        .handle(get_request("/users/me/profile", &jar, None, None))
        .await;
    let body = body_text(profile).await;
    assert!(body.contains("Transfer Device"), "{body}");
}

#[tokio::test]
async fn gate_blocks_old_browsers() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    for (method, uri) in [
        ("GET", "/session/new"),
        ("GET", "/session/transfers/abc"),
        ("GET", "/qr_code/abc"),
        ("POST", "/session"),
    ] {
        let request = if method == "GET" {
            get_request(uri, "", Some(OLD_CHROME), None)
        } else {
            let mut request = form_request("POST", uri, "", &[], true);
            request
                .headers_mut()
                .insert("user-agent", OLD_CHROME.parse().unwrap());
            request
        };
        let response = app(&state).handle(request).await;
        assert_eq!(
            (method, uri, response.status().as_u16()),
            (method, uri, 200)
        );
        let body = body_text(response).await;
        assert!(
            body.contains("Upgrade to a supported web browser"),
            "{method} {uri}: {body:.300}"
        );
        assert!(body.contains("browser-list"), "{method} {uri}");
    }
    // The page names the minimum versions.
    let response = app(&state)
        .handle(get_request("/session/new", "", Some(OLD_CHROME), None))
        .await;
    let body = body_text(response).await;
    for (browser, version) in [
        ("Safari", "17.2+"),
        ("Chrome", "120+"),
        ("Firefox", "121+"),
        ("Opera", "104+"),
    ] {
        assert!(body.contains(browser), "{body}");
        assert!(body.contains(version), "{body}");
    }
    assert!(body.contains("Translate"), "{body}");
}

#[tokio::test]
async fn gate_passes_modern_and_missing_agents() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    for ua in [None, Some(NEW_CHROME), Some("curl/8.4.0")] {
        let response = app(&state)
            .handle(get_request("/session/new", "", ua, None))
            .await;
        assert_eq!(response.status(), 200);
        let body = body_text(response).await;
        assert!(
            !body.contains("Upgrade to a supported web browser"),
            "ua {ua:?}"
        );
    }
}

#[tokio::test]
async fn transfer_link_is_selectable_without_copy_button() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    let (_id, email) = seed_user(&pool, "Transfer Nojs Copy").await;
    let jar = login(&state, &email).await;
    let response = app(&state)
        .handle(get_request("/users/me/profile", &jar, None, None))
        .await;
    assert_eq!(response.status(), 200);
    let body = body_text(response).await;
    // No-JS: the dead copy button is gone; the link stays as a
    // readonly selectable input next to the QR link.
    assert!(!body.contains("Copy auto-login link"), "{body}");
    assert!(body.contains("id=\"session_transfer_url\""), "{body}");
    assert!(body.contains("readonly"), "{body}");
    assert!(body.contains("href=\"/qr_code/"), "{body}");
}

#[tokio::test]
async fn gate_skips_infrastructure() {
    let pool = pool().await;
    seed_account(&pool).await;
    let state = state(&pool).await;
    let response = app(&state)
        .handle(get_request("/up", "", Some(OLD_CHROME), None))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(body_text(response).await, "ok");
}
