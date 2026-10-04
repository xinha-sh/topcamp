//! Users tests against PostgreSQL + the real router.
//!
//! Requires `DATABASE_URL` (default: the isolated
//! `topcamp_test_users` database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_users;"` then apply
//! `migrations/*.sql` in order (re-apply after migration changes).
//! Avatar uploads also need the dev RustFS (`S3Config::dev()`).

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::{RoomRepository, UserRepository};
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_users".to_string()
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

fn unique(tag: &str) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock runs forward")
        .as_nanos();
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("{tag}-{}-{nanos}-{seq}", std::process::id())
}

async fn seed_user(pool: &PgPool, tag: &str) -> (i64, String) {
    seed_user_with(pool, tag, 0).await
}

async fn seed_user_with(pool: &PgPool, tag: &str, role: i32) -> (i64, String) {
    let email = format!("{}@example.com", unique(tag));
    let digest = bcrypt::hash("s3cret", 4).expect("cost-4 digest builds");
    let db = PgDb::new(pool.clone());
    let id = UserRepository::create(&db, tag, &email, Some(&digest))
        .await
        .expect("seed user")
        .id;
    if role == 2 {
        UserRepository::mark_bot(&db, id).await.expect("seed bot");
    } else if role != 0 {
        UserRepository::set_role(&db, id, role)
            .await
            .expect("seed role");
    }
    sqlx::query("UPDATE users SET bio = $2 WHERE id = $1")
        .bind(id)
        .bind(format!("bio of {tag}"))
        .execute(pool)
        .await
        .unwrap();
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
    login_with(pool, email, "s3cret").await
}

