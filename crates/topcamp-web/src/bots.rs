//! Chat bots: the bot JSON API + the admin bot pages.
//!
//! `Messages::ByBotsController` serves JSON under
//! `/rooms/:room_id/:bot_key/messages` (index/create/update/destroy)
//! with boosts under `.../messages/:message_id/boosts`
//! (create/destroy). Authentication is the session cookie or the
//! `"<id>-<token>"` bot key (path segment here, `?bot_key=` on
//! interactive routes, where [`deny_bots`] answers 403); anything
//! else redirects to sign in. `Accounts::BotsController` manages the
//! bots at `/account/bots` (index/new/create/edit/update/destroy)
//! with key resets at `/account/bots/:bot_id/key`.

use std::collections::HashMap;

use serde::Serialize;
use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        Body, Slot,
        error::{bad_request, forbidden, not_found, see_other},
        path_param_segment, query_params, request,
        response::{IntoResponse, Response},
        route, to_bytes,
    },
    view::{ViewExt as _, view},
};

use topcamp_db::PgDb;
use topcamp_db::repositories::{
    AccountRepository, AttachmentRepository, BoostDetail, BotRow, MembershipRepository,
    MessageDetail, MessageRepository, RoomRepository, RoomRow, UserRepository, UserRow,
    WebhookRepository,
};
use topcamp_domain::auth::UserRole;

use crate::pages::{ShellContext, body_classes, document_shell};
use crate::rooms::cast_integer;
use crate::state::{AppState, http_error};

// --- bot authentication ------------------------------------------------------

/// Who called: the session user wins over the bot key, like
/// `restore_authentication || bot_authentication`.
pub(crate) enum Caller {
    Session(UserRow),
    Bot(BotRow),
}

impl Caller {
    fn id(&self) -> i64 {
        match self {
            Caller::Session(user) => user.id,
            Caller::Bot(bot) => bot.id,
        }
    }

    /// `can_administer` without a record: bots never administer.
    fn admin(&self) -> bool {
        match self {
            Caller::Session(user) => user.role == UserRole::Administrator.value(),
            Caller::Bot(_) => false,
        }
    }
}

#[query_params(error = bad_request)]
struct BotKeyQuery {
    bot_key: Option<String>,
}

/// Ruby's `String#strip` (NUL, ASCII whitespace, vertical tab).
fn ruby_strip(value: &str) -> &str {
    value.trim_matches(|c: char| c == '\0' || c.is_ascii_whitespace() || c == '\u{b}')
}

/// `require_authentication` for bot routes: session user, else the
/// bot key (path segment, else `?bot_key=`), else nobody (the caller
/// redirects, like `request_authentication`).
pub(crate) async fn authenticate_caller(cx: &Cx, path_key: Option<&str>) -> Result<Option<Caller>> {
    if let Some(user) = crate::auth::current_user(cx).await? {
        return Ok(Some(Caller::Session(user)));
    }
    let query_key = match path_key {
        Some(_) => None,
        None => query_params::<BotKeyQuery>(cx)?.bot_key.clone(),
    };
    let raw = path_key.or(query_key.as_deref()).unwrap_or("");
    let key = ruby_strip(raw);
    if key.is_empty() {
        return Ok(None);
    }
    let db = &app_context::<AppState>(cx).db;
    let bot = UserRepository::authenticate_bot(db, key)
        .await
        .map_err(http_error)?;
    Ok(bot.map(Caller::Bot))
}

/// `deny_bots` for interactive routes: no session but a valid bot key
/// is 403 (bots only speak the bot API). Session holders pass —
/// `restore_authentication` wins over the key upstream too.
pub(crate) async fn deny_bots(cx: &Cx) -> Result<()> {
    if crate::auth::current_user(cx).await?.is_some() {
        return Ok(());
    }
    if valid_bot_key_present(cx).await? {
        return Err(forbidden().into());
    }
    Ok(())
}

/// A present, valid `?bot_key=` (blank keys never authenticate).
async fn valid_bot_key_present(cx: &Cx) -> Result<bool> {
    let key = query_params::<BotKeyQuery>(cx)?
        .bot_key
        .as_deref()
        .map(ruby_strip)
        .unwrap_or("");
    if key.is_empty() {
        return Ok(false);
    }
    let db = &app_context::<AppState>(cx).db;
    let bot = UserRepository::authenticate_bot(db, key)
        .await
        .map_err(http_error)?;
    Ok(bot.is_some())
}

// --- JSON views --------------------------------------------------------------

/// Rails' `render json:` escaping (`<`, `>`, `&` as escapes).
fn rails_json_escape(json: &str) -> String {
    json.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

fn to_rails_json<T: Serialize>(value: &T) -> String {
    rails_json_escape(&serde_json::to_string(value).expect("serializable"))
}

/// `json_time`: UTC `YYYY-MM-DDTHH:MM:SS.mmmZ` from epoch millis.
fn json_time(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let ms = millis.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let clock = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        clock / 3600,
        clock % 3600 / 60,
        clock % 60
    )
}

/// Days since the Unix epoch to a civil date (Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// `users/_user.json.jbuilder` (field order is the key order).
#[derive(Serialize)]
struct UserJson {
    id: i64,
    name: String,
    role: String,
    avatar_url: String,
}

/// `messages/_message.json.jbuilder`.
#[derive(Serialize)]
struct MessageJson {
    id: i64,
    created_at: String,
    body: MessageBodyJson,
    creator: UserJson,
    room: IdJson,
    url: String,
}

#[derive(Serialize)]
struct MessageBodyJson {
    plain_text: String,
    html: String,
}

#[derive(Serialize)]
struct IdJson {
    id: i64,
}

/// `messages/boosts/_boost.json.jbuilder`.
#[derive(Serialize)]
struct BoostJson {
    id: i64,
    content: String,
    created_at: String,
    booster: UserJson,
    message: BoostMessageJson,
}

#[derive(Serialize)]
struct BoostMessageJson {
    id: i64,
    url: String,
}

fn role_name(role: i32) -> String {
    match role {
        1 => "administrator".to_string(),
        2 => "bot".to_string(),
        _ => "member".to_string(),
    }
}

/// Absolute request origin (`http` unless a forwarded proto says
/// otherwise); empty (relative URLs) when Host is absent.
pub(crate) fn request_base_url(cx: &Cx) -> String {
    let headers = request::headers(cx);
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim();
    if host.is_empty() {
        return String::new();
    }
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http")
        .split(',')
        .next()
        .unwrap_or("http")
        .trim();
    format!("{proto}://{host}")
}

/// One request's user lookups (creators + mention targets), fetched
/// once like the presenter's preloads.
struct JsonUsers<'a> {
    db: &'a PgDb,
    base_url: String,
    profiles: HashMap<i64, topcamp_db::repositories::ProfileUser>,
    forms: HashMap<i64, topcamp_db::repositories::FormUser>,
}

impl<'a> JsonUsers<'a> {
    async fn load(db: &'a PgDb, base_url: String, messages: &[MessageDetail]) -> Result<Self> {
        let mut ids: Vec<i64> = messages.iter().map(|m| m.creator_id).collect();
        for message in messages {
            ids.extend(crate::richtext::mentioned_user_ids(
                message.body.as_deref().unwrap_or(""),
            ));
        }
        ids.sort_unstable();
        ids.dedup();
        let forms = UserRepository::form_users(db, &ids)
            .await
            .map_err(http_error)?;
        let mut profiles = HashMap::new();
        for message in messages {
            if !profiles.contains_key(&message.creator_id)
                && let Some(profile) = UserRepository::find_profile(db, message.creator_id)
                    .await
                    .map_err(http_error)?
            {
                profiles.insert(message.creator_id, profile);
            }
        }
        Ok(Self {
            db,
            base_url,
            profiles,
            forms: forms.into_iter().map(|user| (user.id, user)).collect(),
        })
    }

