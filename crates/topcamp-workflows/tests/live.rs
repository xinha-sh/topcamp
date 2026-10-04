//! Live workflow execution: DBOS + PostgreSQL + RustFS + stub HTTP.
//!
//! Every test launches a real DBOS executor against the scratch database,
//! runs a workflow end to end, and asserts durable effects (rows, objects,
//! replies, outcomes). The worker `OnceLock`s are initialized once per
//! binary; each test owns its rows/keys/ids so parallel execution is safe.
//!
//! Requires `DATABASE_URL` (migrated, incl. `0004_workflows.sql`) and
//! RustFS (`RUSTFS_*`, defaults to compose.yml).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use topcamp_db::outbox::OutboxEntry;
use topcamp_db::repositories::{AttachmentRepository, MessageRepository, NewBlob, NewMessage};
use topcamp_db::PgDb;
use topcamp_storage::{BlobStore, S3BlobStore, S3Config};
use topcamp_workflows::attachments::ProcessAttachment;
use topcamp_workflows::moderation::RemoveBannedContent;
use topcamp_workflows::notifications::SendNotification;
use topcamp_workflows::purge::{variant_key, PurgeBlob};
use topcamp_workflows::relay;
use topcamp_workflows::webhooks::DeliverWebhook;
use topcamp_workflows::worker;

// --- harness -------------------------------------------------------------

fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://topcamp:topcamp@localhost:5432/topcamp".to_string())
}

/// One multi-thread runtime for the whole binary, outliving every test.
/// `#[tokio::test]` runtimes die with their test, which would strand the
/// shared DBOS executor's background tasks — so tests are sync `#[test]`
/// and drive their futures through here instead.
fn run<F: std::future::Future>(future: F) -> F::Output {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("test runtime builds")
        })
        .block_on(future)
}

fn worker_handles() -> (PgDb, S3BlobStore) {
    static SHARED: std::sync::OnceLock<(PgDb, S3BlobStore)> = std::sync::OnceLock::new();
    SHARED
        .get_or_init(|| {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(40)
                .connect_lazy(&database_url())
                .expect("lazy pool builds");
            let db = PgDb::new(pool);
            let config = S3Config::dev();
            let store = S3BlobStore::new(&config).expect("store builds");
            let http = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(7))
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("http client builds");
            // First test wins; the rest reuse the same handles.
            let _ = worker::init(db.clone());
            let _ = worker::init_store(store.clone());
            let _ = worker::init_http(http);
            (db, store)
        })
        .clone()
}

/// One executor for the whole binary: parallel executors on a single
/// sysdb double-execute in-flight workflows (recovery races) and exhaust
/// connections. Production scales by adding processes; the test binary
/// models one process.
async fn launched() -> &'static (dbos::DBOS, relay::Handles) {
    static SHARED: tokio::sync::OnceCell<(dbos::DBOS, relay::Handles)> =
        tokio::sync::OnceCell::const_new();
    SHARED
        .get_or_init(|| async {
            let mut config = dbos::Config::new("topcamp-live", database_url());
            // Fixed version: one binary, one version.
            config.app_version = Some("live-test".to_string());
            // Parallel tests × step checkpoints share this pool.
            config.max_connections = 40;
            let dbos = dbos::DBOS::new(config);
            let handles = relay::register_all(&dbos).expect("register workflows");
            dbos.launch().await.expect("launch DBOS");
            (dbos, handles)
        })
        .await
}