async fn login_with(pool: &PgPool, email: &str, password: &str) -> String {
    let token = csrf_token();
    let body = format!(
        "email_address={}&password={}&authenticity_token={}",
        email.replace('@', "%40"),
        password,
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

async fn get(router: &Router, uri: &str, jar: &str) -> topcoat::router::response::Response {
    let request = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", jar)
        .body(Body::empty())
        .expect("request builds");
    router.handle(request).await
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

async fn body_text(response: topcoat::router::response::Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads")
            .to_vec(),
    )
    .expect("UTF-8")
}

async fn cleanup(pool: &PgPool, user_ids: &[i64], room_ids: &[i64]) {
    for message in
        sqlx::query_scalar::<_, i64>("SELECT id FROM messages WHERE creator_id = ANY($1)")
            .bind(user_ids)
            .fetch_all(pool)
            .await
            .unwrap()
    {
        sqlx::query("DELETE FROM boosts WHERE message_id = $1")
            .bind(message)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(message)
            .execute(pool)
            .await
            .unwrap();
    }
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
    for uid in user_ids {
        for blob in sqlx::query_scalar::<_, i64>(
            "DELETE FROM active_storage_attachments WHERE record_type = 'User' AND record_id = $1 RETURNING blob_id",
        )
        .bind(uid)
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
        for sql in [
            "DELETE FROM memberships WHERE user_id = $1",
            "DELETE FROM sessions WHERE user_id = $1",
            "DELETE FROM bans WHERE user_id = $1",
            "DELETE FROM push_subscriptions WHERE user_id = $1",
            "DELETE FROM searches WHERE user_id = $1",
        ] {
            sqlx::query(sql).bind(uid).execute(pool).await.unwrap();
        }
        sqlx::query("DELETE FROM outbox WHERE payload->>'user_id' = $1")
            .bind(uid.to_string())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(uid)
            .execute(pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn show_renders_panel_with_ping() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Viewer").await;
    let (other, _) = seed_user(&pool, "Target").await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let response = get(&app, &format!("/users/{other}"), &jar).await;
    assert_eq!(response.status(), 200);
    let page = body_text(response).await;
    assert!(page.contains("Target"), "{page}");
    assert!(page.contains("bio of Target"), "{page}");
    assert!(page.contains("Profile avatar"), "{page}");
    assert!(page.contains("/rooms/directs"), "{page}");
    // Non-admin: no email, no ban button.
    assert!(!page.contains("mailto:"), "{page}");
    assert!(!page.contains("/ban"), "{page}");
    cleanup(&pool, &[uid, other], &[]).await;
}

#[tokio::test]
async fn show_as_admin_reveals_email_and_ban() {
    let pool = pool().await;
    let (admin, email) = seed_user_with(&pool, "Admin", 1).await;
    let (other, other_email) = seed_user(&pool, "Target").await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let page = body_text(get(&app, &format!("/users/{other}"), &jar).await).await;
    assert!(page.contains(&format!("mailto:{other_email}")), "{page}");
    assert!(page.contains(&format!("/users/{other}/ban")), "{page}");
    assert!(page.contains("Ban Target"), "{page}");

    // Own page: edit-profile pencil, no ban row.
    let page = body_text(get(&app, &format!("/users/{admin}"), &jar).await).await;
    assert!(page.contains("/users/me/profile"), "{page}");
    assert!(page.contains("Edit my profile"), "{page}");
    assert!(!page.contains("/ban"), "{page}");
    cleanup(&pool, &[admin, other], &[]).await;
}

#[tokio::test]
async fn show_requires_login_and_known_user() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Viewer").await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    assert_eq!(get(&app, "/users/999999999", &jar).await.status(), 404);
    assert_eq!(get(&app, "/users/nope", &jar).await.status(), 404);

    let logged_out = http::Request::builder()
        .method("GET")
        .uri(format!("/users/{uid}"))
        .body(Body::empty())
        .expect("request builds");
    assert_eq!(app.handle(logged_out).await.status(), 303);
    cleanup(&pool, &[uid], &[]).await;
}

#[tokio::test]
async fn show_deactivated_and_bot_branches() {
    let pool = pool().await;
    let (admin, email) = seed_user_with(&pool, "Admin", 1).await;
    let (gone, _) = seed_user(&pool, "Gone").await;
    let (bot, _) = seed_user_with(&pool, "Botty", 2).await;
    let db = PgDb::new(pool.clone());
    UserRepository::deactivate(&db, gone)
        .await
        .expect("deactivate");
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let page = body_text(get(&app, &format!("/users/{gone}"), &jar).await).await;
    assert!(page.contains("no longer on this account"), "{page}");

    let page = body_text(get(&app, &format!("/users/{bot}"), &jar).await).await;
    assert!(page.contains("btn--primary"), "{page}");
    assert!(page.contains("/rooms/directs"), "{page}");
    cleanup(&pool, &[admin, gone, bot], &[]).await;
}

#[tokio::test]
async fn ban_records_ips_drops_sessions_and_enqueues() {
    let pool = pool().await;
    let (admin, email) = seed_user_with(&pool, "Admin", 1).await;
    let (other, other_email) = seed_user(&pool, "Troll").await;
    // Sessions with IPs (login rows carry none in tests).
    sqlx::query("INSERT INTO sessions(created_at,updated_at,last_active_at,user_id,token,ip_address) VALUES (now(),now(),now(),$1,'a','10.0.0.9'),(now(),now(),now(),$1,'b','10.0.0.9'),(now(),now(),now(),$1,'c','10.0.0.10')")
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let response = post_form(&app, "POST", &format!("/users/{other}/ban"), &jar, "").await;
    assert_eq!(response.status(), 303);

    let status: i32 = sqlx::query_scalar("SELECT status FROM users WHERE id = $1")
        .bind(other)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, 2);
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE user_id = $1")
        .bind(other)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 0);
    let mut ips: Vec<String> =
        sqlx::query_scalar("SELECT ip_address FROM bans WHERE user_id = $1 ORDER BY ip_address")
            .bind(other)
            .fetch_all(&pool)
            .await
            .unwrap();
    ips.sort();
    ips.dedup();
    assert_eq!(ips, vec!["10.0.0.10", "10.0.0.9"]);
    let topics: Vec<String> =
        sqlx::query_scalar("SELECT topic FROM outbox WHERE payload->>'user_id' = $1")
            .bind(other.to_string())
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        topics.contains(&"remove_banned_content".to_string()),
        "{topics:?}"
    );

    // Banned login fails; unban restores. Browsers POST with
    // `_method=delete` (no native DELETE), so that path must
    // dispatch to destroy — then raw DELETE for API clients.
    let response = post_form(
        &app,
        "POST",
        &format!("/users/{other}/ban"),
        &jar,
        "_method=delete",
    )
    .await;
    assert_eq!(response.status(), 303);
    let status: i32 = sqlx::query_scalar("SELECT status FROM users WHERE id = $1")
        .bind(other)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, 0);
    let response = post_form(&app, "POST", &format!("/users/{other}/ban"), &jar, "").await;
    assert_eq!(response.status(), 303);
    let response = post_form(&app, "DELETE", &format!("/users/{other}/ban"), &jar, "").await;
    assert_eq!(response.status(), 303);
    let status: i32 = sqlx::query_scalar("SELECT status FROM users WHERE id = $1")
        .bind(other)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, 0);
    let bans: i64 = sqlx::query_scalar("SELECT count(*) FROM bans WHERE user_id = $1")
        .bind(other)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(bans, 0);
    let _ = login(&pool, &other_email).await;
    cleanup(&pool, &[admin, other], &[]).await;
}

