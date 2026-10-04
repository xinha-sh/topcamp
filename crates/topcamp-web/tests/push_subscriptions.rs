//! Push subscriptions: create/touch/422s, destroy, test delivery,
//! logout removal, the dev-mode index, and the bell dialog — against
//! PostgreSQL + the real router.
//!
//! Requires `DATABASE_URL` (default: the isolated
//! `topcamp_test_push` database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_push;"` then apply
//! `migrations/*.sql` in order. DNS resolution is stubbed with
//! `TOPCAMP_TEST_PUSH_DNS` (like upstream's
//! `stub_web_push_dns_resolution`); VAPID stays unset so test
//! delivery is skipped. Tests run in parallel with unique emails;
//! nothing here empties shared tables.

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::{PushSubscriptionRepository, RoomRepository, UserRepository};
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

/// One static DNS mapping for the whole binary (parallel-safe):
/// `fcm` resolves public, `apple` resolves link-local.
fn stub_dns() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var(
            "TOPCAMP_TEST_PUSH_DNS",
            "fcm.googleapis.com=8.8.8.8,web.push.apple.com=169.254.169.254",
        );
    });
}

async fn pool() -> PgPool {
    stub_dns();
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_push".to_string()
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

async fn seed_user(pool: &PgPool, name: &str) -> (i64, String) {
    let email = unique_email(name);
    let digest = bcrypt::hash("s3cret", 4).expect("cost-4 digest builds");
    let db = PgDb::new(pool.clone());
    let id = UserRepository::create(&db, name, &email, Some(&digest))
        .await
        .expect("seed user")
        .id;
    (id, email)
}

async fn seed_account(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO accounts (created_at, updated_at, join_code, name) \
         VALUES (now(), now(), 'push-test-join', 'Topcamp') ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_room(pool: &PgPool, uid: i64, name: &str) -> i64 {
    let db = PgDb::new(pool.clone());
    let room = RoomRepository::create(&db, uid, Some(name), "Rooms::Open")
        .await
        .expect("seed room");
    sqlx::query(
        "INSERT INTO memberships(created_at,updated_at,room_id,user_id) VALUES (now(),now(),$1,$2)",
    )
    .bind(room.id)
    .bind(uid)
    .execute(pool)
    .await
    .unwrap();
    room.id
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

async fn get(pool: &PgPool, uri: &str, jar: &str, user_agent: Option<&str>) -> (u16, String) {
    let mut builder = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", jar)
        .header("accept", "text/html,application/xhtml+xml");
    if let Some(ua) = user_agent {
        builder = builder.header("user-agent", ua);
    }
    let request = builder.body(Body::empty()).expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    let status = response.status().as_u16();
    (status, body_text(response).await)
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

async fn post_json(
    pool: &PgPool,
    uri: &str,
    jar: &str,
    body: &serde_json::Value,
) -> topcoat::router::response::Response {
    let token = csrf_token();
    let request = http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .header("x-csrf-token", token)
        .body(Body::from(body.to_string()))
        .expect("request builds");
    app(pool.clone()).handle(request).await
}

const CHROME_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

fn push_params(endpoint: &str, p256dh_key: &str, auth_key: &str) -> String {
    serde_urlencoded::to_string([
        ("push_subscription[endpoint]", endpoint),
        ("push_subscription[p256dh_key]", p256dh_key),
        ("push_subscription[auth_key]", auth_key),
    ])
    .expect("params encode")
}

async fn subscription_count(pool: &PgPool, uid: i64) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM push_subscriptions WHERE user_id = $1")
        .bind(uid)
        .fetch_one(pool)
        .await
        .expect("count reads")
}

async fn updated_epoch(pool: &PgPool, uid: i64) -> f64 {
    sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM updated_at)::float8 FROM push_subscriptions WHERE user_id = $1",
    )
    .bind(uid)
    .fetch_one(pool)
    .await
    .expect("timestamp reads")
}

// --- create -----------------------------------------------------------

#[tokio::test]
async fn create_new_push_subscription() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Push Pam").await;
    let jar = login(&pool, &email).await;
    let endpoint = format!(
        "https://fcm.googleapis.com/fcm/send/pam-{}",
        unique_email("x")
    );
    let response = post_form(
        &pool,
        "POST",
        "/users/me/push_subscriptions",
        &jar,
        &push_params(&endpoint, "123", "456"),
    )
    .await;
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(body_text(response).await, "");

    let db = PgDb::new(pool.clone());
    let found = PushSubscriptionRepository::find_push_subscription_by_params(
        &db,
        uid,
        &endpoint,
        Some("123"),
        Some("456"),
    )
    .await
    .expect("lookup works")
    .expect("row persisted");
    assert_eq!(found.endpoint.as_deref(), Some(endpoint.as_str()));
    assert_eq!(found.p256dh_key.as_deref(), Some("123"));
    assert_eq!(found.auth_key.as_deref(), Some("456"));
}

