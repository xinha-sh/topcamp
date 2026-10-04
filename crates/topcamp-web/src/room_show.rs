//! `RoomsController#show`: the room page (nav, message area, composer).
//!
//! `GET /rooms/:id` renders the last page of messages;
//! `GET /rooms/:room_id/@:message_id` pages around one message.
//! `GET /rooms/:id?before=<message-id>` pages around an older
//! message, which is how the server-rendered "Load older messages"
//! link reaches history without JavaScript.
//! Unknown or inaccessible rooms redirect home (the alert flash
//! lands with UI-14); anonymous visitors go to sign-in.
//!
//! Bodies render through [`crate::richtext`]. The composer's
//! `lexxy-editor` carries a textarea fallback (no Lexxy JS ships;
//! dynamic behavior arrives via Topcoat UI in UI-05), so posting
//! works without JavaScript. Boost forms and the QR/regenerate
//! buttons point at routes landing in UI-06/UI-07/UI-12. The cable
//! stream source and involvement bell routes land with UI-05/UI-15.

use std::collections::HashMap;

use topcamp_db::PgDb;
use topcamp_db::repositories::{
    AccountRepository, AttachmentRepository, BoostDetail, FormUser, MembershipRepository,
    MessageDetail, MessageRepository, RoomRepository, RoomRow, UserRepository,
};
use topcamp_domain::auth::UserRole;
use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        Slot,
        error::see_other,
        path_param_segment, query_params, request,
        response::{AsyncIntoResponse, IntoResponse, Response},
        route,
    },
    view::{Unescaped, ViewExt as _, view},
};

use crate::pages::{ShellContext, body_classes, document_shell};
use crate::rooms::cast_integer;
use crate::rooms_typed::Kind;
use crate::state::{AppState, http_error};

// --- view data -----------------------------------------------------------------

/// A message creator or booster, as the message partial shows them.
#[derive(Clone)]
pub(crate) struct ActorView {
    pub id: i64,
    pub name: String,
    pub title: String,
    pub avatar_url: String,
}

/// A boost chip on a message (`boosts.ordered`).
#[derive(Clone)]
pub(crate) struct BoostItemView {
    pub id: i64,
    pub message_id: i64,
    pub content: String,
    pub all_emoji: bool,
    pub booster: ActorView,
}

/// A message as `messages/_message` shows it. `unrenderable` covers
/// a gone creator/booster (upstream's `message_tag` rescue).
#[derive(Clone)]
pub(crate) struct ShowMessageView {
    pub id: i64,
    pub room_id: i64,
    pub room_name: String,
    pub client_message_id: String,
    pub creator: ActorView,
    pub created_iso: String,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub all_emoji: bool,
    pub unrenderable: bool,
    /// Pre-formatted client classes: upstream's `messages` controller
    /// adds these after its formatting pass (which also unhides the
    /// message); the server computes them so pages render without JS.
    pub me: bool,
    pub first_of_day: bool,
    /// Threaded under the previous message (same author, within 5
    /// minutes); `mentioned` when a verified mention names the viewer.
    pub threaded: bool,
    pub mentioned: bool,
    /// `richtext::present` output (already-safe HTML).
    pub body_html: String,
    pub boosts: Vec<BoostItemView>,
    /// Attached file, when any: replaces the body in presentation
    /// (upstream's `message.content` picks attachment first).
    pub attachment: Option<ShowAttachmentView>,
}

/// `AttachmentView` for one message blob: inline + download URLs.
#[derive(Clone)]
pub(crate) struct ShowAttachmentView {
    pub filename: String,
    pub content_type: String,
    pub blob_path: String,
    pub download_path: String,
    pub thumb_url: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

/// Everything the room page renders (owned, so views stay 'static).
struct ShowData {
    room_id: i64,
    kind: Kind,
    display_name: String,
    edit_path: String,
    noun: &'static str,
    messages_dom_id: String,
    mention_src: String,
    me: ActorView,
    messages: Vec<ShowMessageView>,
    invitation: bool,
    /// `?before=` href for the oldest visible message, when older
    /// history exists (`None` at the start of history or when empty).
    older_href: Option<String>,
    join_url: String,
    has_logo: bool,
    logo_url: String,
    admin: bool,
    csrf_token: String,
    room_name: String,
    /// `?reply_to=` quote pre-fill for the composer, else empty.
    reply_draft: String,
}

#[query_params(error = bad_request)]
struct ReplyQuery {
    reply_to: Option<String>,
}

/// `?before=<message-id>`: the no-JS history cursor. Like the
/// `messages#index` paging param, it anchors `page_messages` at an
/// older message; the path `@` anchor wins when both are present.
/// Unknown ids fall back to the last page.
#[query_params(error = bad_request)]
struct BeforeQuery {
    before: Option<String>,
}

/// Quote block prefilling the composer for `?reply_to=<message-id>`.
/// Unknown ids (or messages without text) yield an empty draft.
fn reply_draft(messages: &[MessageDetail], reply_to: Option<&str>) -> String {
    let id: i64 = match reply_to.and_then(|raw| raw.parse().ok()) {
        Some(id) => id,
        None => return String::new(),
    };
    let Some(detail) = messages.iter().find(|message| message.id == id) else {
        return String::new();
    };
    let plain = crate::richtext::plain_text(&detail.body.clone().unwrap_or_default());
    let quoted: Vec<String> = plain.lines().map(|line| format!("> {line}")).collect();
    if quoted.is_empty() {
        return String::new();
    }
    format!("{}\n\n", quoted.join("\n"))
}

// --- paths and ids ---------------------------------------------------------------

/// `dom_id(message)` / `dom_id(message, prefix)`: keyed by the
/// client message id.
pub(crate) fn message_dom_id(client_message_id: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        format!("message_{client_message_id}")
    } else {
        format!("{prefix}_message_{client_message_id}")
    }
}

/// `room_dom_id(kind, id, prefix)`: `messages_rooms_open_1` etc.
pub(crate) fn room_dom_id(kind: Kind, id: i64, prefix: &str) -> String {
    if prefix.is_empty() {
        format!("{}_{id}", kind.param_key())
    } else {
        format!("{prefix}_{}_{id}", kind.param_key())
    }
}

pub(crate) fn at_path(room_id: i64, id: i64) -> String {
    format!("/rooms/{room_id}/@{id}")
}

pub(crate) fn message_path(room_id: i64, id: i64) -> String {
    format!("/rooms/{room_id}/messages/{id}")
}

/// The message's attachment bytes (inline); `?disposition=attachment`
/// downloads. Our analogue of the blob redirect path.
pub(crate) fn message_attachment_path(room_id: i64, id: i64) -> String {
    format!("/rooms/{room_id}/messages/{id}/attachment")
}

pub(crate) fn edit_message_path(room_id: i64, id: i64) -> String {
    format!("/rooms/{room_id}/messages/{id}/edit")
}

