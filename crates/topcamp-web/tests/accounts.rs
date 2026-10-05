//! Account settings tests against PostgreSQL + the real router.
//!
//! Requires `DATABASE_URL` (default: the isolated
//! `topcamp_test_accounts` database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_accounts;"` then apply
//! `migrations/*.sql` in order (re-apply after migration changes).

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::{AccountRepository, UserRepository};
use topcamp_domain::auth::{UserRole, UserStatus};
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_accounts".to_string()
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

async fn seed_user(pool: &PgPool, name: &str, admin: bool) -> (i64, String) {
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
    if admin {
        UserRepository::set_role(&db, id, UserRole::Administrator.value())
            .await
            .unwrap();
    }
    (id, email)
}

async fn seed_account(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO accounts (created_at, updated_at, join_code, name) \
         VALUES (now(), now(), 'ACCT-TEST-CODE', 'Topcamp') ON CONFLICT DO NOTHING",
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

fn tiny_png() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xFF, 0xFF, 0x3F, 0x00, 0x05, 0xFE, 0x02, 0xFE, 0xDC, 0xCC, 0x59, 0xE7, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
}

fn init_store() {
    use topcamp_storage::{S3BlobStore, S3Config};
    let _ = topcamp_web::first_run::init_store(
        S3BlobStore::new(&S3Config::dev()).expect("test store builds"),
    );
}

/// Hand-rolled multipart body: `_method`, `authenticity_token`, then
/// the `account[logo]` file part.
fn multipart_body(token: &str, png: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----testboundary42";
    let mut body = Vec::new();
    for (name, value) in [("_method", "patch"), ("authenticity_token", token)] {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"account[logo]\"; filename=\"logo.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(png);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
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
async fn edit_renders_settings_for_admin() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, aemail) = seed_user(&pool, "Ada Admin", true).await;
    let (uid, _) = seed_user(&pool, "Moe Member", false).await;
    let jar = login(&pool, &aemail).await;

    let (status, body) = get_html(&app(pool.clone()), "/account/edit", &jar).await;
    assert_eq!(status, 200, "{body}");
    // The singleton row is shared with parallel tests (other tests
    // rename/regenerate it), so assert structure, not live values.
    assert!(body.contains("account[name]"), "name form: {body}");
    assert!(
        body.contains("name=\"account[name]\" value=\""),
        "name value: {body}"
    );
    assert!(
        body.contains("restrict_room_creation_to_administrators"),
        "toggle: {body}"
    );
    assert!(body.contains("invite_url"), "invite box: {body}");
    assert!(body.contains("/join/"), "join url: {body}");
    assert!(body.contains("Ada Admin"), "admin row: {body}");
    assert!(body.contains("Moe Member"), "member row: {body}");
    assert!(body.contains("/account/bots"), "bots link: {body}");
    assert!(
        body.contains("/account/custom_styles/edit"),
        "styles link: {body}"
    );
    assert!(body.contains("version-badge"), "footer: {body}");

    cleanup_user(&pool, aid).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn edit_renders_readonly_for_member() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, _) = seed_user(&pool, "Axl Admin", true).await;
    let (uid, uemail) = seed_user(&pool, "Uma Member", false).await;
    let jar = login(&pool, &uemail).await;

    let (status, body) = get_html(&app(pool.clone()), "/account/edit", &jar).await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("account[name]"), "no name form: {body}");
    assert!(
        body.contains("<h1 class=\"flex-item-grow txt-x-large\">"),
        "account name shown: {body}"
    );
    assert!(body.contains("invite_url"), "invite box: {body}");
    assert!(body.contains("Uma Member"), "people list: {body}");
    assert!(!body.contains("user[role]"), "no role toggles: {body}");

    cleanup_user(&pool, aid).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn edit_requires_login() {
    let pool = pool().await;
    let (status, _) = get_html(&app(pool.clone()), "/account/edit", "").await;
    assert_eq!(status, 303);
}

