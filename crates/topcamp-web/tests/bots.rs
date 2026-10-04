//! Bot JSON API + chat-bot admin pages against PostgreSQL + the
//! real router.
//!
//! Requires `DATABASE_URL` (default: the isolated `topcamp_test_bots`
//! database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_bots;"` then apply
//! `migrations/*.sql` in order. Tests run in parallel with unique
//! emails; nothing here empties shared tables.

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::{
    BotRow, NewMessage, RoomRepository, UserRepository, WebhookRepository,
};
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_bots".to_string()
    });
    PgPool::connect(&url).await.expect("connect DATABASE_URL")
}

fn app(pool: PgPool) -> Router {
    router(AppState::new(PgDb::new(pool), Cable::new()))
}

fn csrf_token() -> String {
    "cd".repeat(32)
}

fn init_store() {
    use topcamp_storage::{S3BlobStore, S3Config};
    let _ = topcamp_web::first_run::init_store(
        S3BlobStore::new(&S3Config::dev()).expect("test store builds"),
    );
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

async fn seed_admin(pool: &PgPool, name: &str) -> (i64, String) {
    let (id, email) = seed_user(pool, name).await;
    let db = PgDb::new(pool.clone());
    UserRepository::set_role(&db, id, 1)
        .await
        .expect("promote admin");
    (id, email)
}

async fn seed_bot(pool: &PgPool, name: &str) -> BotRow {
    let db = PgDb::new(pool.clone());
    UserRepository::create_bot(&db, name)
        .await
        .expect("seed bot")
}

fn bot_key(bot: &BotRow) -> String {
    bot.bot_key().expect("bots mint tokens")
}

async fn seed_room(pool: &PgPool, uid: i64, name: Option<&str>, kind: &str) -> i64 {
    let db = PgDb::new(pool.clone());
    let room = RoomRepository::create(&db, uid, name, kind)
        .await
        .expect("seed room");
    seed_member(pool, room.id, uid).await;
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
         VALUES (now(), now(), 'bots-test-join', 'Topcamp') ON CONFLICT DO NOTHING",
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

/// Anonymous bot-API request (Host set so URLs come out absolute).
fn bot_request(method: &str, uri: &str, content_type: &str, body: Vec<u8>) -> http::Request<Body> {
    let mut builder = http::Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "topcamp.test");
    if !content_type.is_empty() {
        builder = builder.header("content-type", content_type);
    }
    builder.body(Body::from(body)).expect("request builds")
}

async fn bot_text(pool: &PgPool, method: &str, uri: &str, body: &str) -> (u16, String) {
    let request = bot_request(method, uri, "text/plain", body.as_bytes().to_vec());
    let response = app(pool.clone()).handle(request).await;
    (response.status().as_u16(), body_text(response).await)
}

async fn bot_response(
    pool: &PgPool,
    method: &str,
    uri: &str,
    body: &str,
) -> topcoat::router::response::Response {
    let request = bot_request(method, uri, "text/plain", body.as_bytes().to_vec());
    app(pool.clone()).handle(request).await
}

fn multipart_body(boundary: &str, filename: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"attachment\"; filename=\"{filename}\"\r\nContent-Type: image/png\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn tiny_png() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xFF, 0xFF, 0x3F, 0x00, 0x05, 0xFE, 0x02, 0xFE, 0xDC, 0xCC, 0x59, 0xE7, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
}

async fn get_html(pool: &PgPool, uri: &str, jar: &str) -> (u16, String) {
    let request = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", jar)
        .header("host", "topcamp.test")
        .header("accept", "text/html,application/xhtml+xml")
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

// --- bot message API ---------------------------------------------------------

#[tokio::test]
async fn the_bot_api() {
    let pool = pool().await;
    let (_member_id, _member_email) = seed_user(&pool, "Bot Api Member").await;
    let bot = seed_bot(&pool, "Bot Api Bender").await;
    let key = bot_key(&bot);
    let room = seed_room(&pool, bot.id, Some("Bot Api Room"), "Rooms::Open").await;
    let base = format!("/rooms/{room}/{key}/messages");

    let index = bot_response(&pool, "GET", &base, "").await;
    assert_eq!(index.status().as_u16(), 200);
    assert_eq!(
        header(&index, "content-type"),
        "application/json; charset=utf-8"
    );
    assert_eq!(header(&index, "x-total-count"), "0");
    assert_eq!(body_text(index).await, "[]");

    let created = bot_response(&pool, "POST", &base, "Beep boop").await;
    assert_eq!(created.status().as_u16(), 201);
    let location = header(&created, "location");
    assert!(
        location.starts_with("http://topcamp.test/messages/"),
        "{location}"
    );
    let message_id: i64 = location.rsplit('/').next().unwrap().parse().unwrap();

    let (status, text) = bot_text(&pool, "GET", &base, "").await;
    assert_eq!(status, 200);
    let page: serde_json::Value = serde_json::from_str(&text).expect("index parses");
    assert_eq!(page.as_array().unwrap().len(), 1);
    // Jbuilder's key order survives serialization (checked on the raw
    // text since `Value` sorts keys).
    let positions: Vec<usize> = ["id", "created_at", "body", "creator", "room", "url"]
        .iter()
        .map(|key| text.find(&format!("\"{key}\":")).expect("key present"))
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "key order {positions:?}"
    );
    assert_eq!(page[0]["id"], message_id);
    assert_eq!(page[0]["body"]["plain_text"], "Beep boop");
    assert!(
        page[0]["body"]["html"]
            .as_str()
            .unwrap()
            .contains("Beep boop")
    );
    assert_eq!(page[0]["creator"]["id"], bot.id);
    assert_eq!(page[0]["creator"]["role"], "bot");
    assert!(
        page[0]["creator"]["avatar_url"]
            .as_str()
            .unwrap()
            .starts_with("http://topcamp.test/users/"),
        "{}",
        page[0]["creator"]["avatar_url"]
    );
    assert_eq!(page[0]["room"]["id"], room);
    assert_eq!(
        page[0]["url"],
        format!("http://topcamp.test/rooms/{room}/messages/{message_id}")
    );

    let updated = bot_response(&pool, "PUT", &format!("{base}/{message_id}"), "Beep edited").await;
    assert_eq!(updated.status().as_u16(), 200);
    let shown: serde_json::Value = serde_json::from_str(&body_text(updated).await).unwrap();
    assert_eq!(shown["body"]["plain_text"], "Beep edited");

    let boost = bot_response(&pool, "POST", &format!("{base}/{message_id}/boosts"), "🤖").await;
    assert_eq!(boost.status().as_u16(), 201);
    let boosted: serde_json::Value = serde_json::from_str(&body_text(boost).await).unwrap();
    assert_eq!(boosted["content"], "🤖");
    assert_eq!(boosted["booster"]["id"], bot.id);
    assert_eq!(boosted["message"]["id"], message_id);
    let boost_id = boosted["id"].as_i64().unwrap();
    let removed = bot_response(
        &pool,
        "DELETE",
        &format!("{base}/{message_id}/boosts/{boost_id}"),
        "",
    )
    .await;
    assert_eq!(removed.status().as_u16(), 204);
    let missing = bot_response(
        &pool,
        "DELETE",
        &format!("{base}/{message_id}/boosts/{boost_id}"),
        "",
    )
    .await;
    assert_eq!(missing.status().as_u16(), 404);

    let destroyed = bot_response(&pool, "DELETE", &format!("{base}/{message_id}"), "").await;
    assert_eq!(destroyed.status().as_u16(), 204);
}

#[tokio::test]
async fn bad_bot_keys_redirect_to_sign_in() {
    let pool = pool().await;
    let bot = seed_bot(&pool, "Bad Key Bender").await;
    let room = seed_room(&pool, bot.id, Some("Bad Key Room"), "Rooms::Open").await;

    for uri in [
        format!("/rooms/{room}/1-nope/messages"),
        format!("/rooms/{room}/{}/messages", bot.id),
    ] {
        let response = bot_response(&pool, "GET", &uri, "").await;
        assert_eq!(response.status().as_u16(), 303, "{uri}");
        assert_eq!(header(&response, "location"), "/session/new");
    }
    // The keyless JSON list is the app's own API route (401 unsigned).
    let response = bot_response(&pool, "GET", &format!("/rooms/{room}/messages"), "").await;
    assert_eq!(response.status().as_u16(), 401);
}

#[tokio::test]
async fn bot_keys_do_not_open_the_rest_of_the_app() {
    let pool = pool().await;
    let bot = seed_bot(&pool, "Denied Bender").await;
    let key = bot_key(&bot);
    let room = seed_room(&pool, bot.id, Some("Denied Room"), "Rooms::Open").await;

    for uri in [
        format!("/rooms/{room}/messages?bot_key={key}"),
        format!("/rooms/{room}?bot_key={key}"),
        format!("/account/bots?bot_key={key}"),
        format!("/account/edit?bot_key={key}"),
    ] {
        let request = http::Request::builder()
            .method("GET")
            .uri(&uri)
            .header("accept", "text/html,application/xhtml+xml")
            .body(Body::empty())
            .expect("request builds");
        let response = app(pool.clone()).handle(request).await;
        assert_eq!(response.status().as_u16(), 403, "{uri}");
    }
    // A signed-in session still wins over a valid bot key.
    let (member_id, member_email) = seed_user(&pool, "Denied Member").await;
    seed_member(&pool, room, member_id).await;
    let jar = login(&pool, &member_email).await;
    let (status, _) = get_html(
        &pool,
        &format!("/rooms/{room}/messages?bot_key={key}"),
        &jar,
    )
    .await;
    assert_ne!(status, 403);
}

#[tokio::test]
async fn bot_api_rooms_and_messages_scope_to_membership() {
    let pool = pool().await;
    let bot = seed_bot(&pool, "Scoped Bender").await;
    let key = bot_key(&bot);
    let room = seed_room(&pool, bot.id, Some("Scoped Room"), "Rooms::Open").await;
    let (other_id, _) = seed_user(&pool, "Scoped Other").await;
    let foreign = seed_room(&pool, other_id, Some("Foreign Room"), "Rooms::Open").await;
    let foreign_message = seed_message(&pool, foreign, other_id, "scoped", "elsewhere").await;

    let base = format!("/rooms/{room}/{key}/messages");
    let foreign_base = format!("/rooms/{foreign}/{key}/messages");
    assert_eq!(bot_text(&pool, "GET", &foreign_base, "").await.0, 404);
    assert_eq!(
        bot_text(&pool, "GET", &format!("/rooms/nope/{key}/messages"), "")
            .await
            .0,
        404
    );
    // Somebody else's message, through our own room: 404 (not ours).
    assert_eq!(
        bot_text(&pool, "DELETE", &format!("{base}/{foreign_message}"), "")
            .await
            .0,
        404
    );
    // A stranger's message seeded into our room: 403 to change.
    let mine = seed_message(&pool, room, other_id, "scoped", "theirs").await;
    assert_eq!(
        bot_text(&pool, "DELETE", &format!("{base}/{mine}"), "")
            .await
            .0,
        403
    );
    assert_eq!(
        bot_text(&pool, "PUT", &format!("{base}/{mine}"), "hijack")
            .await
            .0,
        403
    );
}

#[tokio::test]
async fn bot_writes_validate_presence() {
    let pool = pool().await;
    let bot = seed_bot(&pool, "Validating Bender").await;
    let key = bot_key(&bot);
    let room = seed_room(&pool, bot.id, Some("Validating Room"), "Rooms::Open").await;
    let base = format!("/rooms/{room}/{key}/messages");

    assert_eq!(bot_text(&pool, "POST", &base, "  \n").await.0, 422);
    let created = bot_response(&pool, "POST", &base, "boost me").await;
    assert_eq!(created.status().as_u16(), 201);
    let message_id: i64 = header(&created, "location")
        .rsplit('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        bot_text(&pool, "POST", &format!("{base}/{message_id}/boosts"), " ")
            .await
            .0,
        422
    );
    // Long contents store fine (upstream's SQLite ignores varchar(16)).
    let long = "x".repeat(64);
    let response = bot_response(&pool, "POST", &format!("{base}/{message_id}/boosts"), &long).await;
    assert_eq!(response.status().as_u16(), 201);
}

#[tokio::test]
async fn bot_api_paginates_with_link_headers() {
    let pool = pool().await;
    let bot = seed_bot(&pool, "Paging Bender").await;
    let key = bot_key(&bot);
    let room = seed_room(&pool, bot.id, Some("Paging Room"), "Rooms::Open").await;
    for n in 0..41 {
        seed_message(&pool, room, bot.id, "paging", &format!("message {n}")).await;
    }
    let base = format!("/rooms/{room}/{key}/messages");

    let index = bot_response(&pool, "GET", &base, "").await;
    assert_eq!(header(&index, "x-total-count"), "41");
    let page: serde_json::Value = serde_json::from_str(&body_text(index).await).unwrap();
    assert_eq!(page.as_array().unwrap().len(), 40);
    let first_id = page[0]["id"].as_i64().unwrap();

    let index = bot_response(&pool, "GET", &base, "").await;
    assert_eq!(
        header(&index, "link"),
        format!("<http://topcamp.test{base}?before={first_id}>; rel=\"next\"")
    );

    // The previous page holds the oldest message, with no way back.
    let older = bot_response(&pool, "GET", &format!("{base}?before={first_id}"), "").await;
    assert_eq!(header(&older, "x-total-count"), "41");
    let link = header(&older, "link");
    let page: serde_json::Value = serde_json::from_str(&body_text(older).await).unwrap();
    assert_eq!(page.as_array().unwrap().len(), 1);
    assert_eq!(link, "");
    let oldest_id = page[0]["id"].as_i64().unwrap();

    // Forward from the oldest returns the forty, then stops.
    let next = bot_response(&pool, "GET", &format!("{base}?after={oldest_id}"), "").await;
    let page: serde_json::Value = serde_json::from_str(&body_text(next).await).unwrap();
    assert_eq!(page.as_array().unwrap().len(), 40);

    // Uncastable and foreign anchors 404.
    assert_eq!(
        bot_text(&pool, "GET", &format!("{base}?before=nope"), "")
            .await
            .0,
        404
    );
    assert_eq!(
        bot_text(&pool, "GET", &format!("{base}?after=999999999"), "")
            .await
            .0,
        404
    );
}

#[tokio::test]
async fn bot_api_accepts_session_users_too() {
    let pool = pool().await;
    let (member_id, member_email) = seed_user(&pool, "Session Caller").await;
    let bot = seed_bot(&pool, "Session Bender").await;
    let key = bot_key(&bot);
    let room = seed_room(&pool, member_id, Some("Session Room"), "Rooms::Open").await;
    seed_member(&pool, room, bot.id).await;
    seed_message(&pool, room, member_id, "session", "hello").await;
    let jar = login(&pool, &member_email).await;

    let request = http::Request::builder()
        .method("GET")
        .uri(format!("/rooms/{room}/{key}/messages"))
        .header("cookie", &jar)
        .header("host", "topcamp.test")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status().as_u16(), 200);
    let page: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(page.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn bot_update_answers_html_with_a_redirect() {
    let pool = pool().await;
    let bot = seed_bot(&pool, "Redirect Bender").await;
    let key = bot_key(&bot);
    let room = seed_room(&pool, bot.id, Some("Redirect Room"), "Rooms::Open").await;
    let message = seed_message(&pool, room, bot.id, "redirect", "before").await;

    let request = http::Request::builder()
        .method("PUT")
        .uri(format!("/rooms/{room}/{key}/messages/{message}"))
        .header("host", "topcamp.test")
        .header("accept", "text/html,application/xhtml+xml")
        .header("content-type", "text/plain")
        .body(Body::from("after"))
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status().as_u16(), 303);
    assert_eq!(
        header(&response, "location"),
        format!("/rooms/{room}/messages/{message}")
    );
}

#[tokio::test]
async fn bot_attachments_upload_replace_and_remove() {
    init_store();
    let pool = pool().await;
    let bot = seed_bot(&pool, "Attachment Bender").await;
    let key = bot_key(&bot);
    let room = seed_room(&pool, bot.id, Some("Attachment Room"), "Rooms::Open").await;
    let base = format!("/rooms/{room}/{key}/messages");
    let boundary = "----botstest";

    let upload = bot_request(
        "POST",
        &base,
        &format!("multipart/form-data; boundary={boundary}"),
        multipart_body(boundary, "red.png", &tiny_png()),
    );
    let response = app(pool.clone()).handle(upload).await;
    assert_eq!(response.status().as_u16(), 201);
    let message_id: i64 = header(&response, "location")
        .rsplit('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let db = PgDb::new(pool.clone());
    let blob = topcamp_db::repositories::AttachmentRepository::blob_for_record(
        &db,
        "Message",
        message_id,
        "attachment",
    )
    .await
    .expect("attachment lookup")
    .expect("uploaded blob attached");
    assert_eq!(blob.filename, "red.png");

    // A second upload replaces the first (the body stays empty).
    let replace = bot_request(
        "PUT",
        &format!("{base}/{message_id}"),
        &format!("multipart/form-data; boundary={boundary}"),
        multipart_body(boundary, "again.png", &tiny_png()),
    );
    let response = app(pool.clone()).handle(replace).await;
    assert_eq!(response.status().as_u16(), 200);
    let blob = topcamp_db::repositories::AttachmentRepository::blob_for_record(
        &db,
        "Message",
        message_id,
        "attachment",
    )
    .await
    .expect("attachment lookup")
    .expect("replaced blob attached");
    assert_eq!(blob.filename, "again.png");

    // `attachment=` removes it (then the process/purge outbox rows exist).
    let remove = bot_request(
        "PUT",
        &format!("{base}/{message_id}"),
        "application/x-www-form-urlencoded",
        b"attachment=".to_vec(),
    );
    assert_eq!(
        app(pool.clone()).handle(remove).await.status().as_u16(),
        200
    );
    assert!(
        topcamp_db::repositories::AttachmentRepository::blob_for_record(
            &db,
            "Message",
            message_id,
            "attachment"
        )
        .await
        .expect("attachment lookup")
        .is_none()
    );

    // A non-blank, non-file attachment is unbuildable (500).
    let bogus = bot_request(
        "PUT",
        &format!("{base}/{message_id}"),
        "application/x-www-form-urlencoded",
        b"attachment=nope".to_vec(),
    );
    assert_eq!(app(pool.clone()).handle(bogus).await.status().as_u16(), 500);
}

#[tokio::test]
async fn direct_room_posts_enqueue_bot_webhook_deliveries() {
    let pool = pool().await;
    seed_account(&pool).await;
    let bot = seed_bot(&pool, "Webhook Bender").await;
    let (member_id, member_email) = seed_user(&pool, "Webhook Member").await;
    let db = PgDb::new(pool.clone());
    WebhookRepository::create_webhook(&db, bot.id, Some("https://example.com/hook"))
        .await
        .expect("seed webhook");
    let room = seed_room(&pool, bot.id, None, "Rooms::Direct").await;
    seed_member(&pool, room, member_id).await;

    let jar = login(&pool, &member_email).await;
    let response = post_form(
        &pool,
        "POST",
        &format!("/rooms/{room}/messages"),
        &jar,
        "message%5Bbody%5D=hello+bot&message%5Bclient_message_id%5D=",
    )
    .await;
    assert_eq!(response.status().as_u16(), 303);

    let row: Option<(i64, serde_json::Value)> = sqlx::query_as(
        "SELECT message_id, payload FROM (SELECT (payload->>'message_id')::bigint AS message_id, payload \
         FROM outbox WHERE topic = 'deliver_webhook') deliveries \
         WHERE (payload->>'bot_id')::bigint = $1 ORDER BY message_id DESC LIMIT 1",
    )
    .bind(bot.id)
    .fetch_optional(&pool)
    .await
    .expect("outbox lookup");
    let (message_id, payload) = row.expect("a delivery enqueued");
    assert_eq!(payload["bot_id"], bot.id);
    assert_eq!(payload["message_id"], message_id);

    // The bot's own posts never deliver back to it.
    let key = bot_key(&bot);
    let created = bot_response(
        &pool,
        "POST",
        &format!("/rooms/{room}/{key}/messages"),
        "beep",
    )
    .await;
    assert_eq!(created.status().as_u16(), 201);
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM outbox WHERE topic = 'deliver_webhook' AND (payload->>'bot_id')::bigint = $1",
    )
    .bind(bot.id)
    .fetch_one(&pool)
    .await
    .expect("outbox count");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn mention_narrowing_skips_unmentioned_and_unwebhooked_bots() {
    let pool = pool().await;
    let db = PgDb::new(pool.clone());
    let mentioned = seed_bot(&pool, "Mentioned Bender").await;
    let quiet = seed_bot(&pool, "Quiet Bender").await;
    let unhooked = seed_bot(&pool, "Unhooked Bender").await;
    WebhookRepository::create_webhook(&db, mentioned.id, Some("https://example.com/one"))
        .await
        .expect("seed webhook");
    WebhookRepository::create_webhook(&db, quiet.id, Some("https://example.com/two"))
        .await
        .expect("seed webhook");
    let room = seed_room(&pool, mentioned.id, Some("Mention Room"), "Rooms::Open").await;
    seed_member(&pool, room, quiet.id).await;
    seed_member(&pool, room, unhooked.id).await;
    let (member_id, _) = seed_user(&pool, "Mention Member").await;
    seed_member(&pool, room, member_id).await;
    let message = seed_message(&pool, room, member_id, "mention", "hi").await;

    // Only the mentioned, webhooked bot queues (the creator would too).
    let queued = WebhookRepository::enqueue_bot_deliveries(
        &db,
        room,
        false,
        &[mentioned.id, unhooked.id, member_id],
        member_id,
        message,
    )
    .await
    .expect("enqueue deliveries");
    assert_eq!(queued, 1);

    // Direct rooms deliver to every webhooked bot but the creator.
    let queued =
        WebhookRepository::enqueue_bot_deliveries(&db, room, true, &[], member_id, message)
            .await
            .expect("enqueue deliveries");
    assert_eq!(queued, 2);
}

// --- bot admin pages ---------------------------------------------------------

#[tokio::test]
async fn bots_pages_require_an_admin() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (_member_id, member_email) = seed_user(&pool, "Bots Member").await;
    let (_admin_id, admin_email) = seed_admin(&pool, "Bots Admin").await;

    for uri in ["/account/bots", "/account/bots/new", "/account/bots/1/edit"] {
        let request = http::Request::builder()
            .method("GET")
            .uri(uri)
            .header("accept", "text/html,application/xhtml+xml")
            .body(Body::empty())
            .expect("request builds");
        let response = app(pool.clone()).handle(request).await;
        assert_eq!(response.status().as_u16(), 303, "{uri}");
    }
    let member_jar = login(&pool, &member_email).await;
    for uri in ["/account/bots", "/account/bots/new", "/account/bots/1/edit"] {
        let (status, _) = get_html(&pool, uri, &member_jar).await;
        assert_eq!(status, 403, "{uri}");
    }
    let admin_jar = login(&pool, &admin_email).await;
    let (status, text) = get_html(&pool, "/account/bots", &admin_jar).await;
    assert_eq!(status, 200);
    assert!(text.contains("Chat bots"), "index renders");
    let (status, _) = get_html(&pool, "/account/bots/new", &admin_jar).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn bots_crud_round_trip() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (member_id, _) = seed_user(&pool, "Crud Member").await;
    let (_admin_id, admin_email) = seed_admin(&pool, "Crud Admin").await;
    let room = seed_room(&pool, member_id, Some("Crud Room"), "Rooms::Open").await;
    let jar = login(&pool, &admin_email).await;
    let db = PgDb::new(pool.clone());

    // Create with a webhook: the bot joins the open rooms.
    let created = post_form(
        &pool,
        "POST",
        "/account/bots",
        &jar,
        "user%5Bname%5D=Crud+Bender&user%5Bwebhook_url%5D=https%3A%2F%2Fexample.com%2Fhook",
    )
    .await;
    assert_eq!(created.status().as_u16(), 303);
    assert_eq!(header(&created, "location"), "/account/bots");
    let bots = UserRepository::active_bots_ordered(&db)
        .await
        .expect("list bots");
    let bot = bots
        .iter()
        .find(|bot| bot.name == "Crud Bender")
        .expect("bot row");
    let key = bot_key(bot);
    let webhook = WebhookRepository::find_by_user(&db, bot.id)
        .await
        .expect("webhook lookup")
        .expect("webhook row");
    assert_eq!(webhook.url.as_deref(), Some("https://example.com/hook"));
    let membership: Option<i64> =
        sqlx::query_scalar("SELECT id FROM memberships WHERE room_id = $1 AND user_id = $2")
            .bind(room)
            .bind(bot.id)
            .fetch_optional(&pool)
            .await
            .expect("membership lookup");
    assert!(membership.is_some(), "bot joins open rooms");

    // The index shows the bot with per-room curl lines.
    let (status, text) = get_html(&pool, "/account/bots", &jar).await;
    assert_eq!(status, 200);
    assert!(text.contains("Crud Bender"), "bot listed");
    assert!(text.contains("Crud Room"), "room fieldset");
    assert!(
        text.contains(&format!(
            "curl -d &#x27;Hello!&#x27; http://topcamp.test/rooms/{room}/{key}/messages"
        )) || text.contains(&format!(
            "curl -d 'Hello!' http://topcamp.test/rooms/{room}/{key}/messages"
        )),
        "curl text line"
    );
    // No-JS experiment: the curl lines render as plain text; the
    // Stimulus copy hooks are gone.
    assert!(!text.contains("copy-to-clipboard"), "{text}");

    // Edit prefills; update renames and clears the webhook.
    let (status, text) = get_html(&pool, &format!("/account/bots/{}/edit", bot.id), &jar).await;
    assert_eq!(status, 200);
    assert!(text.contains("Crud Bender"), "name prefilled");
    assert!(
        text.contains("https://example.com/hook"),
        "webhook prefilled"
    );
    let updated = post_form(
        &pool,
        "POST",
        &format!("/account/bots/{}", bot.id),
        &jar,
        "_method=patch&user%5Bname%5D=Renamed+Bender&user%5Bwebhook_url%5D=",
    )
    .await;
    assert_eq!(updated.status().as_u16(), 303);
    let renamed = UserRepository::find_active_bot(&db, bot.id)
        .await
        .expect("find bot")
        .expect("still active");
    assert_eq!(renamed.name, "Renamed Bender");
    assert!(
        WebhookRepository::find_by_user(&db, bot.id)
            .await
            .expect("webhook lookup")
            .is_none(),
        "blank URL drops the webhook"
    );

    // Resetting the key rotates the token; the old key stops working.
    let before = bot_key(&renamed);
    let reset = post_form(
        &pool,
        "POST",
        &format!("/account/bots/{}/key", bot.id),
        &jar,
        "_method=put",
    )
    .await;
    assert_eq!(reset.status().as_u16(), 303);
    let rotated = UserRepository::find_active_bot(&db, bot.id)
        .await
        .expect("find bot")
        .expect("still active");
    let after = bot_key(&rotated);
    assert_ne!(before, after);
    assert_eq!(
        bot_text(
            &pool,
            "GET",
            &format!("/rooms/{room}/{before}/messages"),
            ""
        )
        .await
        .0,
        303
    );
    assert_eq!(
        bot_text(&pool, "GET", &format!("/rooms/{room}/{after}/messages"), "")
            .await
            .0,
        200
    );

    // Destroy deactivates (the bot leaves the index + API).
    let destroyed = post_form(
        &pool,
        "POST",
        &format!("/account/bots/{}", bot.id),
        &jar,
        "_method=delete",
    )
    .await;
    assert_eq!(destroyed.status().as_u16(), 303);
    assert!(
        UserRepository::find_active_bot(&db, bot.id)
            .await
            .expect("find bot")
            .is_none()
    );
    assert_eq!(
        bot_text(&pool, "GET", &format!("/rooms/{room}/{after}/messages"), "")
            .await
            .0,
        303
    );
}

#[tokio::test]
async fn bots_writes_validate() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (_admin_id, admin_email) = seed_admin(&pool, "Valid Admin").await;
    let jar = login(&pool, &admin_email).await;

    // Missing `user` entirely: 400. Missing name: 500 (NOT NULL).
    let response = post_form(&pool, "POST", "/account/bots", &jar, "unrelated=1").await;
    assert_eq!(response.status().as_u16(), 400);
    let response = post_form(
        &pool,
        "POST",
        "/account/bots",
        &jar,
        "user%5Bwebhook_url%5D=https%3A%2F%2Fexample.com%2Fhook",
    )
    .await;
    assert_eq!(response.status().as_u16(), 500);
    // Forged CSRF: 403.
    let request = http::Request::builder()
        .method("POST")
        .uri("/account/bots")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{jar}; csrf_token=nope"))
        .body(Body::from("authenticity_token=nope&user%5Bname%5D=Nope"))
        .expect("request builds");
    assert_eq!(
        app(pool.clone()).handle(request).await.status().as_u16(),
        403
    );
    // Unknown bots + humans 404.
    let (member_id, _) = seed_user(&pool, "Not A Bot").await;
    for id in [
        member_id.to_string(),
        "999999999".to_string(),
        "nope".to_string(),
    ] {
        let (status, _) = get_html(&pool, &format!("/account/bots/{id}/edit"), &jar).await;
        assert_eq!(status, 404, "{id}");
    }
}