/// `to_sentence` with the ` and ` connector (`room_display_name`).
pub(crate) fn sentence_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [one, two] => format!("{one} and {two}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// `room_display_name(room, for_user:)`: directs read as their other
/// members' names, falling back to the viewer's own name.
pub(crate) fn room_display_name(
    room: &RoomRow,
    members: &[FormUser],
    me_id: i64,
    me_name: &str,
) -> String {
    if room.kind != "Rooms::Direct" {
        return room.name.clone().unwrap_or_default();
    }
    let others: Vec<String> = members
        .iter()
        .filter(|member| member.id != me_id)
        .map(|member| member.name.clone())
        .collect();
    let sentence = sentence_and(&others);
    if sentence.trim().is_empty() {
        me_name.to_string()
    } else {
        sentence
    }
}

/// `user.title`: the name plus the bio, when the bio is present.
pub(crate) fn actor_title(name: &str, bio: &Option<String>) -> String {
    match bio.as_deref().filter(|bio| !bio.trim().is_empty()) {
        Some(bio) => format!("{name} – {bio}"),
        None => name.to_string(),
    }
}

pub(crate) fn actor_view(user: &FormUser) -> ActorView {
    ActorView {
        id: user.id,
        name: user.name.clone(),
        title: actor_title(&user.name, &user.bio),
        avatar_url: crate::users::avatar_url_for(user.id, &user.updated_number),
    }
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Split `YYYY-MM-DDTHH:MM:SSZ` into its parts; `None` when malformed.
fn split_iso(iso: &str) -> Option<(i32, u32, u32, u32, u32)> {
    let (date, time) = iso.strip_suffix('Z')?.split_once('T')?;
    let mut date = date.split('-');
    let mut time = time.split(':');
    Some((
        date.next()?.parse().ok()?,
        date.next()?.parse().ok()?,
        date.next()?.parse().ok()?,
        time.next()?.parse().ok()?,
        time.next()?.parse().ok()?,
    ))
}

/// `Intl.DateTimeFormat(undefined, { dateStyle: "long" })` in UTC
/// (`local-time` localizes client-side with JS; UI-05).
fn long_date(iso: &str) -> String {
    match split_iso(iso) {
        Some((year, month, day, _, _)) if (1..=12).contains(&month) => {
            format!("{} {day}, {year}", MONTHS[(month - 1) as usize])
        }
        _ => iso.to_string(),
    }
}

/// `Intl.DateTimeFormat(undefined, { timeStyle: "short" })` in UTC.
fn short_time(iso: &str) -> String {
    match split_iso(iso) {
        Some((_, _, _, hour, minute)) if hour < 24 && minute < 60 => {
            let (hour12, suffix) = match hour {
                0 => (12, "AM"),
                1..=11 => (hour, "AM"),
                12 => (12, "PM"),
                _ => (hour - 12, "PM"),
            };
            format!("{hour12}:{minute:02} {suffix}")
        }
        _ => iso.to_string(),
    }
}

// --- handlers ------------------------------------------------------------------

/// `set_room` with `Scope::All`: membership lookup; unknown or
/// inaccessible rooms redirect home (the alert flash is UI-14's).
async fn set_room(db: &PgDb, user_id: i64, param: &str) -> Result<Option<RoomRow>> {
    match cast_integer(param) {
        Some(id) => RoomRepository::find_for_user(db, user_id, id)
            .await
            .map_err(http_error),
        None => Ok(None),
    }
}

/// Absolute invite URL from the request Host (`http` unless a
/// forwarded proto says otherwise); relative when Host is absent.
fn invite_url(cx: &Cx, join_code: &str) -> String {
    let path = format!("/join/{join_code}");
    let headers = request::headers(cx);
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim();
    if host.is_empty() {
        return path;
    }
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http")
        .split(',')
        .next()
        .unwrap_or("http")
        .trim();
    format!("{proto}://{host}{path}")
}

/// `render_show`: page around the anchor when it names a message of
/// this room, else the last page.
async fn page_messages(
    db: &PgDb,
    room_id: i64,
    anchor: Option<&str>,
) -> Result<Vec<MessageDetail>> {
    let anchored = match anchor.and_then(cast_integer) {
        Some(id) => MessageRepository::find_detail_in_room(db, room_id, id)
            .await
            .map_err(http_error)?,
        None => None,
    };
    match anchored {
        Some(message) if message.room_id == room_id => {
            let anchor_id = message.id;
            let mut page = MessageRepository::page_before(db, room_id, anchor_id)
                .await
                .map_err(http_error)?;
            page.push(message);
            page.extend(
                MessageRepository::page_after(db, room_id, anchor_id)
                    .await
                    .map_err(http_error)?,
            );
            Ok(page)
        }
        _ => MessageRepository::last_page(db, room_id)
            .await
            .map_err(http_error),
    }
}

/// Build message views: creators/boosters resolve through one user
/// lookup; a gone creator or booster renders `_unrenderable`.
/// `THREADING_TIME_WINDOW_MILLISECONDS`: threading window for
/// consecutive same-author messages.
const THREAD_WINDOW_MS: i64 = 5 * 60 * 1000;

/// Single-room `message_views` with threading (rooms, boosts,
/// messages, live tail); searches pass several rooms + no threading.
pub(crate) async fn message_views_in_room(
    db: &PgDb,
    room_id: i64,
    room_name: &str,
    messages: &[MessageDetail],
    me_id: i64,
) -> Result<Vec<ShowMessageView>> {
    let rooms = HashMap::from([(room_id, room_name.to_string())]);
    message_views(db, &rooms, messages, me_id, true).await
}

pub(crate) async fn message_views(
    db: &PgDb,
    rooms: &HashMap<i64, String>,
    messages: &[MessageDetail],
    me_id: i64,
    threading: bool,
) -> Result<Vec<ShowMessageView>> {
    let mut ids: Vec<i64> = messages.iter().map(|m| m.creator_id).collect();
    let mut boosts_by_message: HashMap<i64, Vec<BoostDetail>> = HashMap::new();
    for message in messages {
        let boosts = MessageRepository::boosts_for_message(db, message.id)
            .await
            .map_err(http_error)?;
        ids.extend(boosts.iter().map(|boost| boost.booster_id));
        boosts_by_message.insert(message.id, boosts);
        // Verified mention targets join the user fetch.
        ids.extend(crate::richtext::mentioned_user_ids(
            message.body.as_deref().unwrap_or(""),
        ));
    }
    let message_ids: Vec<i64> = messages.iter().map(|m| m.id).collect();
    let attachments =
        AttachmentRepository::attachments_for_records(db, "Message", "attachment", &message_ids)
            .await
            .map_err(http_error)?;
    let mut attachments_by_message: HashMap<i64, topcamp_db::repositories::RecordAttachment> =
        attachments
            .into_iter()
            .map(|att| (att.record_id, att))
            .collect();
    ids.sort_unstable();
    ids.dedup();
    let users = UserRepository::form_users(db, &ids)
        .await
        .map_err(http_error)?;
    let by_id: HashMap<i64, &FormUser> = users.iter().map(|user| (user.id, user)).collect();
    let mention_users: HashMap<i64, crate::richtext::MentionUser> = users
        .iter()
        .map(|user| {
            let title = match user.bio.as_deref().filter(|bio| !bio.trim().is_empty()) {
                Some(bio) => format!("{} \u{2013} {bio}", user.name),
                None => user.name.clone(),
            };
            (
                user.id,
                crate::richtext::MentionUser {
                    id: user.id,
                    name: user.name.clone(),
                    title,
                    avatar_url: crate::users::avatar_url_for(user.id, &user.updated_number),
                },
            )
        })
        .collect();

    let mut views = Vec::with_capacity(messages.len());
    let mut previous_day = "";
    let mut previous: Option<&MessageDetail> = None;
    for message in messages {
        let day = message.created_iso.get(..10).unwrap_or("");
        let first_of_day = day != previous_day;
        previous_day = day;
        // `MessageFormatter#threadMessage`: same author within 5 minutes
        // (search results render with `ThreadStyle.none`).
        let threaded = threading
            && previous.is_some_and(|prev| {
                prev.creator_id == message.creator_id
                    && (prev.created_ms - message.created_ms).abs() <= THREAD_WINDOW_MS
            });
        previous = Some(message);
        let plain = crate::richtext::plain_text(message.body.as_deref().unwrap_or(""));
        let mut unrenderable = false;
        let creator = match by_id.get(&message.creator_id) {
            Some(user) => actor_view(user),
            None => {
                unrenderable = true;
                ActorView {
                    id: message.creator_id,
                    name: String::new(),
                    title: String::new(),
                    avatar_url: String::new(),
                }
            }
        };
        let mut boosts = Vec::new();
        for boost in boosts_by_message.remove(&message.id).unwrap_or_default() {
            match by_id.get(&boost.booster_id) {
                Some(user) => boosts.push(BoostItemView {
                    id: boost.id,
                    message_id: boost.message_id,
                    content: boost.content.clone(),
                    all_emoji: crate::richtext::all_emoji(&boost.content),
                    booster: actor_view(user),
                }),
                None => unrenderable = true,
            }
        }
        let attachment = attachments_by_message.remove(&message.id).map(|att| {
            let blob_path = message_attachment_path(message.room_id, message.id);
            ShowAttachmentView {
                filename: att.blob.filename.clone(),
                content_type: att
                    .blob
                    .content_type
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                download_path: format!("{blob_path}?disposition=attachment"),
                // Worker thumbs land with §22; until then the blob itself
                // previews (identical pixels, correct layout).
                thumb_url: blob_path.clone(),
                blob_path,
                width: att.width,
                height: att.height,
            }
        });
        let body = message.body.as_deref().unwrap_or("");
        let mentioned_ids = crate::richtext::mentioned_user_ids(body);
        views.push(ShowMessageView {
            id: message.id,
            room_id: message.room_id,
            room_name: rooms.get(&message.room_id).cloned().unwrap_or_default(),
            client_message_id: message.client_message_id.clone(),
            creator,
            created_iso: message.created_iso.clone(),
            created_ms: message.created_ms,
            updated_ms: message.updated_ms,
            all_emoji: crate::richtext::all_emoji(&plain),
            unrenderable,
            me: message.creator_id == me_id,
            first_of_day,
            threaded,
            mentioned: mentioned_ids.contains(&me_id),
            body_html: crate::richtext::present_rich(body, &mention_users),
            boosts,
            attachment,
        });
    }
    Ok(views)
}

pub(crate) async fn render_show(
    cx: &Cx,
    room_param: &str,
    anchor: Option<&str>,
) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let Some(room) = set_room(db, user.id, room_param).await? else {
        return crate::flash::redirect_with_alert(cx, "/", "Room not found or inaccessible");
    };
    crate::rooms_typed::remember_last_room(cx, room.id);

    let admin = user.role == UserRole::Administrator.value();
    let kind = Kind::of(&room.kind);
    let member_ids = MembershipRepository::member_user_ids(db, room.id)
        .await
        .map_err(http_error)?;
    let members = UserRepository::form_users(db, &member_ids)
        .await
        .map_err(http_error)?;
    let display_name = room_display_name(&room, &members, user.id, &user.name);
    let before_query = query_params::<BeforeQuery>(cx)?;
    let before = before_query
        .before
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    let messages = page_messages(db, room.id, anchor.or(before)).await?;
    let older_href = match messages.first() {
        Some(oldest) => {
            let older = MessageRepository::page_before(db, room.id, oldest.id)
                .await
                .map_err(http_error)?;
            (!older.is_empty()).then(|| format!("/rooms/{}?before={}", room.id, oldest.id))
        }
        None => None,
    };
    // `message.room` with `for_user: nil`: directs read as every member.
    let room_name = room_display_name(
        &room,
        &members,
        -1,
        &members.first().map(|m| m.name.clone()).unwrap_or_default(),
    );
    let views = message_views_in_room(db, room.id, &room_name, &messages, user.id).await?;

    let account = AccountRepository::first(db).await.map_err(http_error)?;
    let original = RoomRepository::original(db).await.map_err(http_error)?;
    let invitation = original.as_ref().is_some_and(|o| o.id == room.id)
        && !MessageRepository::paged(db, room.id)
            .await
            .map_err(http_error)?;
    let (join_code, has_logo, logo_url) = match &account {
        Some(account) => {
            let attached = AccountRepository::logo_attached(db, account.id)
                .await
                .map_err(http_error)?;
            (
                account.join_code.clone(),
                attached,
                format!("/account/logo?v={}", account.updated_number),
            )
        }
        None => (String::new(), false, "/account/logo".to_string()),
    };

    let Some(me) = UserRepository::find_avatar_user(db, user.id)
        .await
        .map_err(http_error)?
    else {
        return see_other("/session/new").into_response(cx);
    };
    let app = app_context::<AppState>(cx);
    let ctx = crate::sidebar::SidebarCtx {
        db: app.db.clone(),
        bus: app.bus.clone(),
        me: me.clone(),
        admin,
    };
    let csrf_token = crate::csrf::issue(cx);
    let reply_query = query_params::<ReplyQuery>(cx)?;
    let reply_draft = reply_draft(&messages, reply_query.reply_to.as_deref());
    let frame = crate::sidebar::sidebar_frame(cx, ctx, csrf_token.clone(), true).await?;
    let me_view = ActorView {
        id: user.id,
        name: user.name.clone(),
        title: user.name.clone(),
        avatar_url: crate::users::avatar_url_for(user.id, &me.updated_number),
    };
    let data = ShowData {
        room_id: room.id,
        kind,
        display_name,
        edit_path: kind.edit_path(room.id),
        noun: if kind == Kind::Direct { "Ping" } else { "room" },
        messages_dom_id: room_dom_id(kind, room.id, "messages"),
        mention_src: format!("/autocompletable/users?room_id={}", room.id),
        me: me_view,
        messages: views,
        invitation,
        older_href,
        join_url: invite_url(cx, &join_code),
        has_logo,
        logo_url,
        admin,
        csrf_token,
        room_name,
        reply_draft,
    };
    let shell = ShellContext {
        current_user: Some((user.id, user.name.clone())),
        logo_version: account.map(|account| account.updated_number),
    };
    let head = Slot::new(head_view(cx, room.id));
    let nav = Slot::new(nav_view(cx, &data));
    let query = query_params::<crate::confirm::ConfirmQuery>(cx)?;
    let confirm_form = query.confirm.as_deref().unwrap_or("");
    let content = Slot::new(content_view(cx, &data, db, &app.bus, confirm_form));
    let footer = Slot::new(composer_view(cx.clone(), &data, &app.bus));
    document_shell(
        cx,
        data.display_name.clone(),
        body_classes("sidebar", admin),
        content,
        crate::flash::Flash::default(),
        shell,
        Some(Slot::new(frame)),
        Some(nav),
        Some(head),
        Some(footer),
    )
    .boxed()
    .async_into_response(cx)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dom_ids_use_client_message_id() {
        assert_eq!(message_dom_id("hq-1", ""), "message_hq-1");
        assert_eq!(message_dom_id("hq-1", "edit"), "edit_message_hq-1");
        assert_eq!(
            room_dom_id(Kind::Open, 201306877, "messages"),
            "messages_rooms_open_201306877"
        );
        assert_eq!(
            room_dom_id(Kind::Direct, 7, "involvement"),
            "involvement_rooms_direct_7"
        );
    }

    #[test]
    fn message_paths_nest_under_rooms() {
        assert_eq!(at_path(201306877, 933434494), "/rooms/201306877/@933434494");
        assert_eq!(
            message_path(201306877, 933434494),
            "/rooms/201306877/messages/933434494"
        );
        assert_eq!(
            edit_message_path(201306877, 933434494),
            "/rooms/201306877/messages/933434494/edit"
        );
    }

    #[test]
    fn sentences_join_with_and() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(sentence_and(&names(&[])), "");
        assert_eq!(sentence_and(&names(&["A"])), "A");
        assert_eq!(sentence_and(&names(&["A", "B"])), "A and B");
        assert_eq!(sentence_and(&names(&["A", "B", "C"])), "A, B, and C");
    }

    #[test]
    fn actor_titles_join_name_and_bio() {
        assert_eq!(actor_title("Kevin", &None), "Kevin");
        assert_eq!(
            actor_title("Kevin", &Some("Programmer".to_string())),
            "Kevin – Programmer"
        );
        assert_eq!(actor_title("Kevin", &Some("  ".to_string())), "Kevin");
    }

    #[test]
    fn timestamps_format_like_local_time() {
        assert_eq!(long_date("2026-09-26T13:01:08Z"), "September 26, 2026");
        assert_eq!(short_time("2026-09-26T13:01:08Z"), "1:01 PM");
        assert_eq!(short_time("2026-09-26T00:05:08Z"), "12:05 AM");
        assert_eq!(short_time("2026-09-26T12:00:08Z"), "12:00 PM");
        assert_eq!(long_date("bogus"), "bogus");
    }

    #[test]
    fn urlsafe_base64_matches_rails() {
        assert_eq!(
            urlsafe_encode64("https://x.test/join/AB"),
            "aHR0cHM6Ly94LnRlc3Qvam9pbi9BQg=="
        );
    }
}

