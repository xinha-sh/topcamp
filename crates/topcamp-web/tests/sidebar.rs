//! Sidebar + avatar tests against PostgreSQL + the real router.
//!
//! Requires `DATABASE_URL` (default: the isolated `topcamp_test_sidebar`
//! database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_sidebar;"` then apply
//! `migrations/*.sql` in order.
//! Seeds users + rooms + memberships, then removes them. The account
//! restriction flip restores the original settings immediately, so the
//! parallel suites never observe it.

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::{RoomRepository, UserRepository};
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcamp_web::users;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_sidebar".to_string()
    });
    PgPool::connect(&url).await.expect("connect DATABASE_URL")
}

fn app(pool: PgPool) -> Router {
    let _ = users::init_secret(b"sidebar-test-secret".to_vec());
    router(AppState::new(PgDb::new(pool), Cable::new()))
}

/// Serializes the restriction flip against sidebar renders: the flip
/// test briefly restricts creation account-wide, which would hide the
/// new-room button from a concurrent render assertion.
fn serial() -> &'static tokio::sync::Mutex<()> {
    static SERIAL: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    SERIAL.get_or_init(Default::default)
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

async fn seed_shared(pool: &PgPool, uid: i64, name: &str) -> i64 {
    let db = PgDb::new(pool.clone());
    let room = RoomRepository::create(&db, uid, Some(name), "Rooms::Open")
        .await
        .unwrap();
    seed_member(pool, room.id, uid).await;
    room.id
}

async fn seed_direct(pool: &PgPool, creator: i64, members: &[i64]) -> i64 {
    let db = PgDb::new(pool.clone());
    let room = RoomRepository::create(&db, creator, None, "Rooms::Direct")
        .await
        .unwrap();
    for uid in members {
        seed_member(pool, room.id, *uid).await;
    }
    room.id
}

async fn mark_unread(pool: &PgPool, room_id: i64, uid: i64) {
    sqlx::query("UPDATE memberships SET unread_at = now() WHERE room_id = $1 AND user_id = $2")
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

async fn get(pool: &PgPool, uri: &str, jar: &str) -> String {
    let request = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", jar)
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 200, "GET {uri}");
    body_text(response).await
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

#[tokio::test]
async fn sidebar_requires_a_session() {
    let pool = pool().await;
    let request = http::Request::builder()
        .method("GET")
        .uri("/users/me/sidebar")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool).handle(request).await;
    assert_eq!(response.status(), 303);
    assert_eq!(response.headers().get("location").unwrap(), "/session/new");
}

#[tokio::test]
async fn sidebar_frame_matches_upstream_markup() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Frame Fiona").await;
    let (pid, _) = seed_user(&pool, "Ping Pal").await;
    let jar = login(&pool, &email).await;
    // Placeholders show the oldest users first (capped at 20), so under
    // parallel load a fresh user may fall outside the window; the
    // oldest active user is always inside it (parallel suites only
    // ever direct-message their own fresh users).
    let oldest: i64 =
        sqlx::query_scalar("SELECT id FROM users WHERE status = 0 ORDER BY created_at, id LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let _guard = serial().lock().await;
    let html = get(&pool, "/users/me/sidebar", &jar).await;
    for needle in [
        "name=\"csrf-token\"",
        "id=\"user_sidebar\"",
        "id=\"direct_rooms_control\"",
        "id=\"direct_rooms\"",
        "id=\"shared_rooms\"",
        "href=\"/rooms/directs/new\"",
        "sidebar__close",
        "href=\"/users/me/profile\"",
        "href=\"/account/edit\"",
        "My Settings",
        "Account Settings",
        "Close menu",
        "rooms__new-btn",
        "Start a ping with",
        &format!("/rooms/directs?user_ids%5B%5D={oldest}"),
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
    // The endpoint frame has no `src` (only layout-embedded copies do).
    assert!(!html.contains("src=\"/users/me/sidebar\""), "{html}");
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, pid).await;
}

