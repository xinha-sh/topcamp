//! Live PostgreSQL tests: migrations, constraints, outbox claim, FTS,
//! repository impls (`PgDb`), session lifecycle.
//!
//! Requires `DATABASE_URL` pointing at a database with `migrations/`
//! applied (see compose.yml + README). Raw-SQL tests run in a rolled-back
//! transaction; repository tests clean up their rows explicitly, so the
//! shared scratch database is never polluted.

use std::time::Duration;

use sqlx::{PgPool, Row};
use topcamp_db::repositories::{
    AttachmentRepository, MembershipRepository, MessageRepository, NewBlob, NewMessage,
    RoomRepository, SessionRepository, UserRepository,
};
use topcamp_db::{pg::session_key, PgDb};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://topcamp:topcamp@localhost:5432/topcamp".to_string());
    PgPool::connect(&url).await.expect("connect DATABASE_URL")
}

#[tokio::test]
async fn involvement_accepts_four_rejects_bogus() {
    let pool = pool().await;
    let mut tx = pool.begin().await.unwrap();
    let uid: i64 = sqlx::query_scalar(
        "INSERT INTO users(created_at,updated_at,name) VALUES (now(),now(),'t') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO rooms(created_at,updated_at,creator_id,type) VALUES (now(),now(),$1,'Rooms::Open') RETURNING id",
    )
    .bind(uid)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    for level in ["mentions", "everything", "invisible", "nothing"] {
        let n: i64 = sqlx::query_scalar(
            "INSERT INTO memberships(created_at,updated_at,room_id,user_id,involvement) VALUES (now(),now(),$1,$2,$3) RETURNING user_id",
        )
        .bind(rid)
        .bind(uid + 100 + level.len() as i64)
        .bind(level)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(uid);
        assert!(n > 0, "level {level} accepted");
    }
    // Bogus level must fail the CHECK constraint.
    let bad = sqlx::query(
        "INSERT INTO memberships(created_at,updated_at,room_id,user_id,involvement) VALUES (now(),now(),$1,$2,'bogus')",
    )
    .bind(rid)
    .bind(uid)
    .execute(&mut *tx)
    .await;
    assert!(bad.is_err(), "bogus involvement rejected");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn outbox_claim_returns_pending_in_order() {
    let pool = pool().await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO outbox(topic,payload) VALUES ('push_message','{\"m\":1}'),('deliver_webhook','{\"m\":2}')")
        .execute(&mut *tx)
        .await
        .unwrap();
    let rows = topcamp_db::outbox::claim_batch(&mut tx, 10).await.unwrap();
    assert!(rows.len() >= 2);
    let topics: Vec<&str> = rows.iter().map(|r| r.topic.as_str()).collect();
    assert!(topics.contains(&"push_message") && topics.contains(&"deliver_webhook"));
    for row in &rows {
        topcamp_db::outbox::mark_done(&mut tx, row.id)
            .await
            .unwrap();
    }
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox WHERE done_at IS NULL AND claimed_at IS NULL AND id = ANY($1)",
    )
    .bind(rows.iter().map(|r| r.id).collect::<Vec<_>>())
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(remaining, 0);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn fts_matches_and_ranks() {
    let pool = pool().await;
    let mut tx = pool.begin().await.unwrap();
    let uid: i64 = sqlx::query_scalar(
        "INSERT INTO users(created_at,updated_at,name) VALUES (now(),now(),'s') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO rooms(created_at,updated_at,creator_id,type) VALUES (now(),now(),$1,'Rooms::Open') RETURNING id",
    )
    .bind(uid)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    for (cid, body) in [
        ("r1", "hello world meeting notes"),
        ("r2", "hello there world world world"),
        ("r3", "unrelated text here"),
    ] {
        sqlx::query(
            "INSERT INTO messages(created_at,updated_at,client_message_id,creator_id,room_id,search_vector) VALUES (now(),now(),$1,$2,$3,to_tsvector('english',$4))",
        )
        .bind(cid)
        .bind(uid)
        .bind(rid)
        .bind(body)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    // AND semantics: needs both words.
    let hits: Vec<String> = sqlx::query(
        "SELECT client_message_id FROM messages WHERE room_id=$1 AND search_vector @@ plainto_tsquery('english','hello world') ORDER BY ts_rank(search_vector, plainto_tsquery('english','hello world')) DESC",
    )
    .bind(rid)
    .fetch_all(&mut *tx)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<String, _>("client_message_id"))
    .collect();
    assert_eq!(hits, vec!["r2".to_string(), "r1".to_string()]);
    let none: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM messages WHERE room_id=$1 AND search_vector @@ plainto_tsquery('english','hello unrelated')",
    )
    .bind(rid)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(none, 0);
    tx.rollback().await.unwrap();
}