/// `rooms#show` at a message (`/rooms/:room_id/@:message_id`).
///
/// Note: plain `GET /rooms/:id` is served by [`crate::routes::show_room`],
/// which delegates here for HTML clients (the JSON API keeps the route).
///
/// The route macro rejects a literal `@`, so this registers as
/// `/rooms/:room_id/:at_message` AFTER every other `/rooms/*` route
/// (first-registered wins) and 404s segments without the `@` prefix.
#[route(GET "/rooms/{room_id}/{at_message}")]
pub async fn show_at(cx: &Cx) -> Result<Response> {
    use topcoat::router::{error::not_found, response::IntoResponse};
    let room = path_param_segment(cx, "room_id").to_owned();
    let at = path_param_segment(cx, "at_message").to_owned();
    let Some(anchor) = at.strip_prefix('@') else {
        return not_found().into_response(cx);
    };
    render_show(cx, &room, Some(anchor)).await
}

// --- views ---------------------------------------------------------------------

fn head_view(cx: &Cx, room_id: i64) -> impl topcoat::view::View + use<> {
    view! {
        cx =>
        <meta name="current-room-id" content=(room_id.to_string()) />
    }
}

/// `account_logo_tag(style:)`: a nil style leaves a trailing space in
/// the class.
pub(crate) fn account_logo_figure(
    cx: &Cx,
    logo_url: String,
    style: Option<&str>,
) -> impl topcoat::view::View + use<> {
    let class = format!("account-logo avatar {}", style.unwrap_or(""));
    view! {
        cx =>
        <figure class=(class)>
            <img alt="Account logo" width="300" height="300" src=(logo_url) />
        </figure>
    }
}

