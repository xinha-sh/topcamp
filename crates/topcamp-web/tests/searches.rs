//! Searches page, autocomplete, mentions, and unfurl tests.
//!
//! Requires `DATABASE_URL` (default: the isolated `topcamp_test_searches`
//! database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_searches;"` then apply
//! `migrations/*.sql` in order.

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::{NewMessage, RoomRepository, UserRepository};
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router, to_bytes};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_searches".to_string()
    });
    PgPool::connect(&url).await.expect("connect DATABASE_URL")
}

fn app(pool: PgPool) -> Router {
    router(AppState::new(PgDb::new(pool), Cable::new()))
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

async fn seed_message(pool: &PgPool, room_id: i64, uid: i64, tag: &str, body: &str) -> i64 {
    let db = PgDb::new(pool.clone());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock runs forward")
        .as_nanos();
    db.post_message(NewMessage {
        room_id,
        creator_id: uid,
        client_message_id: format!("{tag}-{nanos}"),
        body: topcamp_web::richtext::canonicalize_plain(body),
    })
    .await
    .expect("seed message")
    .id
}

/// Seed a message with a verbatim (rich HTML) body.
async fn seed_rich_message(pool: &PgPool, room_id: i64, uid: i64, tag: &str, body: &str) -> i64 {
    let db = PgDb::new(pool.clone());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock runs forward")
        .as_nanos();
    db.post_message(NewMessage {
        room_id,
        creator_id: uid,
        client_message_id: format!("{tag}-{nanos}"),
        body: body.to_string(),
    })
    .await
    .expect("seed rich message")
    .id
}

async fn seed_account(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO accounts (created_at, updated_at, join_code, name) \
         VALUES (now(), now(), 'search-test-join', 'Topcamp') ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
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

async fn cleanup(pool: &PgPool, users: &[i64], rooms: &[i64], messages: &[i64]) {
    if !messages.is_empty() {
        sqlx::query("DELETE FROM messages WHERE id = ANY($1)")
            .bind(messages)
            .execute(pool)
            .await
            .unwrap();
    }
    if !rooms.is_empty() {
        sqlx::query("DELETE FROM memberships WHERE room_id = ANY($1)")
            .bind(rooms)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM rooms WHERE id = ANY($1)")
            .bind(rooms)
            .execute(pool)
            .await
            .unwrap();
    }
    for uid in users {
        sqlx::query("DELETE FROM searches WHERE user_id = $1")
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
}

#[tokio::test]
async fn searches_page_renders_empty_state() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Seeker").await;
    let jar = login(&pool, &email).await;

    let response = get(&pool, "/searches", &jar).await;
    assert_eq!(response.status(), 200);
    let page = body_text(response).await;
    assert!(page.contains("Exit search"), "{page}");
    assert!(page.contains("searches__input"), "{page}");
    assert!(page.contains("message-area--empty"), "{page}");
    assert!(page.contains("search-results"), "{page}");
    assert!(!page.contains("searches__query"), "{page}");

    cleanup(&pool, &[uid], &[], &[]).await;
}

#[tokio::test]
async fn searches_requires_login() {
    let pool = pool().await;
    let response = get(&pool, "/searches", "").await;
    assert_eq!(response.status(), 303);
}

#[tokio::test]
async fn create_records_and_redirects_to_results() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Seeker").await;
    let jar = login(&pool, &email).await;

    let response = post_form(&pool, "POST", "/searches", &jar, "q=hello+world").await;
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert_eq!(location, "/searches?q=hello+world", "{location}");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM searches WHERE user_id = $1")
        .bind(uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    cleanup(&pool, &[uid], &[], &[]).await;
}

#[tokio::test]
async fn create_sanitizes_punctuation_to_spaces() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Seeker").await;
    let jar = login(&pool, &email).await;

    let response = post_form(&pool, "POST", "/searches", &jar, "q=hi%21there%3F").await;
    assert_eq!(response.status(), 303);
    let query: String = sqlx::query_scalar("SELECT query FROM searches WHERE user_id = $1")
        .bind(uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    // Verbatim upstream: punctuation becomes spaces, untrimmed.
    assert_eq!(query, "hi there ");

    cleanup(&pool, &[uid], &[], &[]).await;
}

#[tokio::test]
async fn create_blank_query_skips_recording() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Seeker").await;
    let jar = login(&pool, &email).await;

    let response = post_form(&pool, "POST", "/searches", &jar, "q=%20%20").await;
    assert_eq!(response.status(), 303);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM searches WHERE user_id = $1")
        .bind(uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    cleanup(&pool, &[uid], &[], &[]).await;
}

#[tokio::test]
async fn index_shows_only_reachable_hits_with_chip() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Seeker").await;
    let (other, _) = seed_user(&pool, "Other").await;
    let room = seed_room(&pool, uid).await;
    let hidden = seed_room(&pool, other).await;
    // One alphanumeric token: hyphens make the english parser mint
    // signed-number lexemes that plain queries never match.
    let term = unique("quarry").replace('-', "x");
    let hit = seed_message(&pool, room, uid, "hit", &format!("found the {term} here")).await;
    let miss = seed_message(&pool, room, uid, "miss", "nothing relevant").await;
    let unreachable = seed_message(&pool, hidden, other, "hidden", &format!("hidden {term}")).await;
    let jar = login(&pool, &email).await;

    let response = get(&pool, &format!("/searches?q={term}"), &jar).await;
    assert_eq!(response.status(), 200);
    let page = body_text(response).await;
    assert!(page.contains("searches__query"), "{page}");
    assert!(
        page.contains(&format!("data-message-id=\"{hit}\"")),
        "{page}"
    );
    assert!(
        !page.contains(&format!("data-message-id=\"{miss}\"")),
        "{page}"
    );
    assert!(
        !page.contains(&format!("data-message-id=\"{unreachable}\"")),
        "{page}"
    );

    cleanup(
        &pool,
        &[uid, other],
        &[room, hidden],
        &[hit, miss, unreachable],
    )
    .await;
}

#[tokio::test]
async fn index_orders_hits_oldest_first() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Seeker").await;
    let room = seed_room(&pool, uid).await;
    // One alphanumeric token (see the hyphen note above).
    let term = unique("sequence").replace('-', "x");
    let first = seed_message(&pool, room, uid, "a", &format!("{term} one")).await;
    let second = seed_message(&pool, room, uid, "b", &format!("{term} two")).await;
    let third = seed_message(&pool, room, uid, "c", &format!("{term} three")).await;
    let jar = login(&pool, &email).await;

    let response = get(&pool, &format!("/searches?q={term}"), &jar).await;
    let page = body_text(response).await;
    let positions = [first, second, third].map(|id| {
        page.find(&format!("data-message-id=\"{id}\""))
            .unwrap_or(usize::MAX)
    });
    assert!(
        positions[0] < positions[1] && positions[1] < positions[2],
        "{positions:?}"
    );

    cleanup(&pool, &[uid], &[room], &[first, second, third]).await;
}

