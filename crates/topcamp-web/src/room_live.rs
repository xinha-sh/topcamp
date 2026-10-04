//! Live room regions (UI-05r): Topcoat-native message updates.
//!
//! - [`message_live`]: one message. Re-emits on edit, emits empty on
//!   remove. Formatting flags (`me`, `threaded`, `first-of-day`) never
//!   change for a stored message (author/time/position are immutable),
//!   so re-emissions reuse the initial flags and only refresh the body.
//! - [`tail_live`]: the cons-tail after the last shown message. HTTP
//!   renders emit empty; connected renders fetch anything missed, then
//!   wait for the next creation. Each tail emits once (new messages,
//!   each a fresh [`message_live`], plus the chained tail) and returns,
//!   so appends are exactly-once by construction.
//! - [`typing_live`]: the typing indicator. Re-emits the current
//!   typists on bus events plus a 1s tick (TTL purges).
//!
//! Presence lifecycle (present/refresh/disconnect) lives in the tail:
//! each tail marks its viewer present on start (viewing also clears
//! `unread_at`), refreshes on a 30s tick, and marks disconnected when
//! the region ends (client gone, or chained on to the next tail, which
//! re-marks).

use topcamp_db::PgDb;
use topcamp_db::repositories::{MembershipRepository, MessageDetail, MessageRepository};
use topcoat::{
    context::Cx,
    router::Slot,
    runtime::connected,
    view::{BoxView, ViewExt as _, emit, live},
};

use crate::live::{LiveBus, RoomEvent, publish_room_read};
use crate::room_show::{ShowMessageView, message_view};
use crate::state::http_error;

/// Everything a message region needs, owned (`'static` bodies).
#[derive(Clone)]
pub(crate) struct LiveMessage {
    pub db: PgDb,
    pub bus: LiveBus,
    pub room_id: i64,
    pub message_id: i64,
    pub client_message_id: String,
    pub room_name: String,
    pub me_id: i64,
    pub csrf_token: String,
    /// `?confirm=` form id open on the page load; live refreshes reuse it.
    pub confirm_form: String,
    /// Initial view: flags are immutable, the body refreshes on edit.
    pub initial: ShowMessageView,
}

/// One live message: re-emit on edit, empty on remove.
pub(crate) fn message_live(cx: &Cx, data: LiveMessage) -> BoxView<'static> {
    let cx = cx.keyed(data.message_id);
    live! { cx =>
        let mut rx = data.bus.room(data.room_id).subscribe();
        let token = emit! {
            (Slot::new(message_view(&cx, &data.initial, &data.csrf_token, &data.confirm_form)))
        }?;
        if !connected(&cx) {
            return Ok(token);
        }
        loop {
            match rx.recv().await {
                Ok(RoomEvent::MessageUpdated { message_id })
                | Ok(RoomEvent::BoostsChanged { message_id })
                    if message_id == data.message_id =>
                {
                    let view = refresh_view(&data).await?;
                    let token = emit! {
                        (Slot::new(message_view(&cx, &view, &data.csrf_token, &data.confirm_form)))
                    }?;
                    // Keep watching (further edits); the swap stream continues.
                    let _ = token;
                }
                Ok(RoomEvent::MessageRemoved { client_message_id })
                    if client_message_id == data.client_message_id =>
                {
                    let token = emit! {}?;
                    return Ok(token);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // May have missed an edit: re-fetch + emit (morph is idempotent).
                    let view = refresh_view(&data).await?;
                    let token = emit! {
                        (Slot::new(message_view(&cx, &view, &data.csrf_token, &data.confirm_form)))
                    }?;
                    let _ = token;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return Ok(token);
                }
            }
        }
    }
    .boxed()
}

/// Re-read one message, reusing the initial formatting flags.
async fn refresh_view(data: &LiveMessage) -> topcoat::Result<ShowMessageView> {
    let detail = MessageRepository::find_detail_in_room(&data.db, data.room_id, data.message_id)
        .await
        .map_err(http_error)?;
    let Some(detail) = detail else {
        // Deleted between event and read: keep showing the last view
        // (the remove event's empty emission follows).
        return Ok(data.initial.clone());
    };
    let mut views = crate::room_show::message_views_in_room(
        &data.db,
        data.room_id,
        &data.room_name,
        std::slice::from_ref(&detail),
        data.me_id,
    )
    .await?;
    let mut view = views.pop().expect("one message in, one view out");
    view.me = data.initial.me;
    view.first_of_day = data.initial.first_of_day;
    view.threaded = data.initial.threaded;
    view.mentioned = data.initial.mentioned;
    Ok(view)
}