/// The notification bell (`rooms/involvements/_bell`): a slot the
/// involvement route fills, plus the not-allowed dialog with the PWA
/// settings partials.
fn bell_view(
    cx: &Cx,
    room_id: i64,
    kind: Kind,
    noun: &'static str,
) -> impl topcoat::view::View + use<> {
    use crate::assets::*;
    let frame_id = room_dom_id(kind, room_id, "involvement");
    let involvement_url = format!("/rooms/{room_id}/involvement");
    let label = format!("Notification settings for this {noun}");
    let user_agent = request::headers(cx)
        .get("user-agent")
        .and_then(|value| value.to_str().ok());
    let platform = crate::user_agent::ApplicationPlatform::new(user_agent);
    let root_url = format!("{}/", crate::bots::request_base_url(cx));
    let browser_settings =
        crate::pwa::browser_settings_view(cx, &platform, &root_url).map(Slot::new);
    let system_settings = Slot::new(crate::pwa::system_settings_view(cx, &platform));
    let install_instructions = crate::pwa::install_instructions_view(cx, &platform).map(Slot::new);
    view! {
        cx =>
        <span>
            <span class="button_to_change_notifying">
                <div data-involvement-url=(involvement_url) id=(frame_id)>
                    <button class="btn" type="button">
                        <img aria-hidden="true" src=(img_notification_bell_loading()) width="20" height="20" />
                        <img aria-hidden="true" hidden="hidden" src=(img_notification_bell_alert()) width="20" height="20" />
                        <span class="for-screen-reader">(label)</span>
                    </button>
                </div>
                <dialog class="dialog pad center center-block border-radius border shadow" style="--inline-space: var(--block-space)">
                    <div class="flex flex-column txt-align-center">
                        <span class="btn btn--faux center txt-x-large">
                            <img aria-hidden="true" src=(img_notification_bell_alert()) width="48" height="48" />
                            <span class="for-screen-reader">"Notifications alert"</span>
                        </span>
                        <section>
                            <h1 class="txt-large margin-none">"Notifications aren’t allowed"</h1>
                            <div class="txt-align-start margin-block-start">
                                if let Some(browser_settings) = browser_settings {
                                    (browser_settings)
                                }
                                (system_settings)
                                if let Some(install_instructions) = install_instructions {
                                    (install_instructions)
                                }
                            </div>
                        </section>
                        <form method="dialog" class="flex align-center gap center">
                            <button class="btn dialog__close" autofocus="true">
                                <span class="for-screen-reader">"Close"</span>
                                <img aria-hidden="true" src=(img_remove()) width="20" height="20" />
                            </button>
                        </form>
                    </div>
                </dialog>
            </span>
        </span>
    }
}

