//! Rooms home tests against PostgreSQL + the real router.
//!
//! Requires `DATABASE_URL` (default: the compose.yml scratch database).
//! Seeds users + rooms + memberships, then removes them. The `?v=`
//! logo assertions refetch while the install row is mid-stash (the
//! first_run suite briefly empties `accounts` in a parallel process).

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::{RoomRepository, UserRepository};
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

async fn seed_room(pool: &PgPool, uid: i64, name: &str) -> i64 {
    let db = PgDb::new(pool.clone());
    let room = RoomRepository::create(&db, uid, Some(name), "Rooms::Open")
        .await
        .unwrap();
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
    let response = app(pool.clone()).handle(login_post(email, "s3cret")).await;
    assert_eq!(response.status(), 303);
    cookies(&response)
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

fn destroy_request(
    room_id: i64,
    method: &str,
    jar: &str,
    token: Option<&str>,
) -> http::Request<Body> {
    let field = token.unwrap_or(&csrf_token()).to_string();
    let body = format!("authenticity_token={field}&_method=delete");
    http::Request::builder()
        .method(method)
        .uri(format!("/rooms/{room_id}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{jar}; csrf_token={}", csrf_token()))
        .body(Body::from(body))
        .expect("destroy request builds")
}

async fn get(pool: &PgPool, uri: &str, jar: &str) -> String {
    let request = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", jar)
        .body(Body::empty())
        .expect("request builds");
    body_text(app(pool.clone()).handle(request).await).await
}

#[tokio::test]
async fn welcome_redirects_to_last_visited_room() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "welly").await;
    let first = seed_room(&pool, uid, "first").await;
    let second = seed_room(&pool, uid, "second").await;
    let jar = login(&pool, &email).await;

    // No cookie: original room (oldest first).
    let request = http::Request::builder()
        .method("GET")
        .uri("/")
        .header("cookie", &jar)
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);
    assert_eq!(
        response.headers().get("location").unwrap(),
        &format!("/rooms/{first}")
    );

    // Cookie for a joined room wins.
    let request = http::Request::builder()
        .method("GET")
        .uri("/")
        .header("cookie", format!("{jar}; last_room={second}"))
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);
    assert_eq!(
        response.headers().get("location").unwrap(),
        &format!("/rooms/{second}")
    );

    // Garbage cookie falls back to the original room.
    let request = http::Request::builder()
        .method("GET")
        .uri("/")
        .header("cookie", format!("{jar}; last_room=new"))
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);
    assert_eq!(
        response.headers().get("location").unwrap(),
        &format!("/rooms/{first}")
    );

    cleanup_room(&pool, first).await;
    cleanup_room(&pool, second).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn welcome_empty_page_matches_upstream_markup() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "roomless").await;
    let jar = login(&pool, &email).await;
    let html = get(&pool, "/", &jar).await;
    for needle in [
        "No rooms yet",
        "body class=\"sidebar\"",
        "message-area--empty",
        "messages-empty",
        "roomless",
        "current-user-id",
        "current-user-name",
        "account/logo?v=",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn rooms_index_redirects_to_newest_room() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "indexy").await;
    let first = seed_room(&pool, uid, "first").await;
    let second = seed_room(&pool, uid, "second").await;
    let jar = login(&pool, &email).await;
    let request = http::Request::builder()
        .method("GET")
        .uri("/rooms")
        .header("cookie", jar)
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);
    assert_eq!(
        response.headers().get("location").unwrap(),
        &format!("/rooms/{second}")
    );
    assert_ne!(first, second);
    cleanup_room(&pool, first).await;
    cleanup_room(&pool, second).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn rooms_index_without_rooms_is_500() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "noroom").await;
    let jar = login(&pool, &email).await;
    let request = http::Request::builder()
        .method("GET")
        .uri("/rooms")
        .header("cookie", jar)
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 500);
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn rooms_new_is_404() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "newbie").await;
    let jar = login(&pool, &email).await;
    let request = http::Request::builder()
        .method("GET")
        .uri("/rooms/new")
        .header("cookie", jar)
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 404);
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn destroy_removes_room_and_redirects_home() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "deleter").await;
    let room = seed_room(&pool, uid, "doomed").await;
    let db = PgDb::new(pool.clone());
    let message = db
        .post_message(topcamp_db::repositories::NewMessage {
            room_id: room,
            creator_id: uid,
            client_message_id: "destroy-seed-1".to_string(),
            body: "bye".to_string(),
        })
        .await
        .unwrap();
    let jar = login(&pool, &email).await;

    let response = app(pool.clone())
        .handle(destroy_request(room, "DELETE", &jar, None))
        .await;
    assert_eq!(response.status(), 303);
    assert_eq!(response.headers().get("location").unwrap(), "/");

    // Memberships, messages (+ rich texts), and the row are gone.
    let memberships: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memberships WHERE room_id = $1")
            .bind(room)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(memberships, 0, "memberships not cleaned");
    let messages: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE room_id = $1")
        .bind(room)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(messages, 0, "messages not cleaned");
    let rooms: i64 = sqlx::query_scalar("SELECT count(*) FROM rooms WHERE id = $1")
        .bind(room)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rooms, 0, "room row not cleaned");
    let texts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM action_text_rich_texts WHERE record_type = 'Message' AND record_id = $1",
    )
    .bind(message.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(texts, 0, "rich texts not cleaned");

    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn destroy_via_post_override_also_works() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "overrider").await;
    let room = seed_room(&pool, uid, "doomed-post").await;
    let jar = login(&pool, &email).await;
    let response = app(pool.clone())
        .handle(destroy_request(room, "POST", &jar, None))
        .await;
    assert_eq!(response.status(), 303);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM rooms WHERE id = $1")
        .bind(room)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn destroy_plain_post_is_404() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "plainposter").await;
    let room = seed_room(&pool, uid, "kept").await;
    let jar = login(&pool, &email).await;
    let token = csrf_token();
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("/rooms/{room}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .body(Body::from(format!("authenticity_token={token}")))
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 404);
    cleanup_room(&pool, room).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn destroy_forbidden_for_plain_member() {
    let pool = pool().await;
    let (owner, _) = seed_user(&pool, "owner").await;
    let room = seed_room(&pool, owner, "owned").await;
    let (member, member_email) = seed_user(&pool, "member").await;
    sqlx::query(
        "INSERT INTO memberships(created_at,updated_at,room_id,user_id) VALUES (now(),now(),$1,$2)",
    )
    .bind(room)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();
    let jar = login(&pool, &member_email).await;
    let response = app(pool.clone())
        .handle(destroy_request(room, "DELETE", &jar, None))
        .await;
    assert_eq!(response.status(), 403);
    cleanup_room(&pool, room).await;
    cleanup_user(&pool, member).await;
    cleanup_user(&pool, owner).await;
}

