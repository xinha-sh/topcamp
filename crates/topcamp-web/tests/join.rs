//! Join (invite-code signup) tests against PostgreSQL + RustFS + the real router.
//!
//! Requires `DATABASE_URL` (default: the isolated `topcamp_test_join`
//! database) and RustFS on `RUSTFS_ENDPOINT` (default
//! `http://localhost:9000`). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_join;"` then apply
//! `migrations/*.sql` in order (re-apply after migration changes).
//! Tests take a process-wide lock: one empties `accounts`, so the
//! suite runs serially.

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::UserRepository;
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_join".to_string()
    });
    PgPool::connect(&url).await.expect("connect DATABASE_URL")
}

fn app(pool: PgPool) -> Router {
    router(AppState::new(PgDb::new(pool), Cable::new()))
}

fn serial() -> &'static tokio::sync::Mutex<()> {
    static SERIAL: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    SERIAL.get_or_init(Default::default)
}

/// Empty `accounts` with the install row(s) stashed; restores on drop
/// (same shape as the first-run tests).
struct FreshInstall {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    conn: sqlx::pool::PoolConnection<sqlx::Postgres>,
}

async fn fresh_install(pool: &PgPool) -> FreshInstall {
    let guard = serial().lock().await;
    let mut conn = pool.acquire().await.expect("acquire stash connection");
    sqlx::query("CREATE TEMP TABLE accounts_backup AS SELECT * FROM accounts")
        .execute(&mut *conn)
        .await
        .expect("stash accounts");
    sqlx::query("DELETE FROM accounts")
        .execute(&mut *conn)
        .await
        .expect("empty accounts");
    FreshInstall {
        _guard: guard,
        conn,
    }
}

