//! 404 page parity + flash-across-redirect against PostgreSQL +
//! the real router.
//!
//! Requires `DATABASE_URL` (default: the isolated
//! `topcamp_test_flash` database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_flash;"` then apply
//! `migrations/*.sql` in order. Tests run in parallel with unique
//! emails; nothing here empties shared tables.

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::UserRepository;
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_flash".to_string()
    });
    PgPool::connect(&url).await.expect("connect DATABASE_URL")
}

fn app(pool: PgPool) -> Router {
    router(AppState::new(PgDb::new(pool), Cable::new()))
}

fn csrf_token() -> String {
    "cd".repeat(32)
}

async fn body_text(response: topcoat::router::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body reads");
    String::from_utf8(bytes.to_vec()).expect("response is UTF-8")
}

fn header(response: &topcoat::router::response::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

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

fn unique_email(name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock runs forward")
        .as_nanos();
    format!(
        "{}-{}-{nanos}@example.com",
        name.to_lowercase().replace(' ', "-"),
        std::process::id()
    )
}

async fn seed_user(pool: &PgPool, name: &str, admin: bool) -> (i64, String) {
    let email = unique_email(name);
    let digest = bcrypt::hash("s3cret", 4).expect("cost-4 digest builds");
    let db = PgDb::new(pool.clone());
    let id = UserRepository::create(&db, name, &email, Some(&digest))
        .await
        .expect("seed user")
        .id;
    if admin {
        UserRepository::set_role(&db, id, 1)
            .await
            .expect("promote admin");
    }
    (id, email)
}

async fn seed_account(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO accounts (created_at, updated_at, join_code, name) \
         VALUES (now(), now(), 'flash-test-join', 'Topcamp') ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn login(pool: &PgPool, email: &str) -> String {
    let token = csrf_token();
    let body = format!(
        "email_address={}&password=s3cret&authenticity_token={}",
        email.replace('@', "%40"),
        token
    );
    let request = http::Request::builder()
        .method("POST")
        .uri("/session")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("csrf_token={token}"))
        .body(Body::from(body))
        .expect("login request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);
    cookies(&response)
}

async fn get_html(pool: &PgPool, uri: &str, jar: &str) -> (u16, String, String) {
    let request = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", jar)
        .header("accept", "text/html,application/xhtml+xml")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    let status = response.status().as_u16();
    let set_cookies = cookies(&response);
    (status, body_text(response).await, set_cookies)
}

async fn post_form(
    pool: &PgPool,
    method: &str,
    uri: &str,
    jar: &str,
    body: &str,
) -> topcoat::router::response::Response {
    let token = csrf_token();
    let request = http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .body(Body::from(format!("authenticity_token={token}&{body}")))
        .expect("request builds");
    app(pool.clone()).handle(request).await
}

// --- 404 page ----------------------------------------------------------

#[tokio::test]
async fn unmatched_paths_render_the_404_page() {
    let pool = pool().await;
    let request = http::Request::builder()
        .method("GET")
        .uri("/nope")
        .header("accept", "text/html,application/xhtml+xml")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status().as_u16(), 404);
    assert_eq!(
        header(&response, "content-type"),
        "text/html; charset=UTF-8"
    );
    let text = body_text(response).await;
    assert!(text.contains("We can’t find that (404)"), "title: {text}");
    assert!(text.contains("Go back"), "home link: {text}");
}

#[tokio::test]
async fn method_mismatch_is_a_404_not_a_405() {
    let pool = pool().await;
    let request = http::Request::builder()
        .method("PUT")
        .uri("/session/new")
        .header("accept", "text/html,application/xhtml+xml")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status().as_u16(), 404);
    assert!(
        body_text(response)
            .await
            .contains("We can’t find that (404)")
    );
}

#[tokio::test]
async fn handler_404s_render_the_page() {
    let pool = pool().await;
    let (_uid, email) = seed_user(&pool, "Page Patty", false).await;
    let jar = login(&pool, &email).await;
    let (status, text, _) = get_html(&pool, "/rooms/new", &jar).await;
    assert_eq!(status, 404);
    assert!(text.contains("We can’t find that (404)"), "page: {text}");
}

#[tokio::test]
async fn explicit_json_gets_a_json_404() {
    let pool = pool().await;
    let (_uid, email) = seed_user(&pool, "Json Jan", false).await;
    let jar = login(&pool, &email).await;
    let request = http::Request::builder()
        .method("GET")
        .uri("/rooms/new")
        .header("cookie", &jar)
        .header("accept", "application/json")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status().as_u16(), 404);
    assert_eq!(
        header(&response, "content-type"),
        "application/json; charset=UTF-8"
    );
    assert_eq!(
        body_text(response).await,
        r#"{"status":404,"error":"Not Found"}"#
    );
}

#[tokio::test]
async fn avatar_bad_tokens_stay_an_empty_404() {
    let pool = pool().await;
    let (_uid, email) = seed_user(&pool, "Avatar Al", false).await;
    let jar = login(&pool, &email).await;
    let (status, text, _) = get_html(&pool, "/users/nope/avatar", &jar).await;
    assert_eq!(status, 404);
    assert_eq!(text, "");
}