#[tokio::test]
async fn deactivating_bots_keeps_null_emails() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (_admin_id, admin_email) = seed_admin(&pool, "Nullmail Admin").await;
    let jar = login(&pool, &admin_email).await;
    let db = PgDb::new(pool.clone());

    // Two bots destroyed back to back: NULL addresses never collide.
    for name in ["Nullmail One", "Nullmail Two"] {
        let created = post_form(
            &pool,
            "POST",
            "/account/bots",
            &jar,
            &format!("user%5Bname%5D={}", name.replace(' ', "+")),
        )
        .await;
        assert_eq!(created.status().as_u16(), 303);
        let bots = UserRepository::active_bots_ordered(&db)
            .await
            .expect("list bots");
        let bot = bots.iter().find(|bot| bot.name == name).expect("bot row");
        let destroyed = post_form(
            &pool,
            "POST",
            &format!("/account/bots/{}", bot.id),
            &jar,
            "_method=delete",
        )
        .await;
        assert_eq!(destroyed.status().as_u16(), 303, "{name}");
        let email: Option<String> =
            sqlx::query_scalar("SELECT email_address FROM users WHERE id = $1")
                .bind(bot.id)
                .fetch_one(&pool)
                .await
                .expect("email lookup");
        assert_eq!(email, None, "{name} keeps a NULL email");
    }
}

