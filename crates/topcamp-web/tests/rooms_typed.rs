//! Typed rooms CRUD tests against PostgreSQL + the real router.
//!
//! Requires `DATABASE_URL` (default: the isolated `topcamp_test_typed`
//! database — open-room creates grant every active user, so they never
//! run against the dev database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_typed;"` then apply
//! `migrations/*.sql` in order (re-apply after migration changes).
//! Seeds users + rooms + memberships, then removes them.

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::{MembershipRepository, RoomRepository, UserRepository};
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_typed".to_string()
    });
    PgPool::connect(&url).await.expect("connect DATABASE_URL")
}

fn app(pool: PgPool) -> Router {
    router(AppState::new(PgDb::new(pool), Cable::new()))
}

/// Serializes the restriction flip against creation-gated endpoints:
/// the flip test briefly restricts creation account-wide, which would
/// 403 a concurrent new/create assertion.
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

async fn seed_room(pool: &PgPool, uid: i64, name: &str, kind: &str) -> i64 {
    let db = PgDb::new(pool.clone());
    let room = RoomRepository::create(&db, uid, Some(name), kind)
        .await
        .unwrap();
    seed_member(pool, room.id, uid).await;
    room.id
}

async fn login(pool: &PgPool, email: &str) -> String {
    let response = app(pool.clone()).handle(login_post(email, "s3cret")).await;
    assert_eq!(response.status(), 303);
    cookies(&response)
}

async fn get(pool: &PgPool, uri: &str, jar: &str) -> (u16, String) {
    let request = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", jar)
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    let status = response.status().as_u16();
    (status, body_text(response).await)
}

async fn mutate(
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

fn location(response: &topcoat::router::response::Response) -> String {
    response
        .headers()
        .get("location")
        .expect("redirect carries Location")
        .to_str()
        .unwrap()
        .to_string()
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
async fn opens_new_renders_the_form() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Open Opal").await;
    let jar = login(&pool, &email).await;
    let _guard = serial().lock().await;
    let (status, html) = get(&pool, "/rooms/opens/new", &jar).await;
    assert_eq!(status, 200);
    for needle in [
        "New chat room",
        "body class=\"\"",
        "name=\"room[name]\"",
        "value=\"New room\"",
        "Everyone",
        "view-transition-name: new-room",
        "Go Back",
        "id=\"user_sidebar\"",
        "action=\"/rooms/opens\"",
        "Give only some access to this room",
        "href=\"/rooms/closeds/new\"",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
    // No-JS experiment: no Turbo frames, no Turbo/Stimulus hooks,
    // no hand-written script tags on the page.
    for absent in [
        "turbo-frame",
        "data-turbo",
        "data-controller=",
        "data-action=",
        "room.js",
    ] {
        assert!(!html.contains(absent), "legacy hook present: {absent}");
    }
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn opens_new_requires_creation_rights() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Shut Sam").await;
    let (aid, aemail) = seed_user(&pool, "Open Amy").await;
    let db = PgDb::new(pool.clone());
    UserRepository::set_role(&db, aid, 1).await.unwrap();
    let member_jar = login(&pool, &email).await;
    let admin_jar = login(&pool, &aemail).await;
    let _guard = serial().lock().await;
    sqlx::query(
        "INSERT INTO accounts (created_at, updated_at, join_code, name) \
         VALUES (now(), now(), 'typed-test', 'Topcamp') ON CONFLICT DO NOTHING",
    )
    .execute(&pool)
    .await
    .unwrap();
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
    let (status, _) = get(&pool, "/rooms/opens/new", &member_jar).await;
    assert_eq!(status, 403);
    let (status, _) = get(&pool, "/rooms/opens/new", &admin_jar).await;
    assert_eq!(status, 200);
    sqlx::query("UPDATE accounts SET settings = $1::jsonb")
        .bind(before)
        .execute(&pool)
        .await
        .unwrap();
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, aid).await;
}

#[tokio::test]
async fn opens_create_grants_every_active_user() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Creator Cid").await;
    let (bid, _) = seed_user(&pool, "Bystander Bo").await;
    let jar = login(&pool, &email).await;
    let _guard = serial().lock().await;
    let response = mutate(
        &pool,
        "POST",
        "/rooms/opens",
        &jar,
        "room%5Bname%5D=Typed+Open",
    )
    .await;
    assert_eq!(response.status(), 303);
    let room_id: i64 = location(&response)
        .trim_start_matches("/rooms/")
        .parse()
        .unwrap();
    let db = PgDb::new(pool.clone());
    let room = RoomRepository::find_by_id(&db, room_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(room.kind, "Rooms::Open");
    assert_eq!(room.name.as_deref(), Some("Typed Open"));
    let members = MembershipRepository::member_user_ids(&db, room_id)
        .await
        .unwrap();
    assert!(members.contains(&uid));
    assert!(members.contains(&bid));
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, bid).await;
}