// --- flash -------------------------------------------------------------

/// The flash cookie off a redirect response (the `flash=` pair).
fn flash_cookie(response: &topcoat::router::response::Response) -> String {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|pair| pair.split(';').next())
        .find(|pair| pair.starts_with("flash="))
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn account_updates_flash_a_notice_once() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (_aid, aemail) = seed_user(&pool, "Flash Admin", true).await;
    let jar = login(&pool, &aemail).await;
    let response = post_form(&pool, "PATCH", "/account", &jar, "account%5Bname%5D=HQ").await;
    assert_eq!(response.status().as_u16(), 303);
    let flash = flash_cookie(&response);
    assert!(!flash.is_empty(), "flash cookie set");

    let (status, text, cleared) =
        get_html(&pool, "/account/edit", &format!("{jar}; {flash}")).await;
    assert_eq!(status, 200);
    assert!(text.contains("<div class=\"flash\">"), "flash renders");
    assert!(!text.contains("data-controller="), "{text}");
    assert!(text.contains(">✓</span>"), "notice text: {text}");
    assert!(!text.contains("--flash-background"), "not the alert style");
    assert!(cleared.contains("flash="), "cookie consumed: {cleared}");

    // Consumed: the next render shows nothing.
    let (status, text, _) = get_html(&pool, "/account/edit", &jar).await;
    assert_eq!(status, 200);
    assert!(!text.contains("element-removal"), "flash gone");
}

#[tokio::test]
async fn profile_updates_flash_notices() {
    let pool = pool().await;
    let (_uid, email) = seed_user(&pool, "Profile Pam", false).await;
    let jar = login(&pool, &email).await;

    let response = post_form(
        &pool,
        "POST",
        "/users/me/profile",
        &jar,
        "_method=patch&user%5Bbio%5D=hello",
    )
    .await;
    assert_eq!(response.status().as_u16(), 303);
    let flash = flash_cookie(&response);
    let (_, text, _) = get_html(&pool, "/users/me/profile", &format!("{jar}; {flash}")).await;
    assert!(text.contains(">✓</span>"), "plain notice: {text}");

    let response = post_form(
        &pool,
        "POST",
        "/users/me/profile",
        &jar,
        "_method=patch&user%5Bavatar%5D=x",
    )
    .await;
    assert_eq!(response.status().as_u16(), 303);
    let flash = flash_cookie(&response);
    let (_, text, _) = get_html(&pool, "/users/me/profile", &format!("{jar}; {flash}")).await;
    assert!(
        text.contains("It may take up to 30 minutes to change everywhere."),
        "avatar notice: {text}"
    );
}

#[tokio::test]
async fn missing_rooms_redirect_with_an_alert() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (_uid, email) = seed_user(&pool, "Roomless Rita", false).await;
    let jar = login(&pool, &email).await;

    let request = http::Request::builder()
        .method("GET")
        .uri("/rooms/999999999")
        .header("cookie", &jar)
        .header("accept", "text/html,application/xhtml+xml")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status().as_u16(), 303);
    assert_eq!(header(&response, "location"), "/");
    let flash = flash_cookie(&response);
    assert!(!flash.is_empty(), "alert cookie set");

    let (status, text, _) = get_html(&pool, "/", &format!("{jar}; {flash}")).await;
    assert_eq!(status, 200);
    assert!(
        text.contains("Room not found or inaccessible"),
        "alert text: {text}"
    );
    assert!(
        text.contains("--flash-background: var(--color-negative)"),
        "alert style"
    );
}

#[tokio::test]
async fn rejected_logins_render_the_alert() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (_uid, email) = seed_user(&pool, "Throttled Theo", false).await;
    // Eleven bad passwords trips the rate limit (10 per 3 minutes).
    // The limiter keys by client IP, so the requests carry one.
    let mut last = None;
    for _ in 0..11 {
        let token = csrf_token();
        let body = format!(
            "email_address={}&password=wrong&authenticity_token={}",
            email.replace('@', "%40"),
            token
        );
        let request = http::Request::builder()
            .method("POST")
            .uri("/session")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", format!("csrf_token={token}"))
            .extension(topcoat::router::RemoteAddr("127.0.0.1:9".parse().unwrap()))
            .body(Body::from(body))
            .expect("request builds");
        last = Some(app(pool.clone()).handle(request).await);
    }
    let response = last.expect("eleven attempts");
    assert_eq!(response.status().as_u16(), 429);
    let text = body_text(response).await;
    assert!(text.contains("<div class=\"flash\">"), "flash renders");
    assert!(!text.contains("data-controller="), "{text}");
    assert!(
        text.contains("Too many requests or unauthorized."),
        "alert text"
    );
    assert!(
        text.contains("--flash-background: var(--color-negative)"),
        "alert style"
    );
}