#[tokio::test]
async fn update_renames_and_sets_flag() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, aemail) = seed_user(&pool, "Nia Name", true).await;
    let jar = login(&pool, &aemail).await;
    let router = app(pool.clone());

    let response = post_form(
        &router,
        "PATCH",
        "/account",
        &jar,
        "account%5Bname%5D=HQ&account%5Bsettings%5D%5Brestrict_room_creation_to_administrators%5D=true",
    )
    .await;
    assert_eq!(response.status(), 303);
    assert_eq!(location(&response), "/account/edit");

    let db = PgDb::new(pool.clone());
    let account = AccountRepository::first(&db).await.unwrap().unwrap();
    assert_eq!(account.name, "HQ");
    assert!(
        AccountRepository::room_creation_restricted(&db)
            .await
            .unwrap()
    );

    // The toggle back off via PUT.
    let response = post_form(
        &router,
        "PUT",
        "/account",
        &jar,
        "account%5Bsettings%5D%5Brestrict_room_creation_to_administrators%5D=false",
    )
    .await;
    assert_eq!(response.status(), 303);
    assert!(
        !AccountRepository::room_creation_restricted(&db)
            .await
            .unwrap()
    );
    // Name kept when the key is absent.
    let account = AccountRepository::first(&db).await.unwrap().unwrap();
    assert_eq!(account.name, "HQ");

    AccountRepository::update_name_settings(&db, account.id, Some("Topcamp"), "{}")
        .await
        .unwrap();
    cleanup_user(&pool, aid).await;
}

#[tokio::test]
async fn update_gates_and_validates() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, _) = seed_user(&pool, "Gus Gate", true).await;
    let (uid, uemail) = seed_user(&pool, "Unauth Member", false).await;
    let jar = login(&pool, &uemail).await;
    let router = app(pool.clone());

    // Non-admins are forbidden.
    let response = post_form(&router, "PATCH", "/account", &jar, "account%5Bname%5D=x").await;
    assert_eq!(response.status(), 403);

    // Missing account params are a 400 (admin).
    let (aid2, aemail2) = seed_user(&pool, "Ava Admin", true).await;
    let ajar = login(&pool, &aemail2).await;
    let response = post_form(&router, "PATCH", "/account", &ajar, "").await;
    assert_eq!(response.status(), 400);

    // Bad CSRF is forbidden.
    let request = http::Request::builder()
        .method("PATCH")
        .uri("/account")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{ajar}; csrf_token=wrong"))
        .body(Body::from("authenticity_token=wrong&account%5Bname%5D=x"))
        .expect("request builds");
    let response = router.handle(request).await;
    assert_eq!(response.status(), 403);

    cleanup_user(&pool, aid).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, aid2).await;
}