#[tokio::test]
async fn opens_create_rejects_a_missing_room_param() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Param Pam").await;
    let jar = login(&pool, &email).await;
    let _guard = serial().lock().await;
    let response = mutate(&pool, "POST", "/rooms/opens", &jar, "").await;
    assert_eq!(response.status(), 400);
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn opens_show_redirects_and_remembers_the_visit() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Shown Shawn").await;
    let room = seed_room(&pool, uid, "Shown Room", "Rooms::Open").await;
    let jar = login(&pool, &email).await;
    let request = http::Request::builder()
        .method("GET")
        .uri(format!("/rooms/opens/{room}"))
        .header("cookie", &jar)
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);
    assert_eq!(location(&response), format!("/rooms/{room}"));
    let remembered = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|pair| pair.starts_with(&format!("last_room={room}")));
    assert!(remembered, "last_room cookie set");
    cleanup_room(&pool, room).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn opens_show_redirects_home_for_unknown_or_direct_rooms() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Scope Scout").await;
    let (bid, _) = seed_user(&pool, "Scope Buddy").await;
    let db = PgDb::new(pool.clone());
    let direct = RoomRepository::create(&db, uid, None, "Rooms::Direct")
        .await
        .unwrap()
        .id;
    seed_member(&pool, direct, uid).await;
    seed_member(&pool, direct, bid).await;
    let jar = login(&pool, &email).await;
    for uri in ["/rooms/opens/999999999", &format!("/rooms/opens/{direct}")] {
        let request = http::Request::builder()
            .method("GET")
            .uri(uri)
            .header("cookie", &jar)
            .body(Body::empty())
            .expect("request builds");
        let response = app(pool.clone()).handle(request).await;
        assert_eq!(response.status(), 303, "GET {uri}");
        assert_eq!(location(&response), "/");
    }
    cleanup_room(&pool, direct).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, bid).await;
}