#[tokio::test]
async fn sidebar_toggle_is_a_popover() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Toggle Tess").await;
    let jar = login(&pool, &email).await;
    let html = get(&pool, "/", &jar).await;
    // The toggle is a popover invoker (top layer, Esc + outside-click
    // dismiss free), never a `<button>` needing JS for the `.open`
    // class; the panel carries `popover`, plus an explicit close.
    for needle in [
        "popovertarget=\"sidebar\"",
        "<aside id=\"sidebar\" popover=\"auto\">",
        "aria-label=\"Open menu\"",
        "aria-label=\"Close menu\"",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
    assert!(
        !html.contains("sidebar__state"),
        "checkbox state must be gone"
    );
    // The stylesheet slides the popover both ways and force-shows it as
    // persistent nav on desktop.
    let css = include_str!("../assets/css/sidebar.css");
    for needle in [
        "#sidebar[popover]:popover-open",
        "@starting-style",
        "allow-discrete",
    ] {
        assert!(css.contains(needle), "missing {needle}");
    }
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn welcome_embeds_the_sidebar_frame() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Welcomed Wendy").await;
    let jar = login(&pool, &email).await;
    let html = get(&pool, "/", &jar).await;
    for needle in [
        "No rooms yet",
        "id=\"user_sidebar\"",
        "src=\"/users/me/sidebar\"",
        "id=\"direct_rooms\"",
        "sidebar__tools",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn sidebar_lists_direct_and_shared_rooms() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Roomy Rita").await;
    let (bid, _) = seed_user(&pool, "Buddy Bob").await;
    let shared = seed_shared(&pool, uid, "Zulu Shared").await;
    let direct = seed_direct(&pool, uid, &[uid, bid]).await;
    let jar = login(&pool, &email).await;
    let html = get(&pool, "/users/me/sidebar", &jar).await;
    for needle in [
        &format!("list_rooms_direct_{direct}"),
        &format!("list_rooms_open_{shared}"),
        &format!("data-room-id=\"{direct}\""),
        "Buddy",
        ">Zulu Shared<",
        "class=\"direct\"",
        "class=\"align-center gap room btn txt-nowrap\"",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
    cleanup_room(&pool, shared).await;
    cleanup_room(&pool, direct).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, bid).await;
}

#[tokio::test]
async fn unread_rooms_get_the_unread_class() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Unread Uma").await;
    let (bid, _) = seed_user(&pool, "Unheard Uli").await;
    let shared = seed_shared(&pool, uid, "Unread Shared").await;
    let direct = seed_direct(&pool, uid, &[uid, bid]).await;
    mark_unread(&pool, shared, uid).await;
    mark_unread(&pool, direct, uid).await;
    let jar = login(&pool, &email).await;
    let html = get(&pool, "/users/me/sidebar", &jar).await;
    assert!(html.contains("class=\"direct unread\""), "{html}");
    assert!(
        html.contains("class=\"align-center gap room btn txt-nowrap unread\""),
        "{html}"
    );
    cleanup_room(&pool, shared).await;
    cleanup_room(&pool, direct).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, bid).await;
}

#[tokio::test]
async fn group_direct_shows_the_avatar_stack_and_initials() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Group Gus").await;
    let (bid, _) = seed_user(&pool, "Bob One").await;
    let (cid, _) = seed_user(&pool, "Cat Two").await;
    let direct = seed_direct(&pool, uid, &[uid, bid, cid]).await;
    let jar = login(&pool, &email).await;
    let html = get(&pool, "/users/me/sidebar", &jar).await;
    assert!(html.contains("avatar__group"), "{html}");
    assert!(html.contains("BO+CT"), "{html}");
    cleanup_room(&pool, direct).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, bid).await;
    cleanup_user(&pool, cid).await;
}