/// Tail state: cursor + the last shown message (for first-of-batch flags).
pub(crate) struct LiveTail {
    pub db: PgDb,
    pub bus: LiveBus,
    pub room_id: i64,
    pub after_id: i64,
    pub room_name: String,
    pub me_id: i64,
    pub csrf_token: String,
    /// Last shown message, for threading the first appended message.
    pub prev: Option<MessageDetail>,
}

/// One viewing session's presence: `mark_present` now (viewing also
/// clears `unread_at`, so the sidebar unpips), `mark_disconnected`
/// when dropped (the region ending means the client is gone or the
/// tail chained on and the next tail re-marks).
struct PresenceGuard {
    db: PgDb,
    membership_id: i64,
}

impl PresenceGuard {
    async fn mark(db: &PgDb, membership_id: i64) -> topcoat::Result<Self> {
        MembershipRepository::mark_present(db, membership_id)
            .await
            .map_err(http_error)?;
        Ok(Self {
            db: db.clone(),
            membership_id,
        })
    }
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        let db = self.db.clone();
        let id = self.membership_id;
        tokio::spawn(async move {
            let _ = MembershipRepository::mark_disconnected(&db, id).await;
        });
    }
}

/// The cons-tail: emit missed/new messages plus the chained tail, once.
pub(crate) fn tail_live(cx: &Cx, tail: LiveTail) -> BoxView<'static> {
    let cx = cx.clone();
    live! { cx =>
        let token = emit! {}?;
        if !connected(&cx) {
            return Ok(token);
        }
        let membership = MembershipRepository::find(&tail.db, tail.room_id, tail.me_id)
            .await
            .map_err(http_error)?;
        let presence = match membership {
            Some(membership) => Some(PresenceGuard::mark(&tail.db, membership.id).await?),
            None => None,
        };
        if presence.is_some() {
            publish_room_read(&tail.bus, tail.me_id, tail.room_id);
        }
        // A refresh tick beside the batch wait (`wait_for_batch`
        // catch-up-fetches first, so dropping + recreating its future
        // on ticks misses nothing).
        let mut refresh = tokio::time::interval(std::time::Duration::from_secs(30));
        let batch = loop {
            tokio::select! {
                batch = wait_for_batch(&tail) => break batch?,
                _ = refresh.tick() => {
                    if let Some(guard) = &presence {
                        let _ = MembershipRepository::mark_refreshed(&tail.db, guard.membership_id).await;
                    }
                }
            }
        };
        let _ = &presence;
        let Some(batch) = batch else {
            return Ok(token);
        };
        let (items, next) = build_batch(&tail, &batch).await?;
        let keyed = cx.keyed(format!("tail:{}", tail.after_id));
        let mut slots = Vec::with_capacity(items.len() + 1);
        for item in &items {
            slots.push(Slot::new(message_live(&keyed.keyed(item.message_id), item.clone())));
        }
        slots.push(Slot::new(tail_live(&keyed, next)));
        let token = emit! {
            for slot in slots {
                (slot)
            }
        }?;
        return Ok(token);
    }
    .boxed()
}