#[tokio::test]
async fn create_accepts_the_bells_json() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Json Josie").await;
    let jar = login(&pool, &email).await;
    let endpoint = format!(
        "https://fcm.googleapis.com/fcm/send/josie-{}",
        unique_email("x")
    );
    let response = post_json(
        &pool,
        "/users/me/push_subscriptions",
        &jar,
        &serde_json::json!({
            "push_subscription": {
                "endpoint": endpoint,
                "p256dh_key": "aaa",
                "auth_key": "bbb",
            }
        }),
    )
    .await;
    assert_eq!(response.status().as_u16(), 200);

    let db = PgDb::new(pool.clone());
    let rows = PushSubscriptionRepository::push_subscriptions_for_user(&db, uid)
        .await
        .expect("list works");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].endpoint.as_deref(), Some(endpoint.as_str()));
}

#[tokio::test]
async fn touch_existing_subscription() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Touch Tina").await;
    let jar = login(&pool, &email).await;
    let endpoint = format!(
        "https://fcm.googleapis.com/fcm/send/tina-{}",
        unique_email("x")
    );
    let params = push_params(&endpoint, "1", "2");
    let response = post_form(&pool, "POST", "/users/me/push_subscriptions", &jar, &params).await;
    assert_eq!(response.status().as_u16(), 200);

    let before = updated_epoch(&pool, uid).await;
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    let response = post_form(&pool, "POST", "/users/me/push_subscriptions", &jar, &params).await;
    assert_eq!(response.status().as_u16(), 200);

    let after = updated_epoch(&pool, uid).await;
    assert!(after > before, "touch bumps updated_at");
    assert_eq!(subscription_count(&pool, uid).await, 1);
}

#[tokio::test]
async fn rejects_subscription_with_non_permitted_endpoint() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Evil Ed").await;
    let jar = login(&pool, &email).await;
    let response = post_form(
        &pool,
        "POST",
        "/users/me/push_subscriptions",
        &jar,
        &push_params("https://attacker.example.com/steal", "123", "456"),
    )
    .await;
    assert_eq!(response.status().as_u16(), 422);
    assert_eq!(subscription_count(&pool, uid).await, 0);
}

#[tokio::test]
async fn rejects_subscription_with_endpoint_resolving_to_a_private_ip() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Private Pete").await;
    let jar = login(&pool, &email).await;
    let endpoint = format!("https://web.push.apple.com/pete-{}", unique_email("x"));
    let response = post_form(
        &pool,
        "POST",
        "/users/me/push_subscriptions",
        &jar,
        &push_params(&endpoint, "123", "456"),
    )
    .await;
    assert_eq!(response.status().as_u16(), 422);
    assert_eq!(subscription_count(&pool, uid).await, 0);
}

#[tokio::test]
async fn re_registering_a_legacy_invalid_subscription_is_rejected() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Legacy Larry").await;
    let jar = login(&pool, &email).await;
    // A row that predates endpoint validation (planted without any).
    let db = PgDb::new(pool.clone());
    PushSubscriptionRepository::create_push_subscription(
        &db,
        uid,
        "https://attacker.example.com/steal",
        Some("123"),
        Some("456"),
        None,
    )
    .await
    .expect("legacy row plants");

    let response = post_form(
        &pool,
        "POST",
        "/users/me/push_subscriptions",
        &jar,
        &push_params("https://attacker.example.com/steal", "123", "456"),
    )
    .await;
    assert_eq!(response.status().as_u16(), 422);
    assert_eq!(subscription_count(&pool, uid).await, 1);
}

// --- destroy ----------------------------------------------------------

#[tokio::test]
async fn destroy_a_push_subscription() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Destroy Dana").await;
    let jar = login(&pool, &email).await;
    let db = PgDb::new(pool.clone());
    let row = PushSubscriptionRepository::create_push_subscription(
        &db,
        uid,
        "https://fcm.googleapis.com/fcm/send/dana",
        Some("1"),
        Some("2"),
        Some("Mozilla/5.0"),
    )
    .await
    .expect("seed subscription");

    let response = post_form(
        &pool,
        "POST",
        &format!("/users/me/push_subscriptions/{}", row.id),
        &jar,
        "_method=delete",
    )
    .await;
    assert_eq!(response.status().as_u16(), 303);
    assert_eq!(
        header(&response, "location"),
        "/users/me/push_subscriptions"
    );
    assert_eq!(subscription_count(&pool, uid).await, 0);
}