async fn db() -> (PgPool, PgDb) {
    let pool = pool().await;
    let db = PgDb::new(pool.clone());
    (pool, db)
}

async fn make_user(db: &PgDb, name: &str, email: &str) -> i64 {
    UserRepository::create(db, name, email, None)
        .await
        .unwrap()
        .id
}

async fn cleanup(pool: &PgPool, user_ids: &[i64], room_ids: &[i64]) {
    for rid in room_ids {
        sqlx::query("DELETE FROM messages WHERE room_id = $1")
            .bind(rid)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM memberships WHERE room_id = $1")
            .bind(rid)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(rid)
            .execute(pool)
            .await
            .unwrap();
    }
    for uid in user_ids {
        sqlx::query("DELETE FROM push_subscriptions WHERE user_id = $1")
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

async fn cleanup_outbox_for_message(pool: &PgPool, message_id: i64) {
    // Targeted delete: parallel tests may own other outbox rows.
    sqlx::query("DELETE FROM outbox WHERE payload::text LIKE $1")
        .bind(format!("%\"message_id\": {message_id}%"))
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn pg_post_message_is_atomic_and_searchable() {
    let (pool, db) = db().await;
    let uid = make_user(&db, "poster", "poster@example.com").await;
    let room = RoomRepository::create(&db, uid, Some("general"), "Rooms::Open")
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memberships(created_at,updated_at,room_id,user_id) VALUES (now(),now(),$1,$2)",
    )
    .bind(room.id)
    .bind(uid)
    .execute(&pool)
    .await
    .unwrap();

    let row = db
        .post_message(NewMessage {
            room_id: room.id,
            creator_id: uid,
            client_message_id: "c1".to_string(),
            body: "hello world meeting notes".to_string(),
        })
        .await
        .unwrap();

    // Rich-text body stored.
    let body: String = sqlx::query_scalar(
        "SELECT body FROM action_text_rich_texts WHERE record_type='Message' AND record_id=$1 AND name='body'",
    )
    .bind(row.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(body, "hello world meeting notes");

    // Outbox row published in-tx (scoped to this message: parallel tests
    // own the global latest row).
    let payload: String = sqlx::query_scalar(
        "SELECT payload::text FROM outbox WHERE topic='push_message' AND payload::text LIKE $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(format!("%\"message_id\": {}%", row.id))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(payload.contains(&row.id.to_string()), "payload {payload}");

    // Author's own membership is not marked unread… (only one member here,
    // the author, so nothing stamped).
    let stamped: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memberships WHERE room_id=$1 AND unread_at IS NOT NULL",
    )
    .bind(room.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stamped, 0);

    // A second member gets stamped unread, and the message is searchable.
    let uid2 = make_user(&db, "reader", "reader@example.com").await;
    sqlx::query(
        "INSERT INTO memberships(created_at,updated_at,room_id,user_id) VALUES (now(),now(),$1,$2)",
    )
    .bind(room.id)
    .bind(uid2)
    .execute(&pool)
    .await
    .unwrap();
    let row2 = db
        .post_message(NewMessage {
            room_id: room.id,
            creator_id: uid,
            client_message_id: "c2".to_string(),
            body: "second note".to_string(),
        })
        .await
        .unwrap();
    let stamped2: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memberships WHERE room_id=$1 AND unread_at IS NOT NULL",
    )
    .bind(room.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stamped2, 1);

    let hits = db.search_reachable(uid2, "hello world", 100).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, row.id);

    sqlx::query("DELETE FROM action_text_rich_texts WHERE record_type='Message'")
        .execute(&pool)
        .await
        .unwrap();
    cleanup_outbox_for_message(&pool, row.id).await;
    cleanup_outbox_for_message(&pool, row2.id).await;
    cleanup(&pool, &[uid, uid2], &[room.id]).await;
}