async fn make_user(pool: &sqlx::PgPool, name: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO users(created_at,updated_at,name) VALUES (now(),now(),$1) RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn make_room(pool: &sqlx::PgPool, uid: i64) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO rooms(created_at,updated_at,creator_id,type) VALUES (now(),now(),$1,'Rooms::Open') RETURNING id",
    )
    .bind(uid)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A message WITHOUT outbox side effects (`MessageRepository::create`
/// directly, not `post_message`), so relay tests own every outbox row.
async fn make_message(db: &PgDb, room_id: i64, uid: i64, client_id: &str, body: &str) -> i64 {
    let mut tx = db.pool().begin().await.unwrap();
    let row = MessageRepository::create(
        db,
        &mut tx,
        NewMessage {
            room_id,
            creator_id: uid,
            client_message_id: client_id.to_string(),
            body: body.to_string(),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    row.id
}

async fn cleanup_message_tree(pool: &sqlx::PgPool, message_ids: &[i64], user_ids: &[i64]) {
    for mid in message_ids {
        sqlx::query(
            "DELETE FROM action_text_rich_texts WHERE record_type = 'Message' AND record_id = $1",
        )
        .bind(mid)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(mid)
            .execute(pool)
            .await
            .unwrap();
    }
    for uid in user_ids {
        // Literal SQL per statement (sqlx 0.9 `SqlSafeStr`): no dynamic
        // table interpolation.
        for sql in [
            "DELETE FROM push_subscriptions WHERE user_id = $1",
            "DELETE FROM webhooks WHERE user_id = $1",
            "DELETE FROM memberships WHERE user_id = $1",
            "DELETE FROM sessions WHERE user_id = $1",
        ] {
            sqlx::query(sql).bind(uid).execute(pool).await.unwrap();
        }
        // Rooms created by the user (rooms.creator_id, not user_id).
        sqlx::query("DELETE FROM rooms WHERE creator_id = $1")
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

/// Blocking std stub HTTP server (no tokio `net`/`io-util` features
/// needed): records POST bodies, answers every request with the fixed
/// reply. The thread runs until the listener is dropped at test end.
struct Stub {
    url: String,
    hits: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
}

fn start_stub(reply_body: &'static str, content_type: &'static str) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let hits_thread = Arc::clone(&hits);
    let bodies_thread = Arc::clone(&bodies);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(_) => break,
            };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut buf = vec![0u8; 65536];
            let mut request = Vec::new();
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        request.extend_from_slice(&buf[..n]);
                        if request.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let body = request
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|pos| String::from_utf8_lossy(&request[pos + 4..]).into_owned())
                .unwrap_or_default();
            bodies_thread.lock().unwrap().push(body);
            hits_thread.fetch_add(1, Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply_body}",
                reply_body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    Stub { url, hits, bodies }
}

// --- purge ---------------------------------------------------------------

async fn seed_blob(db: &PgDb, store: &S3BlobStore, key: &str, bytes: Vec<u8>) -> i64 {
    // Idempotent seed: a previous failed run may have left this key behind.
    if let Some(existing) = AttachmentRepository::find_blob_by_key(db, key)
        .await
        .unwrap()
    {
        for digest in AttachmentRepository::variant_digests(db, existing.id)
            .await
            .unwrap()
        {
            store.delete(&variant_key(key, &digest)).await.unwrap();
        }
        AttachmentRepository::delete_blob(db, existing.id)
            .await
            .unwrap();
    }
    store.delete(key).await.unwrap();
    store
        .put(key, bytes.clone(), Some("application/octet-stream"))
        .await
        .unwrap();
    let row = AttachmentRepository::insert_blob(
        db,
        NewBlob {
            key: key.to_string(),
            filename: "seed.bin".to_string(),
            content_type: Some("application/octet-stream".to_string()),
            byte_size: bytes.len() as i64,
            checksum: None,
            service_name: "rustfs".to_string(),
        },
    )
    .await
    .unwrap();
    row.id
}

#[test]
fn purge_blob_removes_rows_and_objects() {
    run(async {
        let (db, store) = worker_handles();
        let (_, handles) = launched().await;
        let key = "live/purge-basic";
        let blob_id = seed_blob(&db, &store, key, b"purge me".to_vec()).await;
        AttachmentRepository::record_variant(&db, blob_id, "d1")
            .await
            .unwrap();
        let variant = variant_key(key, "d1");
        store.put(&variant, b"v".to_vec(), None).await.unwrap();

        handles
            .purge_blob
            .run_with(PurgeBlob { blob_id }, dbos::RunOptions::default())
            .await
            .expect("purge completes");

        assert!(AttachmentRepository::find_blob(&db, blob_id)
            .await
            .unwrap()
            .is_none());
        assert!(AttachmentRepository::variant_digests(&db, blob_id)
            .await
            .unwrap()
            .is_empty());
        assert!(store.head(key).await.unwrap().is_none());
        assert!(store.head(&variant).await.unwrap().is_none());
    });
}

#[test]
fn purge_blob_rerun_is_a_noop() {
    run(async {
        let (db, store) = worker_handles();
        let (_, handles) = launched().await;
        let key = "live/purge-rerun";
        let blob_id = seed_blob(&db, &store, key, b"purge twice".to_vec()).await;

        for _ in 0..2 {
            handles
                .purge_blob
                .run_with(PurgeBlob { blob_id }, dbos::RunOptions::default())
                .await
                .expect("every run completes");
        }

        // Gone, and no resurrection: the recorded keys delete idempotently.
        assert!(AttachmentRepository::find_blob(&db, blob_id)
            .await
            .unwrap()
            .is_none());
        assert!(store.head(key).await.unwrap().is_none());
    });
}

#[test]
fn purge_blob_same_id_rejoins_recorded_result() {
    run(async {
        let (db, store) = worker_handles();
        let (_, handles) = launched().await;
        let key = "live/purge-rejoin";
        let blob_id = seed_blob(&db, &store, key, b"purge rejoin".to_vec()).await;
        let workflow_id = format!("live-purge-rejoin-{blob_id}");
        let options = || dbos::RunOptions {
            workflow_id: Some(workflow_id.as_str()),
            ..Default::default()
        };

        handles
            .purge_blob
            .run_with(PurgeBlob { blob_id }, options())
            .await
            .expect("first run completes");
        // Same id ⇒ rejoins the recorded execution (the relay's crash story).
        handles
            .purge_blob
            .run_with(PurgeBlob { blob_id }, options())
            .await
            .expect("rejoin completes");

        assert!(AttachmentRepository::find_blob(&db, blob_id)
            .await
            .unwrap()
            .is_none());
    });
}

// --- attachments ---------------------------------------------------------

// 1x1 transparent PNG (valid CRCs).
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x60, 0x00, 0x02, 0x00,
    0x00, 0x05, 0x00, 0x01, 0x7A, 0x5E, 0xAB, 0x3F, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44,
    0xAE, 0x42, 0x60, 0x82,
];

#[test]
fn process_attachment_produces_thumbnail_and_keeps_original() {
    run(async {
        let (db, store) = worker_handles();
        let (_, handles) = launched().await;
        let key = "live/attach-thumb";
        let blob_id = seed_blob(&db, &store, key, PNG.to_vec()).await;

        handles
            .process_attachment
            .run_with(ProcessAttachment { blob_id }, dbos::RunOptions::default())
            .await
            .expect("processing completes");

        // Metadata merged (§22: analyzed + processed).
        let metadata: Option<String> =
            sqlx::query_scalar("SELECT metadata FROM active_storage_blobs WHERE id = $1")
                .bind(blob_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        let metadata = metadata.unwrap_or_default();
        assert!(metadata.contains("analyzed"), "metadata {metadata}");
        assert!(metadata.contains("processed"), "metadata {metadata}");
        // The sniffed type lands in the content_type column, not metadata.
        let content_type: Option<String> =
            sqlx::query_scalar("SELECT content_type FROM active_storage_blobs WHERE id = $1")
                .bind(blob_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(content_type.as_deref(), Some("image/png"));

        // One variant row + real PNG bytes at the conventional key.
        let digests = AttachmentRepository::variant_digests(&db, blob_id)
            .await
            .unwrap();
        assert_eq!(digests.len(), 1);
        let thumb = store
            .get(&variant_key(key, &digests[0]))
            .await
            .unwrap()
            .unwrap();
        assert!(thumb.starts_with(&[0x89, b'P', b'N', b'G']), "PNG magic");

        // The original is untouched.
        assert_eq!(store.get(key).await.unwrap().unwrap(), PNG);

        // Rerun: same digest reused, no duplicate rows.
        handles
            .process_attachment
            .run_with(ProcessAttachment { blob_id }, dbos::RunOptions::default())
            .await
            .expect("rerun completes");
        assert_eq!(
            AttachmentRepository::variant_digests(&db, blob_id)
                .await
                .unwrap()
                .len(),
            1
        );

        // Cleanup (rows + both objects + the ready event this published).
        store.delete(&variant_key(key, &digests[0])).await.unwrap();
        store.delete(key).await.unwrap();
        AttachmentRepository::delete_blob(&db, blob_id)
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM outbox WHERE topic = 'attachment_ready' AND payload->>'blob_id' = $1",
        )
        .bind(blob_id.to_string())
        .execute(db.pool())
        .await
        .unwrap();
    });
}

#[test]
fn process_attachment_keeps_unprocessable_original() {
    run(async {
        let (db, store) = worker_handles();
        let (_, handles) = launched().await;
        let key = "live/attach-junk";
        let junk = b"definitely not an image".to_vec();
        let blob_id = seed_blob(&db, &store, key, junk.clone()).await;

        handles
            .process_attachment
            .run_with(ProcessAttachment { blob_id }, dbos::RunOptions::default())
            .await
            .expect("junk still finalizes");

        // No variants, original bytes intact (§22: originals survive).
        assert!(AttachmentRepository::variant_digests(&db, blob_id)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(store.get(key).await.unwrap().unwrap(), junk);

        store.delete(key).await.unwrap();
        AttachmentRepository::delete_blob(&db, blob_id)
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM outbox WHERE topic = 'attachment_ready' AND payload->>'blob_id' = $1",
        )
        .bind(blob_id.to_string())
        .execute(db.pool())
        .await
        .unwrap();
    });
}

// --- webhooks ------------------------------------------------------------

#[test]
fn deliver_webhook_posts_reply_exactly_once() {
    run(async {
        let (db, _store) = worker_handles();
        let (_, handles) = launched().await;
        let stub = start_stub("bot says hi", "text/plain; charset=utf-8");
        let pool = db.pool().clone();

        let bot_user = make_user(&pool, "live-bot").await;
        let author = make_user(&pool, "live-author").await;
        let room = make_room(&pool, author).await;
        let trigger = make_message(&db, room, author, "live-trigger", "hello bot").await;
        let webhook: i64 = sqlx::query_scalar(
            "INSERT INTO webhooks(created_at,updated_at,url,user_id) VALUES (now(),now(),$1,$2) RETURNING id",
        )
        .bind(&stub.url)
        .bind(bot_user)
        .fetch_one(&pool)
        .await
        .unwrap();

        let input = DeliverWebhook {
            bot_id: webhook,
            message_id: trigger,
        };
        // Two separate executions (distinct ids) simulate redelivery: the bot
        // endpoint sees both POSTs, but the reply posts once.
        let first = handles
            .deliver_webhook
            .run_with(input.clone(), dbos::RunOptions::default())
            .await
            .expect("first delivery completes");
        let second = handles
            .deliver_webhook
            .run_with(input.clone(), dbos::RunOptions::default())
            .await
            .expect("redelivery completes");
        assert_eq!(first, second);
        assert_ne!(first, 0);
        assert_eq!(stub.hits.load(Ordering::SeqCst), 2);

        let replies: i64 =
            sqlx::query_scalar("SELECT count(*) FROM messages WHERE client_message_id = $1")
                .bind(format!("webhook-delivery:{trigger}:{webhook}"))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(replies, 1);
        let body: String = sqlx::query_scalar(
            "SELECT body FROM action_text_rich_texts WHERE record_type='Message' AND record_id=$1 AND name='body'",
        )
        .bind(first)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(body, "bot says hi");

        // The stub saw a JSON payload naming the trigger.
        {
            let bodies = stub.bodies.lock().unwrap();
            assert_eq!(bodies.len(), 2);
            assert!(bodies[0].contains(&trigger.to_string()), "{}", bodies[0]);
        }

        sqlx::query("DELETE FROM webhook_deliveries WHERE delivery_key = $1")
            .bind(format!("webhook-delivery:{trigger}:{webhook}"))
            .execute(&pool)
            .await
            .unwrap();
        cleanup_message_tree(&pool, &[trigger, first], &[bot_user, author]).await;
    });
}

// --- notifications -------------------------------------------------------

#[test]
fn send_notification_delivers_to_endpoints() {
    run(async {
        let (db, _store) = worker_handles();
        let (_, handles) = launched().await;
        let stub = start_stub("ok", "application/json");
        let pool = db.pool().clone();

        let author = make_user(&pool, "live-poster").await;
        let member = make_user(&pool, "live-member").await;
        let room = make_room(&pool, author).await;
        for uid in [author, member] {
            sqlx::query(
                "INSERT INTO memberships(created_at,updated_at,room_id,user_id) VALUES (now(),now(),$1,$2)",
            )
            .bind(room)
            .bind(uid)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO push_subscriptions(created_at,updated_at,endpoint,user_id) VALUES (now(),now(),$1,$2)",
        )
        .bind(&stub.url)
        .bind(member)
        .execute(&pool)
        .await
        .unwrap();
        let message = make_message(&db, room, author, "live-push", "ping").await;

        let outcomes = handles
            .send_notification
            .run_with(
                SendNotification {
                    message_id: message,
                },
                dbos::RunOptions::default(),
            )
            .await
            .expect("notification completes");

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].endpoint, stub.url);
        assert!(outcomes[0].delivered);
        assert_eq!(stub.hits.load(Ordering::SeqCst), 1);
        {
            let bodies = stub.bodies.lock().unwrap();
            assert!(bodies[0].contains(&message.to_string()), "{}", bodies[0]);
        }

        cleanup_message_tree(&pool, &[message], &[author, member]).await;
    });
}