#[tokio::test]
async fn ban_requires_admin_and_target() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Member").await;
    let (other, _) = seed_user(&pool, "Target").await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let response = post_form(&app, "POST", &format!("/users/{other}/ban"), &jar, "").await;
    assert_eq!(response.status(), 403);

    let (admin, admin_email) = seed_user_with(&pool, "Admin", 1).await;
    let admin_jar = login(&pool, &admin_email).await;
    let response = post_form(&app, "POST", "/users/999999999/ban", &admin_jar, "").await;
    assert_eq!(response.status(), 404);

    // Unban via POST _method=delete.
    let response = post_form(
        &app,
        "POST",
        &format!("/users/{other}/ban"),
        &admin_jar,
        "_method=delete",
    )
    .await;
    assert_eq!(response.status(), 303);
    cleanup(&pool, &[uid, other, admin], &[]).await;
}

#[tokio::test]
async fn profile_shows_fields_and_memberships() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Member").await;
    let room_id = seed_room(&pool, uid).await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let page = body_text(get(&app, "/users/me/profile", &jar).await).await;
    assert!(page.contains("user[name]"), "{page}");
    assert!(page.contains("user[email_address]"), "{page}");
    assert!(page.contains("user[password]"), "{page}");
    assert!(page.contains("user[bio]"), "{page}");
    assert!(page.contains("user[avatar]"), "{page}");
    // No avatar attached: the delete button stays hidden.
    assert!(!page.contains("Delete avatar"), "{page}");
    assert!(page.contains("avatar__form"), "{page}");
    assert!(page.contains("Upload avatar"), "{page}");
    assert!(page.contains("membership-item"), "{page}");
    assert!(page.contains(&format!("/rooms/{room_id}")), "{page}");
    assert!(page.contains("Log out"), "{page}");
    assert!(page.contains("Enter your name"), "{page}");
    assert!(page.contains("Change password"), "{page}");
    assert!(page.contains("A few words about yourself"), "{page}");
    assert!(page.contains("colorize--black"), "{page}");
    cleanup(&pool, &[uid], &[room_id]).await;
}