#[tokio::test]
async fn role_toggle_promotes_and_demotes() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, aemail) = seed_user(&pool, "Rita Role", true).await;
    let (uid, _) = seed_user(&pool, "Pete Promote", false).await;
    let jar = login(&pool, &aemail).await;
    let router = app(pool.clone());
    let db = PgDb::new(pool.clone());

    let response = post_form(
        &router,
        "PATCH",
        &format!("/account/users/{uid}"),
        &jar,
        "user%5Brole%5D=administrator",
    )
    .await;
    assert_eq!(response.status(), 303);
    assert_eq!(location(&response), "/account/edit");
    let row = UserRepository::find_by_id(&db, uid).await.unwrap().unwrap();
    assert_eq!(row.role, UserRole::Administrator.value());

    // Unknown roles fall back to member.
    let response = post_form(
        &router,
        "POST",
        &format!("/account/users/{uid}"),
        &jar,
        "_method=patch&user%5Brole%5D=bot",
    )
    .await;
    assert_eq!(response.status(), 303);
    let row = UserRepository::find_by_id(&db, uid).await.unwrap().unwrap();
    assert_eq!(row.role, UserRole::Member.value());

    cleanup_user(&pool, aid).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn deactivate_removes_user_from_list() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, aemail) = seed_user(&pool, "Dee Activate", true).await;
    let (uid, _) = seed_user(&pool, "Gone Guy", false).await;
    let jar = login(&pool, &aemail).await;
    let router = app(pool.clone());
    let db = PgDb::new(pool.clone());

    let response = post_form(
        &router,
        "DELETE",
        &format!("/account/users/{uid}"),
        &jar,
        "",
    )
    .await;
    assert_eq!(response.status(), 303);
    let row = UserRepository::find_by_id(&db, uid).await.unwrap().unwrap();
    assert_eq!(row.status, UserStatus::Deactivated.value());

    let (_, body) = get_html(&router, "/account/edit", &jar).await;
    assert!(!body.contains("Gone Guy"), "deactivated drops out: {body}");

    cleanup_user(&pool, aid).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn user_routes_gate_and_404() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, _) = seed_user(&pool, "Ugo Update", true).await;
    let (uid, uemail) = seed_user(&pool, "Uma Member", false).await;
    let jar = login(&pool, &uemail).await;
    let router = app(pool.clone());

    let response = post_form(
        &router,
        "PATCH",
        &format!("/account/users/{aid}"),
        &jar,
        "user%5Brole%5D=member",
    )
    .await;
    assert_eq!(response.status(), 403);
    let response = post_form(
        &router,
        "DELETE",
        &format!("/account/users/{aid}"),
        &jar,
        "",
    )
    .await;
    assert_eq!(response.status(), 403);

    let (aid2, aemail2) = seed_user(&pool, "Ava Admin", true).await;
    let ajar = login(&pool, &aemail2).await;
    let response = post_form(
        &router,
        "PATCH",
        "/account/users/999999999",
        &ajar,
        "user%5Brole%5D=member",
    )
    .await;
    assert_eq!(response.status(), 404);

    cleanup_user(&pool, aid).await;
    cleanup_user(&pool, uid).await;
    cleanup_user(&pool, aid2).await;
}

#[tokio::test]
async fn logo_serves_stock_when_none_attached() {
    let pool = pool().await;
    seed_account(&pool).await;
    // Public: no session needed. (A parallel logo test may have a
    // custom logo attached; stock-or-custom both must be PNG.)
    let request = http::Request::builder()
        .method("GET")
        .uri("/account/logo")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers().get("content-type").unwrap(), "image/png");
    assert!(
        response
            .headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("max-age=300")
    );
    assert!(response.headers().get("etag").is_some());
}