fn nav_view(cx: &Cx, data: &ShowData) -> impl topcoat::view::View + use<> {
    use crate::assets::*;
    let has_logo = data.has_logo;
    let logo_url = data.logo_url.clone();
    let is_direct = data.kind == Kind::Direct;
    let display_name = data.display_name.clone();
    let edit_path = data.edit_path.clone();
    let room_id = data.room_id;
    let noun = data.noun;
    let transition = format!("view-transition-name: edit-room-{room_id}");
    let settings_label = format!("Settings for this {noun}");
    let bell = Slot::new(bell_view(cx, room_id, data.kind, noun));
    let logo = Slot::new(account_logo_figure(cx, logo_url.clone(), None));
    view! {
        cx =>
        if has_logo {
            (logo)
        }
        <span class="btn btn--reversed btn--faux room--current">
            <h1 class="room__contents txt-medium overflow-ellipsis">
                if is_direct {
                    <span class="for-screen-reader">"Ping with"</span>
                }
                (display_name)
            </h1>
        </span>
        <a class="btn" style=(transition) data-room-id=(room_id.to_string()) href=(edit_path)>
            <img aria-hidden="true" src=(img_menu_dots_horizontal()) width="20" height="20" />
            <span class="for-screen-reader">(settings_label)</span>
        </a>
        (bell)
    }
}

/// `EmojiHelper::REACTIONS`.
const REACTIONS: [(&str, &str); 8] = [
    ("👍", "Thumbs up"),
    ("👏", "Clapping"),
    ("👋", "Waving hand"),
    ("💪", "Muscle"),
    ("❤️", "Red heart"),
    ("😂", "Face with tears of joy"),
    ("🎉", "Party popper"),
    ("🔥", "Fire"),
];

/// `messages/_actions`: the options popup. Attachments land with
/// UI-07, so the Reply branch always renders until then.
fn actions_view(
    cx: &Cx,
    message: &ShowMessageView,
    csrf_token: &str,
) -> impl topcoat::view::View + use<> {
    use crate::assets::*;
    let csrf_token = csrf_token.to_string();
    let boosts_path = format!("/messages/{}/boosts", message.id);
    let new_boost_path = format!("/messages/{}/boosts/new", message.id);
    let edit_path = edit_message_path(message.room_id, message.id);
    let room_page = format!("/rooms/{}", message.room_id);
    let reply_href = format!("{room_page}?reply_to={}", message.id);
    let at_href = at_path(message.room_id, message.id);
    let reactions: Vec<(String, String)> = REACTIONS
        .iter()
        .map(|(character, title)| (character.to_string(), title.to_string()))
        .collect();
    view! {
        cx =>
        <div class="message__actions">
            <details class="position-relative">
                <summary class="btn message__action-btn message__options-btn">
                    <img class="colorize--black" aria-hidden="true" src=(img_menu_dots_horizontal()) width="20" height="20" />
                    <span class="for-screen-reader">"Message options"</span>
                </summary>
                <div class="message__actions-menu border shadow">
                    <div class="quick-boosts">
                        for (character, title) in reactions {
                            <form action=(boosts_path.clone()) accept-charset="UTF-8" method="post">
                                <input type="hidden" name="authenticity_token" value=(csrf_token.clone()) />
                                <input type="hidden" name="boost[content]" id="boost_content" value=(character.clone()) />
                                <button name="button" type="submit" title=(title.clone()) class="btn message__action-btn" data-emoji=(character.clone())>
                                    <figure class="margin-none boost-character">(character)</figure>
                                    <span class="for-screen-reader">(title)</span>
                                </button>
                            </form>
                        }
                        <a class="btn message__action-btn message__boost-btn" href=(new_boost_path)>
                            <img class="colorize--black" aria-hidden="true" src=(img_boost()) width="20" height="20" />
                            <span class="for-screen-reader">"New boost"</span>
                        </a>
                    </div>
                    <div class="flex flex-wrap border-top margin-block-start-half pad-block-start-half message__actions-grid">
                        <a class="btn message__action-btn center full-width" title="Reply" aria-label="Reply" data-tip="Reply" href=(reply_href)>
                            <img class="colorize--black" aria-hidden="true" src=(img_reply()) width="20" height="20" />
                        </a>
                        <a class="btn message__action-btn center full-width" title="Message link" aria-label="Message link" data-tip="Message link" href=(at_href)>
                            <img class="colorize--black" aria-hidden="true" src=(img_link()) width="20" height="20" />
                        </a>
                        <a class="btn message__action-btn center full-width message__edit-btn" title="Edit" aria-label="Edit" data-tip="Edit" href=(edit_path)>
                            <img class="colorize--black" aria-hidden="true" src=(img_pencil()) width="20" height="20" />
                        </a>
                    </div>
                </div>
            </details>
        </div>
    }
}