#[tokio::test]
async fn profile_update_changes_fields_and_password() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Member").await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let response = post_form(
        &app,
        "PATCH",
        "/users/me/profile",
        &jar,
        "user[name]=Renamed&user[bio]=new+bio&_method=patch",
    )
    .await;
    assert_eq!(response.status(), 303);
    let page = body_text(get(&app, "/users/me/profile", &jar).await).await;
    assert!(page.contains("value=\"Renamed\""), "{page}");
    assert!(page.contains("new bio"), "{page}");

    // Blank name keeps the stored one; blank password keeps the digest.
    let response = post_form(
        &app,
        "POST",
        "/users/me/profile",
        &jar,
        "user[name]=++&user[password]=&_method=patch",
    )
    .await;
    assert_eq!(response.status(), 303);
    let name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "Renamed");
    let _ = login_with(&pool, &email, "s3cret").await;

    // New password takes effect.
    let response = post_form(
        &app,
        "POST",
        "/users/me/profile",
        &jar,
        "user[password]=n3wpass&_method=patch",
    )
    .await;
    assert_eq!(response.status(), 303);
    let _ = login_with(&pool, &email, "n3wpass").await;
    cleanup(&pool, &[uid], &[]).await;
}

#[tokio::test]
async fn profile_avatar_upload_and_remove() {
    init_store();
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Member").await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let png = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xFF, 0xFF, 0x3F, 0x00, 0x05, 0xFE, 0x02, 0xFE, 0xDC, 0xCC, 0x59, 0xE7, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    let boundary = "----profiletest";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"user[avatar]\"; filename=\"me.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&png);
    body.extend_from_slice(
        format!(
            "\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"_method\"\r\n\r\npatch\r\n--{boundary}--\r\n"
        )
        .as_bytes(),
    );
    let token = csrf_token();
    let request = http::Request::builder()
        .method("POST")
        .uri("/users/me/profile")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .header("x-csrf-token", token.clone())
        .body(Body::from(body))
        .expect("upload builds");
    // Header CSRF is a messages-only path: this must fail closed.
    assert_eq!(app.handle(request).await.status(), 403);

    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"user[avatar]\"; filename=\"me.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&png);
    body.extend_from_slice(
        format!(
            "\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"authenticity_token\"\r\n\r\n{token}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"_method\"\r\n\r\npatch\r\n--{boundary}--\r\n"
        )
        .as_bytes(),
    );
    let request = http::Request::builder()
        .method("POST")
        .uri("/users/me/profile")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("cookie", format!("{jar}; csrf_token={token}"))
        .body(Body::from(body))
        .expect("upload builds");
    assert_eq!(app.handle(request).await.status(), 303);

    let page = body_text(get(&app, "/users/me/profile", &jar).await).await;
    assert!(
        page.contains("/users/") && page.contains("/avatar?v="),
        "{page}"
    );
    // Avatar attached: the icon-only delete button appears.
    assert!(page.contains("Delete avatar"), "{page}");
    assert!(page.contains("avatar__delete-btn"), "{page}");

    // Remove the avatar (DELETE via the token path).
    let token_path = page
        .split("\"")
        .find(|part| part.starts_with("/users/") && part.ends_with("/avatar"))
        .expect("avatar delete action")
        .to_string();
    let response = post_form(&app, "POST", &token_path, &jar, "_method=delete").await;
    assert_eq!(response.status(), 303);
    let attached: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM active_storage_attachments WHERE record_type = 'User' AND record_id = $1",
    )
    .bind(uid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attached, 0);
    cleanup(&pool, &[uid], &[]).await;
}

#[tokio::test]
async fn account_users_fragment_pages_slices() {
    let pool = pool().await;
    let (admin, email) = seed_user_with(&pool, "Admin", 1).await;
    let (other, _) = seed_user(&pool, "Target").await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let page = body_text(get(&app, "/account/users?page=1", &jar).await).await;
    assert!(page.contains("Target"), "{page}");
    assert!(!page.contains("Show more people"), "{page}");

    let page = body_text(get(&app, "/account/users?page=2", &jar).await).await;
    assert!(!page.contains("Target"), "{page}");

    // Members may page too.
    let (member, member_email) = seed_user(&pool, "Member").await;
    let member_jar = login(&pool, &member_email).await;
    assert_eq!(get(&app, "/account/users", &member_jar).await.status(), 200);
    cleanup(&pool, &[admin, other, member], &[]).await;
}

