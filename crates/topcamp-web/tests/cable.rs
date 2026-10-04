//! Live `/cable` end-to-end: real TCP server + real WebSocket client.
//!
//! One sequential test (the server's lifetime is the test): login over
//! in-process HTTP, upgrade over TCP with the session cookie, channel
//! subscribes (plain + signed, accept + reject), client-action fanout
//! (typing, presence, read pings), HTTP mutation storage, and logout
//! revocation. Message broadcasts reach `/cable` through the outbox
//! bridge + worker (not in-process), so this test asserts storage for
//! HTTP mutations and reserves live-fanout assertions for
//! `tests/procedures.rs` (LiveBus path). The in-process router and the
//! TCP server share one `AppState`, so revocation lands on the socket.
//!
//! Requires `DATABASE_URL` (migrated). Binds `127.0.0.1:3131` — run
//! alone; a second concurrent run steals the port (bind failures are
//! swallowed, the client then talks to the wrong server).

use futures_util::{SinkExt, StreamExt};
use sqlx::PgPool;
use topcamp_cable::Cable;
use topcamp_db::PgDb;
use topcamp_db::repositories::{RoomRepository, UserRepository};
use topcamp_web::router;
use topcamp_web::state::AppState;
use topcoat::router::{Body, Router};

const PORT: u16 = 3131;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://topcamp:topcamp@localhost:5432/topcamp".to_string());
    PgPool::connect(&url).await.expect("connect DATABASE_URL")
}

fn csrf_token() -> String {
    "cd".repeat(32)
}

fn form_post(uri: &str, body: &str) -> http::Request<Body> {
    let token = csrf_token();
    http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("csrf_token={token}"))
        .body(Body::from(body.replace("{TOKEN}", &token)))
        .expect("form request builds")
}

fn jar(response: &topcoat::router::response::Response) -> String {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|pair| pair.split(';').next())
        .collect::<Vec<_>>()
        .join("; ")
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Next text frame that is not a heartbeat `ping`.
async fn next_data(stream: &mut WsStream) -> String {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
            .await
            .expect("frame arrives")
            .expect("stream open")
            .expect("no ws error");
        match message {
            tokio_tungstenite::tungstenite::Message::Text(text) if !text.contains("\"ping\"") => {
                return text.to_string();
            }
            tokio_tungstenite::tungstenite::Message::Text(_) => {}
            other => panic!("expected text frame, got {other:?}"),
        }
    }
}

