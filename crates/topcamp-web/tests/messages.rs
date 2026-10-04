//! Room show + message CRUD tests against PostgreSQL + the real router.
//!
//! Requires `DATABASE_URL` (default: the isolated `topcamp_test_messages`
//! database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_messages;"` then apply
//! `migrations/*.sql` in order (re-apply after migration changes).

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::{MessageRepository, NewMessage, RoomRepository, UserRepository};
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_messages".to_string()
    });
    PgPool::connect(&url).await.expect("connect DATABASE_URL")
}

fn app(pool: PgPool) -> Router {
    router(AppState::new(PgDb::new(pool), Cable::new()))
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

async fn seed_account(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO accounts (created_at, updated_at, join_code, name) \
         VALUES (now(), now(), 'msg-test-join', 'Topcamp') ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn login(pool: &PgPool, email: &str) -> String {
    let response = app(pool.clone()).handle(login_post(email, "s3cret")).await;
    assert_eq!(response.status(), 303);
    cookies(&response)
}

async fn get_html(pool: &PgPool, uri: &str, jar: &str) -> (u16, String) {
    let request = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", jar)
        .header("accept", "text/html,application/xhtml+xml")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    let status = response.status().as_u16();
    (status, body_text(response).await)
}

async fn get_json(pool: &PgPool, uri: &str, jar: &str) -> (u16, String) {
    let request = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", jar)
        .header("accept", "application/json")
        .body(Body::empty())
        .expect("request builds");
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

fn location(response: &topcoat::router::response::Response) -> String {
    response
        .headers()
        .get("location")
        .expect("redirect carries Location")
        .to_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn show_redirects_anonymous_to_login() {
    let pool = pool().await;
    let (status, _) = get_html(&pool, "/rooms/1", "").await;
    assert_eq!(status, 303);
    let request = http::Request::builder()
        .method("GET")
        .uri("/rooms/1")
        .header("accept", "text/html")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(location(&response), "/session/new");
}

#[tokio::test]
async fn show_unknown_and_invalid_rooms_redirect_home() {
    let pool = pool().await;
    let (_, email) = seed_user(&pool, "Ash Drifter").await;
    let jar = login(&pool, &email).await;
    for uri in ["/rooms/999999999", "/rooms/abc"] {
        let request = http::Request::builder()
            .method("GET")
            .uri(uri)
            .header("cookie", &jar)
            .header("accept", "text/html")
            .body(Body::empty())
            .expect("request builds");
        let response = app(pool.clone()).handle(request).await;
        assert_eq!(response.status(), 303, "for {uri}");
        assert_eq!(location(&response), "/");
    }
    let uid: i64 = sqlx::query_scalar("SELECT id FROM users WHERE email_address = $1")
        .bind(&email)
        .fetch_one(&pool)
        .await
        .unwrap();
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn show_renders_room_page_and_remembers_visit() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Rory Poster").await;
    let room_id = seed_room(&pool, uid, Some("Showcase"), "Rooms::Open").await;
    let mid = seed_message(&pool, room_id, uid, "show", "Hello showcase").await;
    let jar = login(&pool, &email).await;
    let request = http::Request::builder()
        .method("GET")
        .uri(format!("/rooms/{room_id}"))
        .header("cookie", &jar)
        .header("accept", "text/html")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 200);
    assert!(
        cookies(&response)
            .split("; ")
            .any(|pair| pair == format!("last_room={room_id}")),
        "remembers the visit"
    );
    let html = body_text(response).await;
    assert!(html.contains("id=\"message-area\""), "message area");
    assert!(
        html.contains(&format!("id=\"messages_rooms_open_{room_id}\"")),
        "messages container"
    );
    assert!(html.contains("id=\"composer\""), "composer");
    assert!(html.contains("Hello showcase"), "message body");
    assert!(
        html.contains("message--formatted"),
        "pre-formatted visible: {html}"
    );
    assert!(html.contains("message--me"), "own message: {html}");
    assert!(html.contains("message--first-of-day"), "separator: {html}");
    // No-JS experiment: server-rendered time, no Stimulus hook.
    assert!(html.contains("message__timestamp"), "time text: {html}");
    assert!(!html.contains("data-local-time-target="), "{html}");
    assert!(html.contains("<title>Showcase</title>"), "title");
    assert!(
        html.contains(&format!("/rooms/{room_id}/@{mid}")),
        "permalink"
    );
    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn show_at_pages_around_the_message() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Perry Anchor").await;
    let room_id = seed_room(&pool, uid, Some("Anchors"), "Rooms::Open").await;
    let m1 = seed_message(&pool, room_id, uid, "at1", "first anchor").await;
    let m2 = seed_message(&pool, room_id, uid, "at2", "second anchor").await;
    let m3 = seed_message(&pool, room_id, uid, "at3", "third anchor").await;
    let jar = login(&pool, &email).await;
    let (status, html) = get_html(&pool, &format!("/rooms/{room_id}/@{m2}"), &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("first anchor"));
    assert!(html.contains("second anchor"));
    assert!(html.contains("third anchor"));
    // An unknown anchor falls back to the last page (no 404).
    let (status, html) = get_html(&pool, &format!("/rooms/{room_id}/@999999999"), &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("third anchor"));
    // Without the `@` prefix the catch-param 404s.
    let (status, _) = get_html(&pool, &format!("/rooms/{room_id}/bogus"), &jar).await;
    assert_eq!(status, 404);
    for mid in [m1, m2, m3] {
        cleanup_message(&pool, mid).await;
    }
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn direct_room_shows_other_member_name() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Dana Viewer").await;
    let (oid, oemail) = seed_user(&pool, "Owen Other").await;
    let room_id = seed_room(&pool, uid, None, "Rooms::Direct").await;
    seed_member(&pool, room_id, oid).await;
    let jar = login(&pool, &email).await;
    let (status, html) = get_html(&pool, &format!("/rooms/{room_id}"), &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("Owen Other"));
    assert!(html.contains("Ping with"));
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, oid).await;
    let _ = oemail;
}

#[tokio::test]
async fn invitation_shows_until_paged() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Ivy Invite").await;
    let room_id = seed_room(&pool, uid, Some("Originals"), "Rooms::Open").await;
    // Backdate: deterministically the original room under concurrency.
    sqlx::query("UPDATE rooms SET created_at = '2000-01-01' WHERE id = $1")
        .bind(room_id)
        .execute(&pool)
        .await
        .unwrap();
    let jar = login(&pool, &email).await;
    let (status, html) = get_html(&pool, &format!("/rooms/{room_id}"), &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("id=\"system_welcome\""), "invitation card");
    assert!(html.contains("/join/msg-test-join"), "join url");
    let mut ids = Vec::new();
    for n in 0..41 {
        ids.push(seed_message(&pool, room_id, uid, "page", &format!("filler {n}")).await);
    }
    let (status, html) = get_html(&pool, &format!("/rooms/{room_id}"), &jar).await;
    assert_eq!(status, 200);
    assert!(
        !html.contains("id=\"system_welcome\""),
        "paged rooms skip it"
    );
    for mid in ids {
        cleanup_message(&pool, mid).await;
    }
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn index_pages_before_and_after() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Ines Index").await;
    let room_id = seed_room(&pool, uid, Some("Paging"), "Rooms::Open").await;
    let m1 = seed_message(&pool, room_id, uid, "pg1", "page one").await;
    let m2 = seed_message(&pool, room_id, uid, "pg2", "page two").await;
    let m3 = seed_message(&pool, room_id, uid, "pg3", "page three").await;
    let jar = login(&pool, &email).await;
    let base = format!("/rooms/{room_id}/messages");
    let (status, html) = get_html(&pool, &base, &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("page three"), "last page has newest");
    let (status, html) = get_html(&pool, &format!("{base}?before={m2}"), &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("page one"));
    assert!(!html.contains("page two"), "before excludes the anchor");
    let (status, html) = get_html(&pool, &format!("{base}?after={m2}"), &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("page three"));
    assert!(!html.contains("page one"), "after excludes older");
    for uri in [
        format!("{base}?before=nope"),
        format!("{base}?before=999999999"),
    ] {
        let (status, _) = get_html(&pool, &uri, &jar).await;
        assert_eq!(status, 404, "for {uri}");
    }
    for mid in [m1, m2, m3] {
        cleanup_message(&pool, mid).await;
    }
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn index_empty_room_is_no_content() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Nia Nothing").await;
    let room_id = seed_room(&pool, uid, Some("Empty"), "Rooms::Open").await;
    let jar = login(&pool, &email).await;
    let (status, _) = get_html(&pool, &format!("/rooms/{room_id}/messages"), &jar).await;
    assert_eq!(status, 204);
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn index_supports_etag_revalidation() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Etta Tag").await;
    let room_id = seed_room(&pool, uid, Some("Tagged"), "Rooms::Open").await;
    let mid = seed_message(&pool, room_id, uid, "etag", "cache me").await;
    let jar = login(&pool, &email).await;
    let uri = format!("/rooms/{room_id}/messages");
    let request = http::Request::builder()
        .method("GET")
        .uri(&uri)
        .header("cookie", &jar)
        .header("accept", "text/html")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 200);
    let etag = response
        .headers()
        .get("etag")
        .expect("etag header")
        .to_str()
        .unwrap()
        .to_string();
    let request = http::Request::builder()
        .method("GET")
        .uri(&uri)
        .header("cookie", &jar)
        .header("accept", "text/html")
        .header("if-none-match", etag)
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 304);
    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn json_clients_keep_the_api() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Aja Api").await;
    let room_id = seed_room(&pool, uid, Some("Apiary"), "Rooms::Open").await;
    let mid = seed_message(&pool, room_id, uid, "api", "api hello").await;
    let jar = login(&pool, &email).await;
    let (status, body) = get_json(&pool, &format!("/rooms/{room_id}"), &jar).await;
    assert_eq!(status, 200);
    assert!(body.contains("\"Apiary\""), "room JSON: {body}");
    let (status, body) = get_json(&pool, &format!("/rooms/{room_id}/messages"), &jar).await;
    assert_eq!(status, 200);
    assert!(body.contains(&format!("\"id\":{mid}")), "list JSON: {body}");
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("/rooms/{room_id}/messages"))
        .header("cookie", &jar)
        .header("content-type", "application/json")
        .body(Body::from(
            "{\"client_message_id\":\"api-1\",\"body\":\"api post\"}",
        ))
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 200);
    let created: i64 = sqlx::query_scalar(
        "SELECT id FROM messages WHERE room_id = $1 AND client_message_id = 'api-1'",
    )
    .bind(room_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    cleanup_message(&pool, created).await;
    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn create_form_posts_land_back_on_the_room() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Cora Create").await;
    let room_id = seed_room(&pool, uid, Some("Posting"), "Rooms::Open").await;
    let jar = login(&pool, &email).await;
    let response = post_form(
        &pool,
        "POST",
        &format!("/rooms/{room_id}/messages"),
        &jar,
        "message%5Bbody%5D=stream+me&message%5Bclient_message_id%5D=",
    )
    .await;
    assert_eq!(response.status(), 303);
    assert_eq!(
        response.headers().get("location").unwrap(),
        &format!("/rooms/{room_id}")
    );
    let created: i64 = sqlx::query_scalar(
        "SELECT m.id FROM messages m JOIN action_text_rich_texts r \
         ON r.record_type = 'Message' AND r.record_id = m.id \
         WHERE m.room_id = $1 AND r.body LIKE '%stream me%'",
    )
    .bind(room_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    cleanup_message(&pool, created).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn create_rejects_missing_form_and_bad_token() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Cora Reject").await;
    let room_id = seed_room(&pool, uid, Some("Rejects"), "Rooms::Open").await;
    let jar = login(&pool, &email).await;
    let uri = format!("/rooms/{room_id}/messages");
    let response = post_form(&pool, "POST", &uri, &jar, "note=lonely").await;
    assert_eq!(response.status(), 400);
    let request = http::Request::builder()
        .method("POST")
        .uri(&uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{jar}; csrf_token=wrong"))
        .body(Body::from(
            "authenticity_token=wrong&message%5Bbody%5D=nope",
        ))
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 403);
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn create_into_unmembered_room_shows_not_found() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Gary Gone").await;
    let (oid, _) = seed_user(&pool, "Holly Host").await;
    // Belongs to Holly only; Gary is signed in but not a member.
    let room_id = seed_room(&pool, oid, Some("Private"), "Rooms::Closed").await;
    let jar = login(&pool, &email).await;
    let response = post_form(
        &pool,
        "POST",
        &format!("/rooms/{room_id}/messages"),
        &jar,
        "message%5Bbody%5D=intruder",
    )
    .await;
    assert_eq!(response.status(), 200);
    let html = body_text(response).await;
    assert!(html.contains("This room was deleted."), "{html}");
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, oid).await;
}