/// One boost chip (`messages/boosts/_boost`).
fn boost_view(
    cx: &Cx,
    boost: &BoostItemView,
    csrf_token: &str,
    confirm_form: &str,
    room_page: &str,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let dom_id = format!("boost_{}", boost.id);
    let path = format!("/users/{}", boost.booster.id);
    let delete_path = format!("/messages/{}/boosts/{}", boost.message_id, boost.id);
    let delete_form = format!("delete-boost-{}", boost.id);
    let trigger_href = format!("{room_page}?confirm={delete_form}");
    let boost_dialog = Slot::new(crate::confirm::delete_dialog_view(
        cx,
        &delete_form,
        "Delete",
        "Delete boost?",
        "Are you sure you want to remove your boost?",
        confirm_form == delete_form,
        room_page,
    ));
    let title = boost.booster.title.clone();
    let avatar_url = boost.booster.avatar_url.clone();
    let aria_label = format!("{} boosted {}", boost.booster.name, boost.content);
    let content = boost.content.clone();
    let csrf = csrf_token.to_string();
    let content_class: &'static str = if boost.all_emoji {
        "txt-small txt-medium"
    } else {
        "txt-small"
    };
    view! {
        cx =>
        <div id=(dom_id) class="boost boost-item flex-inline postion--relative max-width align-center fill-white gap">
                <figure class="avatar boost__avatar flex-item-no-shrink">
                    <a title=(title) class="btn avatar" href=(path)><img aria-label=(aria_label) src=(avatar_url) width="48" height="48" /></a>
                </figure>
                <span class=(content_class)>(content)</span>
                <form id=(delete_form) class="button_to" method="post" action=(delete_path)>
                    <input type="hidden" name="_method" value="delete" />
                    <input type="hidden" name="authenticity_token" value=(csrf) />
                </form>
                <a class="btn btn--negative flex-item-justify-end boost__delete" data-tip="Delete this boost" href=(trigger_href)>
                    <img aria-hidden="true" src=(img_minus()) width="20" height="20" />
                    <span class="for-screen-reader">"Delete this boost"</span>
                </a>
                (boost_dialog)
            </div>
            <span id="delete_boost_accessible_label" class="for-screen-reader">"Press enter to delete this boost"</span>
    }
    .boxed()
}

/// `messages/boosts/_boosts`: the boosting frame around a message's
/// chips and the inline new-boost link.
pub(crate) fn boosts_view(
    cx: &Cx,
    message: &ShowMessageView,
    csrf_token: &str,
    confirm_form: &str,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    // Owned: the fragment response outlives the caller's view.
    let message = message.clone();
    let boosting = message_dom_id(&message.client_message_id, "boosting");
    let boosts_id = message_dom_id(&message.client_message_id, "boosts");
    let new_boost_frame = message_dom_id(&message.client_message_id, "new_boost");
    let new_boost_path = format!("/messages/{}/boosts/new", message.id);
    let room_page = format!("/rooms/{}", message.room_id);
    let chips: Vec<_> = message
        .boosts
        .iter()
        .map(|boost| Slot::new(boost_view(cx, boost, csrf_token, confirm_form, &room_page)))
        .collect();
    view! {
        cx =>
        <div id=(boosting)>
            <div class="boosts flex flex-wrap align-center gap full-width" style="--column-gap: 0.4ch; --row-gap: 0">
                <div class="flex-inline flex-wrap gap" id=(boosts_id)>
                    for chip in chips {
                        (chip)
                    }
                </div>
                <div id=(new_boost_frame)>
                    <div class="flex-inline message__boost-inline">
                        <a class="boost__action txt-small btn" href=(new_boost_path)>
                            <img aria-hidden="true" src=(img_boost()) width="20" height="20" />
                            <span class="for-screen-reader">"Add a boost"</span>
                        </a>
                    </div>
                </div>
            </div>
        </div>
    }
    .boxed()
}

/// `messages/_presentation`: the edited-message replace target
/// (`presentation_message_<client-id>`). An attached file replaces
/// the body, as upstream's `message.content` does.
pub(crate) fn message_presentation_view(
    cx: &Cx,
    message: &ShowMessageView,
) -> topcoat::view::BoxView<'static> {
    let presentation_id = message_dom_id(&message.client_message_id, "presentation");
    let html = match &message.attachment {
        Some(att) => {
            // Bundled icon URLs, the way `Asset` values render in views.
            let config = topcoat::context::try_app_context::<topcoat::asset::AssetConfig>(cx)
                .expect("asset bundle registered");
            let file_icon = config.resolve(crate::assets::img_common_file_text());
            let download_icon = config.resolve(crate::assets::img_download());
            let share_icon = config.resolve(crate::assets::img_share());
            crate::richtext::attachment_html(&crate::richtext::AttachmentRef {
                filename: &att.filename,
                content_type: &att.content_type,
                blob_path: &att.blob_path,
                download_path: &att.download_path,
                thumb_url: &att.thumb_url,
                width: att.width,
                height: att.height,
                file_icon: &file_icon,
                download_icon: &download_icon,
                share_icon: &share_icon,
            })
        }
        None => message.body_html.clone(),
    };
    let content = Unescaped::new_unchecked(html);
    view! {
        cx =>
        <div id=(presentation_id) dir="auto">
            (content)
        </div>
    }
    .boxed()
}

/// `messages/_message` (text or attachment content).
pub(crate) fn message_view(
    cx: &Cx,
    message: &ShowMessageView,
    csrf_token: &str,
    confirm_form: &str,
) -> topcoat::view::BoxView<'static> {
    if message.unrenderable {
        return view! {
            cx =>
            <div class="message message--formatted message--failed center">
                <div class="message__body">
                    <div class="message__body-content txt-align-center">
                        "Failed to load message content"
                    </div>
                </div>
            </div>
        }
        .boxed();
    }
    let dom_id = message_dom_id(&message.client_message_id, "");
    let mut class = String::from("message");
    if message.all_emoji {
        class.push_str(" message--emoji");
    }
    // Pre-formatted: upstream's controller adds these client-side.
    class.push_str(" message--formatted");
    if message.me {
        class.push_str(" message--me");
    }
    if message.first_of_day {
        class.push_str(" message--first-of-day");
    }
    if message.threaded {
        class.push_str(" message--threaded");
    }
    if message.mentioned {
        class.push_str(" message--mentioned");
    }
    let creator_id = message.creator.id.to_string();
    let message_id = message.id.to_string();
    let created_ms = message.created_ms.to_string();
    let updated_ms = message.updated_ms.to_string();
    let created_iso = message.created_iso.clone();
    let creator_title = message.creator.title.clone();
    let creator_path = format!("/users/{}", message.creator.id);
    let creator_avatar = message.creator.avatar_url.clone();
    let creator_name = message.creator.name.clone();
    let edit_frame = message_dom_id(&message.client_message_id, "edit");
    let at_path = at_path(message.room_id, message.id);
    let room_name = message.room_name.clone();
    let actions = Slot::new(actions_view(cx, message, csrf_token));
    let presentation = Slot::new(message_presentation_view(cx, message));
    let boosts = Slot::new(boosts_view(cx, message, csrf_token, confirm_form));
    view! {
        cx =>
        <div id=(dom_id) class=(class) data-user-id=(creator_id) data-message-id=(message_id) data-message-timestamp=(created_ms.clone()) data-message-updated-at=(updated_ms) data-sort-value=(created_ms)>
            <h2 class="message__day-separator"><time datetime=(created_iso.clone())>(long_date(&created_iso))</time></h2>
            <figure class="avatar message__avatar">
                <a title=(creator_title.clone()) class="btn avatar" href=(creator_path)><img aria-hidden="true" src=(creator_avatar) width="48" height="48" /></a>
            </figure>
            <div id=(edit_frame) style="display: contents;">
                <div class="message__body">
                    <div class="message__body-content">
                        <div class="message__meta">
                            <h3 class="message__heading">
                                <span class="message__author" title=(creator_title)>
                                    <strong>(creator_name)</strong>
                                </span>
                                <a class="message__permalink" href=(at_path.clone())><time class="message__timestamp" datetime=(created_iso.clone())>(short_time(&created_iso))</time></a>
                                <span class="message__room">
                                    <a href=(at_path)>(room_name)</a>
                                </span>
                            </h3>
                            (actions)
                        </div>
                        (presentation)
                        (boosts)
                    </div>
                </div>
            </div>
        </div>
    }
    .boxed()
}