#[tokio::test]
async fn logo_upload_serves_variants_and_destroys() {
    use topcamp_db::repositories::AttachmentRepository;
    let pool = pool().await;
    init_store();
    seed_account(&pool).await;
    let (aid, aemail) = seed_user(&pool, "Lana Logo", true).await;
    let jar = login(&pool, &aemail).await;
    let router = app(pool.clone());
    let db = PgDb::new(pool.clone());

    // Upload via the multipart settings form.
    let token = csrf_token();
    let (content_type, body) = multipart_body(&token, &tiny_png());
    let request = http::Request::builder()
        .method("POST")
        .uri("/account")
        .header("content-type", content_type)
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .body(Body::from(body))
        .expect("request builds");
    let response = router.handle(request).await;
    assert_eq!(response.status(), 303);
    assert_eq!(location(&response), "/account/edit");

    let account = AccountRepository::first(&db).await.unwrap().unwrap();
    assert!(
        AccountRepository::logo_attached(&db, account.id)
            .await
            .unwrap()
    );
    let blob = AttachmentRepository::blob_for_record(&db, "Account", account.id, "logo")
        .await
        .unwrap()
        .unwrap();
    let digests = AttachmentRepository::variant_digests(&db, blob.id)
        .await
        .unwrap();
    assert_eq!(digests.len(), 2, "both PNG variants recorded");

    // Both renditions serve the uploaded bytes (1x1 in, 1x1 out —
    // pixel dims are covered by the variants unit tests).
    for uri in ["/account/logo", "/account/logo?size=small"] {
        let request = http::Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .expect("request builds");
        let response = router.handle(request).await;
        assert_eq!(response.status(), 200, "{uri}");
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&bytes[1..4], b"PNG", "{uri} is a png");
    }

    // ETag round-trip.
    let request = http::Request::builder()
        .method("GET")
        .uri("/account/logo")
        .body(Body::empty())
        .expect("request builds");
    let response = router.handle(request).await;
    let etag = response
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let request = http::Request::builder()
        .method("GET")
        .uri("/account/logo")
        .header("if-none-match", &etag)
        .body(Body::empty())
        .expect("request builds");
    let response = router.handle(request).await;
    assert_eq!(response.status(), 304);

    // Destroy resets to stock and purges rows + objects.
    let response = post_form(&router, "DELETE", "/account/logo", &jar, "").await;
    assert_eq!(response.status(), 303);
    assert!(
        !AccountRepository::logo_attached(&db, account.id)
            .await
            .unwrap()
    );
    assert!(
        AttachmentRepository::blob_for_record(&db, "Account", account.id, "logo")
            .await
            .unwrap()
            .is_none()
    );

    cleanup_user(&pool, aid).await;
}

