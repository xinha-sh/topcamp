//! PWA (manifest + service worker + install prompt) tests against
//! PostgreSQL and the real router.
//!
//! Requires `DATABASE_URL` (default: the isolated `topcamp_test_pwa`
//! database). Bootstrap once with:
//! `psql -c "CREATE DATABASE topcamp_test_pwa;"` then apply
//! `migrations/*.sql` in order. Tests run in parallel with unique
//! emails; nothing here empties shared tables.

use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::UserRepository;
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://topcamp:topcamp@localhost:5432/topcamp_test_pwa".to_string()
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
    let bytes = topcoat::router::to_bytes(response.into_body(), usize::MAX)
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

fn get_request(
    uri: &str,
    jar: &str,
    user_agent: Option<&str>,
    host: Option<&str>,
) -> http::Request<Body> {
    let mut builder = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", jar);
    if let Some(ua) = user_agent {
        builder = builder.header("user-agent", ua);
    }
    if let Some(host) = host {
        builder = builder.header("host", host);
    }
    builder.body(Body::empty()).expect("request builds")
}

#[tokio::test]
async fn manifest_serves_account_json() {
    let pool = pool().await;
    seed_account(&pool).await;
    for uri in ["/webmanifest", "/webmanifest.json"] {
        // Signed out: `allow_unauthenticated_access`.
        let response = app(pool.clone())
            .handle(get_request(uri, "", None, Some("topcamp.test")))
            .await;
        assert_eq!((uri, response.status().as_u16()), (uri, 200));
        assert_eq!(
            header(&response, "content-type"),
            "application/json; charset=utf-8"
        );
        let body = body_text(response).await;
        let manifest: serde_json::Value = serde_json::from_str(&body).expect("manifest parses");
        assert_eq!(manifest["name"], "Topcamp");
        assert_eq!(manifest["start_url"], "/");
        assert_eq!(manifest["display"], "standalone");
        assert_eq!(manifest["scope"], "/");
        // Logo icons stay relative, with the account's cache key.
        let icons = manifest["icons"].as_array().expect("icons array");
        assert_eq!(icons.len(), 3);
        let small = icons[0]["src"].as_str().expect("small src");
        assert!(small.starts_with("/account/logo?size=small&v="), "{small}");
        assert_eq!(icons[0]["sizes"], "192x192");
        assert!(
            icons[1]["src"]
                .as_str()
                .unwrap()
                .starts_with("/account/logo?v=")
        );
        assert_eq!(icons[2]["purpose"], "maskable");
        // Shortcuts and screenshots are absolute asset URLs.
        let shortcuts = manifest["shortcuts"].as_array().expect("shortcuts");
        assert_eq!(shortcuts[0]["url"], "rooms/opens/new");
        let add = shortcuts[0]["icons"][0]["src"].as_str().expect("add src");
        assert!(add.starts_with("http://topcamp.test/"), "{add}");
        assert!(add.contains("add-"), "{add}");
        let person = shortcuts[1]["icons"][0]["src"]
            .as_str()
            .expect("person src");
        assert!(person.contains("person-"), "{person}");
        let shots = manifest["screenshots"].as_array().expect("screenshots");
        assert_eq!(shots.len(), 3);
        assert!(shots[0]["src"].as_str().unwrap().contains("android-chat-"));
        assert!(
            shots[1]["src"]
                .as_str()
                .unwrap()
                .contains("android-sidebar-")
        );
        assert!(
            shots[2]["src"]
                .as_str()
                .unwrap()
                .contains("android-dark-mode-")
        );
        // Query strings are raw `&`, not `&amp;` (valid JSON).
        assert!(!body.contains("&amp;"), "{body:.200}");
        assert!(body.ends_with("}\n"));
    }
}

#[tokio::test]
async fn manifest_without_host_stays_relative() {
    let pool = pool().await;
    seed_account(&pool).await;
    let response = app(pool.clone())
        .handle(get_request("/webmanifest.json", "", None, None))
        .await;
    assert_eq!(response.status(), 200);
    let body = body_text(response).await;
    let manifest: serde_json::Value = serde_json::from_str(&body).expect("parses");
    let add = manifest["shortcuts"][0]["icons"][0]["src"]
        .as_str()
        .expect("add src");
    assert!(add.starts_with("/_topcoat/assets/add-"), "{add}");
}

#[tokio::test]
async fn service_worker_serves_push_script() {
    let pool = pool().await;
    seed_account(&pool).await;
    for uri in ["/service-worker", "/service-worker.js"] {
        let response = app(pool.clone())
            .handle(get_request(uri, "", None, None))
            .await;
        assert_eq!((uri, response.status().as_u16()), (uri, 200));
        assert_eq!(
            header(&response, "content-type"),
            "text/javascript; charset=utf-8"
        );
        let body = body_text(response).await;
        for needle in [
            "addEventListener(\"push\"",
            "showNotification",
            "setAppBadge",
            "addEventListener(\"notificationclick\"",
            "clients.matchAll",
        ] {
            assert!(body.contains(needle), "{uri}: {body:.200}");
        }
    }
}