/// `translations_for("invite_message")` entries.
const INVITE_TRANSLATIONS: [(&str, &str); 7] = [
    (
        "🇺🇸",
        "Welcome to Topcamp. To invite some people to chat with you, share the join link below.",
    ),
    (
        "🇪🇸",
        "Bienvenido a Topcamp. Para invitar a algunas personas a chatear contigo, comparte el enlace de unión que se encuentra a continuación.",
    ),
    (
        "🇫🇷",
        "Bienvenue sur Topcamp. Pour inviter des personnes à discuter avec vous, partagez le lien pour rejoindre ci-dessous.",
    ),
    (
        "🇮🇳",
        "Topcamp में आपका स्वागत है। अधिक लोगों को चैट के लिए आमंत्रित करने के लिए, नीचे जुड़ने का लिंक साझा करें।",
    ),
    (
        "🇩🇪",
        "Willkommen bei Topcamp. Um einige Personen zum Chatten einzuladen, teilen Sie den unten stehenden Beitrittslink.",
    ),
    (
        "🇧🇷",
        "Boas vindas ao Topcamp. Para convidar pessoas para conversarem com você, compartilhe o link de convite abaixo.",
    ),
    (
        "🇯🇵",
        "Topcampへようこそ。他の人をチャットに招待するには、下記の参加リンクを共有してください。",
    ),
];

/// `translation_button("invite_message")`.
fn translation_view(cx: &Cx) -> impl topcoat::view::View + use<> {
    use crate::assets::*;
    let entries: Vec<(String, String)> = INVITE_TRANSLATIONS
        .iter()
        .map(|(language, text)| (language.to_string(), text.to_string()))
        .collect();
    view! {
        cx =>
        <details class="position-relative">
            <summary class="btn" tabindex="-1">
                <img src=(img_globe()) width="20" height="20" aria-hidden="true" class="color-icon" />
                <span class="for-screen-reader">"Translate"</span>
            </summary>
            <div class="language-list-menu shadow">
                <dl class="language-list">
                    for (language, text) in entries {
                        <dt>(language)</dt>
                        <dd class="margin-none">(text)</dd>
                    }
                </dl>
            </div>
        </details>
    }
}

/// `Base64.urlsafe_encode64` (padded), for the QR code route.
pub(crate) fn urlsafe_encode64(input: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE.encode(input.as_bytes())
}

/// `accounts/_invite`: the join link with its QR and regenerate
/// controls. The link itself is a readonly selectable input, so no
/// JS copy button is needed; the QR route lands with UI-11.
pub(crate) fn invite_view(
    cx: &Cx,
    join_url: &str,
    admin: bool,
    csrf_token: &str,
) -> impl topcoat::view::View + use<> {
    use crate::assets::*;
    let url = join_url.to_string();
    let qr_path = format!("/qr_code/{}", urlsafe_encode64(join_url));
    let csrf = csrf_token.to_string();
    view! {
        cx =>
        <div class="flex flex-column align-center gap">
            <label class="flex flex-column gap full-width" style="--row-gap: 0.5em">
                <strong id="invite_label" class="invite-label">"Share to invite more people"</strong>
                <span class="flex align-center gap input input--actor fill-white">
                    <img aria-hidden="true" width="20" height="20" class="colorize--black" src=(img_person_add()) />
                    <input type="text" class="input" id="invite_url" value=(url.clone()) aria-labelledby="invite_label" readonly="readonly" />
                </span>
            </label>
            <div class="flex align-center gap">
                <a class="btn" href=(qr_path)>
                    <span class="for-screen-reader">"Show join link QR code"</span>
                    <img aria-hidden="true" width="20" height="20" class="colorize--black" src=(img_qr_code()) />
                </a>
                if admin {
                    <form class="button_to" method="post" action="/account/join_code">
                        <input type="hidden" name="authenticity_token" value=(csrf) />
                        <button class="btn btn--regenerate" type="submit">
                            <img aria-hidden="true" width="20" height="20" class="colorize--black" src=(img_refresh()) />
                            <span class="for-screen-reader">"Regenerate join link"</span>
                        </button>
                    </form>
                }
            </div>
        </div>
    }
}

/// `rooms/show/_invitation`: the welcome card on the original room.
fn invitation_view(cx: &Cx, data: &ShowData) -> impl topcoat::view::View + use<> {
    let logo = Slot::new(account_logo_figure(
        cx,
        data.logo_url.clone(),
        Some("center margin-block-end txt-large"),
    ));
    let translation = Slot::new(translation_view(cx));
    let invite = Slot::new(invite_view(
        cx,
        &data.join_url,
        data.admin,
        &data.csrf_token,
    ));
    view! {
        cx =>
        <div id="system_welcome" class="message message--formatted txt-align-center center">
            <div class="message__body center">
                <div class="message__body-content position-relative">
                    (logo)
                    <div class="flex align-center gap">
                        <div class="system-welcome--translation">
                            (translation)
                        </div>
                        <p>
                            <strong>"Welcome to Topcamp"</strong><br />
                            "To invite people to chat, share the join link below."
                        </p>
                    </div>
                    (invite)
                </div>
            </div>
        </div>
    }
}

fn content_view(
    cx: &Cx,
    data: &ShowData,
    db: &PgDb,
    bus: &crate::live::LiveBus,
    confirm_form: &str,
) -> impl topcoat::view::View + use<> {
    let messages_dom_id = data.messages_dom_id.clone();
    let invitation = data.invitation;
    let older_href = data.older_href.clone();
    let invite = Slot::new(invitation_view(cx, data));
    let csrf_token = data.csrf_token.clone();
    // Live regions (UI-05r): one per message plus the cons-tail.
    let items: Vec<_> = data
        .messages
        .iter()
        .map(|message| {
            Slot::new(crate::room_live::message_live(
                cx,
                crate::room_live::LiveMessage {
                    db: db.clone(),
                    bus: bus.clone(),
                    room_id: data.room_id,
                    message_id: message.id,
                    client_message_id: message.client_message_id.clone(),
                    room_name: data.room_name.clone(),
                    me_id: data.me.id,
                    csrf_token: csrf_token.clone(),
                    confirm_form: confirm_form.to_string(),
                    initial: message.clone(),
                },
            ))
        })
        .collect();
    let prev = data.messages.last().map(|message| MessageDetail {
        id: message.id,
        room_id: message.room_id,
        creator_id: message.creator.id,
        client_message_id: message.client_message_id.clone(),
        created_ms: message.created_ms,
        updated_ms: message.updated_ms,
        created_iso: message.created_iso.clone(),
        body: None,
    });
    let tail = Slot::new(crate::room_live::tail_live(
        cx,
        crate::room_live::LiveTail {
            db: db.clone(),
            bus: bus.clone(),
            room_id: data.room_id,
            // Max id shown: concurrent posts can invert id/created
            // order, and the tail pages by id — a last-row cursor
            // would re-append the higher id.
            after_id: data
                .messages
                .iter()
                .map(|message| message.id)
                .max()
                .unwrap_or(0),
            room_name: data.room_name.clone(),
            me_id: data.me.id,
            csrf_token: csrf_token.clone(),
            prev,
        },
    ));
    view! {
        cx =>
        <div id="message-area" class="message-area" contents="true">
            <div id=(messages_dom_id) class="messages">
                if invitation {
                    (invite)
                }
                if let Some(older_href) = older_href {
                    <a class="btn center" href=(older_href)>"Load older messages"</a>
                }
                for item in items {
                    (item)
                }
                (tail)
            </div>
        </div>
    }
}