    async fn profile(
        &mut self,
        user_id: i64,
    ) -> Result<Option<&topcamp_db::repositories::ProfileUser>> {
        if !self.profiles.contains_key(&user_id) {
            let profile = UserRepository::find_profile(self.db, user_id)
                .await
                .map_err(http_error)?;
            if let Some(profile) = profile {
                self.profiles.insert(user_id, profile);
            }
        }
        Ok(self.profiles.get(&user_id))
    }

    fn user_json(&self, user_id: i64) -> Option<UserJson> {
        self.profiles.get(&user_id).map(|profile| UserJson {
            id: profile.id,
            name: profile.name.clone(),
            role: role_name(profile.role),
            avatar_url: format!(
                "{}{}",
                self.base_url,
                crate::users::avatar_url_for(profile.id, &profile.updated_number)
            ),
        })
    }

    /// `present_rich` over the message's stored body (the same HTML
    /// the room renders, mentions resolved).
    fn body_html(&self, stored: &str) -> String {
        let mentions: HashMap<i64, crate::richtext::MentionUser> =
            crate::richtext::mentioned_user_ids(stored)
                .into_iter()
                .filter_map(|id| {
                    self.forms.get(&id).map(|user| {
                        let title = match user.bio.as_deref().filter(|bio| !bio.trim().is_empty()) {
                            Some(bio) => format!("{} \u{2013} {bio}", user.name),
                            None => user.name.clone(),
                        };
                        (
                            id,
                            crate::richtext::MentionUser {
                                id: user.id,
                                name: user.name.clone(),
                                title,
                                avatar_url: crate::users::avatar_url_for(
                                    user.id,
                                    &user.updated_number,
                                ),
                            },
                        )
                    })
                })
                .collect();
        crate::richtext::present_rich(stored, &mentions)
    }

    fn message_json(&self, message: &MessageDetail) -> Option<MessageJson> {
        let stored = message.body.as_deref().unwrap_or("");
        Some(MessageJson {
            id: message.id,
            created_at: json_time(message.created_ms),
            body: MessageBodyJson {
                plain_text: crate::richtext::plain_text(stored),
                html: self.body_html(stored),
            },
            creator: self.user_json(message.creator_id)?,
            room: IdJson {
                id: message.room_id,
            },
            url: format!(
                "{}/rooms/{}/messages/{}",
                self.base_url, message.room_id, message.id
            ),
        })
    }

    /// `boost_json`: the booster resolves like any creator (gone
    /// boosters drop the row, like the missing-creator guard).
    async fn boost_json(
        &mut self,
        boost: &BoostDetail,
        message: &MessageDetail,
    ) -> Result<Option<BoostJson>> {
        if self.profile(boost.booster_id).await?.is_none() {
            return Ok(None);
        }
        let Some(booster) = self.user_json(boost.booster_id) else {
            return Ok(None);
        };
        Ok(Some(BoostJson {
            id: boost.id,
            content: boost.content.clone(),
            // Boosts never update, so `updated_at` is the creation time.
            created_at: json_time(boost.updated_ms),
            booster,
            message: BoostMessageJson {
                id: message.id,
                url: format!(
                    "{}/rooms/{}/messages/{}",
                    self.base_url, message.room_id, message.id
                ),
            },
        }))
    }
}

// --- bot message API ---------------------------------------------------------

/// `set_room` for bots: the room among the caller's rooms, else 404.
async fn bot_room(db: &PgDb, caller: &Caller, param: &str) -> Result<Option<RoomRow>> {
    match cast_integer(param) {
        Some(id) => RoomRepository::find_for_user(db, caller.id(), id)
            .await
            .map_err(http_error),
        None => Ok(None),
    }
}

/// `@room.messages.find(params[:id])`.
async fn bot_message(db: &PgDb, room_id: i64, param: &str) -> Result<Option<MessageDetail>> {
    match cast_integer(param) {
        Some(id) => MessageRepository::find_detail_in_room(db, room_id, id)
            .await
            .map_err(http_error),
        None => Ok(None),
    }
}

fn ensure_can_administer(caller: &Caller, creator_id: i64) -> Result<()> {
    if caller.admin() || caller.id() == creator_id {
        Ok(())
    } else {
        Err(forbidden().into())
    }
}

fn json_response(status: u16, body: String, headers: &[(&str, String)]) -> Result<Response> {
    let mut response = Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(body))
        .expect("json builds");
    for (name, value) in headers {
        response.headers_mut().insert(
            name.parse::<http::HeaderName>()
                .expect("header name parses"),
            value.parse().expect("header parses"),
        );
    }
    Ok(response)
}

fn head(status: u16) -> Result<Response> {
    Ok(Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("head builds"))
}

#[query_params(error = bad_request)]
struct AfterQuery {
    after: Option<String>,
}

/// `messages/by_bots#index`: the page as JSON (`X-Total-Count` +
/// a `Link` next), like the HTML index's paging.
async fn index_api(cx: &Cx, room_param: &str, key_param: &str) -> Result<Response> {
    let Some(caller) = authenticate_caller(cx, Some(key_param)).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let app = app_context::<AppState>(cx);
    let Some(room) = bot_room(&app.db, &caller, room_param).await? else {
        return not_found().into_response(cx);
    };
    let Some(messages) = crate::messages::find_paged_messages(cx, &app.db, room.id).await? else {
        return not_found().into_response(cx);
    };
    let count = MessageRepository::count_in_room(&app.db, room.id)
        .await
        .map_err(http_error)?;
    let query = query_params::<AfterQuery>(cx)?;
    let after_mode = query
        .after
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    // `after` pages forward from the last; `before` and the default
    // last page step back from the first.
    let next = match (messages.first(), messages.last()) {
        (Some(_), Some(last)) if after_mode => {
            if MessageRepository::exists_after(&app.db, room.id, last.id)
                .await
                .map_err(http_error)?
            {
                Some(("after", last.id))
            } else {
                None
            }
        }
        (Some(first), Some(_)) => {
            if MessageRepository::exists_before(&app.db, room.id, first.id)
                .await
                .map_err(http_error)?
            {
                Some(("before", first.id))
            } else {
                None
            }
        }
        _ => None,
    };
    let base = request_base_url(cx);
    let users = JsonUsers::load(&app.db, base.clone(), &messages).await?;
    let views: Vec<MessageJson> = messages
        .iter()
        .filter_map(|message| users.message_json(message))
        .collect();
    let mut headers = vec![("x-total-count", count.to_string())];
    if let Some((key, id)) = next {
        headers.push((
            "link",
            format!(
                "<{base}/rooms/{}/{key_param}/messages?{key}={id}>; rel=\"next\"",
                room.id
            ),
        ));
    }
    json_response(200, to_rails_json(&views), &headers)
}

/// The bot create/update attachment decision (`Assignment` without
/// the uploader: top-level `attachment`).
enum BotAttachment {
    /// The key wasn't given: raw body (create) or untouched (update).
    Unchanged,
    /// A file part: attach it (create) or replace with it (update).
    Upload(crate::messages::MessageFile),
    /// `nil` or `""`: nothing on a new record, detach on update.
    Delete,
    /// Anything else: `Could not find or build blob` (500).
    Invalid,
}

/// Parse a bot write body: multipart top-level `attachment`,
/// urlencoded `attachment`, else the raw body as the text.
struct BotWrite {
    text: Option<String>,
    attachment: BotAttachment,
}