#[tokio::test]
async fn record_trims_to_ten_and_retouches() {
    use topcamp_db::repositories::SearchRepository;
    let pool = pool().await;
    let (uid, _) = seed_user(&pool, "Seeker").await;
    let db = PgDb::new(pool.clone());
    for i in 0..11 {
        SearchRepository::record(&db, uid, &format!("query-{i}"))
            .await
            .unwrap();
    }
    let rows = SearchRepository::ordered(&db, uid).await.unwrap();
    assert_eq!(rows.len(), 10);
    assert_eq!(rows[0].query, "query-10");
    assert!(!rows.iter().any(|row| row.query == "query-0"));

    // Re-recording touches it back to the front without duplicating.
    SearchRepository::record(&db, uid, "query-1").await.unwrap();
    let rows = SearchRepository::ordered(&db, uid).await.unwrap();
    assert_eq!(rows.len(), 10);
    assert_eq!(rows[0].query, "query-1");

    cleanup(&pool, &[uid], &[], &[]).await;
}

#[tokio::test]
async fn clear_destroys_recents_by_delete_and_post() {
    use topcamp_db::repositories::SearchRepository;
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Seeker").await;
    let jar = login(&pool, &email).await;
    let db = PgDb::new(pool.clone());
    SearchRepository::record(&db, uid, "one").await.unwrap();
    SearchRepository::record(&db, uid, "two").await.unwrap();

    let response = post_form(&pool, "POST", "/searches/clear", &jar, "_method=delete").await;
    assert_eq!(response.status(), 303);
    let rows = SearchRepository::ordered(&db, uid).await.unwrap();
    assert!(rows.is_empty());

    SearchRepository::record(&db, uid, "three").await.unwrap();
    let response = post_form(&pool, "DELETE", "/searches/clear", &jar, "").await;
    assert_eq!(response.status(), 303);
    let rows = SearchRepository::ordered(&db, uid).await.unwrap();
    assert!(rows.is_empty());

    cleanup(&pool, &[uid], &[], &[]).await;
}