#[tokio::test]
async fn show_message_page_and_new_404s() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Shawna Show").await;
    let room_id = seed_room(&pool, uid, Some("Singles"), "Rooms::Open").await;
    let mid = seed_message(&pool, room_id, uid, "solo", "lone message").await;
    let jar = login(&pool, &email).await;
    let (status, html) = get_html(&pool, &format!("/rooms/{room_id}/messages/{mid}"), &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("lone message"));
    assert!(html.contains("<title>Topcamp</title>"));
    for uri in [
        format!("/rooms/{room_id}/messages/new"),
        format!("/rooms/{room_id}/messages/999999999"),
        format!("/rooms/{room_id}/messages/nope"),
    ] {
        let (status, _) = get_html(&pool, &uri, &jar).await;
        assert_eq!(status, 404, "for {uri}");
    }
    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn edit_page_gates_on_creator_or_admin() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Ed Pendant").await;
    let (oid, oemail) = seed_user(&pool, "Olive Stranger").await;
    let (aid, aemail) = seed_user(&pool, "Ada Admin").await;
    let db = PgDb::new(pool.clone());
    UserRepository::set_role(&db, aid, 1).await.unwrap();
    let room_id = seed_room(&pool, uid, Some("Edits"), "Rooms::Open").await;
    seed_member(&pool, room_id, oid).await;
    seed_member(&pool, room_id, aid).await;
    let mid = seed_message(&pool, room_id, uid, "edit", "draft text").await;
    let uri = format!("/rooms/{room_id}/messages/{mid}/edit");
    let (status, html) = get_html(&pool, &uri, &login(&pool, &email).await).await;
    assert_eq!(status, 200);
    assert!(html.contains("draft text"), "editable body: {html}");
    let (status, _) = get_html(&pool, &uri, &login(&pool, &oemail).await).await;
    assert_eq!(status, 403);
    let (status, _) = get_html(&pool, &uri, &login(&pool, &aemail).await).await;
    assert_eq!(status, 200);
    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    for id in [uid, oid, aid] {
        cleanup_user(&pool, id).await;
    }
}