async fn parse_bot_write(cx: &Cx, body: Body) -> Result<BotWrite> {
    let content_type = request::headers(cx)
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    if content_type.starts_with("multipart/form-data") {
        return parse_multipart_bot_write(&content_type, &to_bytes(body, 128 * 1024 * 1024).await?)
            .await;
    }
    let raw = to_bytes(body, 1024 * 1024).await?;
    if content_type.starts_with("application/x-www-form-urlencoded") {
        let mut attachment: Option<Option<String>> = None;
        if let Ok(pairs) = serde_urlencoded::from_bytes::<Vec<(String, String)>>(&raw) {
            for (key, value) in pairs {
                if key == "attachment" {
                    attachment = Some(if value.is_empty() { None } else { Some(value) });
                }
            }
        }
        if let Some(found) = attachment {
            return Ok(BotWrite {
                text: None,
                attachment: match found {
                    None => BotAttachment::Delete,
                    Some(_) => BotAttachment::Invalid,
                },
            });
        }
    }
    Ok(BotWrite {
        text: Some(String::from_utf8_lossy(&raw).into_owned()),
        attachment: BotAttachment::Unchanged,
    })
}

/// Multipart bot writes: the top-level `attachment` part (an empty
/// file counts as given, like the blank form value); anything else
/// falls back to the raw body, like upstream's `message_params`.
async fn parse_multipart_bot_write(content_type: &str, raw: &[u8]) -> Result<BotWrite> {
    use futures_util::stream::{self};
    let boundary =
        multer::parse_boundary(content_type).map_err(|_| bad_request("malformed multipart"))?;
    let bytes = bytes::Bytes::copy_from_slice(raw);
    let stream = stream::once(async move { Ok::<_, multer::Error>(bytes) });
    let mut multipart = multer::Multipart::new(stream, boundary);
    let mut attachment: Option<BotAttachment> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| bad_request("malformed multipart"))?
    {
        if field.name().unwrap_or("") != "attachment" {
            continue;
        }
        let filename = field.file_name().unwrap_or("file").to_string();
        let part_type = field.content_type().map(|mime| mime.to_string());
        let bytes = field
            .bytes()
            .await
            .map_err(|_| bad_request("malformed multipart"))?;
        if bytes.is_empty() {
            attachment = Some(BotAttachment::Delete);
        } else {
            if bytes.len() > 128 * 1024 * 1024 {
                return Err(bad_request("attachment too large").into());
            }
            attachment = Some(BotAttachment::Upload(crate::messages::MessageFile {
                filename,
                content_type: part_type,
                bytes: bytes.to_vec(),
            }));
        }
    }
    match attachment {
        Some(attachment) => Ok(BotWrite {
            text: None,
            attachment,
        }),
        None => Ok(BotWrite {
            text: Some(String::from_utf8_lossy(raw).into_owned()),
            attachment: BotAttachment::Unchanged,
        }),
    }
}

fn invalid_attachment() -> topcoat::Error {
    use topcamp_domain::error::{Error as DomainError, InfrastructureError};
    http_error(DomainError::Infrastructure(InfrastructureError::new(
        "attachments",
    )))
}

/// `deliver_webhooks_to_bots`: every active bot in a direct room,
/// else every mentioned active bot, except the creator — one
/// `deliver_webhook` outbox row each (the relay runs the POST).
pub(crate) async fn deliver_webhooks_to_bots(
    db: &PgDb,
    room: &RoomRow,
    creator_id: i64,
    stored_body: &str,
    message_id: i64,
) -> Result<()> {
    let direct = room.kind == "Rooms::Direct";
    let mentioned = if direct {
        Vec::new()
    } else {
        crate::richtext::mentioned_user_ids(stored_body)
    };
    WebhookRepository::enqueue_bot_deliveries(
        db, room.id, direct, &mentioned, creator_id, message_id,
    )
    .await
    .map_err(http_error)?;
    Ok(())
}

/// `messages/by_bots#create`: raw body or top-level attachment,
/// 201 + `Location` (the message path, not room-nested).
async fn create_api(cx: &Cx, room_param: &str, key_param: &str, body: Body) -> Result<Response> {
    let Some(caller) = authenticate_caller(cx, Some(key_param)).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let input = parse_bot_write(cx, body).await?;
    let attachment_blank = matches!(
        input.attachment,
        BotAttachment::Unchanged | BotAttachment::Delete
    );
    if attachment_blank
        && input
            .text
            .as_deref()
            .is_none_or(|text| text.chars().all(char::is_whitespace))
    {
        return head(422);
    }
    let app = app_context::<AppState>(cx);
    let Some(room) = bot_room(&app.db, &caller, room_param).await? else {
        return not_found().into_response(cx);
    };
    // `Assignment::Invalid` raises (500) before anything stores.
    if matches!(input.attachment, BotAttachment::Invalid) {
        return Err(invalid_attachment());
    }
    let staged = match input.attachment {
        BotAttachment::Upload(file) => Some(crate::messages::stage_upload(&file).await?),
        _ => None,
    };
    let stored = crate::richtext::canonicalize_plain(input.text.as_deref().unwrap_or(""));
    let row = app
        .db
        .post_message_with_attachment(
            topcamp_db::repositories::NewMessage {
                room_id: room.id,
                creator_id: caller.id(),
                client_message_id: crate::messages::uuid_v4(),
                body: stored.clone(),
            },
            staged,
        )
        .await
        .map_err(http_error)?;
    crate::live::publish_message_create(&app.db, &app.bus, room.id, row.id).await?;
    deliver_webhooks_to_bots(&app.db, &room, caller.id(), &stored, row.id).await?;
    let base = request_base_url(cx);
    let mut response = head(201)?;
    response.headers_mut().insert(
        "location",
        format!("{base}/messages/{}", row.id)
            .parse()
            .expect("location parses"),
    );
    Ok(response)
}

/// Detach a message's attachment, purging the orphaned blob.
async fn detach_message_attachment(db: &PgDb, message_id: i64) -> Result<()> {
    if let Some(blob_id) =
        AttachmentRepository::detach_from_record(db, "Message", message_id, "attachment")
            .await
            .map_err(http_error)?
    {
        let mut tx = db.pool().begin().await.map_err(topcamp_db::DbError::Sqlx)?;
        topcamp_db::outbox::publish(&mut tx, "purge_blob", &format!("{{\"blob_id\":{blob_id}}}"))
            .await
            .map_err(topcamp_db::DbError::Sqlx)?;
        tx.commit().await.map_err(topcamp_db::DbError::Sqlx)?;
    }
    Ok(())
}

/// `messages/by_bots#update`: raw body and/or top-level attachment;
/// JSON renders the message, HTML redirects to it.
async fn update_api(
    cx: &Cx,
    room_param: &str,
    key_param: &str,
    id_param: &str,
    body: Body,
) -> Result<Response> {
    let Some(caller) = authenticate_caller(cx, Some(key_param)).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let input = parse_bot_write(cx, body).await?;
    let app = app_context::<AppState>(cx);
    let Some(room) = bot_room(&app.db, &caller, room_param).await? else {
        return not_found().into_response(cx);
    };
    let Some(message) = bot_message(&app.db, room.id, id_param).await? else {
        return not_found().into_response(cx);
    };
    ensure_can_administer(&caller, message.creator_id)?;
    if matches!(input.attachment, BotAttachment::Invalid) {
        return Err(invalid_attachment());
    }
    if let Some(text) = input.text.as_deref() {
        let stored = crate::richtext::canonicalize_plain(text);
        MessageRepository::update_body(&app.db, message.id, &stored)
            .await
            .map_err(http_error)?;
    }
    match input.attachment {
        BotAttachment::Upload(file) => {
            let staged = crate::messages::stage_upload(&file).await?;
            detach_message_attachment(&app.db, message.id).await?;
            let blob = AttachmentRepository::insert_blob(&app.db, staged.blob)
                .await
                .map_err(http_error)?;
            AttachmentRepository::attach_to_record(
                &app.db,
                "Message",
                message.id,
                "attachment",
                blob.id,
            )
            .await
            .map_err(http_error)?;
            // The replacement analyzes after commit
            // (`ActiveStorage::AnalyzeJob`).
            let mut tx = app
                .db
                .pool()
                .begin()
                .await
                .map_err(topcamp_db::DbError::Sqlx)?;
            topcamp_db::outbox::publish(
                &mut tx,
                "process_attachment",
                &format!("{{\"blob_id\":{}}}", blob.id),
            )
            .await
            .map_err(topcamp_db::DbError::Sqlx)?;
            tx.commit().await.map_err(topcamp_db::DbError::Sqlx)?;
        }
        BotAttachment::Delete => {
            detach_message_attachment(&app.db, message.id).await?;
        }
        _ => {}
    }
    crate::live::publish_message_update(&app.bus, room.id, message.id);
    if crate::messages::wants_html(cx) {
        return see_other(crate::room_show::message_path(room.id, message.id)).into_response(cx);
    }
    let base = request_base_url(cx);
    let refreshed = bot_message(&app.db, room.id, id_param)
        .await?
        .expect("updated message still there");
    let users = JsonUsers::load(&app.db, base, std::slice::from_ref(&refreshed)).await?;
    let view = users.message_json(&refreshed).expect("creator still there");
    json_response(200, to_rails_json(&view), &[])
}