#[tokio::test]
async fn logo_routes_gate_non_admins() {
    let pool = pool().await;
    init_store();
    seed_account(&pool).await;
    let (aid, _) = seed_user(&pool, "Liam Logo", true).await;
    let (uid, uemail) = seed_user(&pool, "Moe Member", false).await;
    let jar = login(&pool, &uemail).await;
    let router = app(pool.clone());

    let token = csrf_token();
    let (content_type, body) = multipart_body(&token, &tiny_png());
    let request = http::Request::builder()
        .method("POST")
        .uri("/account")
        .header("content-type", content_type)
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .body(Body::from(body))
        .expect("request builds");
    let response = router.handle(request).await;
    assert_eq!(response.status(), 403);

    let response = post_form(&router, "DELETE", "/account/logo", &jar, "").await;
    assert_eq!(response.status(), 403);

    cleanup_user(&pool, aid).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn avatar_serves_webp_variant_for_uploads() {
    use topcamp_db::repositories::{AttachmentRepository, NewBlob};
    use topcamp_storage::BlobStore;
    let pool = pool().await;
    init_store();
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Wendy Webp", false).await;
    let jar = login(&pool, &email).await;
    let router = app(pool.clone());
    let db = PgDb::new(pool.clone());

    // Attach an uploaded avatar (original PNG in the store).
    let store = topcamp_storage::S3BlobStore::new(&topcamp_storage::S3Config::dev())
        .expect("test store builds");
    let key = format!("test-avatar-{uid}");
    store
        .put(&key, tiny_png(), Some("image/png"))
        .await
        .unwrap();
    let blob = AttachmentRepository::insert_blob(
        &db,
        NewBlob {
            key: key.clone(),
            filename: "avatar.png".to_string(),
            content_type: Some("image/png".to_string()),
            byte_size: tiny_png().len() as i64,
            checksum: None,
            service_name: "rustfs".to_string(),
        },
    )
    .await
    .unwrap();
    AttachmentRepository::attach_to_record(&db, "User", uid, "avatar", blob.id)
        .await
        .unwrap();

    let uri = format!("/users/{}/avatar", topcamp_web::users::avatar_token(uid));
    let request = http::Request::builder()
        .method("GET")
        .uri(&uri)
        .header("cookie", &jar)
        .body(Body::empty())
        .expect("request builds");
    let response = router.handle(request).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "image/webp"
    );
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&bytes[0..4], b"RIFF", "webp magic");
    assert_eq!(&bytes[8..12], b"WEBP", "webp magic");
    let digests = AttachmentRepository::variant_digests(&db, blob.id)
        .await
        .unwrap();
    assert_eq!(digests.len(), 1, "square variant recorded");

    // Cleanup rows + objects (isolated DB, shared dev bucket).
    for digest in &digests {
        store
            .delete(&topcamp_storage::variant_key(&key, digest))
            .await
            .unwrap();
    }
    store.delete(&key).await.unwrap();
    AttachmentRepository::delete_blob(&db, blob.id)
        .await
        .unwrap();
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn join_code_regenerates() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, aemail) = seed_user(&pool, "Judy Join", true).await;
    let jar = login(&pool, &aemail).await;
    let router = app(pool.clone());
    let db = PgDb::new(pool.clone());

    let before = AccountRepository::first(&db)
        .await
        .unwrap()
        .unwrap()
        .join_code;
    let response = post_form(&router, "POST", "/account/join_code", &jar, "").await;
    assert_eq!(response.status(), 303);
    assert_eq!(location(&response), "/account/edit");
    let after = AccountRepository::first(&db)
        .await
        .unwrap()
        .unwrap()
        .join_code;
    assert_ne!(before, after);
    assert_eq!(after.len(), 14); // XXXX-XXXX-XXXX

    // Members are forbidden.
    let (uid, uemail) = seed_user(&pool, "Moe Member", false).await;
    let ujar = login(&pool, &uemail).await;
    let response = post_form(&router, "POST", "/account/join_code", &ujar, "").await;
    assert_eq!(response.status(), 403);

    cleanup_user(&pool, aid).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn theme_defaults_to_system_with_picker() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, _) = seed_user(&pool, "Ada Theme", true).await;
    let (uid, uemail) = seed_user(&pool, "Uma Theme", false).await;
    let jar = login(&pool, &uemail).await;
    let router = app(pool.clone());

    let (status, body) = get_html(&router, "/account/edit", &jar).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("data-theme=\"system\""),
        "default follows the OS: {body}"
    );
    assert!(
        body.contains("action=\"/account/theme\""),
        "appearance picker posts to the setter: {body}"
    );
    for value in ["light", "dark", "system"] {
        assert!(
            body.contains(&format!("name=\"theme\" value=\"{value}\"")),
            "{value} option: {body}"
        );
    }
    assert!(
        body.contains("name=\"theme\" value=\"system\" checked=\"checked\""),
        "system pre-checked: {body}"
    );

    cleanup_user(&pool, aid).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn theme_cookie_dark_renders_dark_root() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, _) = seed_user(&pool, "Ada Dark", true).await;
    let (uid, uemail) = seed_user(&pool, "Uma Dark", false).await;
    let jar = login(&pool, &uemail).await;
    let router = app(pool.clone());

    let dark_jar = format!("{jar}; topcamp_theme=dark");
    let (status, body) = get_html(&router, "/account/edit", &dark_jar).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("data-theme=\"dark\""),
        "dark cookie pins the root: {body}"
    );
    assert!(
        body.contains("name=\"theme\" value=\"dark\" checked=\"checked\""),
        "dark pre-checked: {body}"
    );

    // Garbage falls back to system.
    let neon_jar = format!("{jar}; topcamp_theme=neon");
    let (status, body) = get_html(&router, "/account/edit", &neon_jar).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("data-theme=\"system\""), "fallback: {body}");

    cleanup_user(&pool, aid).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn theme_setter_sets_cookie_and_redirects_back() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, _) = seed_user(&pool, "Ada Setter", true).await;
    let (uid, uemail) = seed_user(&pool, "Uma Setter", false).await;
    let jar = login(&pool, &uemail).await;
    let router = app(pool.clone());

    let response = post_form(&router, "POST", "/account/theme", &jar, "theme=dark").await;
    assert_eq!(response.status(), 303);
    assert_eq!(location(&response), "/account/edit");
    let set = cookies(&response);
    assert!(set.contains("topcamp_theme=dark"), "dark persists: {set}");

    // The persisted cookie renders on the next page.
    let (status, body) = get_html(
        &router,
        "/account/edit",
        &format!("{jar}; topcamp_theme=dark"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("data-theme=\"dark\""), "{body}");

    // System clears the cookie; unknown values are a 400.
    let response = post_form(&router, "POST", "/account/theme", &jar, "theme=system").await;
    assert_eq!(response.status(), 303);
    assert_eq!(location(&response), "/account/edit");
    let response = post_form(&router, "POST", "/account/theme", &jar, "theme=neon").await;
    assert_eq!(response.status(), 400);

    // Bad CSRF is forbidden.
    let request = http::Request::builder()
        .method("POST")
        .uri("/account/theme")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{jar}; csrf_token=wrong"))
        .body(Body::from("authenticity_token=wrong&theme=dark"))
        .expect("request builds");
    let response = router.handle(request).await;
    assert_eq!(response.status(), 403);

    cleanup_user(&pool, aid).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn remove_member_dialog_opens_via_query_param() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (_aid, aemail) = seed_user(&pool, "Rita Remover", true).await;
    let (uid, _) = seed_user(&pool, "Moe Member", false).await;
    let jar = login(&pool, &aemail).await;
    let router = app(pool.clone());

    // Plain settings: the remove dialog renders closed.
    let (status, body) = get_html(&router, "/account/edit", &jar).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("role=\"alertdialog\""),
        "dialog renders: {body}"
    );
    assert!(!body.contains("<dialog open"), "dialog closed: {body}");

    // `?confirm=remove-member-<id>` opens that member's dialog, with a
    // Cancel link back to the same page slice.
    let uri = format!("/account/edit?confirm=remove-member-{uid}");
    let (status, body) = get_html(&router, &uri, &jar).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<dialog open"), "remove opens: {body}");
    assert!(
        body.contains(&format!("form=\"remove-member-{uid}\">Remove</button>")),
        "confirm submits the form: {body}"
    );
    assert!(
        body.contains("href=\"/account/edit?page=1\">Cancel</a>"),
        "cancel links back: {body}"
    );
}

