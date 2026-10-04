//! Boost route tests against PostgreSQL + the real router.
//!
//! Requires `DATABASE_URL` (default: the isolated `topcamp_test_boosts`
//! database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_boosts;"` then apply
//! `migrations/*.sql` in order (re-apply after migration changes).

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::{MessageRepository, NewMessage, RoomRepository, UserRepository};
use topcamp_web::live::{LiveBus, RoomEvent};
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_boosts".to_string()
    });
    PgPool::connect(&url).await.expect("connect DATABASE_URL")
}

fn app_with_bus(pool: PgPool) -> (Router, LiveBus) {
    let state = AppState::new(PgDb::new(pool), Cable::new());
    let bus = state.bus.clone();
    (router(state), bus)
}

fn app(pool: PgPool) -> Router {
    app_with_bus(pool).0
}

fn csrf_token() -> String {
    "cd".repeat(32)
}

fn login_post(email: &str, password: &str) -> http::Request<Body> {
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
        .expect("login request builds")
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

async fn seed_user(pool: &PgPool, name: &str) -> (i64, String) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock runs forward")
        .as_nanos();
    let email = format!(
        "{}-{}-{nanos}@example.com",
        name.to_lowercase().replace(' ', "-"),
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

async fn seed_room(pool: &PgPool, uid: i64, name: Option<&str>, kind: &str) -> i64 {
    let db = PgDb::new(pool.clone());
    let room = RoomRepository::create(&db, uid, name, kind)
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

async fn seed_message(pool: &PgPool, room_id: i64, uid: i64, tag: &str, text: &str) -> i64 {
    let db = PgDb::new(pool.clone());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock runs forward")
        .as_nanos();
    db.post_message(NewMessage {
        room_id,
        creator_id: uid,
        client_message_id: format!("{tag}-{nanos}"),
        body: topcamp_web::richtext::canonicalize_plain(text),
    })
    .await
    .expect("seed message")
    .id
}

async fn login(pool: &PgPool, email: &str) -> String {
    let response = app(pool.clone()).handle(login_post(email, "s3cret")).await;
    assert_eq!(response.status(), 303);
    cookies(&response)
}

async fn get_html(router: &Router, uri: &str, jar: &str) -> (u16, String) {
    let request = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", jar)
        .header("accept", "text/html,application/xhtml+xml")
        .body(Body::empty())
        .expect("request builds");
    let response = router.handle(request).await;
    let status = response.status().as_u16();
    (status, body_text(response).await)
}

async fn post_form(
    router: &Router,
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
    router.handle(request).await
}

fn location(response: &topcoat::router::response::Response) -> String {
    response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

async fn cleanup_message(pool: &PgPool, id: i64) {
    let db = PgDb::new(pool.clone());
    MessageRepository::destroy(&db, id).await.unwrap();
}

async fn cleanup_room(pool: &PgPool, room_id: i64) {
    sqlx::query("DELETE FROM memberships WHERE room_id = $1")
        .bind(room_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(room_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn cleanup_user(pool: &PgPool, uid: i64) {
    sqlx::query("DELETE FROM memberships WHERE user_id = $1")
        .bind(uid)
        .execute(pool)
        .await
        .unwrap();
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
async fn index_renders_boosting_fragment() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Bea Booster").await;
    let room_id = seed_room(&pool, uid, Some("Boosts"), "Rooms::Open").await;
    let mid = seed_message(&pool, room_id, uid, "boost-idx", "boost me").await;
    let db = PgDb::new(pool.clone());
    MessageRepository::create_boost(&db, mid, uid, "🎉")
        .await
        .unwrap();
    let jar = login(&pool, &email).await;

    let (status, body) =
        get_html(&app(pool.clone()), &format!("/messages/{mid}/boosts"), &jar).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("boosting_"), "boosting frame: {body}");
    assert!(body.contains("🎉"), "chip content: {body}");
    assert!(
        body.contains(&format!("/messages/{mid}/boosts/new")),
        "inline link: {body}"
    );

    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn new_renders_boost_form() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Ned Newboost").await;
    let room_id = seed_room(&pool, uid, Some("Boosts"), "Rooms::Open").await;
    let mid = seed_message(&pool, room_id, uid, "boost-new", "boost me").await;
    let jar = login(&pool, &email).await;

    let (status, body) = get_html(
        &app(pool.clone()),
        &format!("/messages/{mid}/boosts/new"),
        &jar,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("boost__form"), "form: {body}");
    assert!(body.contains("boost[content]"), "content input: {body}");
    assert!(body.contains("authenticity_token"), "csrf: {body}");

    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn create_stores_boost_redirects_and_fans_out() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Cid Creator").await;
    let room_id = seed_room(&pool, uid, Some("Boosts"), "Rooms::Open").await;
    let mid = seed_message(&pool, room_id, uid, "boost-create", "boost me").await;
    let jar = login(&pool, &email).await;

    let (router, bus) = app_with_bus(pool.clone());
    let mut rx = bus.room(room_id).subscribe();
    let response = post_form(
        &router,
        "POST",
        &format!("/messages/{mid}/boosts"),
        &jar,
        "boost%5Bcontent%5D=%F0%9F%91%8F",
    )
    .await;
    assert_eq!(response.status(), 303);
    assert_eq!(location(&response), format!("/rooms/{room_id}"));

    let db = PgDb::new(pool.clone());
    let boosts = MessageRepository::boosts_for_message(&db, mid)
        .await
        .unwrap();
    assert_eq!(boosts.len(), 1);
    assert_eq!(boosts[0].content, "👏");
    assert_eq!(boosts[0].booster_id, uid);

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("event arrives")
        .expect("channel open");
    assert!(
        matches!(event, RoomEvent::BoostsChanged { message_id } if message_id == mid),
        "boosts changed: {event:?}"
    );

    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn create_rejects_missing_form_and_bad_token() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Rita Reject").await;
    let room_id = seed_room(&pool, uid, Some("Boosts"), "Rooms::Open").await;
    let mid = seed_message(&pool, room_id, uid, "boost-rej", "boost me").await;
    let jar = login(&pool, &email).await;
    let router = app(pool.clone());

    let response = post_form(
        &router,
        "POST",
        &format!("/messages/{mid}/boosts"),
        &jar,
        "",
    )
    .await;
    assert_eq!(response.status(), 400);

    let request = http::Request::builder()
        .method("POST")
        .uri(format!("/messages/{mid}/boosts"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{jar}; csrf_token=wrong"))
        .body(Body::from("authenticity_token=wrong&boost%5Bcontent%5D=x"))
        .expect("request builds");
    let response = router.handle(request).await;
    assert_eq!(response.status(), 403);

    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn unreachable_message_404s() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Una Reach").await;
    let (oid, _) = seed_user(&pool, "Otto Other").await;
    let room_id = seed_room(&pool, oid, Some("Private"), "Rooms::Closed").await;
    let mid = seed_message(&pool, room_id, oid, "boost-unreach", "not yours").await;
    let jar = login(&pool, &email).await;
    let router = app(pool.clone());

    let (status, _) = get_html(&router, &format!("/messages/{mid}/boosts"), &jar).await;
    assert_eq!(status, 404);
    let (status, _) = get_html(&router, &format!("/messages/{mid}/boosts/new"), &jar).await;
    assert_eq!(status, 404);
    let response = post_form(
        &router,
        "POST",
        &format!("/messages/{mid}/boosts"),
        &jar,
        "boost%5Bcontent%5D=x",
    )
    .await;
    assert_eq!(response.status(), 404);
    let (status, _) = get_html(&router, "/messages/999999999/boosts", &jar).await;
    assert_eq!(status, 404);

    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, oid).await;
}

#[tokio::test]
async fn destroy_removes_own_boost_with_no_content() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Deb Destroy").await;
    let room_id = seed_room(&pool, uid, Some("Boosts"), "Rooms::Open").await;
    let mid = seed_message(&pool, room_id, uid, "boost-del", "boost me").await;
    let db = PgDb::new(pool.clone());
    let bid = MessageRepository::create_boost(&db, mid, uid, "🔥")
        .await
        .unwrap();
    let jar = login(&pool, &email).await;

    let (router, bus) = app_with_bus(pool.clone());
    let mut rx = bus.room(room_id).subscribe();
    let response = post_form(
        &router,
        "DELETE",
        &format!("/messages/{mid}/boosts/{bid}"),
        &jar,
        "",
    )
    .await;
    assert_eq!(response.status(), 204);

    let boosts = MessageRepository::boosts_for_message(&db, mid)
        .await
        .unwrap();
    assert!(boosts.is_empty());

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("event arrives")
        .expect("channel open");
    assert!(
        matches!(event, RoomEvent::BoostsChanged { message_id } if message_id == mid),
        "boosts changed: {event:?}"
    );

    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn destroy_via_method_override() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Moe Override").await;
    let room_id = seed_room(&pool, uid, Some("Boosts"), "Rooms::Open").await;
    let mid = seed_message(&pool, room_id, uid, "boost-ovr", "boost me").await;
    let db = PgDb::new(pool.clone());
    let bid = MessageRepository::create_boost(&db, mid, uid, "🔥")
        .await
        .unwrap();
    let jar = login(&pool, &email).await;

    let response = post_form(
        &app(pool.clone()),
        "POST",
        &format!("/messages/{mid}/boosts/{bid}"),
        &jar,
        "_method=delete",
    )
    .await;
    assert_eq!(response.status(), 204);
    let boosts = MessageRepository::boosts_for_message(&db, mid)
        .await
        .unwrap();
    assert!(boosts.is_empty());

    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn destroy_rejects_others_boost() {
    let pool = pool().await;
    let (uid, _email) = seed_user(&pool, "Owen Owner").await;
    let (oid, oemail) = seed_user(&pool, "Olive Other").await;
    let room_id = seed_room(&pool, uid, Some("Boosts"), "Rooms::Open").await;
    sqlx::query(
        "INSERT INTO memberships(created_at,updated_at,room_id,user_id) VALUES (now(),now(),$1,$2)",
    )
    .bind(room_id)
    .bind(oid)
    .execute(&pool)
    .await
    .unwrap();
    let mid = seed_message(&pool, room_id, uid, "boost-own", "boost me").await;
    let db = PgDb::new(pool.clone());
    let bid = MessageRepository::create_boost(&db, mid, uid, "🔥")
        .await
        .unwrap();
    let jar = login(&pool, &oemail).await;

    let response = post_form(
        &app(pool.clone()),
        "DELETE",
        &format!("/messages/{mid}/boosts/{bid}"),
        &jar,
        "",
    )
    .await;
    assert_eq!(response.status(), 404);
    let boosts = MessageRepository::boosts_for_message(&db, mid)
        .await
        .unwrap();
    assert_eq!(boosts.len(), 1);

    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, oid).await;
}

#[tokio::test]
async fn signed_out_requests_redirect_to_login() {
    let pool = pool().await;
    let router = app(pool.clone());
    let (status, _) = get_html(&router, "/messages/1/boosts", "").await;
    assert_eq!(status, 303);
    let response = post_form(
        &router,
        "POST",
        "/messages/1/boosts",
        "",
        "boost%5Bcontent%5D=x",
    )
    .await;
    assert_eq!(response.status(), 303);
}

#[tokio::test]
async fn boost_procedure_stores_and_fans_out() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Pam Procedural").await;
    let room_id = seed_room(&pool, uid, Some("Boosts"), "Rooms::Open").await;
    let mid = seed_message(&pool, room_id, uid, "boost-proc", "boost me").await;
    let jar = login(&pool, &email).await;

    let (router, bus) = app_with_bus(pool.clone());
    let mut rx = bus.room(room_id).subscribe();
    let args = format!("[{{\"t\":\"i64\",\"bits\":64,\"v\":\"{mid}\"}},\"🚀\"]",);
    let request = http::Request::builder()
        .method("POST")
        .uri("/live/messages/boost")
        .header("cookie", &jar)
        .header("content-type", "application/json")
        .body(Body::from(args))
        .expect("request builds");
    let response = router.handle(request).await;
    assert_eq!(response.status(), 200);
    let text = body_text(response).await;
    assert!(text.contains("\"ok\""), "ok envelope: {text}");
    assert!(!text.contains("\"err\""), "no error: {text}");

    let db = PgDb::new(pool.clone());
    let boosts = MessageRepository::boosts_for_message(&db, mid)
        .await
        .unwrap();
    assert_eq!(boosts.len(), 1);
    assert_eq!(boosts[0].content, "🚀");

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("event arrives")
        .expect("channel open");
    assert!(
        matches!(event, RoomEvent::BoostsChanged { message_id } if message_id == mid),
        "boosts changed: {event:?}"
    );

    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}