#[tokio::test]
async fn update_stores_body_and_redirects() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Uma Update").await;
    let room_id = seed_room(&pool, uid, Some("Rewrites"), "Rooms::Open").await;
    let mid = seed_message(&pool, room_id, uid, "upd", "before").await;
    let jar = login(&pool, &email).await;
    let uri = format!("/rooms/{room_id}/messages/{mid}");
    let response = post_form(&pool, "PATCH", &uri, &jar, "message%5Bbody%5D=after").await;
    assert_eq!(response.status(), 303);
    assert_eq!(location(&response), uri);
    let stored: String = sqlx::query_scalar(
        "SELECT body FROM action_text_rich_texts \
         WHERE record_type = 'Message' AND record_id = $1",
    )
    .bind(mid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(stored.contains("after"), "stored: {stored}");
    // The Rack-style override dispatches the same action.
    let response = post_form(
        &pool,
        "POST",
        &uri,
        &jar,
        "_method=patch&message%5Bbody%5D=override",
    )
    .await;
    assert_eq!(response.status(), 303);
    let response = post_form(&pool, "POST", &uri, &jar, "message%5Bbody%5D=x").await;
    assert_eq!(response.status(), 404, "plain POSTs 404");
    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn update_json_asks_missing_template() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Juno Json").await;
    let room_id = seed_room(&pool, uid, Some("Formats"), "Rooms::Open").await;
    let mid = seed_message(&pool, room_id, uid, "fmt", "format me").await;
    let jar = login(&pool, &email).await;
    let token = csrf_token();
    let request = http::Request::builder()
        .method("PATCH")
        .uri(format!("/rooms/{room_id}/messages/{mid}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .body(Body::from(format!(
            "authenticity_token={token}&message%5Bbody%5D=json"
        )))
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 500);
    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn destroy_lands_back_on_the_room() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Dee Stroy").await;
    let room_id = seed_room(&pool, uid, Some("Deletions"), "Rooms::Open").await;
    let m1 = seed_message(&pool, room_id, uid, "del1", "doomed one").await;
    let m2 = seed_message(&pool, room_id, uid, "del2", "doomed two").await;
    let jar = login(&pool, &email).await;
    let uri = format!("/rooms/{room_id}/messages/{m1}");
    let response = post_form(&pool, "DELETE", &uri, &jar, "").await;
    assert_eq!(response.status(), 303);
    assert_eq!(
        response.headers().get("location").unwrap(),
        &format!("/rooms/{room_id}")
    );
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id = $1")
        .bind(m1)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
    let uri = format!("/rooms/{room_id}/messages/{m2}");
    let response = post_form(&pool, "POST", &uri, &jar, "_method=delete").await;
    assert_eq!(response.status(), 303);
    cleanup_message(&pool, m2).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn boosts_render_on_the_message() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Bea Boost").await;
    let (oid, _) = seed_user(&pool, "Otto Other").await;
    let room_id = seed_room(&pool, uid, Some("Boosted"), "Rooms::Open").await;
    seed_member(&pool, room_id, oid).await;
    let mid = seed_message(&pool, room_id, uid, "bst", "boost me").await;
    sqlx::query(
        "INSERT INTO boosts (created_at, updated_at, booster_id, content, message_id) \
         VALUES (now(), now(), $1, '👍', $2)",
    )
    .bind(oid)
    .bind(mid)
    .execute(&pool)
    .await
    .unwrap();
    let jar = login(&pool, &email).await;
    let (status, html) = get_html(&pool, &format!("/rooms/{room_id}"), &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("boost-item"), "chip: {html}");
    assert!(html.contains("Otto Other boosted"), "label: {html}");
    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, oid).await;
}