/// Serializes the custom-styles writers: the singleton row is shared
/// with parallel tests, and only these tests touch its column.
fn styles_serial() -> &'static tokio::sync::Mutex<()> {
    static SERIAL: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    SERIAL.get_or_init(Default::default)
}

async fn get_css(router: &Router, etag: Option<&str>) -> topcoat::router::response::Response {
    let mut builder = http::Request::builder()
        .method("GET")
        .uri("/account/custom_styles.css");
    if let Some(tag) = etag {
        builder = builder.header("if-none-match", tag);
    }
    let request = builder.body(Body::empty()).expect("request builds");
    router.handle(request).await
}

#[tokio::test]
async fn custom_styles_edit_renders_form_for_admin() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, aemail) = seed_user(&pool, "Ada Styler", true).await;
    let jar = login(&pool, &aemail).await;

    let (status, body) = get_html(&app(pool.clone()), "/account/custom_styles/edit", &jar).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("Custom styles"), "heading: {body}");
    assert!(
        body.contains("name=\"account[custom_styles]\""),
        "textarea: {body}"
    );
    assert!(
        body.contains("action=\"/account/custom_styles\""),
        "form: {body}"
    );
    assert!(body.contains("authenticity_token"), "csrf: {body}");

    cleanup_user(&pool, aid).await;
}