#[test]
fn send_notification_without_recipients_sends_nothing() {
    run(async {
        let (db, _store) = worker_handles();
        let (_, handles) = launched().await;
        let pool = db.pool().clone();
        let author = make_user(&pool, "live-lonely").await;
        let room = make_room(&pool, author).await;
        // No membership, no subscription: empty outcomes, no HTTP.
        let message = make_message(&db, room, author, "live-lonely", "hello?").await;

        let outcomes = handles
            .send_notification
            .run_with(
                SendNotification {
                    message_id: message,
                },
                dbos::RunOptions::default(),
            )
            .await
            .expect("notification completes");
        assert!(outcomes.is_empty());

        cleanup_message_tree(&pool, &[message], &[author]).await;
    });
}

// --- moderation ----------------------------------------------------------

#[test]
fn remove_banned_content_destroys_user_messages() {
    run(async {
        let (db, _store) = worker_handles();
        let (_, handles) = launched().await;
        let pool = db.pool().clone();
        let banned = make_user(&pool, "live-banned").await;
        let other = make_user(&pool, "live-spared").await;
        let room = make_room(&pool, other).await;
        let doomed = make_message(&db, room, banned, "live-doomed", "spam").await;
        let spared = make_message(&db, room, other, "live-spared", "ham").await;

        let removed = handles
            .remove_banned_content
            .run_with(
                RemoveBannedContent { user_id: banned },
                dbos::RunOptions::default(),
            )
            .await
            .expect("removal completes");
        assert_eq!(removed, 1);

        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE id = $1")
            .bind(doomed)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
        let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE id = $1")
            .bind(spared)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kept, 1);

        // Rerun: the snapshot is empty now, nothing happens.
        let rerun = handles
            .remove_banned_content
            .run_with(
                RemoveBannedContent { user_id: banned },
                dbos::RunOptions::default(),
            )
            .await
            .expect("rerun completes");
        assert_eq!(rerun, 0);

        // The realtime remove was queued for the bridge.
        let removes: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM outbox WHERE topic = 'cable_fanout' AND payload->'message'->>'message_id' = $1",
        )
        .bind(doomed.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(removes, 1);
        sqlx::query("DELETE FROM outbox WHERE topic = 'cable_fanout' AND payload->'message'->>'message_id' = $1")
            .bind(doomed.to_string())
            .execute(&pool)
            .await
            .unwrap();

        // `doomed` is already gone; listing it still cleans its richtext.
        cleanup_message_tree(&pool, &[doomed, spared], &[banned, other]).await;
    });
}