/// `rooms/show/_composer`: the message form. The `lexxy-editor`
/// nests a textarea fallback so posting works without Lexxy's JS;
/// only the textarea submits (`lexxy-editor` is not a control).
fn composer_view(
    cx: Cx,
    data: &ShowData,
    bus: &crate::live::LiveBus,
) -> impl topcoat::view::View + use<> {
    use crate::assets::*;
    use crate::live::{post_message, typing_start, typing_stop};
    use topcoat::runtime::{Event, signal};
    let action = format!("/rooms/{}/messages", data.room_id);
    let mention_src = data.mention_src.clone();
    // Upstream relies on Turbo's CSRF header (no token field); without
    // Turbo the token rides as a hidden field instead (UI-05).
    let csrf_token = data.csrf_token.clone();
    let room = data.room_id;
    let reply_draft = data.reply_draft.clone();
    let draft_initial = reply_draft.clone();
    let typing = Slot::new(crate::room_live::typing_live(
        &cx,
        crate::room_live::LiveTyping {
            bus: bus.clone(),
            room_id: data.room_id,
            me_id: data.me.id,
        },
    ));
    view! {
        cx =>
        // Composer state (UI-05r): the draft mirrors the textarea, `keys`
        // throttles typing pings, `err` shows post failures. The draft
        // starts from the server-rendered `?reply_to=` quote so posting
        // works with or without the runtime.
        let draft = signal(&cx, move || draft_initial.clone());
        let keys = signal(&cx, || 0i64);
        let err = signal(&cx, String::new);
        <div class="composer flex align-end gap position-relative">
            <a class="btn flex-item-no-shrink margin-block-end composer__context-btn" style="view-transition-name: input-switcher" href="/searches">
                <img aria-hidden="true" src=(img_search()) width="20" height="20" />
                <span class="for-screen-reader">"Search"</span>
            </a>
            <div id="composer-frame">
                <form id="composer" class="margin-block flex-item-grow contain" action=(action) accept-charset="UTF-8" method="post" enctype="multipart/form-data" @submit=$(async |e: Event| { e.prevent_default(); if !draft.get().trim().is_empty() { let result = post_message(room, draft.get()).await; if result.is_ok() { draft.set("".to_owned()); keys.set(0i64); err.set("".to_owned()); typing_stop(room).await; } else { err.set(result.unwrap_err()); } } })>
                    <input type="hidden" name="authenticity_token" value=(csrf_token) />
                    <p class="txt-small" :hidden=$(err.get().is_empty())>$(err.get())</p>
                    <fieldset contents="">
                        <div class="flex flex-column">
                            <div class="composer__filelist flex flex--align-center gap flex-wrap"></div>
                            <div class="flex composer__input input input--actor fill-white min-width" style="--input-border-radius: 1.3rem">
                                <div class="flex align-end gap full-width">
                                    <img aria-hidden="true" class="composer__input-hint colorize--black" style="view-transition-name: input-btn;" src=(img_messages_outlined()) width="22" height="22" />
                                    <div class="flex flex-column flex-item-grow min-width gap">
                                        <lexxy-editor rows="1" class="input lexxy-content" style="order: -1" aria-multiline="true" aria-label="Write a message" permitted-attachment-types="application/vnd.topcamp.mention application/vnd.actiontext.opengraph-embed" data-direct-upload-url="/rails/active_storage/direct_uploads" data-blob-url-template="/rails/active_storage/blobs/redirect/:signed_id/:filename" id="message_body" input="message_body_trix_input_message" name="message[body]">
                                            <textarea name="message[body]" rows="1" aria-label="Write a message" class="input" style="background: transparent; border: 0; width: 100%; resize: none; min-height: 24px; padding: 0; field-sizing: content;" :value=$(draft.get()) @input=$(async |e: Event| { let v = e.target.value; let empty = v.is_empty(); draft.set(v); keys.set(keys.get() + 1i64); if keys.get() % 8i64 == 0i64 { typing_start(room).await; } if empty { typing_stop(room).await; } }) @keydown=$(async |e: Event| { if e.key == "Enter" { if !e.shift_key { if !e.is_composing { e.prevent_default(); if !draft.get().trim().is_empty() { let result = post_message(room, draft.get()).await; if result.is_ok() { draft.set("".to_owned()); keys.set(0i64); err.set("".to_owned()); typing_stop(room).await; } else { err.set(result.unwrap_err()); } } } } } })>(reply_draft)</textarea>
                                            <lexxy-prompt trigger="@" name="mention" src=(mention_src) remote-filtering="true" empty-results="No matches"></lexxy-prompt>
                                        </lexxy-editor>
                                    </div>
                                    <label class="btn btn--borderless txt-small flex-item-no-shrink composer__attachment-btn input--file">
                                        <img class="colorize--black" aria-hidden="true" src=(img_attachment()) width="22" height="22" />
                                        <input type="file" name="message[attachment]" />
                                        <span class="for-screen-reader">"Attach a file"</span>
                                    </label>
                                    <button class="btn btn--borderless txt-small flex-item-no-shrink composer__rich-text-btn" type="button">
                                        <img class="colorize--black" aria-hidden="true" src=(img_text_options()) width="20" height="20" />
                                        <span class="for-screen-reader">"Rich text"</span>
                                    </button>
                                    <button name="send" type="submit" class="btn btn--reversed flex-item-no-shrink txt-small">
                                        <img aria-hidden="true" src=(img_arrow_up()) width="20" height="20" />
                                        <span class="for-screen-reader">"Send Message"</span>
                                    </button>
                                </div>
                            </div>
                        </div>
                    </fieldset>
                    (typing)
                    <input type="hidden" name="message[client_message_id]" id="message_client_message_id" />
                </form>
            </div>
        </div>
    }
}