#[tokio::test]
async fn custom_styles_update_round_trip() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, aemail) = seed_user(&pool, "Uma Updater", true).await;
    let jar = login(&pool, &aemail).await;
    let router = app(pool.clone());
    let _guard = styles_serial().lock().await;

    let css = ".x{color:red}";
    let response = post_form(
        &router,
        "PATCH",
        "/account/custom_styles",
        &jar,
        &format!("account%5Bcustom_styles%5D={css}"),
    )
    .await;
    assert_eq!(response.status(), 303);
    assert_eq!(location(&response), "/account/custom_styles/edit");

    let response = get_css(&router, None).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/css; charset=utf-8")
    );
    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(!etag.is_empty(), "etag set");
    assert_eq!(body_text(response).await, css);

    // Conditional fetch hits 304.
    let response = get_css(&router, Some(&etag)).await;
    assert_eq!(response.status(), 304);

    // The edit form pre-fills the live value.
    let (status, body) = get_html(&router, "/account/custom_styles/edit", &jar).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(css), "prefill: {body}");

    // Reset the shared row for parallel suites.
    let response = post_form(
        &router,
        "PATCH",
        "/account/custom_styles",
        &jar,
        "account%5Bcustom_styles%5D=",
    )
    .await;
    assert_eq!(response.status(), 303);

    cleanup_user(&pool, aid).await;
}

#[tokio::test]
async fn custom_styles_gate_member_and_signed_out() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, _) = seed_user(&pool, "Axl Styler", true).await;
    let (uid, uemail) = seed_user(&pool, "Mel Member", false).await;
    let jar = login(&pool, &uemail).await;
    let router = app(pool.clone());

    let (status, _) = get_html(&router, "/account/custom_styles/edit", &jar).await;
    assert_eq!(status, 403, "member cannot open the form");
    let response = post_form(
        &router,
        "PATCH",
        "/account/custom_styles",
        &jar,
        "account%5Bcustom_styles%5D=.x{color:red}",
    )
    .await;
    assert_eq!(response.status(), 403, "member cannot save");

    let (status, _) = get_html(&router, "/account/custom_styles/edit", "").await;
    assert_eq!(status, 303, "signed-out redirects to sign-in");

    // The stylesheet itself stays public (pages link it signed-out too).
    let response = get_css(&router, None).await;
    assert_eq!(response.status(), 200);

    cleanup_user(&pool, aid).await;
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn custom_styles_css_stays_inert() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, aemail) = seed_user(&pool, "Ivy Inert", true).await;
    let jar = login(&pool, &aemail).await;
    let router = app(pool.clone());
    let _guard = styles_serial().lock().await;

    // Markup-breaking input must serve verbatim as a stylesheet: no
    // HTML wrapper to break out of, so nothing to escape.
    let css = "</style><script>alert(1)</script>";
    let response = post_form(
        &router,
        "PATCH",
        "/account/custom_styles",
        &jar,
        "account%5Bcustom_styles%5D=%3C%2Fstyle%3E%3Cscript%3Ealert(1)%3C%2Fscript%3E",
    )
    .await;
    assert_eq!(response.status(), 303);

    let response = get_css(&router, None).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/css; charset=utf-8")
    );
    let served = body_text(response).await;
    assert_eq!(served, css);
    assert!(!served.contains("<html"), "no document wrapper");

    // Reset the shared row for parallel suites.
    let response = post_form(
        &router,
        "PATCH",
        "/account/custom_styles",
        &jar,
        "account%5Bcustom_styles%5D=",
    )
    .await;
    assert_eq!(response.status(), 303);

    cleanup_user(&pool, aid).await;
}

#[tokio::test]
async fn shell_links_the_install_stylesheet() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (aid, aemail) = seed_user(&pool, "Lena Linker", true).await;
    let jar = login(&pool, &aemail).await;

    let (status, body) = get_html(&app(pool.clone()), "/account/edit", &jar).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("<link rel=\"stylesheet\" href=\"/account/custom_styles.css\""),
        "shell links the stylesheet: {body}"
    );

    cleanup_user(&pool, aid).await;
}