/// `messages/by_bots#destroy`: drop the message, `head :no_content`.
async fn destroy_api(
    cx: &Cx,
    room_param: &str,
    key_param: &str,
    id_param: &str,
) -> Result<Response> {
    let Some(caller) = authenticate_caller(cx, Some(key_param)).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let app = app_context::<AppState>(cx);
    let Some(room) = bot_room(&app.db, &caller, room_param).await? else {
        return not_found().into_response(cx);
    };
    let Some(message) = bot_message(&app.db, room.id, id_param).await? else {
        return not_found().into_response(cx);
    };
    ensure_can_administer(&caller, message.creator_id)?;
    MessageRepository::destroy(&app.db, message.id)
        .await
        .map_err(http_error)?;
    crate::live::publish_message_remove(&app.bus, room.id, &message.client_message_id);
    head(204)
}

// --- bot boosts API ----------------------------------------------------------

/// The room among the caller's rooms, then its message; 404 without.
async fn bot_boost_message(
    db: &PgDb,
    caller: &Caller,
    room_param: &str,
    message_param: &str,
) -> Result<Option<(RoomRow, MessageDetail)>> {
    let Some(room) = bot_room(db, caller, room_param).await? else {
        return Ok(None);
    };
    let Some(message) = bot_message(db, room.id, message_param).await? else {
        return Ok(None);
    };
    Ok(Some((room, message)))
}

/// `messages/boosts/by_bots#create`: the raw body boosts, 201 + JSON.
async fn create_boost_api(
    cx: &Cx,
    room_param: &str,
    key_param: &str,
    message_param: &str,
    body: Body,
) -> Result<Response> {
    let Some(caller) = authenticate_caller(cx, Some(key_param)).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let content = String::from_utf8_lossy(&to_bytes(body, 1024 * 1024).await?).into_owned();
    if content.chars().all(char::is_whitespace) {
        return head(422);
    }
    let app = app_context::<AppState>(cx);
    let Some((room, message)) =
        bot_boost_message(&app.db, &caller, room_param, message_param).await?
    else {
        return not_found().into_response(cx);
    };
    let id = MessageRepository::create_boost(&app.db, message.id, caller.id(), &content)
        .await
        .map_err(http_error)?;
    crate::live::publish_boosts_changed(&app.bus, room.id, message.id);
    let boost = MessageRepository::find_boost(&app.db, message.id, id, caller.id())
        .await
        .map_err(http_error)?
        .expect("created boost still there");
    let base = request_base_url(cx);
    let mut users = JsonUsers::load(&app.db, base, std::slice::from_ref(&message)).await?;
    let view = users
        .boost_json(&boost, &message)
        .await?
        .expect("booster still there");
    json_response(201, to_rails_json(&view), &[])
}

/// `messages/boosts/by_bots#destroy`: drop the caller's boost (404
/// for anyone else's), `head :no_content`.
async fn destroy_boost_api(
    cx: &Cx,
    room_param: &str,
    key_param: &str,
    message_param: &str,
    id_param: &str,
) -> Result<Response> {
    let Some(caller) = authenticate_caller(cx, Some(key_param)).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let app = app_context::<AppState>(cx);
    let Some((room, message)) =
        bot_boost_message(&app.db, &caller, room_param, message_param).await?
    else {
        return not_found().into_response(cx);
    };
    let boost_id = cast_integer(id_param).unwrap_or(0);
    let found = MessageRepository::find_boost(&app.db, message.id, boost_id, caller.id())
        .await
        .map_err(http_error)?;
    if found.is_none() {
        return not_found().into_response(cx);
    }
    MessageRepository::delete_boost(&app.db, boost_id)
        .await
        .map_err(http_error)?;
    crate::live::publish_boosts_changed(&app.bus, room.id, message.id);
    head(204)
}

// --- bot API routes ----------------------------------------------------------

/// `GET /rooms/:room_id/:bot_key/messages`.
#[route(GET "/rooms/{room_id}/{bot_key}/messages")]
pub async fn api_index(cx: &Cx) -> Result<Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    let key = path_param_segment(cx, "bot_key").to_owned();
    index_api(cx, &room, &key).await
}

/// `POST /rooms/:room_id/:bot_key/messages`.
#[route(POST "/rooms/{room_id}/{bot_key}/messages")]
pub async fn api_create(cx: &Cx, body: Body) -> Result<Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    let key = path_param_segment(cx, "bot_key").to_owned();
    create_api(cx, &room, &key, body).await
}

/// `PATCH/PUT /rooms/:room_id/:bot_key/messages/:id`.
#[route([PATCH, PUT] "/rooms/{room_id}/{bot_key}/messages/{id}")]
pub async fn api_update(cx: &Cx, body: Body) -> Result<Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    let key = path_param_segment(cx, "bot_key").to_owned();
    let id = path_param_segment(cx, "id").to_owned();
    update_api(cx, &room, &key, &id, body).await
}

/// `DELETE /rooms/:room_id/:bot_key/messages/:id`.
#[route(DELETE "/rooms/{room_id}/{bot_key}/messages/{id}")]
pub async fn api_destroy(cx: &Cx) -> Result<Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    let key = path_param_segment(cx, "bot_key").to_owned();
    let id = path_param_segment(cx, "id").to_owned();
    destroy_api(cx, &room, &key, &id).await
}

/// `POST /rooms/:room_id/:bot_key/messages/:message_id/boosts`.
#[route(POST "/rooms/{room_id}/{bot_key}/messages/{message_id}/boosts")]
pub async fn api_boost_create(cx: &Cx, body: Body) -> Result<Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    let key = path_param_segment(cx, "bot_key").to_owned();
    let message = path_param_segment(cx, "message_id").to_owned();
    create_boost_api(cx, &room, &key, &message, body).await
}

/// `DELETE /rooms/:room_id/:bot_key/messages/:message_id/boosts/:id`.
#[route(DELETE "/rooms/{room_id}/{bot_key}/messages/{message_id}/boosts/{id}")]
pub async fn api_boost_destroy(cx: &Cx) -> Result<Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    let key = path_param_segment(cx, "bot_key").to_owned();
    let message = path_param_segment(cx, "message_id").to_owned();
    let id = path_param_segment(cx, "id").to_owned();
    destroy_boost_api(cx, &room, &key, &message, &id).await
}

// --- bot admin pages ---------------------------------------------------------

/// Shell identity + logo for bot pages.
async fn page_shell(db: &PgDb, user_id: i64, name: &str) -> Result<ShellContext> {
    Ok(ShellContext {
        current_user: Some((user_id, name.to_string())),
        logo_version: AccountRepository::first(db)
            .await
            .map_err(http_error)?
            .map(|account| account.updated_number),
    })
}