/// Action Cable identifiers ride the wire as JSON-encoded STRINGS, e.g.
/// `{"command":"subscribe","identifier":"{\"channel\":\"RoomChannel\"}"}`.
/// A bare object fails `decode_command` and is silently ignored upstream.
fn wire_ident(channel: &str, room_id: i64) -> String {
    let inner = format!(r#"{{"channel":"{channel}","room_id":{room_id}}}"#);
    serde_json::to_string(&inner).expect("identifier escapes")
}

/// Bare channel identifiers (no room id): reads, unreads, heartbeat.
fn wire_channel(channel: &str) -> String {
    let inner = format!(r#"{{"channel":"{channel}"}}"#);
    serde_json::to_string(&inner).expect("identifier escapes")
}

/// Turbo identifiers carry the signed stream name, not a room id.
fn signed_ident(channel: &str, signed: &str) -> String {
    let inner = serde_json::json!({"channel": channel, "signed_stream_name": signed}).to_string();
    serde_json::to_string(&inner).expect("identifier escapes")
}

async fn send(ws: &mut WsStream, text: String) {
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        ws.send(tokio_tungstenite::tungstenite::Message::Text(text.into())),
    )
    .await
    .expect("send answers")
    .unwrap();
}

async fn subscribe(ws: &mut WsStream, ident: &str) -> String {
    send(
        ws,
        format!(r#"{{"command":"subscribe","identifier":{ident}}}"#),
    )
    .await;
    next_data(ws).await
}

async fn perform(ws: &mut WsStream, ident: &str, action: &str) {
    let data = serde_json::to_string(&serde_json::json!({"action": action}).to_string())
        .expect("data escapes");
    send(
        ws,
        format!(r#"{{"command":"message","identifier":{ident},"data":{data}}}"#),
    )
    .await;
}

/// Server + client + database share this runtime: multi-thread, so a
/// parked task can never starve the server it waits on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cable_end_to_end() {
    let pool = pool().await;
    let db = PgDb::new(pool.clone());

    // Idempotent seed: a previous failed run may have left the user.
    let stale: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM users WHERE email_address = 'cable-e2e@example.com'")
            .fetch_all(&pool)
            .await
            .unwrap();
    for uid in stale {
        sqlx::query("DELETE FROM action_text_rich_texts WHERE record_type = 'Message' AND record_id IN (SELECT id FROM messages WHERE creator_id = $1)")
            .bind(uid)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM messages WHERE creator_id = $1")
            .bind(uid)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM memberships WHERE user_id = $1")
            .bind(uid)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM rooms WHERE creator_id = $1")
            .bind(uid)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(uid)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(uid)
            .execute(&pool)
            .await
            .unwrap();
    }

    // Seed: user + room + membership.
    let digest = bcrypt::hash("s3cret", 4).expect("cost-4 digest builds");
    let uid = UserRepository::create(&db, "cable-e2e", "cable-e2e@example.com", Some(&digest))
        .await
        .expect("seed user")
        .id;
    let room = RoomRepository::create(&db, uid, Some("cable-room"), "Rooms::Open")
        .await
        .expect("seed room");
    sqlx::query(
        "INSERT INTO memberships(created_at,updated_at,room_id,user_id) VALUES (now(),now(),$1,$2)",
    )
    .bind(room.id)
    .bind(uid)
    .execute(&pool)
    .await
    .unwrap();

    // One state, two routers: TCP serves one, in-process HTTP drives the other.
    let state = AppState::new(db.clone(), Cable::new());
    let stream_key = state.stream_key.clone();
    let http_router: Router = router(state.clone());

    // Login over in-process HTTP.
    let login = form_post(
        "/session",
        "email_address=cable-e2e%40example.com&password=s3cret&authenticity_token={TOKEN}",
    );
    let response = http_router.handle(login).await;
    eprintln!("MARK logged-in");
    assert_eq!(response.status(), 303);
    let session_jar = jar(&response);
    assert!(session_jar.contains('='), "login sets a cookie");

    // Serve over TCP. `unsafe` (Rust 1.98): this binary runs a single
    // test, so no other thread observes the environment concurrently.
    unsafe {
        std::env::set_var("HOST", "127.0.0.1");
        std::env::set_var("PORT", PORT.to_string());
    }
    tokio::spawn(async move {
        let _ = topcoat::start(router(state)).await;
    });
    // Wait for the listener.
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(format!("127.0.0.1:{PORT}"))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Upgrade WITHOUT a session: 403 (`reject_unauthorized_connection`).
    // Handshake headers are manual: pre-built requests bypass
    // tungstenite's own header generation.
    let bare = http::Request::builder()
        .uri(format!("ws://127.0.0.1:{PORT}/cable"))
        .header("host", format!("127.0.0.1:{PORT}"))
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-protocol", "actioncable-v1-json")
        .body(())
        .unwrap();
    let err = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio_tungstenite::connect_async(bare).await.unwrap_err()
    })
    .await
    .expect("bare upgrade answers");
    eprintln!("MARK bare-answered");
    assert!(
        matches!(
            &err,
            tokio_tungstenite::tungstenite::Error::Http(response)
            if response.status() == http::StatusCode::FORBIDDEN
        ),
        "bare upgrade rejected with 403, got {err:?}"
    );

    // Upgrade WITH the session: welcome + negotiated subprotocol.
    let authed = http::Request::builder()
        .uri(format!("ws://127.0.0.1:{PORT}/cable"))
        .header("host", format!("127.0.0.1:{PORT}"))
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-version", "13")
        .header("cookie", &session_jar)
        .header("sec-websocket-protocol", "actioncable-v1-json")
        .body(())
        .unwrap();
    let (mut ws, response) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio_tungstenite::connect_async(authed),
    )
    .await
    .expect("authed upgrade answers")
    .expect("upgrade");
    assert_eq!(
        response.headers().get("sec-websocket-protocol").unwrap(),
        "actioncable-v1-json"
    );
    eprintln!("MARK authed");
    assert_eq!(next_data(&mut ws).await, r#"{"type":"welcome"}"#);

    // Subscribe to the room: confirm. Unknown channel: reject.
    let room_ident = wire_ident("RoomChannel", room.id);
    eprintln!("MARK send-room");
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            format!(r#"{{"command":"subscribe","identifier":{room_ident}}}"#).into(),
        )),
    )
    .await
    .expect("send answers")
    .unwrap();
    let confirm = next_data(&mut ws).await;
    assert!(confirm.contains("confirm_subscription"), "{confirm}");
    eprintln!("MARK send-reject");
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"command":"subscribe","identifier":"{\"channel\":\"Nope\"}"}"#.into(),
        )),
    )
    .await
    .expect("send answers")
    .unwrap();
    let reject = next_data(&mut ws).await;
    assert!(reject.contains("reject_subscription"), "{reject}");

    // Turbo subscriptions: signed names confirm, forgeries reject.
    let messages_stream = topcamp_cable::room_stream("Rooms::Open", room.id);
    let signed = topcamp_cable::sign_stream(stream_key.bytes(), &messages_stream);
    let room_messages_ident = signed_ident("RoomMessagesChannel", &signed);
    let confirm = subscribe(&mut ws, &room_messages_ident).await;
    assert!(confirm.contains("confirm_subscription"), "{confirm}");
    let mut tampered = signed.clone();
    tampered.push('x');
    let reject = subscribe(&mut ws, &signed_ident("RoomMessagesChannel", &tampered)).await;
    assert!(reject.contains("reject_subscription"), "{reject}");
    // Guard: `:messages` streams only serve `RoomMessagesChannel`.
    let reject = subscribe(&mut ws, &signed_ident("Turbo::StreamsChannel", &signed)).await;
    assert!(reject.contains("reject_subscription"), "{reject}");
    let rooms_signed = topcamp_cable::sign_stream(stream_key.bytes(), "rooms");
    let confirm = subscribe(
        &mut ws,
        &signed_ident("Turbo::StreamsChannel", &rooms_signed),
    )
    .await;
    assert!(confirm.contains("confirm_subscription"), "{confirm}");

    // Post messages over in-process HTTP: storage only. (Broadcasts
    // reach `/cable` subscribers through the outbox bridge + worker,
    // which do not run in-process; the Topcoat UI takes the LiveBus
    // path instead — see `tests/procedures.rs`.)
    let post = http::Request::builder()
        .method("POST")
        .uri(format!("/rooms/{}/messages", room.id))
        .header("cookie", &session_jar)
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"client_message_id":"cable-e2e-1","body":"hello cable"}"#,
        ))
        .unwrap();
    let posted = http_router.handle(post).await;
    assert_eq!(posted.status(), 200);
    let stored: i64 =
        sqlx::query_scalar("SELECT id FROM messages WHERE client_message_id = 'cable-e2e-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(stored > 0);

    // Unreads subscription confirms (pings arrive via the bridge).
    let unreads_ident = wire_channel("UnreadRoomsChannel");
    let confirm = subscribe(&mut ws, &unreads_ident).await;
    assert!(confirm.contains("confirm_subscription"), "{confirm}");
    let post = http::Request::builder()
        .method("POST")
        .uri(format!("/rooms/{}/messages", room.id))
        .header("cookie", &session_jar)
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"client_message_id":"cable-e2e-2","body":"second"}"#,
        ))
        .unwrap();
    assert_eq!(http_router.handle(post).await.status(), 200);
    let stored: i64 =
        sqlx::query_scalar("SELECT id FROM messages WHERE client_message_id = 'cable-e2e-2'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(stored > 0);

    // Reads: `present` marks the room read and pings `{room_id}`.
    let reads_ident = wire_channel("ReadRoomsChannel");
    let confirm = subscribe(&mut ws, &reads_ident).await;
    assert!(confirm.contains("confirm_subscription"), "{confirm}");

    // Typing on the same stream arrives under the typing identifier.
    let typing_ident = wire_ident("TypingNotificationsChannel", room.id);
    eprintln!("MARK send-typing");
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            format!(r#"{{"command":"subscribe","identifier":{typing_ident}}}"#).into(),
        )),
    )
    .await
    .expect("send answers")
    .unwrap();
    assert!(next_data(&mut ws).await.contains("confirm_subscription"));
    eprintln!("MARK send-typing-msg");
    tokio::time::timeout(std::time::Duration::from_secs(10), ws.send(tokio_tungstenite::tungstenite::Message::Text(
        format!(
            "{{\"command\":\"message\",\"identifier\":{typing_ident},\"data\":\"{{\\\"action\\\":\\\"start\\\"}}\"}}"
        )
        .into(),
    )))
    .await
    .expect("send answers")
    .unwrap();
    // The typing broadcast fans out to BOTH room-stream subscriptions
    // (room + typing identifiers, either order); drain both so no echo
    // leaks into the next read.
    let mut saw_room_echo = false;
    let mut saw_typing = false;
    for _ in 0..6 {
        if saw_room_echo && saw_typing {
            break;
        }
        let frame = next_data(&mut ws).await;
        if frame.contains("\"start\"") {
            if frame.contains("TypingNotificationsChannel") {
                saw_typing = true;
            }
            if frame.contains("RoomChannel") {
                saw_room_echo = true;
            }
        }
    }
    assert!(saw_typing, "typing broadcast under typing identifier");
    assert!(saw_room_echo, "typing broadcast under room identifier");

    // Presence marks the membership connected; `present` pings the
    // reads stream; `absent` disconnects like an unsubscribe.
    let presence_ident = wire_ident("PresenceChannel", room.id);
    eprintln!("MARK send-presence");
    let confirm = subscribe(&mut ws, &presence_ident).await;
    assert!(confirm.contains("confirm_subscription"), "{confirm}");
    let connections: i32 = sqlx::query_scalar(
        "SELECT connections FROM memberships WHERE room_id = $1 AND user_id = $2",
    )
    .bind(room.id)
    .bind(uid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(connections, 1);
    perform(&mut ws, &presence_ident, "present").await;
    let read = next_data(&mut ws).await;
    assert!(read.contains("ReadRoomsChannel"), "{read}");
    assert!(read.contains(&format!("\"room_id\":{}", room.id)), "{read}");
    assert!(!read.contains("action"), "{read}");
    perform(&mut ws, &presence_ident, "absent").await;
    eprintln!("MARK send-unsub");
    send(
        &mut ws,
        format!(r#"{{"command":"unsubscribe","identifier":{presence_ident}}}"#),
    )
    .await;
    // Unsubscribe acks nothing; poll for the effect.
    for _ in 0..50 {
        let now: i32 = sqlx::query_scalar(
            "SELECT connections FROM memberships WHERE room_id = $1 AND user_id = $2",
        )
        .bind(room.id)
        .bind(uid)
        .fetch_one(&pool)
        .await
        .unwrap();
        if now == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let cleared: i32 = sqlx::query_scalar(
        "SELECT connections FROM memberships WHERE room_id = $1 AND user_id = $2",
    )
    .bind(room.id)
    .bind(uid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cleared, 0);

    // Edit: 303 + stored body (live fanout goes over the LiveBus;
    // `/cable` clients get worker-relayed broadcasts via the bridge).
    let first_id: i64 =
        sqlx::query_scalar("SELECT id FROM messages WHERE client_message_id = 'cable-e2e-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let token = csrf_token();
    let update = http::Request::builder()
        .method("PATCH")
        .uri(format!("/rooms/{}/messages/{first_id}", room.id))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{session_jar}; csrf_token={token}"))
        .body(Body::from(format!(
            "message%5Bbody%5D=edited+live&authenticity_token={token}"
        )))
        .unwrap();
    let updated = http_router.handle(update).await;
    assert_eq!(updated.status(), 303);
    let edited: String = sqlx::query_scalar(
        "SELECT body FROM action_text_rich_texts WHERE record_type = 'Message' AND record_id = $1",
    )
    .bind(first_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(edited.contains("edited live"), "{edited}");

    // Destroy: 303 + rows gone (no Turbo remove stream anymore).
    let destroy = http::Request::builder()
        .method("POST")
        .uri(format!("/rooms/{}/messages/{first_id}", room.id))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{session_jar}; csrf_token={token}"))
        .body(Body::from(format!(
            "authenticity_token={token}&_method=delete"
        )))
        .unwrap();
    let destroyed = http_router.handle(destroy).await;
    assert_eq!(destroyed.status(), 303);
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id = $1")
        .bind(first_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(remaining, 0);

    // Logout revokes the socket: disconnect{remote, reconnect:true}.
    let token = csrf_token();
    let logout = http::Request::builder()
        .method("POST")
        .uri("/session")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("{session_jar}; csrf_token={token}"))
        .body(Body::from(format!(
            "authenticity_token={token}&_method=delete"
        )))
        .unwrap();
    let _ = http_router.handle(logout).await;
    let bye = next_data(&mut ws).await;
    assert!(bye.contains("\"disconnect\""), "{bye}");
    assert!(bye.contains("\"remote\""), "{bye}");
    assert!(bye.contains("\"reconnect\":true"), "{bye}");

    // Cleanup: message tree + outbox + user. (The first message was
    // destroyed mid-test; its tree rows are already gone.)
    let posted_id: Option<i64> =
        sqlx::query_scalar("SELECT id FROM messages WHERE client_message_id = 'cable-e2e-1'")
            .fetch_optional(&pool)
            .await
            .unwrap();
    let second_id: i64 =
        sqlx::query_scalar("SELECT id FROM messages WHERE client_message_id = 'cable-e2e-2'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let posted_id = posted_id.unwrap_or(second_id);
    sqlx::query(
        "DELETE FROM action_text_rich_texts WHERE record_type = 'Message' AND record_id = $1",
    )
    .bind(posted_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(posted_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "DELETE FROM action_text_rich_texts WHERE record_type = 'Message' AND record_id = $1",
    )
    .bind(second_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(second_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM outbox WHERE payload->>'message_id' = $1")
        .bind(posted_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM outbox WHERE payload->>'message_id' = $1")
        .bind(second_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM memberships WHERE room_id = $1 AND user_id = $2")
        .bind(room.id)
        .bind(uid)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(room.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(uid)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(uid)
        .execute(&pool)
        .await
        .unwrap();
}
