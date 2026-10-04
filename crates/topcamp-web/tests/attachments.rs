//! Message attachment tests against PostgreSQL + the real router.
//!
//! Requires `DATABASE_URL` (default: the isolated
//! `topcamp_test_attachments` database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_attachments;"` then apply
//! `migrations/*.sql` in order (re-apply after migration changes).
//! Uploads also need the dev RustFS (`S3Config::dev()`).

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::{RoomRepository, UserRepository};
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_attachments".to_string()
    });
    PgPool::connect(&url).await.expect("connect DATABASE_URL")
}

fn app(pool: PgPool) -> Router {
    router(AppState::new(PgDb::new(pool), Cable::new()))
}

fn csrf_token() -> String {
    "ab".repeat(32)
}

fn init_store() {
    use topcamp_storage::{S3BlobStore, S3Config};
    let _ = topcamp_web::first_run::init_store(
        S3BlobStore::new(&S3Config::dev()).expect("test store builds"),
    );
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

fn unique(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock runs forward")
        .as_nanos();
    format!("{tag}-{}-{nanos}", std::process::id())
}

async fn seed_user(pool: &PgPool, tag: &str) -> (i64, String) {
    let email = format!("{}@example.com", unique(tag));
    let digest = bcrypt::hash("s3cret", 4).expect("cost-4 digest builds");
    let db = PgDb::new(pool.clone());
    let id = UserRepository::create(&db, tag, &email, Some(&digest))
        .await
        .expect("seed user")
        .id;
    (id, email)
}

async fn seed_room(pool: &PgPool, uid: i64) -> i64 {
    let db = PgDb::new(pool.clone());
    let room = RoomRepository::create(&db, uid, Some(&unique("room")), "Rooms::Open")
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
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|pair| pair.split(';').next())
        .collect::<Vec<_>>()
        .join("; ")
}

/// `FileUploader` shape: the raw file part plus
/// `message[client_message_id]`, CSRF in the `X-CSRF-Token` header.
async fn upload(
    router: &Router,
    room_id: i64,
    jar: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
    client_id: &str,
) -> topcoat::router::response::Response {
    let boundary = "----attachmenttest";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"message[attachment]\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(
        format!(
            "\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"message[client_message_id]\"\r\n\r\n{client_id}\r\n--{boundary}--\r\n"
        )
        .as_bytes(),
    );
    let token = csrf_token();
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("/rooms/{room_id}/messages"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .header("x-csrf-token", token)
        .body(Body::from(body))
        .expect("upload builds");
    router.handle(request).await
}

async fn message_id_for(pool: &PgPool, client_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT id FROM messages WHERE client_message_id = $1")
        .bind(client_id)
        .fetch_one(pool)
        .await
        .expect("message row exists")
}

async fn get(
    router: &Router,
    uri: &str,
    jar: &str,
    extra: Option<(&str, &str)>,
) -> topcoat::router::response::Response {
    let mut builder = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", jar);
    if let Some((name, value)) = extra {
        builder = builder.header(name, value);
    }
    router
        .handle(builder.body(Body::empty()).expect("request builds"))
        .await
}

async fn body_bytes(response: topcoat::router::response::Response) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec()
}

async fn body_text(response: topcoat::router::response::Response) -> String {
    String::from_utf8(body_bytes(response).await).expect("UTF-8")
}

