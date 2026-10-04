//! First-run setup tests against PostgreSQL + RustFS + the real router.
//!
//! Requires `DATABASE_URL` (default: the isolated `topcamp_test_firstrun`
//! database — setup tests empty `accounts`, so they never run against
//! the dev database) and RustFS on `RUSTFS_ENDPOINT` (default
//! `http://localhost:9000`). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_firstrun;"` then apply
//! `migrations/*.sql` in order (re-apply after migration changes).
//! Setup tests take a process-wide lock and stash the install row(s) in
//! a temp table on a held connection, restoring them afterwards.

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_firstrun".to_string()
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

/// Empty `accounts` table with the install row(s) stashed; restores on
/// [`FreshInstall::restore`]. Holds the serial lock for its lifetime.
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

/// Self-minted double-submit pair (any well-formed equal pair verifies).
fn csrf_token() -> String {
    "cd".repeat(32)
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

fn setup_post(fields: &[(&str, &str)], file: Option<(&str, &str, &[u8])>) -> http::Request<Body> {
    let token = csrf_token();
    let mut owned: Vec<(String, String)> = fields
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    owned.push(("authenticity_token".to_string(), token.clone()));
    let refs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    http::Request::builder()
        .method("POST")
        .uri("/first_run")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("cookie", format!("csrf_token={token}"))
        .body(Body::from(multipart_body(&refs, file)))
        .expect("setup request builds")
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

/// Remove every row a setup run created (keys captured by email).
async fn cleanup_setup(pool: &PgPool, email: &str) {
    let uid: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE email_address = $1")
        .bind(email)
        .fetch_optional(pool)
        .await
        .unwrap();
    let Some(uid) = uid else {
        return;
    };
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(uid)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM memberships WHERE user_id = $1")
        .bind(uid)
        .execute(pool)
        .await
        .unwrap();
    let room_ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM rooms WHERE creator_id = $1")
        .bind(uid)
        .fetch_all(pool)
        .await
        .unwrap();
    for room_id in room_ids {
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
        sqlx::query(
            "DELETE FROM outbox WHERE topic = 'process_attachment' AND payload = $1::jsonb",
        )
        .bind(format!("{{\"blob_id\":{blob_id}}}"))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM active_storage_blobs WHERE id = $1")
            .bind(blob_id)
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(uid)
        .execute(pool)
        .await
        .unwrap();
    // The stash lives in a temp table on the held connection, so the
    // only visible row is this test's account.
    sqlx::query("DELETE FROM accounts")
        .execute(pool)
        .await
        .unwrap();
}

/// Smallest valid PNG (1x1), as avatar bytes.
fn tiny_png() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xFF, 0xFF, 0x3F, 0x00, 0x05, 0xFE, 0x02, 0xFE, 0xDC, 0xCC, 0x59, 0xE7, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
}

#[tokio::test]
async fn setup_page_matches_upstream_markup() {
    let pool = pool().await;
    let install = fresh_install(&pool).await;
    let request = http::Request::builder()
        .method("GET")
        .uri("/first_run")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 200);
    let html = body_text(response).await;
    for needle in [
        "Set up Topcamp",
        "body class=\"signup\"",
        "action=\"/first_run\"",
        "enctype=\"multipart/form-data\"",
        "nametag",
        "nametag__lanyard",
        "name=\"user[avatar]\"",
        "type=\"file\"",
        "name=\"user[name]\"",
        "name=\"user[email_address]\"",
        "type=\"email\"",
        "name=\"user[password]\"",
        "maxlength=\"72\"",
        "language-list-menu",
        "lanyard",
        "camera",
        "default-avatar",
        "person",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
    install.restore().await;
}

#[tokio::test]
async fn setup_provisions_install_and_signs_in() {
    let pool = pool().await;
    let install = fresh_install(&pool).await;
    let email = "first-run-ok@example.com";
    // Idempotent: a previous panicked run's residue can't poison this one.
    cleanup_setup(&pool, email).await;
    let response = app(pool.clone())
        .handle(setup_post(
            &[
                ("user[name]", "First Admin"),
                ("user[email_address]", email),
                ("user[password]", "s3cret-setup"),
            ],
            None,
        ))
        .await;
    assert_eq!(response.status(), 303);
    assert_eq!(response.headers().get("location").unwrap(), "/");
    let jar = cookies(&response);
    assert!(!jar.is_empty(), "setup signs the admin in");

    // Account singleton with a grouped join code.
    let account: (String, String) = sqlx::query_as("SELECT name, join_code FROM accounts LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(account.0, "Topcamp");
    assert_eq!(account.1.len(), 14, "join code: {}", account.1);

    // Administrator with a verifying digest.
    let user: (i64, String, i32, String) = sqlx::query_as(
        "SELECT id, name, role, password_digest FROM users WHERE email_address = $1",
    )
    .bind(email)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(user.1, "First Admin");
    assert_eq!(user.2, 1, "administrator role");
    assert!(bcrypt::verify("s3cret-setup", &user.3).unwrap());

    // First room + grant.
    let room: (i64, String, String) =
        sqlx::query_as("SELECT id, name, type FROM rooms WHERE creator_id = $1")
            .bind(user.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(room.1, "All Talk");
    assert_eq!(room.2, "Rooms::Open");
    let memberships: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memberships WHERE room_id = $1 AND user_id = $2")
            .bind(room.0)
            .bind(user.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(memberships, 1);

    // The session authenticates: welcome redirects into the new room.
    let authed = http::Request::builder()
        .method("GET")
        .uri("/")
        .header("cookie", jar)
        .body(Body::empty())
        .expect("authed request builds");
    let response = app(pool.clone()).handle(authed).await;
    assert_eq!(response.status(), 303);
    assert_eq!(
        response.headers().get("location").unwrap(),
        &format!("/rooms/{}", room.0)
    );

    cleanup_setup(&pool, email).await;
    install.restore().await;
}

#[tokio::test]
async fn setup_with_avatar_stores_blob_and_attachment() {
    use topcamp_storage::{BlobStore, S3BlobStore, S3Config};
    let pool = pool().await;
    let install = fresh_install(&pool).await;
    let _ = topcamp_web::first_run::init_store(
        S3BlobStore::new(&S3Config::dev()).expect("test store builds"),
    );
    let email = "first-run-avatar@example.com";
    cleanup_setup(&pool, email).await;
    let png = tiny_png();
    let response = app(pool.clone())
        .handle(setup_post(
            &[
                ("user[name]", "Av Atar"),
                ("user[email_address]", email),
                ("user[password]", "s3cret-avatar"),
            ],
            Some(("user[avatar]", "avatar.png", &png)),
        ))
        .await;
    assert_eq!(response.status(), 303);

    let uid: i64 = sqlx::query_scalar("SELECT id FROM users WHERE email_address = $1")
        .bind(email)
        .fetch_one(&pool)
        .await
        .unwrap();
    let blob: (i64, String, String, i64, String) = sqlx::query_as(
        "SELECT b.id, b.key, b.filename, b.byte_size, b.checksum FROM active_storage_blobs b JOIN active_storage_attachments a ON a.blob_id = b.id WHERE a.record_type = 'User' AND a.record_id = $1 AND a.name = 'avatar'",
    )
    .bind(uid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(blob.1.len(), 28, "base36 key: {}", blob.1);
    assert_eq!(blob.2, "avatar.png");
    assert_eq!(blob.3, png.len() as i64);
    assert!(!blob.4.is_empty(), "checksum recorded");

    // Bytes landed in RustFS under the key.
    let store = S3BlobStore::new(&S3Config::dev()).expect("test store builds");
    let stored = store
        .get(&blob.1)
        .await
        .expect("head object")
        .expect("object exists");
    assert_eq!(stored, png);

    // Analysis was enqueued for the worker relay.
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox WHERE topic = 'process_attachment' AND payload = $1::jsonb",
    )
    .bind(format!("{{\"blob_id\":{}}}", blob.0))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1);

    store.delete(&blob.1).await.expect("delete test object");
    cleanup_setup(&pool, email).await;
    install.restore().await;
}

#[tokio::test]
async fn repeat_visits_redirect_home() {
    let pool = pool().await;
    let _guard = serial().lock().await;
    // Self-seeded install row: this database is ours alone.
    sqlx::query("DELETE FROM accounts")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO accounts (created_at, updated_at, join_code, name, singleton_guard) VALUES (now(), now(), 'test-test-test0', 'Topcamp', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    // The guard runs before CSRF: no token needed for the redirect.
    let get = http::Request::builder()
        .method("GET")
        .uri("/first_run")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(get).await;
    assert_eq!(response.status(), 303);
    assert_eq!(response.headers().get("location").unwrap(), "/");

    let post = http::Request::builder()
        .method("POST")
        .uri("/first_run")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(multipart_body(&[], None)))
        .expect("request builds");
    let response = app(pool.clone()).handle(post).await;
    assert_eq!(response.status(), 303);
    assert_eq!(response.headers().get("location").unwrap(), "/");
}

#[tokio::test]
async fn setup_without_name_is_500_and_leaves_no_account() {
    let pool = pool().await;
    let install = fresh_install(&pool).await;
    let response = app(pool.clone())
        .handle(setup_post(
            &[
                ("user[email_address]", "noname@example.com"),
                ("user[password]", "s3cret"),
            ],
            None,
        ))
        .await;
    assert_eq!(response.status(), 500);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "no partial account row");
    install.restore().await;
}

#[tokio::test]
async fn setup_without_csrf_is_forbidden() {
    let pool = pool().await;
    let install = fresh_install(&pool).await;
    let request = http::Request::builder()
        .method("POST")
        .uri("/first_run")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(multipart_body(
            &[
                ("authenticity_token", "deadbeef"),
                ("user[name]", "Csrf Test"),
            ],
            None,
        )))
        .expect("request builds");
    let response = app(pool).handle(request).await;
    assert_eq!(response.status(), 403);
    install.restore().await;
}