impl FreshInstall {
    async fn restore(mut self) {
        sqlx::query("INSERT INTO accounts OVERRIDING SYSTEM VALUE SELECT * FROM accounts_backup")
            .execute(&mut *self.conn)
            .await
            .expect("restore accounts");
        sqlx::query(
            "SELECT setval(pg_get_serial_sequence('accounts','id'), COALESCE((SELECT max(id) FROM accounts), 1))",
        )
        .execute(&mut *self.conn)
        .await
        .expect("reset accounts identity");
    }
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

fn redirect_location(response: &topcoat::router::response::Response) -> String {
    response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
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

fn unique(tag: &str) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock runs forward")
        .as_nanos();
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("{tag}-{}-{nanos}-{seq}", std::process::id())
}

async fn seed_account(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO accounts (created_at, updated_at, join_code, name) \
         VALUES (now(), now(), 'JOIN-CODE-1234', 'Topcamp') ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
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

async fn seed_admin(pool: &PgPool, name: &str, email: &str) -> i64 {
    let digest = bcrypt::hash("s3cret", 4).expect("cost-4 digest builds");
    let db = PgDb::new(pool.clone());
    let id = UserRepository::create(&db, name, email, Some(&digest))
        .await
        .expect("seed admin")
        .id;
    sqlx::query("UPDATE users SET role = 1 WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn login(pool: &PgPool, email: &str) -> String {
    let token = csrf_token();
    let body = format!(
        "email_address={}&password={}&authenticity_token={}",
        email.replace('@', "%40"),
        "s3cret",
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

async fn get(pool: &PgPool, uri: &str, jar: &str) -> topcoat::router::response::Response {
    let request = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", jar)
        .header("accept", "text/html,application/xhtml+xml")
        .body(Body::empty())
        .expect("request builds");
    app(pool.clone()).handle(request).await
}

const BOUNDARY: &str = "topcamp-test-boundary";

fn multipart_body(fields: &[(&str, &str)], file: Option<(&str, &str, &[u8])>) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    if let Some((name, filename, bytes)) = file {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: image/png\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn join_post(
    uri: &str,
    jar: &str,
    fields: &[(&str, &str)],
    file: Option<(&str, &str, &[u8])>,
    with_token: bool,
) -> http::Request<Body> {
    let token = csrf_token();
    let mut owned: Vec<(String, String)> = fields
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    if with_token {
        owned.push(("authenticity_token".to_string(), token.clone()));
    }
    let refs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    http::Request::builder()
        .method("POST")
        .uri(uri)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .body(Body::from(multipart_body(&refs, file)))
        .expect("join request builds")
}

async fn cleanup_user(pool: &PgPool, uid: i64) {
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(uid)
        .execute(pool)
        .await
        .unwrap();
    let blob_ids: Vec<i64> = sqlx::query_scalar(
        "SELECT blob_id FROM active_storage_attachments WHERE record_type = 'User' AND record_id = $1",
    )
    .bind(uid)
    .fetch_all(pool)
    .await
    .unwrap();
    sqlx::query(
        "DELETE FROM active_storage_attachments WHERE record_type = 'User' AND record_id = $1",
    )
    .bind(uid)
    .execute(pool)
    .await
    .unwrap();
    for blob_id in &blob_ids {
        sqlx::query("DELETE FROM active_storage_variant_records WHERE blob_id = $1")
            .bind(blob_id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM active_storage_blobs WHERE id = $1")
            .bind(blob_id)
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM memberships WHERE user_id = $1")
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
async fn new_renders_signup_nametag() {
    let _serial = serial().lock().await;
    let pool = pool().await;
    seed_account(&pool).await;
    // Idempotent: a previous panicked run's admin would win
    // `first_administrator` and poison the help-contact assertion.
    sqlx::query("DELETE FROM users")
        .execute(&pool)
        .await
        .unwrap();
    let owner_email = format!("{}@example.com", unique("olivia"));
    let admin = seed_admin(&pool, "Olivia Owner", &owner_email).await;

    let page = body_text(get(&pool, "/join/JOIN-CODE-1234", "").await).await;
    assert!(page.contains("<title>Sign up</title>"), "{page}");
    assert!(page.contains("Topcamp</strong>"), "{page}");
    assert!(page.contains("action=\"/join/JOIN-CODE-1234\""), "{page}");
    assert!(page.contains("name=\"user[name]\""), "{page}");
    assert!(page.contains("data-1p-ignore=\"true\""), "{page}");
    assert!(page.contains("name=\"user[email_address]\""), "{page}");
    assert!(page.contains("name=\"user[password]\""), "{page}");
    assert!(page.contains("maxlength=\"72\""), "{page}");
    assert!(page.contains("name=\"user[avatar]\""), "{page}");
    assert!(!page.contains("data-controller="), "{page}");
    assert!(page.contains("href=\"/session/new\""), "{page}");
    // Attribute quotes render `&quot;`-escaped, like the sign-in page.
    assert!(
        page.contains(&format!("mailto:&quot;Olivia Owner&quot; <{owner_email}>")),
        "{page}"
    );
    assert!(page.contains("Topcamp™ version"), "{page}");
    assert!(page.contains("Enter your name"), "{page}");

    cleanup_user(&pool, admin).await;
}

#[tokio::test]
async fn new_rejects_bad_code_with_404() {
    let _serial = serial().lock().await;
    let pool = pool().await;
    seed_account(&pool).await;
    let response = get(&pool, "/join/NOPE-NOPE-NOPE", "").await;
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn new_without_account_is_500() {
    let pool = pool().await;
    let install = fresh_install(&pool).await;
    let response = get(&pool, "/join/JOIN-CODE-1234", "").await;
    assert_eq!(response.status(), 500);
    install.restore().await;
}

#[tokio::test]
async fn new_redirects_signed_in_users_home() {
    let _serial = serial().lock().await;
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "member").await;
    let jar = login(&pool, &email).await;

    let response = get(&pool, "/join/JOIN-CODE-1234", &jar).await;
    assert_eq!(response.status(), 303);
    assert_eq!(redirect_location(&response), "/");

    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn create_registers_logs_in_and_goes_home() {
    use topcamp_storage::{S3BlobStore, S3Config};
    let _serial = serial().lock().await;
    let pool = pool().await;
    seed_account(&pool).await;
    let _ = topcamp_web::first_run::init_store(
        S3BlobStore::new(&S3Config::dev()).expect("test store builds"),
    );
    let email = format!("{}@example.com", unique("newbie"));

    let png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let request = join_post(
        "/join/JOIN-CODE-1234",
        "",
        &[
            ("user[name]", "Nina Newbie"),
            ("user[email_address]", &email),
            ("user[password]", "s3cret-new"),
        ],
        Some(("user[avatar]", "avatar.png", &png)),
        true,
    );
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);
    assert_eq!(redirect_location(&response), "/");
    let jar = cookies(&response);
    assert!(jar.contains("session"), "{jar}");

    // The session works: home renders (unsigned users bounce to sign-in).
    let home = get(&pool, "/", &jar).await;
    assert_eq!(home.status(), 200);

    // The row carries a digest, and the avatar attached.
    let row: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT name, email_address, password_digest FROM users WHERE email_address = $1",
    )
    .bind(&email)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "Nina Newbie");
    assert!(
        row.2
            .is_some_and(|digest| bcrypt::verify("s3cret-new", &digest).unwrap())
    );
    let uid: i64 = sqlx::query_scalar("SELECT id FROM users WHERE email_address = $1")
        .bind(&email)
        .fetch_one(&pool)
        .await
        .unwrap();
    let attachments: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM active_storage_attachments WHERE record_type = 'User' AND record_id = $1 AND name = 'avatar'",
    )
    .bind(uid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attachments, 1);

    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn create_without_avatar_still_registers() {
    let _serial = serial().lock().await;
    let pool = pool().await;
    seed_account(&pool).await;
    let email = format!("{}@example.com", unique("plain"));

    let request = join_post(
        "/join/JOIN-CODE-1234",
        "",
        &[
            ("user[name]", "Pam Plain"),
            ("user[email_address]", &email),
            ("user[password]", "s3cret"),
        ],
        None,
        true,
    );
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);
    assert_eq!(redirect_location(&response), "/");

    let uid: i64 = sqlx::query_scalar("SELECT id FROM users WHERE email_address = $1")
        .bind(&email)
        .fetch_one(&pool)
        .await
        .unwrap();
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn create_with_empty_password_stores_null_digest() {
    let _serial = serial().lock().await;
    let pool = pool().await;
    seed_account(&pool).await;
    let email = format!("{}@example.com", unique("nopass"));

    let request = join_post(
        "/join/JOIN-CODE-1234",
        "",
        &[
            ("user[name]", "Noah Nopass"),
            ("user[email_address]", &email),
            ("user[password]", ""),
        ],
        None,
        true,
    );
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);

    let digest: Option<String> =
        sqlx::query_scalar("SELECT password_digest FROM users WHERE email_address = $1")
            .bind(&email)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(digest, None);
    let uid: i64 = sqlx::query_scalar("SELECT id FROM users WHERE email_address = $1")
        .bind(&email)
        .fetch_one(&pool)
        .await
        .unwrap();
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn create_duplicate_email_goes_to_signin_prefilled() {
    let _serial = serial().lock().await;
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "taken").await;

    let request = join_post(
        "/join/JOIN-CODE-1234",
        "",
        &[
            ("user[name]", "Tara Taken"),
            ("user[email_address]", &email),
            ("user[password]", "s3cret"),
        ],
        None,
        true,
    );
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);
    let escaped = email.replace('@', "%40");
    assert_eq!(
        redirect_location(&response),
        format!("/session/new?email_address={escaped}")
    );

    // The sign-in form prefills the address.
    let page =
        body_text(get(&pool, &format!("/session/new?email_address={escaped}"), "").await).await;
    assert!(page.contains(&format!("value=\"{email}\"")), "{page}");

    // No second row was created.
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE email_address = $1")
        .bind(&email)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn create_missing_name_is_500() {
    let _serial = serial().lock().await;
    let pool = pool().await;
    seed_account(&pool).await;
    let email = format!("{}@example.com", unique("noname"));

    let request = join_post(
        "/join/JOIN-CODE-1234",
        "",
        &[
            ("user[email_address]", &email),
            ("user[password]", "s3cret"),
        ],
        None,
        true,
    );
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 500);

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE email_address = $1")
        .bind(&email)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn create_rejects_bad_code_with_404() {
    let _serial = serial().lock().await;
    let pool = pool().await;
    seed_account(&pool).await;
    let email = format!("{}@example.com", unique("badcode"));

    let request = join_post(
        "/join/NOPE-NOPE-NOPE",
        "",
        &[
            ("user[name]", "Ben Badcode"),
            ("user[email_address]", &email),
            ("user[password]", "s3cret"),
        ],
        None,
        true,
    );
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn create_requires_csrf() {
    let _serial = serial().lock().await;
    let pool = pool().await;
    seed_account(&pool).await;

    let request = join_post(
        "/join/JOIN-CODE-1234",
        "",
        &[
            ("user[name]", "Cid Csrf"),
            ("user[email_address]", "cid@example.com"),
            ("user[password]", "s3cret"),
        ],
        None,
        false,
    );
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn create_redirects_signed_in_users_home() {
    let _serial = serial().lock().await;
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "member").await;
    let jar = login(&pool, &email).await;

    let request = join_post(
        "/join/JOIN-CODE-1234",
        &jar,
        &[
            ("user[name]", "Sam Signedin"),
            ("user[email_address]", "sam@example.com"),
            ("user[password]", "s3cret"),
        ],
        None,
        true,
    );
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 303);
    assert_eq!(redirect_location(&response), "/");

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE email_address = 'sam@example.com'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    cleanup_user(&pool, uid).await;
}

#[tokio::test]
async fn login_prefills_email_query() {
    let _serial = serial().lock().await;
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, _) = seed_user(&pool, "member").await;

    let page = body_text(get(&pool, "/session/new?email_address=a%40b.com", "").await).await;
    assert!(page.contains("value=\"a@b.com\""), "{page}");

    cleanup_user(&pool, uid).await;
}
