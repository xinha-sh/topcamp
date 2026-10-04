//! One-click demo login on the sign-in screen (debug builds): the
//! help-contact mailto is replaced by a `POST /session/demo` button
//! that signs in as the first administrator. Release builds keep
//! upstream's mailto.
//!
//! Requires `DATABASE_URL` (default: the isolated
//! `topcamp_test_demo` database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_demo;"` then apply
//! `migrations/*.sql` in order. Tests run in parallel with unique
//! emails; nothing here empties shared tables, so no test asserts
//! WHICH administrator the demo session belongs to.

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::UserRepository;
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_demo".to_string()
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

async fn seed_admin(pool: &PgPool, name: &str) -> (i64, String) {
    let email = unique_email(name);
    let digest = bcrypt::hash("s3cret", 4).expect("cost-4 digest builds");
    let db = PgDb::new(pool.clone());
    let id = UserRepository::create(&db, name, &email, Some(&digest))
        .await
        .expect("seed admin")
        .id;
    UserRepository::set_role(&db, id, 1)
        .await
        .expect("promote admin");
    (id, email)
}

async fn seed_account(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO accounts (created_at, updated_at, join_code, name) \
         VALUES (now(), now(), 'demo-test-join', 'Topcamp') ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn sign_in_screen_offers_demo_login_instead_of_mailto() {
    let pool = pool().await;
    seed_account(&pool).await;
    seed_admin(&pool, "Demo Dana").await;

    let request = http::Request::builder()
        .method("GET")
        .uri("/session/new")
        .header("accept", "text/html")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status().as_u16(), 200);
    let text = body_text(response).await;
    assert!(
        text.contains("action=\"/session/demo\""),
        "demo form: {text}"
    );
    assert!(!text.contains("mailto:"), "no mail client detour: {text}");
}

#[tokio::test]
async fn demo_button_signs_in() {
    let pool = pool().await;
    seed_account(&pool).await;
    seed_admin(&pool, "Demo Dan").await;

    let token = csrf_token();
    let request = http::Request::builder()
        .method("POST")
        .uri("/session/demo")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("csrf_token={token}"))
        .body(Body::from(format!("authenticity_token={token}")))
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status().as_u16(), 303);
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "/"
    );

    // The session the demo login minted authenticates.
    let jar = cookies(&response);
    assert!(!jar.is_empty(), "demo login sets a session cookie");
    let request = http::Request::builder()
        .method("GET")
        .uri("/")
        .header("cookie", jar)
        .header("accept", "text/html")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status().as_u16(), 200);
}

#[tokio::test]
async fn demo_login_rejects_missing_csrf() {
    let pool = pool().await;
    seed_account(&pool).await;
    seed_admin(&pool, "Demo Dot").await;

    let request = http::Request::builder()
        .method("POST")
        .uri("/session/demo")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("csrf_token={}", csrf_token()))
        .body(Body::from("authenticity_token=wrong"))
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status().as_u16(), 403);
}