// --- relay ---------------------------------------------------------------

#[test]
fn relay_dispatches_claimed_row_and_marks_done() {
    run(async {
        let (db, _store) = worker_handles();
        let (_, handles) = launched().await;
        let pool = db.pool().clone();
        let author = make_user(&pool, "live-relay").await;
        let room = make_room(&pool, author).await;
        let message = make_message(&db, room, author, "live-relay", "via relay").await;
        // No subscriptions ⇒ the workflow succeeds with empty outcomes.
        let mut tx = pool.begin().await.unwrap();
        topcamp_db::outbox::publish(
            &mut tx,
            "push_message",
            &format!("{{\"message_id\":{message},\"room_id\":{room}}}"),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        // Poll: a parallel relay pass may own this row transiently
        // (claimed but not yet done). Each iteration either dispatches it
        // (if pending) or observes the other pass finishing it.
        let mut done: i64 = 0;
        for _ in 0..50 {
            let _ = relay::run_once(&db, handles, 10).await.unwrap();
            // `->>` extraction, not LIKE: jsonb normalizes key order
            // and spacing (`{"room_id": 178, "message_id": 156}`).
            done = sqlx::query_scalar(
                "SELECT count(*) FROM outbox WHERE topic='push_message' AND done_at IS NOT NULL AND payload->>'message_id' = $1",
            )
            .bind(message.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            if done == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(done, 1);

        sqlx::query("DELETE FROM outbox WHERE payload->>'message_id' = $1")
            .bind(message.to_string())
            .execute(&pool)
            .await
            .unwrap();
        cleanup_message_tree(&pool, &[message], &[author]).await;
    });
}

#[test]
fn relay_releases_failed_dispatch_for_retry() {
    run(async {
        let (db, _store) = worker_handles();
        let (_, handles) = launched().await;
        let pool = db.pool().clone();
        // Bogus payload (no message_id): dispatch fails, claim releases.
        let mut tx = pool.begin().await.unwrap();
        topcamp_db::outbox::publish(&mut tx, "push_message", "{\"relay_bogus_marker\":1}")
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // No exact-count assert (parallel rows); the scoped checks below
        // prove the bogus row was attempted and left undone.
        let _ = relay::run_once(&db, handles, 10).await.unwrap();

        let row: (i32, bool) = sqlx::query_as(
            "SELECT attempts, done_at IS NOT NULL FROM outbox WHERE payload::text LIKE '%relay_bogus_marker%' ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(row.0 >= 1, "attempts bumped");
        assert!(!row.1, "not done");

        sqlx::query("DELETE FROM outbox WHERE payload::text LIKE '%relay_bogus_marker%'")
            .execute(&pool)
            .await
            .unwrap();
    });
}

#[test]
fn relay_dispatch_rejects_unknown_topic() {
    run(async {
        let (_db, _store) = worker_handles();
        let (_, handles) = launched().await;
        let entry = OutboxEntry {
            id: -1,
            topic: "no_such_topic".to_string(),
            payload: "{}".to_string(),
        };
        let err = relay::dispatch(&_db, handles, &entry).await.unwrap_err();
        assert!(err.to_string().contains("unknown topic"), "{err}");
    });
}