#[tokio::test]
async fn reply_to_prefills_composer_with_quote() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Reply Rita").await;
    let room_id = seed_room(&pool, uid, Some("Reply Room"), "Rooms::Open").await;
    let mid = seed_message(&pool, room_id, uid, "reply-src", "Hello reply me").await;
    let jar = login(&pool, &email).await;

    // Plain room page: the composer starts empty.
    let (status, html) = get_html(&pool, &format!("/rooms/{room_id}"), &jar).await;
    assert_eq!(status, 200);
    assert!(!html.contains("&gt; Hello reply me"), "no quote: {html}");

    // `?reply_to=` pre-fills the composer with a quote block.
    let (status, html) = get_html(&pool, &format!("/rooms/{room_id}?reply_to={mid}"), &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("&gt; Hello reply me"), "quote: {html}");

    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn composer_uploads_attachments_as_multipart_without_js() {
    fn init_store() {
        use topcamp_storage::{S3BlobStore, S3Config};
        let _ = topcamp_web::first_run::init_store(
            S3BlobStore::new(&S3Config::dev()).expect("test store builds"),
        );
    }

    init_store();
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Attach Andy").await;
    let room_id = seed_room(&pool, uid, Some("Attach Room"), "Rooms::Open").await;
    let jar = login(&pool, &email).await;

    // The composer wires the file input into the multipart form, so a
    // plain browser post carries the attachment.
    let (status, html) = get_html(&pool, &format!("/rooms/{room_id}"), &jar).await;
    assert_eq!(status, 200);
    assert!(
        html.contains("name=\"message[attachment]\""),
        "named file input: {html}"
    );
    assert!(
        html.contains("enctype=\"multipart/form-data\""),
        "multipart composer form: {html}"
    );

    // The same post the browser would send stores the attachment.
    let boundary = "----attachboundary7";
    let token = csrf_token();
    let mut body = Vec::new();
    for (name, value) in [
        ("authenticity_token", token.as_str()),
        ("message[body]", "see attached"),
    ] {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"message[attachment]\"; filename=\"note.txt\"\r\nContent-Type: text/plain\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(b"hello attachment");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("/rooms/{room_id}/messages"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .body(Body::from(body))
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status().as_u16(), 303);

    let (status, html) = get_html(&pool, &format!("/rooms/{room_id}"), &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("note.txt"), "attachment renders: {html}");

    let mid: i64 =
        sqlx::query_scalar("SELECT id FROM messages WHERE room_id = $1 ORDER BY id DESC LIMIT 1")
            .bind(room_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    cleanup_message(&pool, mid).await;
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn show_loads_older_messages_without_js() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Holly History").await;
    let room_id = seed_room(&pool, uid, Some("Histories"), "Rooms::Open").await;
    // Overflow one page so the last page starts mid-history. No
    // backdate: the room stays paged, so the invitation (and the
    // original-room race with its test) never comes into play.
    let mut ids = Vec::new();
    for n in 0..41 {
        ids.push(seed_message(&pool, room_id, uid, "hist", &format!("history {n}")).await);
    }
    // Strictly increasing timestamps so `page_before` sees every row.
    sqlx::query(
        "UPDATE messages SET created_at = '2024-01-01'::timestamptz + (id - $1) * interval '1 second' WHERE room_id = $2",
    )
    .bind(ids[0])
    .bind(room_id)
    .execute(&pool)
    .await
    .unwrap();
    let jar = login(&pool, &email).await;
    let (status, html) = get_html(&pool, &format!("/rooms/{room_id}"), &jar).await;
    assert_eq!(status, 200);
    assert!(
        !html.contains("history 0"),
        "oldest falls off the last page"
    );
    assert!(html.contains("history 40"), "newest stays visible");
    // The last page holds history 1..40, so the cursor names ids[1].
    let cursor = format!("/rooms/{room_id}?before={}", ids[1]);
    assert!(html.contains("Load older messages"), "history link: {html}");
    assert!(html.contains(&cursor), "cursor at oldest visible: {html}");
    assert!(!html.contains("Copy join link"), "no dead button: {html}");
    // Following the cursor pages around the older message.
    let (status, html) = get_html(&pool, &cursor, &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("history 0"), "older history reachable");
    assert!(
        !html.contains("Load older messages"),
        "start of history links no further: {html}"
    );
    // An unknown cursor falls back to the last page (no 404).
    let (status, html) = get_html(&pool, &format!("/rooms/{room_id}?before=999999999"), &jar).await;
    assert_eq!(status, 200);
    assert!(html.contains("history 40"), "falls back to last page");
    for mid in ids {
        cleanup_message(&pool, mid).await;
    }
    cleanup_room(&pool, room_id).await;
    cleanup_user(&pool, uid).await;
}