/// `link_back_to(destination)`.
fn back_nav(cx: &Cx, href: &str) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let href = href.to_string();
    view! {
        cx =>
        <div class="flex-item-justify-start">
            <a class="btn" href=(href)>
                <img aria-hidden="true" width="20" height="20" src=(img_arrow_left()) />
                <span class="for-screen-reader">"Go Back"</span>
            </a>
        </div>
    }
    .boxed()
}

/// `translation_button(key)` for the bot pages' three keys.
fn translations_view(cx: &Cx, key: &str) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    const CHAT_BOTS: &[(&str, &str)] = &[
        (
            "🇺🇸",
            "Chat bots. With Chat bots, other sites and services can post updates directly to Topcamp.",
        ),
        (
            "🇪🇸",
            "Bots de chat. Con los bots de chat, otros sitios y servicios pueden publicar actualizaciones directamente en Topcamp.",
        ),
        (
            "🇫🇷",
            "Bots de discussion. Avec les bots de discussion, d'autres sites et services peuvent publier des mises à jour directement sur Topcamp.",
        ),
        (
            "🇮🇳",
            "चैट बॉट। चैट बॉट के साथ, अन्य साइटों और सेवाएं सीधे कैम्पफायर में अपडेट पोस्ट कर सकती हैं।",
        ),
        (
            "🇩🇪",
            "Chat-Bots. Mit Chat-Bots können andere Websites und Dienste Updates direkt in Topcamp veröffentlichen.",
        ),
        (
            "🇧🇷",
            "Chat bots. Com Chat bots, outros sites e serviços podem postar atualizações diretamente no Topcamp.",
        ),
        (
            "🇯🇵",
            "チャットボット。チャットボットを使用すると、他のサイトやサービスがTopcampに直接更新情報を投稿できます。",
        ),
    ];
    const BOT_NAME: &[(&str, &str)] = &[
        ("🇺🇸", "Name the bot"),
        ("🇪🇸", "Nombrar al bot"),
        ("🇫🇷", "Nommer le bot"),
        ("🇮🇳", "बॉट का नाम दें"),
        ("🇩🇪", "Benenne den Bot"),
        ("🇧🇷", "Dê um nome ao bot"),
        ("🇯🇵", "ボットに名前を付ける"),
    ];
    const WEBHOOK_URL: &[(&str, &str)] = &[
        ("🇺🇸", "Webhook URL"),
        ("🇪🇸", "URL del Webhook"),
        ("🇫🇷", "URL du webhook"),
        ("🇮🇳", "वेबहुक URL"),
        ("🇩🇪", "Webhook-URL"),
        ("🇧🇷", "URL do Webhook"),
        ("🇯🇵", "Webhook URL"),
    ];
    let table = match key {
        "bot_name" => BOT_NAME,
        "webhook_url" => WEBHOOK_URL,
        _ => CHAT_BOTS,
    };
    let entries: Vec<(String, String)> = table
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
    .boxed()
}

/// One bot card (`accounts/bots/_bot`): avatar, name, edit link, and
/// a curl fieldset per non-direct room.
#[allow(clippy::too_many_arguments)]
fn bot_card_view(
    cx: &Cx,
    bot: &BotRow,
    bot_key: &str,
    rooms: &[(i64, String)],
    base_url: &str,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let avatar = crate::users::avatar_url_for(bot.id, &bot.updated_number);
    let name = bot.name.clone();
    let name_read = bot.name.clone();
    let edit_href = format!("/account/bots/{}/edit", bot.id);
    let transition = format!("view-transition-name: chat-bot-{}", bot.id);
    let fieldsets: Vec<topcoat::view::BoxView<'static>> = rooms
        .iter()
        .map(|(room_id, room_name)| {
            let url = format!("{base_url}/rooms/{room_id}/{bot_key}/messages");
            let text_line = format!("curl -d 'Hello!' {url}");
            let upload_line = format!("curl -F \"attachment=@/path/to/file\" {url}");
            let room_name = room_name.clone();
            view! {
                cx =>
                <fieldset class="gap max-width pad border border-radius">
                    <legend class="min-width txt-align-start pad-inline">
                        <strong class="overflow-ellipsis">(room_name)</strong>
                    </legend>
                    <div class="flex align-center gap">
                        <img aria-hidden="true" width="24" height="24" src=(img_messages_outlined()) class="colorize--black" />
                        <div class="flex-item-grow">
                            <input type="text" class="input full-width fill-white" value=(text_line.clone()) aria-label="curl command for posting messages" readonly="readonly" />
                        </div>
                    </div>
                    <div class="flex align-center gap">
                        <img aria-hidden="true" width="24" height="24" src=(img_attachment()) class="colorize--black" />
                        <div class="flex-item-grow">
                            <input type="text" class="input full-width fill-white" value=(upload_line.clone()) aria-label="curl command for posting attachments" readonly="readonly" />
                        </div>
                    </div>
                </fieldset>
            }
            .boxed()
        })
        .collect();
    let fieldsets: Vec<Slot> = fieldsets.into_iter().map(Slot::new).collect();
    view! {
        cx =>
        <li class="flex flex-column gap flush fill-shade border-radius pad-block pad-inline-double">
            <div class="flex align-center gap">
                <figure class="avatar flex-item--no-shrink" style="--avatar-size: 2.65em;">
                    <img loading="lazy" src=(avatar) />
                </figure>
                <div class="min-width">
                    <div class="overflow-ellipsis txt-large"><strong>(name)</strong></div>
                </div>
                <a class="btn flex-item-justify-end" style=(transition) href=(edit_href)>
                    <img aria-hidden="true" width="20" height="20" src=(img_pencil()) />
                    <span class="for-screen-reader">"Edit "(name_read)</span>
                </a>
            </div>
            for fieldset in fieldsets {
                (fieldset)
            }
        </li>
    }
    .boxed()
}

/// `accounts/bots/index`.
fn index_view(
    cx: &Cx,
    cards: Vec<topcoat::view::BoxView<'static>>,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let cards: Vec<Slot> = cards.into_iter().map(Slot::new).collect();
    let translations = Slot::new(translations_view(cx, "chat_bots"));
    view! {
        cx =>
        <section class="panel panel--wide txt-align-center flex flex-column position-relative" style="view-transition-name: chat-bots">
            <div class="flex align-center gap">
                <div class="panel__button">
                    (translations)
                </div>
                <div class="pad-inline-double center">
                    <h1 class="margin-none">"Chat bots"</h1>
                    <p class="margin-none-block-start">"With Chat bots, other sites and services can post updates directly to Topcamp."</p>
                    <a class="btn btn--reversed txt-large" aria-label="Add a chat bot" href="/account/bots/new">
                        <img aria-hidden="true" width="20" height="20" src=(img_bot()) />
                        <img aria-hidden="true" width="20" height="20" src=(img_add()) />
                    </a>
                </div>
            </div>
            <div class="pad-inline pad-block-start">
                <menu class="flex flex-column gap margin-none pad">
                    for card in cards {
                        (card)
                    }
                </menu>
            </div>
        </section>
    }
    .boxed()
}