#[tokio::test]
async fn opens_edit_and_update_rename_the_room() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Edit Erin").await;
    let room = seed_room(&pool, uid, "Before Edit", "Rooms::Open").await;
    let jar = login(&pool, &email).await;
    let (status, html) = get(&pool, &format!("/rooms/opens/{room}/edit"), &jar).await;
    assert_eq!(status, 200);
    for needle in [
        "Edit settings for Before Edit",
        "value=\"Before Edit\"",
        &format!("href=\"/rooms/closeds/{room}/edit\""),
        "Delete Before Edit",
        "name=\"_method\" value=\"patch\"",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
    let response = mutate(
        &pool,
        "POST",
        &format!("/rooms/opens/{room}"),
        &jar,
        "room%5Bname%5D=After+Edit&_method=patch",
    )
    .await;
    assert_eq!(response.status(), 303);
    assert_eq!(location(&response), format!("/rooms/{room}"));
    let db = PgDb::new(pool.clone());
    let renamed = RoomRepository::find_by_id(&db, room)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renamed.name.as_deref(), Some("After Edit"));
    cleanup_room(&pool, room).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn opens_update_converts_a_closed_room() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Convert Connie").await;
    let room = seed_room(&pool, uid, "Convert Me", "Rooms::Closed").await;
    let jar = login(&pool, &email).await;
    let response = mutate(
        &pool,
        "PATCH",
        &format!("/rooms/opens/{room}"),
        &jar,
        "room%5Bname%5D=Convert+Me",
    )
    .await;
    assert_eq!(response.status(), 303);
    let db = PgDb::new(pool.clone());
    let converted = RoomRepository::find_by_id(&db, room)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(converted.kind, "Rooms::Open");
    cleanup_room(&pool, room).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn opens_edit_hides_the_form_from_non_administers() {
    let pool = pool().await;
    let (aid, aemail) = seed_user(&pool, "Owner Otto").await;
    let (uid, email) = seed_user(&pool, "Member Mia").await;
    let room = seed_room(&pool, aid, "Otto Room", "Rooms::Open").await;
    seed_member(&pool, room, uid).await;
    let jar = login(&pool, &email).await;
    let (status, html) = get(&pool, &format!("/rooms/opens/{room}/edit"), &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("flex-item-grow txt-x-large"), "{html}");
    assert!(!html.contains("name=\"room[name]\""), "{html}");
    assert!(!html.contains("Delete Otto Room"), "{html}");
    let response = mutate(
        &pool,
        "POST",
        &format!("/rooms/opens/{room}"),
        &jar,
        "room%5Bname%5D=Hijacked&_method=patch",
    )
    .await;
    assert_eq!(response.status(), 403);
    cleanup_room(&pool, room).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, aid).await;
    let _ = aemail;
}

#[tokio::test]
async fn opens_destroy_is_a_signed_in_500() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Doomed Dan").await;
    let room = seed_room(&pool, uid, "Doomed Room", "Rooms::Open").await;
    let jar = login(&pool, &email).await;
    let request = http::Request::builder()
        .method("DELETE")
        .uri(format!("/rooms/opens/{room}"))
        .header("cookie", &jar)
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 500);
    // Signed-out callers hit the session gate first.
    let request = http::Request::builder()
        .method("DELETE")
        .uri(format!("/rooms/opens/{room}"))
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);
    cleanup_room(&pool, room).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn closeds_new_and_create_grant_only_grantees() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Closed Clara").await;
    let (bid, _) = seed_user(&pool, "Grantee Gus").await;
    let (oid, _) = seed_user(&pool, "Outsider Olive").await;
    let jar = login(&pool, &email).await;
    let _guard = serial().lock().await;
    let (status, html) = get(&pool, "/rooms/closeds/new", &jar).await;
    assert_eq!(status, 200);
    for needle in [
        "New chat room",
        "Give everyone access to this room",
        "action=\"/rooms/closeds\"",
        &format!("name=\"user_ids[]\" value=\"{uid}\""),
        "href=\"/rooms/opens/new\"",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
    let response = mutate(
        &pool,
        "POST",
        "/rooms/closeds",
        &jar,
        &format!("room%5Bname%5D=Secret+Club&user_ids%5B%5D={uid}&user_ids%5B%5D={bid}"),
    )
    .await;
    assert_eq!(response.status(), 303);
    let room_id: i64 = location(&response)
        .trim_start_matches("/rooms/")
        .parse()
        .unwrap();
    let db = PgDb::new(pool.clone());
    let room = RoomRepository::find_by_id(&db, room_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(room.kind, "Rooms::Closed");
    let members = MembershipRepository::member_user_ids(&db, room_id)
        .await
        .unwrap();
    assert!(members.contains(&uid));
    assert!(members.contains(&bid));
    assert!(!members.contains(&oid));
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, bid).await;
    cleanup_user(&pool, oid).await;
}