#[tokio::test]
async fn pg_session_lifecycle_with_expiry() {
    let (pool, db) = db().await;
    let uid = make_user(&db, "sessioned", "sessioned@example.com").await;

    let token = session_key(&[7u8; 32]);
    let record = db
        .start(
            uid,
            Some("test-agent"),
            Some("127.0.0.1"),
            Duration::from_secs(3600),
            &token,
        )
        .await
        .unwrap();
    assert_eq!(record.user_id, uid);

    let found = db.find_by_token(&token).await.unwrap();
    assert_eq!(found.map(|r| r.id), Some(record.id));

    db.resume(
        record.id,
        Some("other-agent"),
        None,
        Duration::from_secs(3600),
    )
    .await
    .unwrap();
    let ua: String = sqlx::query_scalar("SELECT user_agent FROM sessions WHERE id=$1")
        .bind(record.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ua, "other-agent");

    SessionRepository::destroy(&db, record.id).await.unwrap();
    assert!(db.find_by_token(&token).await.unwrap().is_none());

    cleanup(&pool, &[uid], &[]).await;
}

#[tokio::test]
async fn pg_expired_session_is_invisible() {
    let (pool, db) = db().await;
    let uid = make_user(&db, "expired", "expired@example.com").await;
    let token = session_key(&[9u8; 32]);
    db.start(uid, None, None, Duration::from_secs(0), &token)
        .await
        .unwrap();
    assert!(db.find_by_token(&token).await.unwrap().is_none());
    cleanup(&pool, &[uid], &[]).await;
}

#[tokio::test]
async fn pg_user_roles_and_deactivation() {
    let (pool, db) = db().await;
    let uid = make_user(&db, "mod", "mod@example.com").await;

    // Allowlist: bogus codes fall back to member (0).
    db.set_role(uid, 99).await.unwrap();
    let role: i32 = sqlx::query_scalar("SELECT role FROM users WHERE id=$1")
        .bind(uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(role, 0);
    db.set_role(uid, 1).await.unwrap();

    assert!(db
        .find_active_by_email("mod@example.com")
        .await
        .unwrap()
        .is_some());
    db.deactivate(uid).await.unwrap();
    assert!(db
        .find_active_by_email("mod@example.com")
        .await
        .unwrap()
        .is_none());

    cleanup(&pool, &[uid], &[]).await;
}

#[tokio::test]
async fn pg_moderation_snapshot_and_destroy() {
    let (pool, db) = db().await;
    let uid = make_user(&db, "banned", "banned@example.com").await;
    let room = RoomRepository::create(&db, uid, Some("x"), "Rooms::Open")
        .await
        .unwrap();
    for cid in ["m1", "m2", "m3"] {
        db.post_message(NewMessage {
            room_id: room.id,
            creator_id: uid,
            client_message_id: cid.to_string(),
            body: format!("bannable {cid}"),
        })
        .await
        .unwrap();
    }

    // Snapshot: oldest first.
    let ids = db.ids_for_creator(uid).await.unwrap();
    assert_eq!(ids.len(), 3);
    assert!(ids[0] < ids[1] && ids[1] < ids[2]);

    // Reachable (member) search finds them before the ban.
    sqlx::query(
        "INSERT INTO memberships(created_at,updated_at,room_id,user_id) VALUES (now(),now(),$1,$2)",
    )
    .bind(room.id)
    .bind(uid)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        db.search_reachable(uid, "bannable", 100)
            .await
            .unwrap()
            .len(),
        3
    );

    // Destroy chunk: rows gone and unsearchable.
    for id in &ids {
        MessageRepository::destroy(&db, *id).await.unwrap();
    }
    assert!(db.ids_for_creator(uid).await.unwrap().is_empty());
    assert!(db
        .search_reachable(uid, "bannable", 100)
        .await
        .unwrap()
        .is_empty());

    sqlx::query("DELETE FROM action_text_rich_texts WHERE record_type='Message'")
        .execute(&pool)
        .await
        .unwrap();
    for id in &ids {
        cleanup_outbox_for_message(&pool, *id).await;
    }
    cleanup(&pool, &[uid], &[room.id]).await;
}

#[tokio::test]
async fn pg_push_endpoints_cover_room_members() {
    let (pool, db) = db().await;
    let uid = make_user(&db, "subscriber", "subscriber@example.com").await;
    let room = RoomRepository::create(&db, uid, Some("y"), "Rooms::Open")
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memberships(created_at,updated_at,room_id,user_id) VALUES (now(),now(),$1,$2)",
    )
    .bind(room.id)
    .bind(uid)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO push_subscriptions(created_at,updated_at,endpoint,user_id) VALUES (now(),now(),$1,$2)",
    )
    .bind("https://push.example/endpoint-1")
    .bind(uid)
    .execute(&pool)
    .await
    .unwrap();

    let endpoints = db.push_endpoints_for_room(room.id).await.unwrap();
    assert_eq!(
        endpoints,
        vec!["https://push.example/endpoint-1".to_string()]
    );

    // A room with no subscriptions yields none.
    let room2 = RoomRepository::create(&db, uid, Some("z"), "Rooms::Open")
        .await
        .unwrap();
    assert!(db
        .push_endpoints_for_room(room2.id)
        .await
        .unwrap()
        .is_empty());

    cleanup(&pool, &[uid], &[room.id, room2.id]).await;
}