/// Wait for the next non-empty batch after the cursor (`None` = bus closed).
/// Catch-up first (covers the HTTP→connect race), then bus events.
async fn wait_for_batch(tail: &LiveTail) -> topcoat::Result<Option<Vec<MessageDetail>>> {
    let missed = MessageRepository::page_after_id(&tail.db, tail.room_id, tail.after_id)
        .await
        .map_err(http_error)?;
    if !missed.is_empty() {
        return Ok(Some(missed));
    }
    let mut rx = tail.bus.room(tail.room_id).subscribe();
    loop {
        match rx.recv().await {
            Ok(RoomEvent::MessageCreated { message_id }) if message_id > tail.after_id => {
                let batch = MessageRepository::page_after_id(&tail.db, tail.room_id, tail.after_id)
                    .await
                    .map_err(http_error)?;
                if !batch.is_empty() {
                    return Ok(Some(batch));
                }
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                let batch = MessageRepository::page_after_id(&tail.db, tail.room_id, tail.after_id)
                    .await
                    .map_err(http_error)?;
                if !batch.is_empty() {
                    return Ok(Some(batch));
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(None),
        }
    }
}

/// Build one batch: a live region per message plus the chained tail.
async fn build_batch(
    tail: &LiveTail,
    batch: &[MessageDetail],
) -> topcoat::Result<(Vec<LiveMessage>, LiveTail)> {
    let mut views = crate::room_show::message_views_in_room(
        &tail.db,
        tail.room_id,
        &tail.room_name,
        batch,
        tail.me_id,
    )
    .await?;
    // Thread the batch head under the last shown message.
    if let (Some(first), Some(prev)) = (views.first_mut(), &tail.prev) {
        let head = &batch[0];
        first.threaded = prev.creator_id == head.creator_id
            && (prev.created_ms - head.created_ms).abs() <= 5 * 60 * 1000;
        first.first_of_day = prev.created_iso.get(..10) != head.created_iso.get(..10);
    }
    let last = batch.last().expect("batch is not empty");
    // Monotonic max: batches order by created_at, which concurrent
    // posts can invert against id order.
    let after_id = tail
        .after_id
        .max(batch.iter().map(|detail| detail.id).max().unwrap_or(0));
    let next = LiveTail {
        db: tail.db.clone(),
        bus: tail.bus.clone(),
        room_id: tail.room_id,
        after_id,
        room_name: tail.room_name.clone(),
        me_id: tail.me_id,
        csrf_token: tail.csrf_token.clone(),
        prev: Some(last.clone()),
    };
    let items = views
        .into_iter()
        .map(|view| LiveMessage {
            db: tail.db.clone(),
            bus: tail.bus.clone(),
            room_id: tail.room_id,
            message_id: view.id,
            client_message_id: view.client_message_id.clone(),
            room_name: tail.room_name.clone(),
            me_id: tail.me_id,
            csrf_token: tail.csrf_token.clone(),
            confirm_form: String::new(),
            initial: view,
        })
        .collect();
    Ok((items, next))
}

/// Typing indicator state.
pub(crate) struct LiveTyping {
    pub bus: LiveBus,
    pub room_id: i64,
    pub me_id: i64,
}

fn typist_names(typists: &[(i64, String)], me_id: i64) -> String {
    typists
        .iter()
        .filter(|(id, _)| *id != me_id)
        .map(|(_, name)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn indicator_class(names: &str) -> String {
    if names.is_empty() {
        "typing-indicator gap txt-small align-center flex-inline".to_string()
    } else {
        "typing-indicator gap txt-small align-center flex-inline typing-indicator--active"
            .to_string()
    }
}

/// The typing indicator: current typists, re-emitted on change.
/// Emits the whole indicator (class + author), so the morph toggles
/// `typing-indicator--active` with the names.
pub(crate) fn typing_live(cx: &Cx, data: LiveTyping) -> BoxView<'static> {
    let cx = cx.clone();
    live! { cx =>
        let mut shown = data.bus.typing_now(data.room_id);
        let token = emit! {
            <div class=(indicator_class(&typist_names(&shown, data.me_id)))>
                <div class="typing-indicator__author spinner">(typist_names(&shown, data.me_id))</div>
            </div>
        }?;
        if !connected(&cx) {
            return Ok(token);
        }
        let mut rx = data.bus.room(data.room_id).subscribe();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Ok(RoomEvent::Typing { .. }) => {}
                        Ok(_) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return Ok(token);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    }
                }
                _ = tick.tick() => {}
            }
            let now = data.bus.typing_now(data.room_id);
            if now != shown {
                shown = now;
                let token = emit! {
                    <div class=(indicator_class(&typist_names(&shown, data.me_id)))>
                        <div class="typing-indicator__author spinner">(typist_names(&shown, data.me_id))</div>
                    </div>
                }?;
                let _ = token;
            }
        }
    }
    .boxed()
}
