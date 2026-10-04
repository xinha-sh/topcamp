//! Demo seed for videos and manual walkthroughs: a populated account
//! (users, rooms, messages, boosts) behind one command.
//!
//! Usage: `DATABASE_URL=postgres://topcamp:topcamp@localhost:5432/<db>`
//! `cargo run -p topcamp-web --bin seed` (the database must already be
//! migrated). Idempotent: exits quietly when `demo@example.com` exists.
//! Sign in with the one-click demo button (debug builds, no password)
//! or as `demo@example.com` / `topcamp`.

use topcamp_db::repositories::{
    MembershipRepository, MessageRepository, NewMessage, RoomRepository, UserRepository,
};
use topcamp_db::{PgDb, connect};

const PASSWORD: &str = "topcamp";

async fn user(db: &PgDb, name: &str, email: &str, admin: bool, digest: &str) -> i64 {
    if let Some(row) = db
        .find_active_credentials_by_email(email)
        .await
        .expect("credential lookup")
    {
        return row.user_id;
    }
    let id = UserRepository::create(db, name, email, Some(digest))
        .await
        .expect("seed user")
        .id;
    if admin {
        UserRepository::set_role(db, id, 1)
            .await
            .expect("seed admin");
    }
    println!("user {email}");
    id
}

async fn message(db: &PgDb, tag: &str, n: i64, room_id: i64, uid: i64, text: &str) {
    db.post_message(NewMessage {
        room_id,
        creator_id: uid,
        client_message_id: format!("{tag}-{n}"),
        body: topcamp_web::richtext::canonicalize_plain(text),
    })
    .await
    .expect("seed message");
}

const GENERAL: [&str; 45] = [
    "Morning all — shipping the no-JS milestone today",
    "Sidebar is a real popover now, Esc just works",
    "Confirm dialogs are server-rendered, zero JavaScript",
    "Reply quotes pre-fill from ?reply_to=",
    "Attachments post as plain multipart forms",
    "History pages with ?before= links",
    "View transitions make it feel instant",
    "Tooltips via data-tip, no JS needed",
    "Theme cookie: light, dark, or system",
    "Anyone tried the mobile drawer yet?",
    "Works great on my phone, light-dismiss included",
    "The slide animation is subtle, I like it",
    "Dark mode follows the OS by default",
    "Forcing dark from account settings sticks",
    "Invalid form fields get a red border, no JS",
    "Copy buttons are just selectable text now",
    "Message permalinks for everything",
    "Boosts still work the same",
    "Ping rooms for quick side threads",
    "Direct messages feel snappy",
    "No Turbo, no Stimulus, no hand-written JS",
    "Topcoat router + live regions only",
    "Server pre-renders all the JS-added state",
    "Older messages load one page at a time",
    "Composer drafts survive reply prefill",
    "Posting works with the runtime off entirely",
    "Dialog focus and backdrop from native HTML",
    "Member list paginates on the server",
    "Role toggles are plain forms with Save",
    "Avatar uploads show variants",
    "Bot keys reset behind a confirm",
    "Room deletion asks first, always",
    "Flashes render server-side as usual",
    "Presence dots via live shards",
    "Typing indicators still stream",
    "Cable fanout untouched",
    "Outbox relays webhooks the same",
    "Migrations run in order, seeds after",
    "Demo data resets by dropping the database",
    "Video walkthrough starts here",
    "Welcome to Topcamp, no-JS edition",
    "Forty-five messages so paging shows up",
    "This one should be behind Load older",
    "And a few more for good measure",
    "Last one — see you in the video",
];