#[tokio::test]
async fn autocomplete_html_returns_prompt_items() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Mentionable").await;
    let jar = login(&pool, &email).await;

    let response = get(&pool, "/autocompletable/users", &jar).await;
    assert_eq!(response.status(), 200);
    let page = body_text(response).await;
    assert!(page.contains("lexxy-prompt-item"), "{page}");
    assert!(page.contains("Mentionable"), "{page}");
    assert!(page.contains("User/"), "{page}");
    assert!(page.contains("type=\"editor\""), "{page}");
    assert!(page.contains("class=\"mention\""), "{page}");
    assert!(
        page.contains("/users/") && page.contains("/avatar?v="),
        "{page}"
    );

    cleanup(&pool, &[uid], &[], &[]).await;
}

#[tokio::test]
async fn autocomplete_filters_scopes_and_pages() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Zelda").await;
    let (narrow, _) = seed_user(&pool, "Narrow Guest").await;
    let room = seed_room(&pool, uid).await;
    let jar = login(&pool, &email).await;

    // `filter` narrows by name substring.
    let page = body_text(get(&pool, "/autocompletable/users?filter=Zeld", &jar).await).await;
    assert!(page.contains("Zelda"), "{page}");
    assert!(!page.contains("Narrow Guest"), "{page}");

    // `query` is the autocomplete-input alias.
    let page = body_text(get(&pool, "/autocompletable/users?query=Narrow", &jar).await).await;
    assert!(page.contains("Narrow Guest"), "{page}");

    // LIKE metacharacters match literally, not as wildcards.
    let page = body_text(get(&pool, "/autocompletable/users?filter=%25", &jar).await).await;
    assert!(!page.contains("lexxy-prompt-item"), "{page}");

    // Room scope keeps members, drops outsiders.
    let page = body_text(
        get(
            &pool,
            &format!("/autocompletable/users?room_id={room}"),
            &jar,
        )
        .await,
    )
    .await;
    assert!(page.contains("Zelda"), "{page}");
    assert!(!page.contains("Narrow Guest"), "{page}");

    // A room the seeker cannot see 404s (upstream `.find`).
    let stranger_room = seed_room(&pool, narrow).await;
    let response = get(
        &pool,
        &format!("/autocompletable/users?room_id={stranger_room}"),
        &jar,
    )
    .await;
    assert_eq!(response.status(), 404);

    cleanup(&pool, &[uid, narrow], &[room, stranger_room], &[]).await;
}

#[tokio::test]
async fn autocomplete_json_shape_has_absolute_avatars() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Json User").await;
    let jar = login(&pool, &email).await;

    let request = http::Request::builder()
        .method("GET")
        .uri("/autocompletable/users?filter=Json")
        .header("cookie", &jar)
        .header("accept", "application/json")
        .header("host", "topcamp.example")
        .body(Body::empty())
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 200);
    let body = body_text(response).await;
    let items: Vec<serde_json::Value> = serde_json::from_str(&body).expect("JSON parses");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["value"], uid);
    assert_eq!(items[0]["name"], "Json User");
    assert!(
        items[0]["avatar_url"]
            .as_str()
            .unwrap_or("")
            .starts_with("http://topcamp.example/users/"),
        "{body}"
    );
    assert!(
        items[0]["sgid"].as_str().unwrap_or("").starts_with("User/"),
        "{body}"
    );

    cleanup(&pool, &[uid], &[], &[]).await;
}

#[tokio::test]
async fn autocomplete_requires_login() {
    let pool = pool().await;
    let response = get(&pool, "/autocompletable/users", "").await;
    assert_eq!(response.status(), 303);
}