#[tokio::test]
async fn account_users_next_link_targets_full_edit_page() {
    let pool = pool().await;
    let (admin, email) = seed_user_with(&pool, "Admin", 1).await;
    let jar = login(&pool, &email).await;
    // 501 bulk users so page 1 grows a "Show more people" link.
    let tag = unique("bulk");
    let mut ids: Vec<i64> = Vec::new();
    for i in 0..501 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO users (created_at, updated_at, name, email_address, role, status)
             VALUES (now(), now(), $1, $2, 0, 0) RETURNING id",
        )
        .bind(format!("{tag}-{i}"))
        .bind(format!("{tag}-{i}@example.com"))
        .fetch_one(&pool)
        .await
        .unwrap();
        ids.push(id);
    }
    let app = app(pool.clone());
    let page = body_text(get(&app, "/account/users?page=1", &jar).await).await;
    assert!(
        page.contains("href=\"/account/edit?page=2\""),
        "next-page link must be the full edit page (no-JS fallback): {page}"
    );
    assert!(page.contains("Show more people"), "{page}");
    let mut all = ids;
    all.push(admin);
    cleanup(&pool, &all, &[]).await;
}

#[tokio::test]
async fn involvement_cycles_and_validates() {
    let pool = pool().await;
    let (uid, email) = seed_user(&pool, "Member").await;
    let room_id = seed_room(&pool, uid).await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let page = body_text(get(&app, &format!("/rooms/{room_id}/involvement"), &jar).await).await;
    assert!(page.contains("involvement=everything"), "{page}");

    let response = post_form(
        &app,
        "PUT",
        &format!("/rooms/{room_id}/involvement?involvement=everything"),
        &jar,
        "",
    )
    .await;
    assert_eq!(response.status(), 303);
    let level: String = sqlx::query_scalar(
        "SELECT involvement FROM memberships WHERE room_id = $1 AND user_id = $2",
    )
    .bind(room_id)
    .bind(uid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(level, "everything");

    let response = post_form(
        &app,
        "PUT",
        &format!("/rooms/{room_id}/involvement?involvement=bogus"),
        &jar,
        "",
    )
    .await;
    assert_eq!(response.status(), 500);

    // Non-members 404.
    let (outsider, outsider_email) = seed_user(&pool, "Outsider").await;
    let outsider_jar = login(&pool, &outsider_email).await;
    assert_eq!(
        get(
            &app,
            &format!("/rooms/{room_id}/involvement"),
            &outsider_jar
        )
        .await
        .status(),
        404
    );
    cleanup(&pool, &[uid, outsider], &[room_id]).await;
}

#[tokio::test]
async fn deactivate_drops_rows_and_mangles_email() {
    let pool = pool().await;
    let (admin, email) = seed_user_with(&pool, "Admin", 1).await;
    let (other, other_email) = seed_user(&pool, "Leaver").await;
    let room_id = seed_room(&pool, other).await;
    let jar = login(&pool, &email).await;
    let app = app(pool.clone());

    let response = post_form(&app, "DELETE", &format!("/account/users/{other}"), &jar, "").await;
    assert_eq!(response.status(), 303);
    let row: (i32, Option<String>) =
        sqlx::query_as("SELECT status, email_address FROM users WHERE id = $1")
            .bind(other)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0, 1);
    assert!(
        row.1.as_deref().unwrap_or("").contains("-deactivated-"),
        "{row:?}"
    );
    assert!(!row.1.as_deref().unwrap_or("").starts_with(&other_email));
    let memberships: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memberships WHERE user_id = $1")
            .bind(other)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(memberships, 0);
    cleanup(&pool, &[admin, other], &[room_id]).await;
}