#[tokio::test]
async fn layout_links_manifest() {
    let pool = pool().await;
    seed_account(&pool).await;
    let response = app(pool.clone())
        .handle(get_request("/session/new", "", None, None))
        .await;
    let body = body_text(response).await;
    assert!(
        body.contains("<link rel=\"manifest\" href=\"/webmanifest.json\""),
        "{body:.500}"
    );
}

#[tokio::test]
async fn no_offline_page_upstream_has_none() {
    let pool = pool().await;
    seed_account(&pool).await;
    let response = app(pool.clone())
        .handle(get_request("/offline.html", "", None, None))
        .await;
    assert_eq!(response.status(), 404);
}

// Upstream's golden user agents (`tests/golden/a/facts.json`) and the
// install branch each renders: `None` renders nothing.
const INSTALL_CASES: &[(&str, &str, Option<&str>)] = &[
    (
        "chrome_mac",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36",
        None,
    ),
    (
        "chrome_windows",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36",
        None,
    ),
    (
        "safari_mac",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15",
        Some("Add to Dock"),
    ),
    (
        "safari_ios",
        "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1",
        Some("Add to Home Screen"),
    ),
    (
        "chrome_android",
        "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Mobile Safari/537.36",
        None,
    ),
    (
        "firefox_mac",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.5; rv:131.0) Gecko/20100101 Firefox/131.0",
        None,
    ),
    (
        "firefox_android",
        "Mozilla/5.0 (Android 14; Mobile; rv:131.0) Gecko/131.0 Firefox/131.0",
        Some("Install</em> in the menu"),
    ),
    (
        "edge_windows",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0",
        None,
    ),
    (
        "legacy_edge",
        "Mozilla/5.0 (Windows NT 10.0) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/46.0.2486.0 Safari/537.36 Edge/13.10586",
        Some("in the address bar"),
    ),
    ("curl", "curl/8.4.0", Some("Some platforms require")),
];

#[tokio::test]
async fn install_prompt_branches_by_platform() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (_id, email) = seed_user(&pool, "PWA Install").await;
    let jar = login(&pool, &email).await;
    for (name, ua, marker) in INSTALL_CASES {
        let response = app(pool.clone())
            .handle(get_request("/users/me/profile", &jar, Some(ua), None))
            .await;
        assert_eq!((name, response.status().as_u16()), (name, 200));
        let body = body_text(response).await;
        match marker {
            None => assert!(
                !body.contains("pwa__instructions"),
                "{name} renders no prompt"
            ),
            Some(marker) => {
                assert!(body.contains("pwa__instructions"), "{name}");
                assert!(body.contains("Install Topcamp as a web app."), "{name}");
                assert!(body.contains(marker), "{name}: {body:.400}");
                // No-JS experiment: static instructions, no Stimulus hook.
                assert!(!body.contains("pwa-install#"), "{name}");
                assert!(body.contains("Install now"), "{name}");
            }
        }
    }
}

#[tokio::test]
async fn link_previews_get_the_topcamp_titled_block_page() {
    // Apple Messages previews ride an old Safari UA the gem doesn't
    // call a bot, so the gate blocks them — but with the plain
    // "Topcamp" title instead of "Unsupported browser".
    let pool = pool().await;
    seed_account(&pool).await;
    let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_11_1) AppleWebKit/601.2.4 (KHTML, like Gecko) Version/9.0.1 Safari/601.2.4 facebookexternalhit/1.1 Facebot Twitterbot/1.0";
    let response = app(pool.clone())
        .handle(get_request("/session/new", "", Some(ua), None))
        .await;
    assert_eq!(response.status(), 200);
    let body = body_text(response).await;
    assert!(body.contains("Upgrade to a supported web browser"));
    assert!(body.contains("<title>Topcamp</title>"), "{body:.300}");
}

#[tokio::test]
async fn install_prompt_sits_atop_the_profile() {
    let pool = pool().await;
    seed_account(&pool).await;
    let (_id, email) = seed_user(&pool, "PWA Order").await;
    let jar = login(&pool, &email).await;
    let safari = INSTALL_CASES[2].1;
    let response = app(pool.clone())
        .handle(get_request("/users/me/profile", &jar, Some(safari), None))
        .await;
    let body = body_text(response).await;
    let prompt = body.find("pwa__instructions").expect("prompt renders");
    let avatar = body.find("avatar__form").expect("avatar form renders");
    assert!(prompt < avatar, "prompt opens the section");
}