#[tokio::test]
async fn destroy_unknown_room_redirects_home() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "stranger").await;
    let jar = login(&pool, &email).await;
    let response = app(pool.clone())
        .handle(destroy_request(999_999_999, "DELETE", &jar, None))
        .await;
    assert_eq!(response.status(), 303);
    assert_eq!(response.headers().get("location").unwrap(), "/");
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn destroy_signed_out_redirects_to_sign_in() {
    let pool = pool().await;
    let token = csrf_token();
    let request = http::Request::builder()
        .method("DELETE")
        .uri("/rooms/1")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("csrf_token={token}"))
        .body(Body::from(format!(
            "authenticity_token={token}&_method=delete"
        )))
        .expect("request builds");
    let response = app(pool).handle(request).await;
    assert_eq!(response.status(), 303);
    assert_eq!(response.headers().get("location").unwrap(), "/session/new");
}

#[tokio::test]
async fn destroy_without_csrf_is_forbidden() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "csrfroom").await;
    let room = seed_room(&pool, uid, "csrf-kept").await;
    let jar = login(&pool, &email).await;
    let response = app(pool.clone())
        .handle(destroy_request(room, "DELETE", &jar, Some("deadbeef")))
        .await;
    assert_eq!(response.status(), 403);
    cleanup_room(&pool, room).await;
    cleanup_user(&pool, uid).await;
}