/// The shared new/edit bot form (`accounts/bots/_form`); a missing
/// avatar previews the default bot image.
fn bot_form_view(
    cx: &Cx,
    name: &str,
    webhook_url: &str,
    avatar_src: Option<&str>,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let name = name.to_string();
    let webhook_url = webhook_url.to_string();
    let avatar_src = avatar_src.map(str::to_string);
    let name_translations = Slot::new(translations_view(cx, "bot_name"));
    let webhook_translations = Slot::new(translations_view(cx, "webhook_url"));
    view! {
        cx =>
        <h1 class="for-screen-reader">"Chat Bot Setup"</h1>
        <label class="align-center center avatar__form gap">
            <div class="btn input--file">
                <img aria-hidden="true" width="20" height="20" src=(img_camera()) />
                <input class="input" accept="image/*" type="file" name="user[avatar]" id="user_avatar" />
                <span class="for-screen-reader">"Upload bot avatar"</span>
            </div>
            <div class="avatar input--file txt-xx-large" style="--avatar-size: var(--btn-size);">
                if let Some(src) = avatar_src {
                    <img alt="Bot avatar" width="48" height="48" src=(src) />
                } else {
                    <img alt="Bot avatar" width="48" height="48" src=(img_default_bot_avatar()) />
                }
            </div>
        </label>
        <div class="flex align-center gap">
            (name_translations)
            <label class="flex align-center gap flex-item-grow txt-large input input--actor">
                <input class="input" autocomplete="name" placeholder="Name the bot" autofocus="autofocus" required="required" data-1p-ignore="true" type="text" value=(name) name="user[name]" id="user_name" />
                <img aria-hidden="true" width="24" height="24" src=(img_bot()) class="colorize--black" />
            </label>
        </div>
        <div class="flex align-center gap">
            (webhook_translations)
            <label class="flex align-center gap flex-item-grow txt-large input input--actor">
                <input class="input" placeholder="Webhook URL" type="url" value=(webhook_url) name="user[webhook_url]" id="user_webhook_url" />
                <img aria-hidden="true" width="24" height="24" src=(img_web()) class="colorize--black" />
            </label>
        </div>
        <button class="btn btn--reversed center txt-large" type="submit">
            <img aria-hidden="true" width="20" height="20" src=(img_check()) />
            <span class="for-screen-reader">"Save changes"</span>
        </button>
    }
    .boxed()
}

/// `accounts/bots/new`.
fn new_view(
    cx: &Cx,
    form: topcoat::view::BoxView<'static>,
    csrf_token: &str,
) -> topcoat::view::BoxView<'static> {
    let form = Slot::new(form);
    let csrf = csrf_token.to_string();
    view! {
        cx =>
        <section class="panel">
            <form action="/account/bots" accept-charset="UTF-8" method="post" enctype="multipart/form-data" class="flex flex-column gap">
                <input type="hidden" name="authenticity_token" value=(csrf) />
                (form)
            </form>
        </section>
    }
    .boxed()
}

/// `accounts/bots/edit`: the form plus the delete + reset-key buttons.
fn edit_view(
    cx: &Cx,
    bot_id: i64,
    form: topcoat::view::BoxView<'static>,
    csrf_token: &str,
    confirm_form: &str,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let form = Slot::new(form);
    let csrf = csrf_token.to_string();
    let csrf_delete = csrf.clone();
    let csrf_key = csrf.clone();
    let action = format!("/account/bots/{bot_id}");
    let action_delete = action.clone();
    let key_action = format!("/account/bots/{bot_id}/key");
    let edit_page = format!("/account/bots/{bot_id}/edit");
    let delete_href = format!("{edit_page}?confirm=delete-bot");
    let key_href = format!("{edit_page}?confirm=reset-bot-key");
    let delete_dialog = Slot::new(crate::confirm::delete_dialog_view(
        cx,
        "delete-bot",
        "Delete",
        "Delete this chat bot?",
        "Are you sure you want to permanently remove this bot from the account? This can't be undone.",
        confirm_form == "delete-bot",
        &edit_page,
    ));
    let key_dialog = Slot::new(crate::confirm::delete_dialog_view(
        cx,
        "reset-bot-key",
        "Generate",
        "Generate a new key?",
        "Are you sure you want to change the bot key? All usage of this bot must be updated.",
        confirm_form == "reset-bot-key",
        &edit_page,
    ));
    let transition = format!("view-transition-name: chat-bot-{bot_id}");
    view! {
        cx =>
        <section class="panel" style=(transition)>
            <form action=(action) accept-charset="UTF-8" method="post" enctype="multipart/form-data" class="flex flex-column gap">
                <input type="hidden" name="_method" value="patch" />
                <input type="hidden" name="authenticity_token" value=(csrf) />
                (form)
            </form>
            <hr class="separator full-width margin-block-double" />
            <div class="flex align-center gap justify-space-between">
                <form id="delete-bot" class="button_to" method="post" action=(action_delete)>
                    <input type="hidden" name="_method" value="delete" />
                    <input type="hidden" name="authenticity_token" value=(csrf_delete) />
                </form>
                <a class="btn txt--small btn--negative" aria-label="Delete this chat bot" data-tip="Delete this chat bot" href=(delete_href)>
                    <img aria-hidden="true" width="20" height="20" src=(img_trash()) />
                    <img aria-hidden="true" width="20" height="20" src=(img_bot()) />
                </a>
                (delete_dialog)
                <form id="reset-bot-key" class="button_to" method="post" action=(key_action)>
                    <input type="hidden" name="_method" value="put" />
                    <input type="hidden" name="authenticity_token" value=(csrf_key) />
                </form>
                <a class="btn full-width txt--small btn--negative" aria-label="Generate a new key" data-tip="Generate a new key" href=(key_href)>
                    <img aria-hidden="true" width="20" height="20" src=(img_refresh()) />
                    <img aria-hidden="true" width="20" height="20" src=(img_key()) />
                </a>
                (key_dialog)
            </div>
        </section>
    }
    .boxed()
}

async fn render_page(
    cx: &Cx,
    title: &str,
    content: topcoat::view::BoxView<'static>,
    nav: topcoat::view::BoxView<'static>,
    shell: ShellContext,
    admin: bool,
) -> Result<Response> {
    use topcoat::router::response::AsyncIntoResponse;
    document_shell(
        cx,
        title.to_string(),
        body_classes("", admin),
        Slot::new(content),
        crate::flash::Flash::default(),
        shell,
        None,
        Some(Slot::new(nav)),
        None,
        Some(Slot::new(crate::accounts::footer_view(cx))),
    )
    .boxed()
    .async_into_response(cx)
    .await
}

/// The session admin, `deny_bots` first (a bot key 403s instead of
/// redirecting, like the before-action order).
async fn admin_caller(cx: &Cx) -> Result<Option<UserRow>> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return Ok(None);
    };
    if user.role != UserRole::Administrator.value() {
        return Err(forbidden().into());
    }
    Ok(Some(user))
}

/// `User.active_bots.find(params[:id])`.
async fn set_bot(db: &PgDb, param: &str) -> Result<Option<BotRow>> {
    match cast_integer(param) {
        Some(id) => UserRepository::find_active_bot(db, id)
            .await
            .map_err(http_error),
        None => Ok(None),
    }
}

