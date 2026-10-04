//! Live login tests against PostgreSQL + the real router.
//!
//! Requires `DATABASE_URL` (default: the compose.yml scratch database).
//! Creates one user with a cost-4 bcrypt digest, exercises success +
//! failure paths, then removes the user and its sessions.

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::UserRepository;
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://topcamp:topcamp@localhost:5432/topcamp".to_string());
    PgPool::connect(&url).await.expect("connect DATABASE_URL")
}

fn app(pool: PgPool) -> Router {
    router(AppState::new(PgDb::new(pool), Cable::new()))
}

/// Self-minted double-submit pair (any well-formed equal pair verifies).
fn csrf_token() -> String {
    "cd".repeat(32)
}

fn form_post(email: &str, password: &str) -> http::Request<Body> {
    let token = csrf_token();
    let body = format!(
        "email_address={}&password={}&authenticity_token={}",
        email.replace('@', "%40"),
        password.replace('@', "%40"),
        token
    );
    http::Request::builder()
        .method("POST")
        .uri("/session")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("csrf_token={token}"))
        .body(Body::from(body))
        .expect("form request builds")
}

async fn body_text(response: topcoat::router::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body reads");
    String::from_utf8(bytes.to_vec()).expect("response is UTF-8")
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

/// Seed a user with a run-unique email, so a previous panicked run's
/// residue can never collide on the unique address.
async fn seed_user(pool: &PgPool, name: &str) -> (i64, String) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock runs forward")
        .as_nanos();
    let email = format!(
        "{}-{}-{nanos}@example.com",
        name.to_lowercase(),
        std::process::id()
    );
    let digest = bcrypt::hash("s3cret", 4).expect("cost-4 digest builds");
    let db = PgDb::new(pool.clone());
    let id = UserRepository::create(&db, name, &email, Some(&digest))
        .await
        .expect("seed user")
        .id;
    (id, email)
}

async fn cleanup(pool: &PgPool, uid: i64) {
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(uid)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(uid)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn login_success_sets_session_cookie() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "loggy-ok").await;
    let response = app(pool.clone()).handle(form_post(&email, "s3cret")).await;
    assert_eq!(response.status(), 303);
    assert_eq!(response.headers().get("location").unwrap(), "/");
    let jar = cookies(&response);
    assert!(!jar.is_empty(), "login sets a cookie");

    // The session authenticates: an authed search returns 200, not 401.
    let authed = http::Request::builder()
        .method("GET")
        .uri("/search?q=hello")
        .header("cookie", jar)
        .body(Body::empty())
        .expect("authed request builds");
    let response = app(pool.clone()).handle(authed).await;
    assert_eq!(response.status(), 200);

    // A session row was persisted for the user.
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE user_id = $1")
        .bind(uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    cleanup(&pool, uid).await;
}

#[tokio::test]
async fn login_wrong_password_renders_401() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "loggy-bad").await;
    let response = app(pool.clone()).handle(form_post(&email, "wrong")).await;
    assert_eq!(response.status(), 401);
    let html = body_text(response).await;
    assert!(
        html.contains("Too many requests or unauthorized."),
        "rejection flash: {html}"
    );
    assert!(html.contains("panel shake"), "shake panel: {html}");
    assert!(html.contains("<form"), "form re-rendered: {html}");
    cleanup(&pool, uid).await;
}

#[tokio::test]
async fn login_without_csrf_token_is_forbidden() {
    let pool = pool().await;
    let body = "email_address=nobody%40example.com&password=whatever&authenticity_token=deadbeef";
    let request = http::Request::builder()
        .method("POST")
        .uri("/session")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .expect("request builds");
    let response = app(pool).handle(request).await;
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn login_unknown_email_renders_401() {
    let pool = pool().await;
    let response = app(pool.clone())
        .handle(form_post("nobody@example.com", "whatever"))
        .await;
    assert_eq!(response.status(), 401);
    let html = body_text(response).await;
    assert!(
        html.contains("Too many requests or unauthorized."),
        "rejection flash: {html}"
    );
}

#[tokio::test]
async fn sign_in_page_matches_upstream_markup() {
    let pool = pool().await;
    // Versioned logo needs the singleton account row (fresh databases
    // have none until first run).
    sqlx::query(
        "INSERT INTO accounts (created_at, updated_at, join_code, name) \
         VALUES (now(), now(), 'live-join', 'Topcamp') ON CONFLICT DO NOTHING",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (uid, _email) = seed_user(&pool, "pagey").await;
    // First administrator renders the help contact + version badge.
    sqlx::query("UPDATE users SET role = 1 WHERE id = $1")
        .bind(uid)
        .execute(&pool)
        .await
        .unwrap();
    let request = http::Request::builder()
        .method("GET")
        .uri("/session/new")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 200);
    let html = body_text(response).await;
    for needle in [
        "class=\"panel\"",
        "account-logo",
        "account/logo?v=",
        "action=\"/session\"",
        "name=\"email_address\"",
        "type=\"password\"",
        "maxlength=\"72\"",
        "language-list-menu",
        // Debug builds swap the help-contact mailto for the demo
        // login (release keeps upstream's mailto; untestable
        // in-process since test builds are debug).
        "action=\"/session/demo\"",
        "version-badge",
        "action-cable-url",
        "csrf-token",
    ] {
        assert!(html.contains(needle), "missing {needle}: {html}");
    }
    assert!(!html.contains("panel shake"), "no shake on fresh page");
    assert!(!html.contains("mailto:"), "no mail client detour: {html}");
    cleanup(&pool, uid).await;
}

#[tokio::test]
async fn root_redirects_member_to_their_room() {
    use topcamp_db::repositories::RoomRepository;
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "landy").await;
    let db = PgDb::new(pool.clone());
    let room = RoomRepository::create(&db, uid, Some("general"), "Rooms::Open")
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memberships(created_at,updated_at,room_id,user_id) VALUES (now(),now(),$1,$2)",
    )
    .bind(room.id)
    .bind(uid)
    .execute(&pool)
    .await
    .unwrap();

    let login = app(pool.clone()).handle(form_post(&email, "s3cret")).await;
    assert_eq!(login.status(), 303);
    let jar = cookies(&login);
    let request = http::Request::builder()
        .method("GET")
        .uri("/")
        .header("cookie", jar)
        .body(Body::empty())
        .expect("root request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);
    assert_eq!(
        response.headers().get("location").unwrap(),
        &format!("/rooms/{}", room.id)
    );

    sqlx::query("DELETE FROM memberships WHERE room_id = $1")
        .bind(room.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(room.id)
        .execute(&pool)
        .await
        .unwrap();
    cleanup(&pool, uid).await;
}