#[tokio::test]
async fn closeds_update_revises_the_member_list() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Revise Rita").await;
    let (bid, _) = seed_user(&pool, "Leaver Leo").await;
    let (cid, _) = seed_user(&pool, "Joiner Jo").await;
    let room = seed_room(&pool, uid, "Revised Room", "Rooms::Closed").await;
    seed_member(&pool, room, bid).await;
    let jar = login(&pool, &email).await;
    let (status, html) = get(&pool, &format!("/rooms/closeds/{room}/edit"), &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("checked=\"checked\""), "{html}");
    let response = mutate(
        &pool,
        "POST",
        &format!("/rooms/closeds/{room}"),
        &jar,
        &format!(
            "room%5Bname%5D=Revised+Room&user_ids%5B%5D={uid}&user_ids%5B%5D={cid}&_method=patch"
        ),
    )
    .await;
    assert_eq!(response.status(), 303);
    let db = PgDb::new(pool.clone());
    let members = MembershipRepository::member_user_ids(&db, room)
        .await
        .unwrap();
    assert!(members.contains(&uid));
    assert!(members.contains(&cid));
    assert!(!members.contains(&bid));
    cleanup_room(&pool, room).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, bid).await;
    cleanup_user(&pool, cid).await;
}

#[tokio::test]
async fn closeds_destroy_is_a_signed_in_500() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Closed Carl").await;
    let room = seed_room(&pool, uid, "Carl Room", "Rooms::Closed").await;
    let jar = login(&pool, &email).await;
    let request = http::Request::builder()
        .method("DELETE")
        .uri(format!("/rooms/closeds/{room}"))
        .header("cookie", &jar)
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 500);
    cleanup_room(&pool, room).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn directs_new_renders_the_ping_composer() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Ping Petra").await;
    let jar = login(&pool, &email).await;
    let (status, html) = get(&pool, "/rooms/directs/new", &jar).await;
    assert_eq!(status, 200);
    for needle in [
        "directs--new",
        "name=\"user_ids[]\"",
        "id=\"direct_rooms_control\"",
        "rooms_direct[user_ids_input]",
        "Type names to ping someone",
        "Start Ping",
        "Cancel changes",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
    // No-JS experiment: the Turbo frame is a plain div and the
    // Stimulus autocomplete hooks are gone.
    for absent in [
        "turbo-frame",
        "data-turbo",
        "data-controller=",
        "data-action=",
        "autocompletable-user",
    ] {
        assert!(!html.contains(absent), "legacy hook present: {absent}");
    }
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn directs_create_finds_or_creates_by_member_set() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Ping Pablo").await;
    let (bid, _) = seed_user(&pool, "Ping Partner").await;
    let jar = login(&pool, &email).await;
    // Placeholder pings submit the id in the query string.
    let uri = format!("/rooms/directs?user_ids%5B%5D={bid}");
    let first = mutate(&pool, "POST", &uri, &jar, "").await;
    assert_eq!(first.status(), 303);
    let first_id = location(&first);
    let second = mutate(&pool, "POST", &uri, &jar, "").await;
    assert_eq!(second.status(), 303);
    assert_eq!(
        location(&second),
        first_id,
        "same member set reuses the room"
    );
    let room_id: i64 = first_id.trim_start_matches("/rooms/").parse().unwrap();
    let db = PgDb::new(pool.clone());
    let room = RoomRepository::find_by_id(&db, room_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(room.kind, "Rooms::Direct");
    let membership = MembershipRepository::find(&db, room_id, uid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(membership.involvement, "everything");
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, bid).await;
}

#[tokio::test]
async fn directs_create_drops_unknown_user_ids() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Lonely Len").await;
    let jar = login(&pool, &email).await;
    let response = mutate(
        &pool,
        "POST",
        "/rooms/directs?user_ids%5B%5D=999999999",
        &jar,
        "",
    )
    .await;
    assert_eq!(response.status(), 303);
    let room_id: i64 = location(&response)
        .trim_start_matches("/rooms/")
        .parse()
        .unwrap();
    let db = PgDb::new(pool.clone());
    let members = MembershipRepository::member_user_ids(&db, room_id)
        .await
        .unwrap();
    assert_eq!(members, vec![uid]);
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn directs_show_redirects_integers_and_404s_the_rest() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Show Sid").await;
    let jar = login(&pool, &email).await;
    // No existence check: even a missing room redirects.
    let (status, _) = get(&pool, "/rooms/directs/123456789", &jar).await;
    assert_eq!(status, 303);
    let request = http::Request::builder()
        .method("GET")
        .uri("/rooms/directs/abc")
        .header("cookie", &jar)
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 404);
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn directs_edit_shows_the_other_members() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Edit Ed").await;
    let (bid, _) = seed_user(&pool, "Buddy Bea").await;
    let db = PgDb::new(pool.clone());
    let direct = RoomRepository::create(&db, uid, None, "Rooms::Direct")
        .await
        .unwrap()
        .id;
    seed_member(&pool, direct, uid).await;
    seed_member(&pool, direct, bid).await;
    let jar = login(&pool, &email).await;
    let (status, html) = get(&pool, &format!("/rooms/directs/{direct}/edit"), &jar).await;
    assert_eq!(status, 200);
    for needle in [
        "Edit settings for Buddy Bea",
        "Buddy Bea",
        "Delete Ping",
        &format!("action=\"/rooms/directs/{direct}\""),
        "name=\"_method\" value=\"delete\"",
        "Go Back",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
    assert!(!html.contains("Edit Ed</strong>"), "{html}");
    cleanup_room(&pool, direct).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, bid).await;
}

