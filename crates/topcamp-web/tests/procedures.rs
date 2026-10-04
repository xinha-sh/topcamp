//! Live procedure e2e (UI-05r): HTTP calls into the `/live/*`
//! procedures, asserting store effects plus bus fanout. Each test
//! holds a bus clone subscribed before the call, so fanout is
//! observed end to end (HTTP → procedure → db + bus).
//!
//! Requires `DATABASE_URL` (default: the isolated
//! `topcamp_test_procedures` database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_procedures;"` then apply
//! `migrations/*.sql` in order (re-apply after migration changes).

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::{MembershipRepository, RoomRepository, UserRepository};
use topcamp_web::live::{LiveBus, RoomEvent, UserEvent};
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_procedures".to_string()
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

async fn seed_member(pool: &PgPool, room_id: i64, uid: i64) {
    sqlx::query(
        "INSERT INTO memberships(created_at,updated_at,room_id,user_id) VALUES (now(),now(),$1,$2)",
    )
    .bind(room_id)
    .bind(uid)
    .execute(pool)
    .await
    .unwrap();
}

async fn login(pool: &PgPool, email: &str) -> String {
    let response = app(pool.clone()).handle(login_post(email, "s3cret")).await;
    assert_eq!(response.status(), 303);
    cookies(&response)
}

/// An `i64` argument in surrogate form (`{"t":"i64",...}`).
fn int_arg(id: i64) -> String {
    format!("{{\"t\":\"i64\",\"bits\":64,\"v\":\"{id}\"}}")
}

/// A procedure call: `POST <path>` with the JSON arg array.
async fn call(router: &Router, path: &str, jar: &str, args_json: &str) -> (u16, String) {
    let request = http::Request::builder()
        .method("POST")
        .uri(path)
        .header("cookie", jar)
        .header("content-type", "application/json")
        .body(Body::from(args_json.to_string()))
        .expect("request builds");
    let response = router.handle(request).await;
    let status = response.status().as_u16();
    (status, body_text(response).await)
}

async fn cleanup_message(pool: &PgPool, id: i64) {
    let db = PgDb::new(pool.clone());
    topcamp_db::repositories::MessageRepository::destroy(&db, id)
        .await
        .unwrap();
}

async fn cleanup_room(pool: &PgPool, id: i64) {
    sqlx::query("DELETE FROM memberships WHERE room_id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(id)
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
async fn post_procedure_stores_and_fans_out() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Pam Poster").await;
    let (oid, _) = seed_user(&pool, "Olly Other").await;
    let room_id = seed_room(&pool, uid, Some("Live"), "Rooms::Open").await;
    seed_member(&pool, room_id, oid).await;
    let jar = login(&pool, &email).await;

    let (router, bus) = app_with_bus(pool.clone());
    let mut rx_room = bus.room(room_id).subscribe();
    let mut rx_other = bus.user(oid).subscribe();
    let mut rx_self = bus.user(uid).subscribe();

    let (status, body) = call(
        &router,
        "/live/messages/post",
        &jar,
        &format!("[{},\"hello live\"]", int_arg(room_id)),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"ok\""), "ok envelope: {body}");

    let created: i64 = sqlx::query_scalar(
        "SELECT m.id FROM messages m JOIN action_text_rich_texts r \
         ON r.record_type = 'Message' AND r.record_id = m.id \
         WHERE m.room_id = $1 AND r.body LIKE '%hello live%'",
    )
    .bind(room_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx_room.recv())
        .await
        .expect("room event arrives")
        .unwrap();
    assert!(
        matches!(event, RoomEvent::MessageCreated { message_id } if message_id == created),
        "room fanout: {event:?}"
    );
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx_other.recv())
        .await
        .expect("member event arrives")
        .unwrap();
    assert!(
        matches!(event, UserEvent::MessageInRoom { room_id: rid } if rid == room_id),
        "member fanout: {event:?}"
    );
    // The author is a member too, so their sidebar pips as well.
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx_self.recv())
        .await
        .expect("self event arrives")
        .unwrap();
    assert!(
        matches!(event, UserEvent::MessageInRoom { room_id: rid } if rid == room_id),
        "self fanout: {event:?}"
    );

    cleanup_message(&pool, created).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, oid).await;
}

#[tokio::test]
async fn post_procedure_rejects_empty_and_signed_out() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Eve Empty").await;
    let room_id = seed_room(&pool, uid, Some("Drafts"), "Rooms::Open").await;
    let jar = login(&pool, &email).await;
    let (router, _bus) = app_with_bus(pool.clone());

    let (status, body) = call(
        &router,
        "/live/messages/post",
        &jar,
        &format!("[{},\"  \"]", int_arg(room_id)),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("Write a message first"), "{body}");

    let (status, body) = call(
        &router,
        "/live/messages/post",
        "",
        &format!("[{},\"hi\"]", int_arg(room_id)),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("signed out"), "{body}");

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE room_id = $1")
        .bind(room_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn typing_procedures_toggle_typists_and_publish() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Ty Per").await;
    let room_id = seed_room(&pool, uid, Some("Keys"), "Rooms::Open").await;
    let jar = login(&pool, &email).await;
    let (router, bus) = app_with_bus(pool.clone());
    let mut rx = bus.room(room_id).subscribe();

    let (status, body) = call(
        &router,
        "/live/typing/start",
        &jar,
        &format!("[{}]", int_arg(room_id)),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.trim(), "true");
    assert_eq!(bus.typing_now(room_id).len(), 1);
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("typing event arrives")
        .unwrap();
    assert!(
        matches!(event, RoomEvent::Typing { user_id, start: true, .. } if user_id == uid),
        "typing start: {event:?}"
    );

    // Repeats dedupe: no second event.
    let (status, _) = call(
        &router,
        "/live/typing/start",
        &jar,
        &format!("[{}]", int_arg(room_id)),
    )
    .await;
    assert_eq!(status, 200);
    assert!(rx.try_recv().is_err());

    let (status, body) = call(
        &router,
        "/live/typing/stop",
        &jar,
        &format!("[{}]", int_arg(room_id)),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(bus.typing_now(room_id).is_empty());
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("typing stop arrives")
        .unwrap();
    assert!(
        matches!(event, RoomEvent::Typing { user_id, start: false, .. } if user_id == uid),
        "typing stop: {event:?}"
    );

    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn presence_procedures_mark_and_unpip() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Paz Present").await;
    let room_id = seed_room(&pool, uid, Some("Here"), "Rooms::Open").await;
    // An unread stamp to clear.
    sqlx::query("UPDATE memberships SET unread_at = now() WHERE room_id = $1 AND user_id = $2")
        .bind(room_id)
        .bind(uid)
        .execute(&pool)
        .await
        .unwrap();
    let jar = login(&pool, &email).await;
    let (router, bus) = app_with_bus(pool.clone());
    let mut rx = bus.user(uid).subscribe();

    let (status, body) = call(
        &router,
        "/live/presence/present",
        &jar,
        &format!("[{}]", int_arg(room_id)),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.trim(), "true");
    let db = PgDb::new(pool.clone());
    assert!(
        !MembershipRepository::is_unread(&db, room_id, uid)
            .await
            .unwrap()
    );
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("read event arrives")
        .unwrap();
    assert!(
        matches!(event, UserEvent::RoomRead { room_id: rid } if rid == room_id),
        "read fanout: {event:?}"
    );

    let (status, _) = call(
        &router,
        "/live/presence/absent",
        &jar,
        &format!("[{}]", int_arg(room_id)),
    )
    .await;
    assert_eq!(status, 200);

    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}