#[tokio::test]
async fn delete_confirm_dialog_opens_via_query_param() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (_admin_id, admin_email) = seed_admin(&pool, "Confirm Admin").await;
    let bot = seed_bot(&pool, "Confirm Bender").await;
    let jar = login(&pool, &admin_email).await;
    let edit = format!("/account/bots/{}/edit", bot.id);

    // Plain edit: both dialogs render closed.
    let (status, text) = get_html(&pool, &edit, &jar).await;
    assert_eq!(status, 200);
    assert!(
        text.contains("role=\"alertdialog\""),
        "dialogs render: {text}"
    );
    assert!(!text.contains("<dialog open"), "dialogs closed: {text}");

    // `?confirm=delete-bot` opens only the delete dialog.
    let (status, text) = get_html(&pool, &format!("{edit}?confirm=delete-bot"), &jar).await;
    assert_eq!(status, 200);
    assert!(text.contains("<dialog open"), "delete opens: {text}");
    assert!(
        text.contains("form=\"delete-bot\">Delete</button>"),
        "confirm submits the form: {text}"
    );
    assert!(
        text.contains(&format!("href=\"{edit}\">Cancel</a>")),
        "cancel links back: {text}"
    );

    // `?confirm=reset-bot-key` opens only the key dialog.
    let (status, text) = get_html(&pool, &format!("{edit}?confirm=reset-bot-key"), &jar).await;
    assert_eq!(status, 200);
    assert!(
        text.contains("form=\"reset-bot-key\">Generate</button>"),
        "key confirm submits its form: {text}"
    );
}

#[tokio::test]
async fn bot_curl_commands_are_selectable_without_copy_buttons() {
    let pool = pool().await;
    seed_account(&pool).await;
    let bot = seed_bot(&pool, "Nojs Curl Bender").await;
    let (_admin_id, admin_email) = seed_admin(&pool, "Nojs Curl Admin").await;
    seed_room(&pool, bot.id, Some("Nojs Curl Room"), "Rooms::Open").await;
    let jar = login(&pool, &admin_email).await;

    let (status, text) = get_html(&pool, "/account/bots", &jar).await;
    assert_eq!(status, 200);
    // No-JS: the dead copy buttons are gone; the curl commands stay
    // as readonly selectable inputs.
    assert!(text.contains("Nojs Curl Bender"), "{text}");
    assert!(!text.contains("Copy message command"), "{text}");
    assert!(!text.contains("Copy attachment command"), "{text}");
    assert!(
        text.contains("aria-label=\"curl command for posting messages\""),
        "{text}"
    );
    assert!(
        text.contains("aria-label=\"curl command for posting attachments\""),
        "{text}"
    );
    assert!(text.contains("readonly"), "{text}");
}