#[tokio::test]
async fn directs_update_is_always_404() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Patch Pat").await;
    let db = PgDb::new(pool.clone());
    let direct = RoomRepository::create(&db, uid, None, "Rooms::Direct")
        .await
        .unwrap()
        .id;
    seed_member(&pool, direct, uid).await;
    let jar = login(&pool, &email).await;
    for method in ["PATCH", "PUT"] {
        let response = mutate(&pool, method, &format!("/rooms/directs/{direct}"), &jar, "").await;
        assert_eq!(response.status(), 404, "{method} signed in");
    }
    let request = http::Request::builder()
        .method("PATCH")
        .uri(format!("/rooms/directs/{direct}"))
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 404);
    cleanup_room(&pool, direct).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn directs_destroy_deletes_the_ping_for_any_member() {
    let pool = pool().await;
    let (aid, _) = seed_user(&pool, "Ping Owner").await;
    let (uid, email) = seed_user(&pool, "Ping Member").await;
    let db = PgDb::new(pool.clone());
    let direct = RoomRepository::create(&db, aid, None, "Rooms::Direct")
        .await
        .unwrap()
        .id;
    seed_member(&pool, direct, aid).await;
    seed_member(&pool, direct, uid).await;
    let jar = login(&pool, &email).await;
    let response = mutate(
        &pool,
        "POST",
        &format!("/rooms/directs/{direct}"),
        &jar,
        "_method=delete",
    )
    .await;
    assert_eq!(response.status(), 303);
    assert_eq!(location(&response), "/");
    assert!(
        RoomRepository::find_by_id(&db, direct)
            .await
            .unwrap()
            .is_none()
    );
    cleanup_room(&pool, direct).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, aid).await;
}

#[tokio::test]
async fn typed_indexes_redirect_like_rooms() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Index Ira").await;
    let first = seed_room(&pool, uid, "Index First", "Rooms::Open").await;
    let second = seed_room(&pool, uid, "Index Second", "Rooms::Open").await;
    let jar = login(&pool, &email).await;
    for uri in ["/rooms/opens", "/rooms/closeds", "/rooms/directs"] {
        let request = http::Request::builder()
            .method("GET")
            .uri(uri)
            .header("cookie", &jar)
            .body(Body::empty())
            .expect("request builds");
        let response = app(pool.clone()).handle(request).await;
        assert_eq!(response.status(), 303, "GET {uri}");
        assert_eq!(location(&response), format!("/rooms/{second}"));
    }
    cleanup_room(&pool, first).await;
    cleanup_room(&pool, second).await;
    cleanup_user(&pool, uid).await;
}