#[tokio::main]
async fn main() {
    let url = std::env::var("DATABASE_URL").expect("set DATABASE_URL first");
    let pool = connect(&url).await.expect("connect DATABASE_URL");
    let db = PgDb::new(pool);

    sqlx::query(
        "INSERT INTO accounts (created_at, updated_at, join_code, name) \
         VALUES (now(), now(), 'demo-join', 'Topcamp') ON CONFLICT DO NOTHING",
    )
    .execute(db.pool())
    .await
    .expect("seed account");

    let digest = bcrypt::hash(PASSWORD, 4).expect("digest builds");
    let dan = user(&db, "Demo Dan", "demo@example.com", true, &digest).await;
    if !fresh(&db, dan).await {
        println!("already seeded (demo@example.com exists)");
        return;
    }
    let ada = user(&db, "Ada", "ada@example.com", false, &digest).await;
    let grace = user(&db, "Grace", "grace@example.com", false, &digest).await;
    let ken = user(&db, "Ken", "ken@example.com", false, &digest).await;
    let members = [dan, ada, grace, ken];

    let general = RoomRepository::create(&db, dan, Some("General"), "Rooms::Open")
        .await
        .expect("seed room")
        .id;
    let design = RoomRepository::create(&db, ada, Some("Design"), "Rooms::Open")
        .await
        .expect("seed room")
        .id;
    let ping = RoomRepository::create(&db, ada, None, "Rooms::Direct")
        .await
        .expect("seed ping")
        .id;
    for room_id in [general, design] {
        for uid in members {
            MembershipRepository::create(&db, room_id, uid)
                .await
                .expect("seed membership");
        }
    }
    for uid in [ada, grace] {
        MembershipRepository::create(&db, ping, uid)
            .await
            .expect("seed ping membership");
    }

    for (n, text) in GENERAL.into_iter().enumerate() {
        let uid = members[n % members.len()];
        message(&db, "general", n as i64, general, uid, text).await;
    }
    let design_notes = [
        "New sidebar ships in this milestone",
        "Popover drawer with Esc to close",
        "Dark tokens land with the theme cookie",
        "Tooltip pass on all icon buttons",
        "Invalid states without JavaScript",
        "View transitions between pages",
        "Dialog entrance, 180 milliseconds",
        "Ship it",
    ];
    for (n, text) in design_notes.into_iter().enumerate() {
        message(
            &db,
            "design",
            n as i64,
            design,
            members[(n + 1) % members.len()],
            text,
        )
        .await;
    }
    let ping_notes = [
        "quick question — is the drawer in this build?",
        "yes, popover + slide, try Esc",
        "nice, filming the walkthrough tomorrow",
        "seed data is ready when you are",
    ];
    for (n, text) in ping_notes.into_iter().enumerate() {
        message(&db, "ping", n as i64, ping, [ada, grace][n % 2], text).await;
    }

    // One quoted reply and two boosts, for the video close-ups.
    message(
        &db,
        "quote",
        0,
        general,
        grace,
        "> Sidebar is a real popover now\n\nshipping it",
    )
    .await;
    let first: i64 =
        sqlx::query_scalar("SELECT id FROM messages WHERE room_id = $1 ORDER BY id LIMIT 1")
            .bind(general)
            .fetch_one(db.pool())
            .await
            .expect("first message");
    db.create_boost(first, ada, "🚀").await.expect("seed boost");
    db.create_boost(first, ken, "🎉").await.expect("seed boost");

    let bot = UserRepository::create_bot(&db, "Deploy Bot")
        .await
        .expect("seed bot");
    MembershipRepository::create(&db, general, bot.id)
        .await
        .expect("seed bot membership");

    println!(
        "seeded: 4 users, 3 rooms, {} messages, 2 boosts, 1 bot",
        45 + 8 + 4 + 1
    );
    println!("sign in: demo button (debug) or demo@example.com / {PASSWORD}");
}

/// True when this admin owns no rooms yet (fresh seed, not a rerun).
async fn fresh(db: &PgDb, uid: i64) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM rooms WHERE creator_id = $1")
        .bind(uid)
        .fetch_one(db.pool())
        .await
        .map(|count| count == 0)
        .unwrap_or(false)
}