fn header(response: &topcoat::router::response::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

async fn cleanup(pool: &PgPool, room_id: i64, uid: i64) {
    for message in sqlx::query_scalar::<_, i64>("SELECT id FROM messages WHERE room_id = $1")
        .bind(room_id)
        .fetch_all(pool)
        .await
        .unwrap()
    {
        sqlx::query("DELETE FROM boosts WHERE message_id = $1")
            .bind(message)
            .execute(pool)
            .await
            .unwrap();
        for blob in sqlx::query_scalar::<_, i64>(
            "DELETE FROM active_storage_attachments WHERE record_type = 'Message' AND record_id = $1 RETURNING blob_id",
        )
        .bind(message)
        .fetch_all(pool)
        .await
        .unwrap()
        {
            sqlx::query("DELETE FROM active_storage_variant_records WHERE blob_id = $1")
                .bind(blob)
                .execute(pool)
                .await
                .unwrap();
            sqlx::query("DELETE FROM active_storage_blobs WHERE id = $1")
                .bind(blob)
                .execute(pool)
                .await
                .unwrap();
        }
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(message)
            .execute(pool)
            .await
            .unwrap();
    }
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
async fn png_upload_links_from_the_message_page() {
    init_store();
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Attacher").await;
    let room_id = seed_room(&pool, uid).await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let png = tiny_png();
    let client_id = unique("client");
    let response = upload(
        &app,
        room_id,
        &jar,
        "dot.png",
        "image/png",
        &png,
        &client_id,
    )
    .await;
    assert_eq!(response.status(), 303);
    let id = message_id_for(&pool, &client_id).await;

    let page =
        body_text(get(&app, &format!("/rooms/{room_id}/messages/{id}"), &jar, None).await).await;
    // No-JS experiment: the preview is a plain link, no lightbox hooks.
    assert!(page.contains("<a class=\"flex\" href="), "{page}");
    assert!(!page.contains("data-lightbox-"), "{page}");
    assert!(page.contains("width=\"1\" height=\"1\""), "{page}");
    assert!(
        page.contains(&format!("/rooms/{room_id}/messages/{id}/attachment")),
        "{page}"
    );
    cleanup(&pool, room_id, uid).await;
}

#[tokio::test]
async fn attachment_serves_bytes_with_etag_and_dispositions() {
    init_store();
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Server").await;
    let room_id = seed_room(&pool, uid).await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let png = tiny_png();
    let client_id = unique("client");
    let response = upload(
        &app,
        room_id,
        &jar,
        "dot.png",
        "image/png",
        &png,
        &client_id,
    )
    .await;
    assert_eq!(response.status(), 303);
    let id = message_id_for(&pool, &client_id).await;
    let uri = format!("/rooms/{room_id}/messages/{id}/attachment");

    let response = get(&app, &uri, &jar, None).await;
    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "content-type"), "image/png");
    assert!(
        header(&response, "content-disposition").starts_with("inline;"),
        "{}",
        header(&response, "content-disposition")
    );
    let etag = header(&response, "etag");
    assert!(!etag.is_empty());
    assert_eq!(body_bytes(response).await, png);

    let response = get(&app, &uri, &jar, Some(("if-none-match", &etag))).await;
    assert_eq!(response.status(), 304);

    let response = get(&app, &format!("{uri}?disposition=attachment"), &jar, None).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        header(&response, "content-disposition"),
        "attachment; filename=\"dot.png\""
    );
    cleanup(&pool, room_id, uid).await;
}

#[tokio::test]
async fn pdf_upload_renders_a_file_link() {
    init_store();
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Pdfer").await;
    let room_id = seed_room(&pool, uid).await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let pdf = b"%PDF-1.4 fake".to_vec();
    let client_id = unique("client");
    let response = upload(
        &app,
        room_id,
        &jar,
        "doc.pdf",
        "application/pdf",
        &pdf,
        &client_id,
    )
    .await;
    assert_eq!(response.status(), 303);
    let id = message_id_for(&pool, &client_id).await;

    let page =
        body_text(get(&app, &format!("/rooms/{room_id}/messages/{id}"), &jar, None).await).await;
    assert!(page.contains("message__file-link"), "{page}");
    assert!(page.contains("doc.pdf"), "{page}");
    // No-JS experiment: the share button and lightbox hooks are gone;
    // the file link is a plain download anchor.
    assert!(!page.contains("web-share"), "{page}");
    let link_at = page.find("message__file-link").expect("file link renders");
    let link_end = page[link_at..]
        .find("</div>")
        .map(|end| link_at + end)
        .unwrap_or(page.len());
    let link = &page[link_at..link_end];
    assert!(!link.contains("data-lightbox-target"), "{link}");
    assert!(!link.contains("lightbox#open"), "{link}");

    let response = get(
        &app,
        &format!("/rooms/{room_id}/messages/{id}/attachment"),
        &jar,
        None,
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "content-type"), "application/pdf");
    cleanup(&pool, room_id, uid).await;
}

#[tokio::test]
async fn svg_serves_as_octet_stream_and_links() {
    init_store();
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Svger").await;
    let room_id = seed_room(&pool, uid).await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>".to_vec();
    let client_id = unique("client");
    let response = upload(
        &app,
        room_id,
        &jar,
        "pic.svg",
        "image/svg+xml",
        &svg,
        &client_id,
    )
    .await;
    assert_eq!(response.status(), 303);
    let id = message_id_for(&pool, &client_id).await;

    let response = get(
        &app,
        &format!("/rooms/{room_id}/messages/{id}/attachment"),
        &jar,
        None,
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        header(&response, "content-type"),
        "application/octet-stream"
    );

    let page =
        body_text(get(&app, &format!("/rooms/{room_id}/messages/{id}"), &jar, None).await).await;
    assert!(page.contains("message__file-link"), "{page}");
    cleanup(&pool, room_id, uid).await;
}