#[tokio::test]
async fn new_room_button_follows_creation_rights() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Member Max").await;
    let (aid, aemail) = seed_user(&pool, "Admin Amy").await;
    let db = PgDb::new(pool.clone());
    UserRepository::set_role(&db, aid, 1).await.unwrap();
    let member_jar = login(&pool, &email).await;
    let admin_jar = login(&pool, &aemail).await;
    // Restriction flags live on the singleton account row, absent on a
    // fresh database until first run.
    sqlx::query(
        "INSERT INTO accounts (created_at, updated_at, join_code, name) \
         VALUES (now(), now(), 'sidebar-join', 'Topcamp') ON CONFLICT DO NOTHING",
    )
    .execute(&pool)
    .await
    .unwrap();
    let _guard = serial().lock().await;

    // Unrestricted: members see the button.
    let html = get(&pool, "/users/me/sidebar", &member_jar).await;
    assert!(html.contains("rooms__new-btn"), "{html}");

    // Restricted: members lose it, administrators keep it.
    let before: Option<String> =
        sqlx::query_scalar("SELECT settings::text FROM accounts ORDER BY id LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query(
        "UPDATE accounts SET settings = jsonb_set(COALESCE(settings, '{}'), \
         '{restrict_room_creation_to_administrators}', 'true')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let html = get(&pool, "/users/me/sidebar", &member_jar).await;
    assert!(!html.contains("rooms__new-btn"), "{html}");
    let html = get(&pool, "/users/me/sidebar", &admin_jar).await;
    assert!(html.contains("rooms__new-btn"), "{html}");
    sqlx::query("UPDATE accounts SET settings = $1::jsonb")
        .bind(before)
        .execute(&pool)
        .await
        .unwrap();

    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, aid).await;
}

#[tokio::test]
async fn avatar_serves_initials_for_users_without_uploads() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Avatar Ava").await;
    let jar = login(&pool, &email).await;
    let request = http::Request::builder()
        .method("GET")
        .uri(format!("/users/{}/avatar", users::avatar_token(uid)))
        .header("cookie", jar)
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "image/svg+xml; charset=utf-8"
    );
    assert!(response.headers().contains_key("etag"));
    let body = body_text(response).await;
    assert!(body.contains("<svg"), "{body}");
    assert!(body.contains("\n      AA\n"), "{body}");
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn avatar_rejects_forged_tokens_and_requires_a_session() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Signed Sam").await;
    let jar = login(&pool, &email).await;
    for uri in ["/users/9.deadbeef/avatar", "/users/nope/avatar"] {
        let request = http::Request::builder()
            .method("GET")
            .uri(uri)
            .header("cookie", &jar)
            .body(Body::empty())
            .expect("request builds");
        let response = app(pool.clone()).handle(request).await;
        assert_eq!(response.status(), 404, "GET {uri}");
    }
    // Signed URLs still need a session.
    let request = http::Request::builder()
        .method("GET")
        .uri(format!("/users/{}/avatar", users::avatar_token(uid)))
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn avatar_supports_conditional_gets() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Cached Cid").await;
    let jar = login(&pool, &email).await;
    let uri = format!("/users/{}/avatar", users::avatar_token(uid));
    let request = http::Request::builder()
        .method("GET")
        .uri(&uri)
        .header("cookie", &jar)
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    let etag = response
        .headers()
        .get("etag")
        .expect("avatar carries an ETag")
        .to_str()
        .unwrap()
        .to_string();
    let request = http::Request::builder()
        .method("GET")
        .uri(&uri)
        .header("cookie", &jar)
        .header("if-none-match", etag)
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 304);
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn avatar_destroy_needs_the_method_override_and_redirects_home() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Delete Dee").await;
    let jar = login(&pool, &email).await;
    let uri = format!("/users/{}/avatar", users::avatar_token(uid));

    // A bare POST is not a destroy.
    let token = csrf_token();
    let request = http::Request::builder()
        .method("POST")
        .uri(&uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .body(Body::from(format!("authenticity_token={token}")))
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 404);

    // The override destroys (nothing attached) and lands on the profile.
    let request = http::Request::builder()
        .method("POST")
        .uri(&uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .body(Body::from(format!(
            "authenticity_token={token}&_method=delete"
        )))
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);
    assert_eq!(
        response.headers().get("location").unwrap(),
        "/users/me/profile"
    );
    cleanup_user(&pool, uid).await;
}