/// `accounts/bots#index`.
async fn index_bots(cx: &Cx) -> Result<Response> {
    let Some(user) = admin_caller(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let bots = UserRepository::active_bots_ordered(db)
        .await
        .map_err(http_error)?;
    let base = request_base_url(cx);
    let mut cards = Vec::with_capacity(bots.len());
    for bot in &bots {
        let mut rooms = RoomRepository::list_for_user(db, bot.id)
            .await
            .map_err(http_error)?;
        rooms.retain(|room| room.kind != "Rooms::Direct");
        rooms.sort_by_cached_key(|room| room.name.clone().unwrap_or_default().to_ascii_lowercase());
        let rooms: Vec<(i64, String)> = rooms
            .into_iter()
            .map(|room| (room.id, room.name.unwrap_or_default()))
            .collect();
        let key = bot.bot_key().unwrap_or_else(|| format!("{}-", bot.id));
        cards.push(bot_card_view(cx, bot, &key, &rooms, &base));
    }
    let content = index_view(cx, cards);
    let shell = page_shell(db, user.id, &user.name).await?;
    let admin = user.role == UserRole::Administrator.value();
    render_page(
        cx,
        "Chat bots",
        content,
        back_nav(cx, "/account/edit"),
        shell,
        admin,
    )
    .await
}

/// `accounts/bots#new`.
async fn new_bots(cx: &Cx) -> Result<Response> {
    let Some(user) = admin_caller(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let csrf_token = crate::csrf::issue(cx);
    let form = bot_form_view(cx, "", "", None);
    let content = new_view(cx, form, &csrf_token);
    let shell = page_shell(db, user.id, &user.name).await?;
    render_page(
        cx,
        "New chat bot",
        content,
        back_nav(cx, "/account/bots"),
        shell,
        true,
    )
    .await
}

/// Parsed `user[...]` bot submission (`present` mirrors
/// `params.require("user")`: any `user[*]` key counts).
#[derive(Default)]
struct BotForm {
    present: bool,
    name: Option<String>,
    webhook_url: Option<String>,
    avatar_text: Option<String>,
    authenticity_token: Option<String>,
    method_override: Option<String>,
}

struct BotAvatarFile {
    filename: String,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

fn parse_bot_form(raw: &[u8]) -> BotForm {
    let mut form = BotForm::default();
    let Ok(pairs) = serde_urlencoded::from_bytes::<Vec<(String, String)>>(raw) else {
        return form;
    };
    for (key, value) in pairs {
        match key.as_str() {
            "user[name]" => {
                form.present = true;
                form.name = Some(value);
            }
            "user[webhook_url]" => {
                form.present = true;
                form.webhook_url = Some(value);
            }
            "user[avatar]" => {
                form.present = true;
                form.avatar_text = Some(value);
            }
            "authenticity_token" => form.authenticity_token = Some(value),
            "_method" => form.method_override = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    form
}

async fn parse_multipart_bot_form(
    content_type: &str,
    raw: &[u8],
) -> Result<(BotForm, Option<BotAvatarFile>, Option<String>)> {
    use futures_util::stream::{self};
    let boundary =
        multer::parse_boundary(content_type).map_err(|_| bad_request("malformed multipart"))?;
    let bytes = bytes::Bytes::copy_from_slice(raw);
    let stream = stream::once(async move { Ok::<_, multer::Error>(bytes) });
    let mut multipart = multer::Multipart::new(stream, boundary);
    let mut form = BotForm::default();
    let mut avatar = None;
    let mut avatar_text = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| bad_request("malformed multipart"))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "user[avatar]" {
            form.present = true;
            if field.file_name().is_some() {
                let filename = field.file_name().unwrap_or("avatar").to_string();
                let part_type = field.content_type().map(|mime| mime.to_string());
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|_| bad_request("malformed multipart"))?;
                if bytes.is_empty() {
                    avatar_text = Some(String::new());
                } else {
                    if bytes.len() > 5 * 1024 * 1024 {
                        return Err(bad_request("avatar too large").into());
                    }
                    avatar = Some(BotAvatarFile {
                        filename,
                        content_type: part_type,
                        bytes: bytes.to_vec(),
                    });
                }
            } else {
                let value = field
                    .text()
                    .await
                    .map_err(|_| bad_request("malformed multipart"))?;
                avatar_text = Some(value);
            }
            continue;
        }
        let value = field
            .text()
            .await
            .map_err(|_| bad_request("malformed multipart"))?;
        match name.as_str() {
            "user[name]" => {
                form.present = true;
                form.name = Some(value);
            }
            "user[webhook_url]" => {
                form.present = true;
                form.webhook_url = Some(value);
            }
            "authenticity_token" => form.authenticity_token = Some(value),
            "_method" => form.method_override = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    Ok((form, avatar, avatar_text))
}

async fn parse_bot_submission(cx: &Cx, body: Body) -> Result<(BotForm, Option<BotAvatarFile>)> {
    let content_type = request::headers(cx)
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    if content_type.starts_with("multipart/form-data") {
        let (mut form, avatar, avatar_text) =
            parse_multipart_bot_form(&content_type, &to_bytes(body, 8 * 1024 * 1024).await?)
                .await?;
        form.avatar_text = avatar_text;
        Ok((form, avatar))
    } else {
        Ok((parse_bot_form(&to_bytes(body, 1024 * 1024).await?), None))
    }
}

fn csrf_ok(cx: &Cx, input: &BotForm) -> bool {
    input
        .authenticity_token
        .as_deref()
        .is_some_and(|token| crate::csrf::verify(cx, token))
}

fn missing_name() -> topcoat::Error {
    use topcamp_domain::error::{Error as DomainError, InfrastructureError};
    http_error(DomainError::Infrastructure(InfrastructureError::new(
        "users",
    )))
}

/// `accounts/bots#create`: `User.create_bot!`, webhook when the key's
/// given (`""` included), avatar upload, open-room memberships.
async fn create_bots(cx: &Cx, body: Body) -> Result<Response> {
    let Some(user) = admin_caller(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let _ = user;
    let (input, avatar) = parse_bot_submission(cx, body).await?;
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    if !input.present {
        return Err(bad_request("param is missing or the value is empty: user").into());
    }
    // `users.name` is NOT NULL (missing key raises, blank stores).
    let Some(name) = input.name else {
        return Err(missing_name());
    };
    if let Some(text) = input.avatar_text.as_deref().filter(|text| !text.is_empty()) {
        let _ = text;
        return Err(invalid_attachment());
    }
    let db = &app_context::<AppState>(cx).db;
    let bot = UserRepository::create_bot(db, &name)
        .await
        .map_err(http_error)?;
    if let Some(url) = input.webhook_url.as_deref() {
        WebhookRepository::create_webhook(db, bot.id, Some(url))
            .await
            .map_err(http_error)?;
    }
    if let Some(file) = avatar {
        crate::users::replace_avatar(
            db,
            bot.id,
            crate::users::AvatarFile {
                filename: file.filename,
                content_type: file.content_type,
                bytes: file.bytes,
            },
        )
        .await?;
    }
    // `User::create`'s after-commit open-room grants.
    for room_id in RoomRepository::open_ids(db).await.map_err(http_error)? {
        MembershipRepository::create(db, room_id, bot.id)
            .await
            .map_err(http_error)?;
    }
    see_other("/account/bots").into_response(cx)
}

/// `accounts/bots#edit`.
async fn edit_bots(cx: &Cx, id_param: &str) -> Result<Response> {
    let Some(user) = admin_caller(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let Some(bot) = set_bot(db, id_param).await? else {
        return not_found().into_response(cx);
    };
    let webhook = WebhookRepository::find_by_user(db, bot.id)
        .await
        .map_err(http_error)?;
    let csrf_token = crate::csrf::issue(cx);
    let form = bot_form_view(
        cx,
        &bot.name,
        webhook
            .as_ref()
            .and_then(|hook| hook.url.as_deref())
            .unwrap_or(""),
        Some(&crate::users::avatar_url_for(bot.id, &bot.updated_number)),
    );
    let query = query_params::<crate::confirm::ConfirmQuery>(cx)?;
    let confirm_form = query.confirm.as_deref().unwrap_or("");
    let content = edit_view(cx, bot.id, form, &csrf_token, confirm_form);
    let shell = page_shell(db, user.id, &user.name).await?;
    render_page(
        cx,
        "Edit bot",
        content,
        back_nav(cx, "/account/bots"),
        shell,
        true,
    )
    .await
}

/// Detach a bot's avatar, purging the orphaned blob.
async fn delete_bot_avatar(db: &PgDb, bot_id: i64) -> Result<()> {
    if let Some(blob_id) = AttachmentRepository::detach_from_record(db, "User", bot_id, "avatar")
        .await
        .map_err(http_error)?
    {
        let mut tx = db.pool().begin().await.map_err(topcamp_db::DbError::Sqlx)?;
        topcamp_db::outbox::publish(&mut tx, "purge_blob", &format!("{{\"blob_id\":{blob_id}}}"))
            .await
            .map_err(topcamp_db::DbError::Sqlx)?;
        tx.commit().await.map_err(topcamp_db::DbError::Sqlx)?;
    }
    Ok(())
}

/// `accounts/bots#update`: the webhook first, then the bot (rename +
/// avatar), like `update_bot!`.
async fn update_bots(cx: &Cx, id_param: &str, body: Body) -> Result<Response> {
    let Some(user) = admin_caller(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let _ = user;
    let (input, avatar) = parse_bot_submission(cx, body).await?;
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    if !input.present {
        return Err(bad_request("param is missing or the value is empty: user").into());
    }
    if input
        .avatar_text
        .as_deref()
        .is_some_and(|text| !text.is_empty())
    {
        return Err(invalid_attachment());
    }
    let db = &app_context::<AppState>(cx).db;
    let Some(bot) = set_bot(db, id_param).await? else {
        return not_found().into_response(cx);
    };
    let webhook = WebhookRepository::find_by_user(db, bot.id)
        .await
        .map_err(http_error)?;
    // A blank/missing URL drops the webhook; a fresh one upserts.
    match (
        input
            .webhook_url
            .as_deref()
            .filter(|url| !url.trim().is_empty()),
        webhook,
    ) {
        (Some(url), Some(hook)) => {
            if hook.url.as_deref() != Some(url) {
                WebhookRepository::set_webhook_url(db, hook.id, url)
                    .await
                    .map_err(http_error)?;
            }
        }
        (Some(url), None) => {
            WebhookRepository::create_webhook(db, bot.id, Some(url))
                .await
                .map_err(http_error)?;
        }
        (None, Some(hook)) => {
            WebhookRepository::destroy_webhook(db, hook.id)
                .await
                .map_err(http_error)?;
        }
        (None, None) => {}
    }
    // Renames write only on change (no touch otherwise).
    if let Some(name) = input.name.as_deref().filter(|name| *name != bot.name) {
        UserRepository::rename_bot(db, bot.id, name)
            .await
            .map_err(http_error)?;
    }
    if let Some(file) = avatar {
        crate::users::replace_avatar(
            db,
            bot.id,
            crate::users::AvatarFile {
                filename: file.filename,
                content_type: file.content_type,
                bytes: file.bytes,
            },
        )
        .await?;
    } else if input.avatar_text.is_some() {
        delete_bot_avatar(db, bot.id).await?;
    }
    see_other("/account/bots").into_response(cx)
}

/// `accounts/bots#destroy`: `@bot.deactivate`.
async fn destroy_bots(cx: &Cx, id_param: &str, body: Body) -> Result<Response> {
    let Some(user) = admin_caller(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let _ = user;
    let (input, _) = parse_bot_submission(cx, body).await?;
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    let db = &app_context::<AppState>(cx).db;
    let Some(bot) = set_bot(db, id_param).await? else {
        return not_found().into_response(cx);
    };
    UserRepository::deactivate(db, bot.id)
        .await
        .map_err(http_error)?;
    see_other("/account/bots").into_response(cx)
}

/// `accounts/bots/keys#update`: reset the bot key.
async fn reset_key(cx: &Cx, bot_param: &str, body: Body) -> Result<Response> {
    let Some(user) = admin_caller(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let _ = user;
    let (input, _) = parse_bot_submission(cx, body).await?;
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    let db = &app_context::<AppState>(cx).db;
    let Some(bot) = set_bot(db, bot_param).await? else {
        return not_found().into_response(cx);
    };
    UserRepository::reset_bot_key(db, bot.id)
        .await
        .map_err(http_error)?;
    see_other("/account/bots").into_response(cx)
}

// --- bot admin routes --------------------------------------------------------

/// `GET /account/bots`.
#[route(GET "/account/bots")]
pub async fn bots_index(cx: &Cx) -> Result<Response> {
    index_bots(cx).await
}

/// `GET /account/bots/new`.
#[route(GET "/account/bots/new")]
pub async fn bots_new(cx: &Cx) -> Result<Response> {
    new_bots(cx).await
}

/// `POST /account/bots`.
#[route(POST "/account/bots")]
pub async fn bots_create(cx: &Cx, body: Body) -> Result<Response> {
    create_bots(cx, body).await
}

/// `GET /account/bots/:id/edit`.
#[route(GET "/account/bots/{id}/edit")]
pub async fn bots_edit(cx: &Cx) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    edit_bots(cx, &id).await
}

/// `PATCH/PUT /account/bots/:id`.
#[route([PATCH, PUT] "/account/bots/{id}")]
pub async fn bots_update(cx: &Cx, body: Body) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    update_bots(cx, &id, body).await
}

/// `DELETE /account/bots/:id`.
#[route(DELETE "/account/bots/{id}")]
pub async fn bots_destroy(cx: &Cx, body: Body) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    destroy_bots(cx, &id, body).await
}

/// `POST /account/bots/:id`: `_method` patch/put/delete dispatch.
#[route(POST "/account/bots/{id}")]
pub async fn bots_modify(cx: &Cx, body: Body) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    let raw = to_bytes(body, 8 * 1024 * 1024).await?;
    let content_type = request::headers(cx)
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    // The override rides the same submission the target parses.
    let override_method = if content_type.starts_with("multipart/form-data") {
        parse_multipart_bot_form(&content_type, &raw)
            .await?
            .0
            .method_override
    } else {
        parse_bot_form(&raw).method_override
    };
    let body = Body::from(raw.to_vec());
    match override_method.as_deref() {
        Some("patch") | Some("put") => update_bots(cx, &id, body).await,
        Some("delete") => destroy_bots(cx, &id, body).await,
        _ => not_found().into_response(cx),
    }
}

/// `PATCH/PUT /account/bots/:bot_id/key`.
#[route([PATCH, PUT] "/account/bots/{bot_id}/key")]
pub async fn bots_key_update(cx: &Cx, body: Body) -> Result<Response> {
    let bot_id = path_param_segment(cx, "bot_id").to_owned();
    reset_key(cx, &bot_id, body).await
}

/// `POST /account/bots/:bot_id/key`: `_method` put/patch dispatch.
#[route(POST "/account/bots/{bot_id}/key")]
pub async fn bots_key_modify(cx: &Cx, body: Body) -> Result<Response> {
    let bot_id = path_param_segment(cx, "bot_id").to_owned();
    let raw = to_bytes(body, 1024 * 1024).await?;
    let input = parse_bot_form(&raw);
    let body = Body::from(raw.to_vec());
    match input.method_override.as_deref() {
        Some("patch") | Some("put") => reset_key(cx, &bot_id, body).await,
        _ => not_found().into_response(cx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_time_goldens() {
        assert_eq!(json_time(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(json_time(1), "1970-01-01T00:00:00.001Z");
        assert_eq!(json_time(86_400_000), "1970-01-02T00:00:00.000Z");
        assert_eq!(json_time(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(json_time(1_709_208_000_000), "2024-02-29T12:00:00.000Z");
        assert_eq!(json_time(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(json_time(-86_400_000), "1969-12-31T00:00:00.000Z");
    }

    #[test]
    fn rails_json_escapes_html_chars() {
        #[derive(Serialize)]
        struct Sample {
            text: String,
        }
        let json = to_rails_json(&Sample {
            text: "a<b>&c".to_string(),
        });
        assert_eq!(json, r#"{"text":"a\u003cb\u003e\u0026c"}"#);
    }

    #[test]
    fn role_names_match_upstream() {
        assert_eq!(role_name(0), "member");
        assert_eq!(role_name(1), "administrator");
        assert_eq!(role_name(2), "bot");
        assert_eq!(role_name(9), "member");
    }

    #[test]
    fn ruby_strip_trims_nul_and_vertical_tab() {
        assert_eq!(ruby_strip("  5-abc  "), "5-abc");
        assert_eq!(ruby_strip(" 5-abc "), "5-abc");
        assert_eq!(ruby_strip("5-abc\u{b}"), "5-abc");
        assert_eq!(ruby_strip("   "), "");
    }
}