#[tokio::test]
async fn sloppy_client_type_sniffs_to_image() {
    init_store();
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Sniffer").await;
    let room_id = seed_room(&pool, uid).await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let png = tiny_png();
    let client_id = unique("client");
    let response = upload(
        &app,
        room_id,
        &jar,
        "dot.bin",
        "application/octet-stream",
        &png,
        &client_id,
    )
    .await;
    assert_eq!(response.status(), 303);
    let id = message_id_for(&pool, &client_id).await;

    let page =
        body_text(get(&app, &format!("/rooms/{room_id}/messages/{id}"), &jar, None).await).await;
    // No-JS experiment: the preview is a plain link, no lightbox hooks.
    assert!(page.contains("<a class=\"flex\" href="), "{page}");
    assert!(!page.contains("data-lightbox-"), "{page}");
    cleanup(&pool, room_id, uid).await;
}

#[tokio::test]
async fn video_upload_plays_inline() {
    init_store();
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Videographer").await;
    let room_id = seed_room(&pool, uid).await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let mp4 = b"\x00\x00\x00\x18ftypmp42 fake".to_vec();
    let client_id = unique("client");
    let response = upload(
        &app,
        room_id,
        &jar,
        "clip.mp4",
        "video/mp4",
        &mp4,
        &client_id,
    )
    .await;
    assert_eq!(response.status(), 303);
    let id = message_id_for(&pool, &client_id).await;

    let page =
        body_text(get(&app, &format!("/rooms/{room_id}/messages/{id}"), &jar, None).await).await;
    assert!(page.contains("<video"), "{page}");
    assert!(page.contains("controls=\"controls\""), "{page}");
    cleanup(&pool, room_id, uid).await;
}

#[tokio::test]
async fn attachment_edit_shows_no_text_editor() {
    init_store();
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Editor").await;
    let room_id = seed_room(&pool, uid).await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let client_id = unique("client");
    let response = upload(
        &app,
        room_id,
        &jar,
        "doc.pdf",
        "application/pdf",
        b"%PDF-1.4 fake",
        &client_id,
    )
    .await;
    assert_eq!(response.status(), 303);
    let id = message_id_for(&pool, &client_id).await;

    let page = body_text(
        get(
            &app,
            &format!("/rooms/{room_id}/messages/{id}/edit"),
            &jar,
            None,
        )
        .await,
    )
    .await;
    assert!(page.contains("message__file-link"), "{page}");
    assert!(page.contains("Delete message"), "{page}");
    assert!(!page.contains("message_body_trix_input"), "{page}");
    cleanup(&pool, room_id, uid).await;
}

#[tokio::test]
async fn attachment_serving_requires_membership() {
    init_store();
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Owner").await;
    let (outsider, outsider_email) = seed_user(&pool, "Outsider").await;
    let room_id = seed_room(&pool, uid).await;
    let jar = login(&pool, &email).await;
    let outsider_jar = login(&pool, &outsider_email).await;
    let app = app(pool.clone());

    let client_id = unique("client");
    let response = upload(
        &app,
        room_id,
        &jar,
        "doc.pdf",
        "application/pdf",
        b"%PDF-1.4 fake",
        &client_id,
    )
    .await;
    assert_eq!(response.status(), 303);
    let id = message_id_for(&pool, &client_id).await;
    let uri = format!("/rooms/{room_id}/messages/{id}/attachment");

    let response = get(&app, &uri, &outsider_jar, None).await;
    assert_eq!(response.status(), 404);

    let logged_out = http::Request::builder()
        .method("GET")
        .uri(&uri)
        .body(Body::empty())
        .expect("request builds");
    assert_eq!(app.handle(logged_out).await.status(), 303);

    cleanup(&pool, room_id, outsider).await;
    cleanup(&pool, room_id, uid).await;
}

#[tokio::test]
async fn missing_attachment_serves_404() {
    init_store();
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Texter").await;
    let room_id = seed_room(&pool, uid).await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    // Plain text message (urlencoded, no file part).
    let token = csrf_token();
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("/rooms/{room_id}/messages"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .body(Body::from(format!(
            "authenticity_token={token}&message[body]=hello"
        )))
        .expect("request builds");
    assert_eq!(app.handle(request).await.status(), 303);
    let id = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM messages WHERE room_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(room_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let response = get(
        &app,
        &format!("/rooms/{room_id}/messages/{id}/attachment"),
        &jar,
        None,
    )
    .await;
    assert_eq!(response.status(), 404);
    cleanup(&pool, room_id, uid).await;
}

#[tokio::test]
async fn upload_without_csrf_is_forbidden() {
    init_store();
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Forger").await;
    let room_id = seed_room(&pool, uid).await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let boundary = "----attachmenttest";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"message[client_message_id]\"\r\n\r\nx\r\n--{boundary}--\r\n"
    );
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("/rooms/{room_id}/messages"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("cookie", format!("{jar}; csrf_token=00"))
        .body(Body::from(body))
        .expect("request builds");
    assert_eq!(app.handle(request).await.status(), 403);
    cleanup(&pool, room_id, uid).await;
}