#[tokio::test]
async fn logout_removes_the_sessions_endpoint() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Logout Lou").await;
    let jar = login(&pool, &email).await;
    let endpoint = format!(
        "https://fcm.googleapis.com/fcm/send/lou-{}",
        unique_email("x")
    );
    let db = PgDb::new(pool.clone());
    PushSubscriptionRepository::create_push_subscription(
        &db,
        uid,
        &endpoint,
        Some("1"),
        Some("2"),
        None,
    )
    .await
    .expect("seed subscription");

    let token = csrf_token();
    let request = http::Request::builder()
        .method("POST")
        .uri("/session")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .body(Body::from(format!(
            "authenticity_token={token}&_method=delete&{}",
            serde_urlencoded::to_string([("push_subscription_endpoint", endpoint.as_str())])
                .expect("endpoint encodes"),
        )))
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status().as_u16(), 303);
    assert_eq!(subscription_count(&pool, uid).await, 0);
}

// --- test notifications -----------------------------------------------

#[tokio::test]
async fn test_notification_redirects_to_the_index() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Test Thea").await;
    let jar = login(&pool, &email).await;
    let db = PgDb::new(pool.clone());
    let row = PushSubscriptionRepository::create_push_subscription(
        &db,
        uid,
        "https://fcm.googleapis.com/fcm/send/thea",
        Some("1"),
        Some("2"),
        None,
    )
    .await
    .expect("seed subscription");

    let response = post_form(
        &pool,
        "POST",
        &format!("/users/me/push_subscriptions/{}/test_notifications", row.id),
        &jar,
        "",
    )
    .await;
    assert_eq!(response.status().as_u16(), 303);
    assert_eq!(
        header(&response, "location"),
        "/users/me/push_subscriptions"
    );
}

#[tokio::test]
async fn test_notification_for_a_foreign_subscription_is_404() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, _owner_email) = seed_user(&pool, "Owner Owen").await;
    let (_uid, email) = seed_user(&pool, "Foreign Fran").await;
    let jar = login(&pool, &email).await;
    let db = PgDb::new(pool.clone());
    let row = PushSubscriptionRepository::create_push_subscription(
        &db,
        uid,
        "https://fcm.googleapis.com/fcm/send/owen",
        Some("1"),
        Some("2"),
        None,
    )
    .await
    .expect("seed subscription");

    let response = post_form(
        &pool,
        "POST",
        &format!("/users/me/push_subscriptions/{}/test_notifications", row.id),
        &jar,
        "",
    )
    .await;
    assert_eq!(response.status().as_u16(), 404);
    assert!(
        body_text(response)
            .await
            .contains("We can’t find that (404)")
    );
}

// --- index + bell + profile --------------------------------------------

#[tokio::test]
async fn index_lists_the_users_subscriptions() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Index Ida").await;
    let jar = login(&pool, &email).await;
    let db = PgDb::new(pool.clone());
    PushSubscriptionRepository::create_push_subscription(
        &db,
        uid,
        "https://fcm.googleapis.com/fcm/send/ida",
        Some("1"),
        Some("2"),
        Some(CHROME_MAC),
    )
    .await
    .expect("seed subscription");

    let (status, text) = get(&pool, "/users/me/push_subscriptions", &jar, None).await;
    assert_eq!(status, 200);
    assert!(
        text.contains("Push Notification Subscriptions"),
        "title: {text}"
    );
    assert!(
        text.contains("https://fcm.googleapis.com/fcm/send/ida"),
        "endpoint: {text}"
    );
    assert!(
        text.contains("Chrome 141.0.0.0 on Macintosh"),
        "agent line: {text}"
    );
    assert!(text.contains("Send test notification"), "test btn: {text}");
    assert!(text.contains("Delete subscription"), "delete btn: {text}");
    assert!(text.contains("vapid-public-key"), "vapid meta: {text}");
}

#[tokio::test]
async fn bell_carries_the_not_allowed_dialog() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Bell Bea").await;
    let jar = login(&pool, &email).await;
    let room_id = seed_room(&pool, uid, "Bell Room").await;

    let (status, text) = get(&pool, &format!("/rooms/{room_id}"), &jar, Some(CHROME_MAC)).await;
    assert_eq!(status, 200);
    // No-JS experiment: the dialog renders without Stimulus hooks.
    assert!(!text.contains("data-notifications-target="), "{text}");
    assert!(
        text.contains("Notifications aren’t allowed"),
        "heading: {text}"
    );
    assert!(
        text.contains("Check your Chrome settings"),
        "browser settings: {text}"
    );
    assert!(
        text.contains("Check your macOS settings"),
        "system settings: {text}"
    );
}

#[tokio::test]
async fn profile_links_the_dev_mode_index() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (_uid, email) = seed_user(&pool, "Dev Dan").await;
    let jar = login(&pool, &email).await;

    let (status, text) = get(&pool, "/users/me/profile", &jar, None).await;
    assert_eq!(status, 200);
    assert!(
        text.contains("Push Notifications Dev Mode"),
        "dev link: {text}"
    );
    assert!(
        text.contains("href=\"/users/me/push_subscriptions\""),
        "dev href: {text}"
    );
}