#[tokio::test]
async fn pg_blob_lifecycle_and_rows_first_purge() {
    let (pool, db) = db().await;
    let blob = AttachmentRepository::insert_blob(
        &db,
        NewBlob {
            key: "purge-test-key".to_string(),
            filename: "note.txt".to_string(),
            content_type: Some("text/plain".to_string()),
            byte_size: 12,
            checksum: None,
            service_name: "rustfs".to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        AttachmentRepository::find_blob_by_key(&db, "purge-test-key")
            .await
            .unwrap()
            .map(|b| b.id),
        Some(blob.id)
    );

    AttachmentRepository::attach_to_record(&db, "Message", 1, "file", blob.id)
        .await
        .unwrap();
    let variant = AttachmentRepository::record_variant(&db, blob.id, "thumb")
        .await
        .unwrap();
    assert_eq!(
        AttachmentRepository::find_variant(&db, blob.id, "thumb")
            .await
            .unwrap(),
        Some(variant)
    );
    // Same digest records idempotently to the same row.
    assert_eq!(
        AttachmentRepository::record_variant(&db, blob.id, "thumb")
            .await
            .unwrap(),
        variant
    );

    // Rows-first purge removes variant + attachment + blob.
    let removed = AttachmentRepository::delete_blob(&db, blob.id)
        .await
        .unwrap();
    assert_eq!(removed, 3);
    assert!(
        AttachmentRepository::find_blob_by_key(&db, "purge-test-key")
            .await
            .unwrap()
            .is_none()
    );
    // Repeat purge is a no-op, never a resurrection.
    assert_eq!(
        AttachmentRepository::delete_blob(&db, blob.id)
            .await
            .unwrap(),
        0
    );

    let _ = pool;
}

#[tokio::test]
async fn pg_membership_presence_and_involvement() {
    let (pool, db) = db().await;
    let uid = make_user(&db, "present", "present@example.com").await;
    let room = RoomRepository::create(&db, uid, Some("p"), "Rooms::Open")
        .await
        .unwrap();

    let membership = MembershipRepository::create(&db, room.id, uid)
        .await
        .unwrap();
    assert_eq!(membership.involvement, "mentions");
    assert_eq!(
        MembershipRepository::find(&db, room.id, uid)
            .await
            .unwrap()
            .map(|m| m.id),
        Some(membership.id)
    );

    // Presence subscribe/unsubscribe round-trip.
    MembershipRepository::mark_connected(&db, membership.id)
        .await
        .unwrap();
    MembershipRepository::mark_connected(&db, membership.id)
        .await
        .unwrap();
    let connections: i32 = sqlx::query_scalar("SELECT connections FROM memberships WHERE id=$1")
        .bind(membership.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(connections, 2);
    MembershipRepository::mark_disconnected(&db, membership.id)
        .await
        .unwrap();
    MembershipRepository::mark_disconnected(&db, membership.id)
        .await
        .unwrap();
    // Floored at zero, timestamp drained.
    MembershipRepository::mark_disconnected(&db, membership.id)
        .await
        .unwrap();
    let (drained, stamped): (i32, Option<String>) =
        sqlx::query_as("SELECT connections, connected_at::text FROM memberships WHERE id=$1")
            .bind(membership.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(drained, 0);
    assert!(stamped.is_none());

    // `present` connects and clears the unread stamp.
    sqlx::query("UPDATE memberships SET unread_at = now() WHERE id=$1")
        .bind(membership.id)
        .execute(&pool)
        .await
        .unwrap();
    MembershipRepository::mark_present(&db, membership.id)
        .await
        .unwrap();
    let (present_count, unread_left): (i32, Option<String>) =
        sqlx::query_as("SELECT connections, unread_at::text FROM memberships WHERE id=$1")
            .bind(membership.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(present_count, 1);
    assert!(unread_left.is_none());

    // `refresh` leaves a fresh membership untouched.
    MembershipRepository::mark_refreshed(&db, membership.id)
        .await
        .unwrap();
    let fresh: i32 = sqlx::query_scalar("SELECT connections FROM memberships WHERE id=$1")
        .bind(membership.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(fresh, 1);

    // Stale memberships restart instead of accumulating.
    sqlx::query("UPDATE memberships SET connected_at = now() - interval '61 seconds', connections = 7 WHERE id=$1")
        .bind(membership.id)
        .execute(&pool)
        .await
        .unwrap();
    MembershipRepository::mark_connected(&db, membership.id)
        .await
        .unwrap();
    let revived: i32 = sqlx::query_scalar("SELECT connections FROM memberships WHERE id=$1")
        .bind(membership.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(revived, 1);
    sqlx::query("UPDATE memberships SET connected_at = now() - interval '61 seconds', connections = 7 WHERE id=$1")
        .bind(membership.id)
        .execute(&pool)
        .await
        .unwrap();
    MembershipRepository::mark_disconnected(&db, membership.id)
        .await
        .unwrap();
    let (zeroed, no_stamp): (i32, Option<String>) =
        sqlx::query_as("SELECT connections, connected_at::text FROM memberships WHERE id=$1")
            .bind(membership.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(zeroed, 0);
    assert!(no_stamp.is_none());

    // Involvement allowlist: bogus levels fail the CHECK.
    MembershipRepository::set_involvement(&db, membership.id, "everything")
        .await
        .unwrap();
    assert!(
        MembershipRepository::set_involvement(&db, membership.id, "bogus")
            .await
            .is_err()
    );

    // Fanout read + boot reset.
    assert_eq!(
        MembershipRepository::member_user_ids(&db, room.id)
            .await
            .unwrap(),
        vec![uid]
    );
    MembershipRepository::mark_connected(&db, membership.id)
        .await
        .unwrap();
    MembershipRepository::disconnect_all(&db).await.unwrap();
    let reset: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memberships WHERE connections <> 0 OR connected_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reset, 0);

    MembershipRepository::destroy(&db, membership.id)
        .await
        .unwrap();
    assert!(MembershipRepository::find(&db, room.id, uid)
        .await
        .unwrap()
        .is_none());
    cleanup(&pool, &[uid], &[room.id]).await;
}

#[tokio::test]
async fn pg_login_credentials_roundtrip() {
    let (pool, db) = db().await;
    let uid = UserRepository::create(&db, "logger", "logger@example.com", Some("digest-1"))
        .await
        .unwrap()
        .id;
    let creds = db
        .find_active_credentials_by_email("logger@example.com")
        .await
        .unwrap()
        .expect("credentials found");
    assert_eq!(creds.user_id, uid);
    assert_eq!(creds.password_digest.as_deref(), Some("digest-1"));
    // Unknown email yields nothing.
    assert!(db
        .find_active_credentials_by_email("nobody@example.com")
        .await
        .unwrap()
        .is_none());
    // Deactivated accounts disappear from login lookup.
    db.deactivate(uid).await.unwrap();
    assert!(db
        .find_active_credentials_by_email("logger@example.com")
        .await
        .unwrap()
        .is_none());
    cleanup(&pool, &[uid], &[]).await;
}

#[tokio::test]
async fn pg_rooms_list_for_user() {
    let (pool, db) = db().await;
    let uid = make_user(&db, "roomy", "roomy@example.com").await;
    let other = make_user(&db, "stranger", "stranger@example.com").await;
    let mine = RoomRepository::create(&db, uid, Some("zzz-last"), "Rooms::Open")
        .await
        .unwrap();
    let mine2 = RoomRepository::create(&db, uid, Some("aaa-first"), "Rooms::Open")
        .await
        .unwrap();
    let theirs = RoomRepository::create(&db, other, Some("nope"), "Rooms::Open")
        .await
        .unwrap();
    for rid in [mine.id, mine2.id] {
        sqlx::query(
            "INSERT INTO memberships(created_at,updated_at,room_id,user_id) VALUES (now(),now(),$1,$2)",
        )
        .bind(rid)
        .bind(uid)
        .execute(&pool)
        .await
        .unwrap();
    }
    let rooms = RoomRepository::list_for_user(&db, uid).await.unwrap();
    let names: Vec<_> = rooms.iter().map(|r| r.name.clone().unwrap()).collect();
    assert_eq!(names, vec!["aaa-first".to_string(), "zzz-last".to_string()]);
    assert!(RoomRepository::list_for_user(&db, other)
        .await
        .unwrap()
        .is_empty());
    cleanup(&pool, &[uid, other], &[mine.id, mine2.id, theirs.id]).await;
}