#[tokio::test]
async fn mention_span_renders_live_and_flags_mentioned() {
    let _ = topcamp_web::users::init_secret(b"search-test-secret".to_vec());
    let pool = pool().await;
    seed_account(&pool).await;
    let (author, author_email) = seed_user(&pool, "Author").await;
    let (target, target_email) = seed_user(&pool, "Target Person").await;
    let room = seed_room(&pool, author).await;
    seed_member(&pool, room, target).await;
    let sgid = topcamp_web::users::mention_sgid(target);
    let id = seed_rich_message(
        &pool,
        room,
        author,
        "mention",
        &format!("<div>ping <span class=\"mention\" sgid=\"{sgid}\">stale</span> now</div>"),
    )
    .await;

    // The mentioned user sees the live mention + the flagged row.
    let jar = login(&pool, &target_email).await;
    let response = get(&pool, &format!("/rooms/{room}"), &jar).await;
    assert_eq!(response.status(), 200);
    let page = body_text(response).await;
    assert!(page.contains("class=\"mention\""), "{page}");
    assert!(page.contains("Target Person"), "{page}");
    assert!(!page.contains("stale"), "{page}");
    // Leading space: the server pre-renders the mentioned class
    // on every room page (no-JS experiment, no Stimulus hooks).
    assert!(page.contains(" message--mentioned"), "{page}");

    // Anyone else sees the mention without the flag.
    let jar = login(&pool, &author_email).await;
    let page = body_text(get(&pool, &format!("/rooms/{room}"), &jar).await).await;
    assert!(page.contains("class=\"mention\""), "{page}");
    assert!(!page.contains(" message--mentioned"), "{page}");

    cleanup(&pool, &[author, target], &[room], &[id]).await;
}

#[tokio::test]
async fn unfurl_rejects_private_and_media_urls() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Seeker").await;
    let jar = login(&pool, &email).await;

    for url in [
        "http%3A%2F%2F127.0.0.1%2F",
        "http%3A%2F%2F10.0.0.9%2Fadmin",
        "https%3A%2F%2Fexample.com%2Fphoto.png",
        "https%3A%2F%2Fexample.com%2Fmovie.mp4%3Fdl%3D1",
    ] {
        let response = post_form(&pool, "POST", "/unfurl_link", &jar, &format!("url={url}")).await;
        assert_eq!(response.status(), 204, "{url}");
    }

    cleanup(&pool, &[uid], &[], &[]).await;
}

#[tokio::test]
async fn unfurl_raises_on_mailto_and_treats_collections_as_empty() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Seeker").await;
    let jar = login(&pool, &email).await;
    let token = csrf_token();

    // A `mailto:` URL without an address raises (500), like the reference.
    let response = post_form(&pool, "POST", "/unfurl_link", &jar, "url=mailto%3Afoo").await;
    assert_eq!(response.status(), 500);

    // A hash or array passes `require` but isn't a string: 204 either way.
    for body in [
        "url%5Ba%5D=http%3A%2F%2Fexample.com",
        "url%5B%5D=http%3A%2F%2Fexample.com",
    ] {
        let response = post_form(&pool, "POST", "/unfurl_link", &jar, body).await;
        assert_eq!(response.status(), 204, "{body}");
    }
    for body in ["{\"url\": {\"a\": 1}}", "{\"url\": [1]}"] {
        let request = http::Request::builder()
            .method("POST")
            .uri("/unfurl_link")
            .header("content-type", "application/json")
            .header("cookie", format!("{jar}; csrf_token={token}"))
            .header("x-csrf-token", token.clone())
            .body(Body::from(body))
            .expect("request builds");
        let response = app(pool.clone()).handle(request).await;
        assert_eq!(response.status(), 204, "{body}");
    }
    // Missing or blank JSON urls are still 400.
    for body in ["{}", "{\"url\": \"\"}", "{\"url\": null}"] {
        let request = http::Request::builder()
            .method("POST")
            .uri("/unfurl_link")
            .header("content-type", "application/json")
            .header("cookie", format!("{jar}; csrf_token={token}"))
            .header("x-csrf-token", token.clone())
            .body(Body::from(body))
            .expect("request builds");
        let response = app(pool.clone()).handle(request).await;
        assert_eq!(response.status(), 400, "{body}");
    }

    cleanup(&pool, &[uid], &[], &[]).await;
}

#[tokio::test]
async fn unfurl_requires_url_and_token() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (uid, email) = seed_user(&pool, "Seeker").await;
    let jar = login(&pool, &email).await;

    let response = post_form(&pool, "POST", "/unfurl_link", &jar, "").await;
    assert_eq!(response.status(), 400);

    let request = http::Request::builder()
        .method("POST")
        .uri("/unfurl_link")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{jar}; csrf_token=wrong"))
        .body(Body::from(
            "authenticity_token=wrong&url=https%3A%2F%2Fexample.com%2F",
        ))
        .expect("request builds");
    let response = app(pool.clone()).handle(request).await;
    assert_eq!(response.status(), 403);

    cleanup(&pool, &[uid], &[], &[]).await;
}
