//! `MessagesController`: paged message fragments + message CRUD.
//!
//! `GET /rooms/:room_id/messages` renders one page of message items
//! (`before`/`after`, else the last page; 204 when empty) with an
//! ETag over the page. `POST` creates from the composer form and
//! answers a Turbo Stream append; show/edit render in the application
//! layout; update redirects to the message (JSON asks 500, like
//! upstream's missing template); destroy answers a remove stream.
//! `POST` to a member path dispatches `_method` the way Rack's
//! override does (patch/put → update, delete → destroy, else 404).
//!
//! Form posts carry `message[body]` (+ `message[client_message_id]`,
//! empty without JS, so blank ids mint a uuid — upstream's JS always
//! fills it). `FileUploader` posts multipart with a raw
//! `message[attachment]` file part (classic assignment, no signed
//! id); the bytes serve from `/rooms/:room_id/messages/:id/attachment`
//! (inline, or `?disposition=attachment` to download). Broadcasts and
//! bot webhook deliveries land with UI-05/UI-13.

use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        Body, Slot,
        error::{bad_request, forbidden, not_found, see_other},
        path_param_segment, query_params, request,
        response::{AsyncIntoResponse, IntoResponse, Response},
        route, to_bytes,
    },
    view::{ViewExt as _, view},
};

use topcamp_db::PgDb;
use topcamp_db::repositories::{
    AccountRepository, AttachmentRepository, MembershipRepository, MessageRepository, NewBlob,
    NewMessage, NewMessageAttachment, RoomRepository, RoomRow, UserRepository,
};
use topcamp_domain::auth::UserRole;

use crate::pages::{ShellContext, body_classes, document_shell};
use crate::rooms::cast_integer;
use crate::state::{AppState, http_error};

/// Parsed `message[...]` submission. `present` mirrors
/// `params.require("message")`: any `message[*]` key counts, even
/// with blank values.
struct MessageForm {
    present: bool,
    body: Option<String>,
    client_message_id: Option<String>,
    attachment: bool,
    authenticity_token: Option<String>,
    method_override: Option<String>,
}

fn parse_message_form(raw: &[u8]) -> MessageForm {
    let mut form = MessageForm {
        present: false,
        body: None,
        client_message_id: None,
        attachment: false,
        authenticity_token: None,
        method_override: None,
    };
    let Ok(pairs) = serde_urlencoded::from_bytes::<Vec<(String, String)>>(raw) else {
        return form;
    };
    for (key, value) in pairs {
        match key.as_str() {
            "message[body]" => {
                form.present = true;
                form.body = Some(value);
            }
            "message[client_message_id]" => {
                form.present = true;
                form.client_message_id = Some(value);
            }
            "message[attachment]" => {
                form.present = true;
                form.attachment = true;
            }
            "authenticity_token" => form.authenticity_token = Some(value),
            "_method" => form.method_override = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    form
}

fn csrf_ok(cx: &Cx, input: &MessageForm) -> bool {
    if input
        .authenticity_token
        .as_deref()
        .is_some_and(|token| crate::csrf::verify(cx, token))
    {
        return true;
    }
    // `FileUploader` posts `message[attachment]` by XHR with the token
    // in a header (Rails checks `X-CSRF-Token` too).
    request::headers(cx)
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|token| crate::csrf::verify(cx, token))
}

/// One uploaded file part (`message[attachment]`, or the bot API's
/// top-level `attachment`).
pub(crate) struct MessageFile {
    pub(crate) filename: String,
    pub(crate) content_type: Option<String>,
    pub(crate) bytes: Vec<u8>,
}

struct MultipartMessage {
    form: MessageForm,
    file: Option<MessageFile>,
}

/// `FileUploader` posts multipart: the raw file plus
/// `message[client_message_id]` (no body part).
async fn parse_multipart_message(content_type: &str, raw: &[u8]) -> Result<MultipartMessage> {
    use futures_util::stream::{self};
    let boundary =
        multer::parse_boundary(content_type).map_err(|_| bad_request("malformed multipart"))?;
    let bytes = bytes::Bytes::copy_from_slice(raw);
    let stream = stream::once(async move { Ok::<_, multer::Error>(bytes) });
    let mut multipart = multer::Multipart::new(stream, boundary);
    let mut form = MessageForm {
        present: false,
        body: None,
        client_message_id: None,
        attachment: false,
        authenticity_token: None,
        method_override: None,
    };
    let mut file = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| bad_request("malformed multipart"))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "message[attachment]" {
            let filename = field.file_name().unwrap_or("file").to_string();
            let content_type = field.content_type().map(|mime| mime.to_string());
            let bytes = field
                .bytes()
                .await
                .map_err(|_| bad_request("malformed multipart"))?;
            if !bytes.is_empty() {
                if bytes.len() > 128 * 1024 * 1024 {
                    return Err(bad_request("attachment too large").into());
                }
                form.present = true;
                form.attachment = true;
                file = Some(MessageFile {
                    filename,
                    content_type,
                    bytes: bytes.to_vec(),
                });
            }
            continue;
        }
        let value = field
            .text()
            .await
            .map_err(|_| bad_request("malformed multipart"))?;
        match name.as_str() {
            "message[body]" => {
                form.present = true;
                form.body = Some(value);
            }
            "message[client_message_id]" => {
                form.present = true;
                form.client_message_id = Some(value);
            }
            "authenticity_token" => form.authenticity_token = Some(value),
            "_method" => form.method_override = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    Ok(MultipartMessage { form, file })
}

/// `concerns::set_room` for messages: membership lookup by
/// `room_id`, else the missing-room path.
async fn set_room(db: &PgDb, user_id: i64, param: &str) -> Result<Option<RoomRow>> {
    match cast_integer(param) {
        Some(id) => RoomRepository::find_for_user(db, user_id, id)
            .await
            .map_err(http_error),
        None => Ok(None),
    }
}

/// `@room.messages.find(params[:id])`.
async fn set_message(
    db: &PgDb,
    room_id: i64,
    param: &str,
) -> Result<Option<topcamp_db::repositories::MessageDetail>> {
    match cast_integer(param) {
        Some(id) => MessageRepository::find_detail_in_room(db, room_id, id)
            .await
            .map_err(http_error),
        None => Ok(None),
    }
}

/// `ensure_can_administer` for a message: administrator or creator.
fn ensure_can_administer(admin: bool, user_id: i64, creator_id: i64) -> Result<()> {
    if admin || user_id == creator_id {
        Ok(())
    } else {
        Err(forbidden().into())
    }
}

/// The client prefers HTML over the JSON API (browsers send
/// `text/html`; API clients and curl send JSON or `*/*`).
pub(crate) fn wants_html(cx: &Cx) -> bool {
    // Runtime WS renders carry no `Accept` (handshake headers only) but
    // always take the view branch: the socket layer needs ndjson frames,
    // and the JSON API answer would fail the connected render.
    if topcoat::runtime::connected_untracked(cx) {
        return true;
    }
    request::headers(cx)
        .get("accept")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html") || accept.contains("application/xhtml"))
}

/// `room_display_name` inputs for one message's room line.
pub(crate) async fn room_name_for(db: &PgDb, room: &RoomRow) -> Result<String> {
    if room.kind != "Rooms::Direct" {
        return Ok(room.name.clone().unwrap_or_default());
    }
    let ids = MembershipRepository::member_user_ids(db, room.id)
        .await
        .map_err(http_error)?;
    let members = UserRepository::form_users(db, &ids)
        .await
        .map_err(http_error)?;
    Ok(crate::room_show::room_display_name(
        room,
        &members,
        -1,
        &members.first().map(|m| m.name.clone()).unwrap_or_default(),
    ))
}

/// One message's view (unrenderable when its creator is gone).
async fn message_view_for(
    db: &PgDb,
    room: &RoomRow,
    message: &topcamp_db::repositories::MessageDetail,
    me_id: i64,
) -> Result<crate::room_show::ShowMessageView> {
    let room_name = room_name_for(db, room).await?;
    let mut views = crate::room_show::message_views_in_room(
        db,
        room.id,
        &room_name,
        std::slice::from_ref(message),
        me_id,
    )
    .await?;
    Ok(views.pop().expect("one message in, one view out"))
}

/// Shell identity + logo for message pages.
async fn page_shell(db: &PgDb, user_id: i64, name: &str) -> Result<ShellContext> {
    Ok(ShellContext {
        current_user: Some((user_id, name.to_string())),
        logo_version: AccountRepository::first(db)
            .await
            .map_err(http_error)?
            .map(|account| account.updated_number),
    })
}

#[query_params(error = bad_request)]
struct PagingQuery {
    before: Option<String>,
    after: Option<String>,
}

// --- index ---------------------------------------------------------------------

/// `find_paged_messages`: `before` wins over `after`; a present but
/// uncastable id 404s; absent both, the last page.
pub(crate) async fn find_paged_messages(
    cx: &Cx,
    db: &PgDb,
    room_id: i64,
) -> Result<Option<Vec<topcamp_db::repositories::MessageDetail>>> {
    let query = query_params::<PagingQuery>(cx)?;
    let before = query
        .before
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    let after = query
        .after
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    if let Some(raw) = before {
        let Some(id) = cast_integer(raw) else {
            return Ok(None);
        };
        let anchor = MessageRepository::find_detail_in_room(db, room_id, id)
            .await
            .map_err(http_error)?;
        let Some(anchor) = anchor else {
            return Ok(None);
        };
        return Ok(Some(
            MessageRepository::page_before(db, room_id, anchor.id)
                .await
                .map_err(http_error)?,
        ));
    }
    if let Some(raw) = after {
        let Some(id) = cast_integer(raw) else {
            return Ok(None);
        };
        let anchor = MessageRepository::find_detail_in_room(db, room_id, id)
            .await
            .map_err(http_error)?;
        let Some(anchor) = anchor else {
            return Ok(None);
        };
        return Ok(Some(
            MessageRepository::page_after(db, room_id, anchor.id)
                .await
                .map_err(http_error)?,
        ));
    }
    Ok(Some(
        MessageRepository::last_page(db, room_id)
            .await
            .map_err(http_error)?,
    ))
}

/// `messages#index` (`layout false`): the page before/after a
/// message, or the last page; 204 when empty; 304 on a matching
/// ETag. Reached from the API list route for HTML clients.
pub(crate) async fn index_html(cx: &Cx, room_id: i64) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let room = RoomRepository::find_for_user(db, user.id, room_id)
        .await
        .map_err(http_error)?;
    let Some(room) = room else {
        return not_found().into_response(cx);
    };
    let Some(messages) = find_paged_messages(cx, db, room.id).await? else {
        return not_found().into_response(cx);
    };
    if messages.is_empty() {
        return Ok(Response::builder()
            .status(204)
            .body(Body::empty())
            .expect("204 builds"));
    }
    let etag = messages
        .iter()
        .map(|m| format!("messages/{}-{}", m.id, m.updated_ms))
        .collect::<Vec<_>>()
        .join("/");
    if request::headers(cx)
        .get("if-none-match")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|requested| requested == etag || requested == format!("\"{etag}\""))
    {
        return Ok(Response::builder()
            .status(304)
            .body(Body::empty())
            .expect("304 builds"));
    }
    let room_name = room_name_for(db, &room).await?;
    let csrf_token = crate::csrf::issue(cx);
    let views =
        crate::room_show::message_views_in_room(db, room.id, &room_name, &messages, user.id)
            .await?;
    let items: Vec<_> = views
        .iter()
        .map(|message| Slot::new(crate::room_show::message_view(cx, message, &csrf_token, "")))
        .collect();
    let mut response = view! {
        cx =>
        for item in items {
            (item)
        }
    }
    .boxed()
    .async_into_response(cx)
    .await?;
    response
        .headers_mut()
        .insert("etag", format!("\"{etag}\"").parse().expect("etag parses"));
    Ok(response)
}

// --- create --------------------------------------------------------------------

/// The deleted-room composer (`room_not_found`), in the application
/// layout, for creates into a room that's gone.
async fn room_not_found_page(cx: &Cx, user_id: i64, name: &str, admin: bool) -> Result<Response> {
    let db = &app_context::<AppState>(cx).db;
    let shell = page_shell(db, user_id, name).await?;
    let content = Slot::new(view! {
        cx =>
        <div id="composer-frame">
            <span class="composer__input input input--actor shake margin-block-end txt-negative txt-align-center" style="--input-border-color: var(--color-negative)">
                <span>"This room was deleted."</span>
            </span>
        </div>
    });
    document_shell(
        cx,
        "Topcamp".to_string(),
        body_classes("", admin),
        content,
        crate::flash::Flash::default(),
        shell,
        None,
        None,
        None,
        None,
    )
    .boxed()
    .async_into_response(cx)
    .await
}

fn storage_unavailable() -> topcoat::Error {
    use topcamp_domain::error::{Error as DomainError, InfrastructureError};
    http_error(DomainError::Infrastructure(InfrastructureError::new(
        "storage",
    )))
}

/// Stage an uploaded file for the atomic post: bytes to RustFS now
/// (outside the transaction), rows via `post_message_with_attachment`
/// so the message and its blob commit together — the live tail never
/// renders a message mid-attach. The §22 analyze step's
/// dimension/content-type fragment merges inline so previews lay out
/// without waiting for the worker (which re-merges idempotently).
pub(crate) async fn stage_upload(file: &MessageFile) -> Result<NewMessageAttachment> {
    use topcamp_storage::BlobStore;
    let Some(store) = crate::first_run::store() else {
        return Err(storage_unavailable());
    };
    let key = crate::first_run::generate_blob_key();
    store
        .put(&key, file.bytes.clone(), file.content_type.as_deref())
        .await
        .map_err(|_| storage_unavailable())?;
    let mut content_type = file.content_type.clone();
    let mut fragment = String::from(r#"{"analyzed":true"#);
    // `Marcel`-lite: sniff raster images so a sloppy client type still
    // dispatches (and lays out) as an image.
    if let Ok(format) = image::guess_format(&file.bytes) {
        content_type = Some(sniffed_content_type(format, content_type.as_deref()).to_string());
        if let Ok(image) = image::load_from_memory(&file.bytes) {
            use std::fmt::Write as _;
            write!(
                fragment,
                r#","width":{},"height":{}"#,
                image.width(),
                image.height()
            )
            .expect("fragment builds");
        }
    }
    fragment.push('}');
    Ok(NewMessageAttachment {
        blob: NewBlob {
            key,
            filename: file.filename.clone(),
            content_type,
            byte_size: file.bytes.len() as i64,
            checksum: Some(crate::first_run::checksum(&file.bytes)),
            service_name: "rustfs".to_string(),
        },
        metadata_json: fragment,
    })
}

/// Content type for a sniffed image format; formats without a web type
/// keep the client's label (defaulting to octet-stream).
fn sniffed_content_type(format: image::ImageFormat, client_type: Option<&str>) -> &str {
    match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        image::ImageFormat::Avif => "image/avif",
        image::ImageFormat::Tiff => "image/tiff",
        image::ImageFormat::Bmp => "image/bmp",
        image::ImageFormat::Ico => "image/vnd.microsoft.icon",
        _ => client_type.unwrap_or("application/octet-stream"),
    }
}

/// `messages#create` for form posts: store the message, answer a
/// Turbo Stream append. Reached from the API create route for form
/// clients.
pub(crate) async fn create_html(cx: &Cx, room_id: i64, body: Body) -> Result<Response> {
    let content_type = request::headers(cx)
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let (input, file) = if content_type.starts_with("multipart/form-data") {
        let parsed =
            parse_multipart_message(&content_type, &to_bytes(body, 128 * 1024 * 1024).await?)
                .await?;
        (parsed.form, parsed.file)
    } else {
        (
            parse_message_form(&to_bytes(body, 1024 * 1024).await?),
            None,
        )
    };
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    if !input.present {
        return Err(bad_request("param is missing or the value is empty: message").into());
    }
    let db = &app_context::<AppState>(cx).db;
    let admin = user.role == UserRole::Administrator.value();
    let room = RoomRepository::find_for_user(db, user.id, room_id)
        .await
        .map_err(http_error)?;
    let Some(room) = room else {
        return room_not_found_page(cx, user.id, &user.name, admin).await;
    };
    let client_message_id = input
        .client_message_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(uuid_v4);
    let stored = crate::richtext::canonicalize_plain(input.body.as_deref().unwrap_or(""));
    let app = app_context::<AppState>(cx);
    let staged = match file {
        Some(file) => Some(stage_upload(&file).await?),
        None => None,
    };
    let row = app
        .db
        .post_message_with_attachment(
            NewMessage {
                room_id: room.id,
                creator_id: user.id,
                client_message_id,
                body: stored.clone(),
            },
            staged,
        )
        .await
        .map_err(http_error)?;
    // Live pages append via the room's tail (UI-05r); the form
    // fallback lands back on the room (no Turbo).
    crate::live::publish_message_create(&app.db, &app.bus, room.id, row.id).await?;
    crate::bots::deliver_webhooks_to_bots(&app.db, &room, user.id, &stored, row.id).await?;
    see_other(format!("/rooms/{}", room.id)).into_response(cx)
}

/// Random client message id (`SecureRandom.uuid`), for composer
/// posts without JS (upstream's composer always fills it).
pub(crate) fn uuid_v4() -> String {
    use rand::Rng as _;
    let mut rng = rand::rng();
    let bytes: [u8; 16] = rng.random();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-4{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[16..20],
        &hex[20..32]
    )
}

// --- show / edit -----------------------------------------------------------------

/// `messages#show`, in the application layout without sidebar/nav.
async fn show_message(cx: &Cx, room_param: &str, id_param: &str) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let admin = user.role == UserRole::Administrator.value();
    let Some(room) = set_room(db, user.id, room_param).await? else {
        return not_found().into_response(cx);
    };
    let Some(message) = set_message(db, room.id, id_param).await? else {
        return not_found().into_response(cx);
    };
    let view = message_view_for(db, &room, &message, user.id).await?;
    let csrf_token = crate::csrf::issue(cx);
    let item = Slot::new(crate::room_show::message_view(cx, &view, &csrf_token, ""));
    let shell = page_shell(db, user.id, &user.name).await?;
    document_shell(
        cx,
        "Topcamp".to_string(),
        body_classes("", admin),
        item,
        crate::flash::Flash::default(),
        shell,
        None,
        None,
        None,
        None,
    )
    .boxed()
    .async_into_response(cx)
    .await
}

/// `messages/edit`: the edit frame. Attachment messages show the
/// attachment plus delete (no text editor); the `lexxy-editor`
/// nests a textarea with the plain body (no Lexxy JS ships).
fn edit_view(
    cx: &Cx,
    view: &crate::room_show::ShowMessageView,
    stored_body: &str,
    csrf_token: &str,
    confirming: bool,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let frame = crate::room_show::message_dom_id(&view.client_message_id, "edit");
    let form_id = crate::room_show::message_dom_id(&view.client_message_id, "form");
    let delete_form = crate::room_show::message_dom_id(&view.client_message_id, "delete_form");
    let dom_id = crate::room_show::message_dom_id(&view.client_message_id, "");
    let path = crate::room_show::message_path(view.room_id, view.id);
    let edit_page = crate::room_show::edit_message_path(view.room_id, view.id);
    let trigger_href = format!("{edit_page}?confirm={delete_form}");
    let mention_src = format!("/autocompletable/users?room_id={}", view.room_id);
    let editable = stored_body.to_string();
    let plain = crate::richtext::plain_text(stored_body);
    let csrf = csrf_token.to_string();
    let delete_dialog = Slot::new(crate::confirm::delete_dialog_view(
        cx,
        &delete_form,
        "Delete",
        "Delete message?",
        "Are you sure you want to delete this message?",
        confirming,
        &edit_page,
    ));
    let editing: topcoat::view::BoxView<'static> = if view.attachment.is_some() {
        let presentation = Slot::new(crate::room_show::message_presentation_view(cx, view));
        view! {
            cx =>
            <div class="message__body-content message__body-content--editing gap">
                (presentation)
                <div class="message__edit-btns flex align-center justify-space-between gap full-width pad-block-start-half">
                    <a class="btn btn--negative center margin-block-end" data-tip="Delete message" href=(trigger_href)>
                        <img aria-hidden="true" src=(img_trash()) />
                        <span class="for-screen-reader">"Delete message"</span>
                    </a>
                    (delete_dialog)
                </div>
            </div>
        }
        .boxed()
    } else {
        let path_action = path.clone();
        let path_cancel = path.clone();
        let csrf_form = csrf.clone();
        let check = img_check();
        let form_id_input = format!("message_body_trix_input_{dom_id}");
        view! {
            cx =>
            <div class="message__body-content message__body-content--editing gap">
            <div class="composer--edit composer--rich-text">
                <form id=(form_id) action=(path_action) accept-charset="UTF-8" method="post">
                    <input type="hidden" name="_method" value="patch" />
                    <input type="hidden" name="authenticity_token" value=(csrf_form) />
                    <div class="full-width input input--actor min-width fill-white">
                        <lexxy-editor rows="1" class="input lexxy-content" aria-multiline="true" aria-label="Edit message" autofocus="autofocus" permitted-attachment-types="application/vnd.topcamp.mention application/vnd.actiontext.opengraph-embed" data-direct-upload-url="/rails/active_storage/direct_uploads" data-blob-url-template="/rails/active_storage/blobs/redirect/:signed_id/:filename" id="message_body" input=(form_id_input) name="message[body]" value=(editable)>
                            <textarea name="message[body]" rows="1" aria-label="Edit message" class="input" style="background: transparent; border: 0; width: 100%; resize: none; min-height: 24px; padding: 0; field-sizing: content;">(plain)</textarea>
                            <lexxy-prompt trigger="@" name="mention" src=(mention_src) remote-filtering="true" empty-results="No matches"></lexxy-prompt>
                        </lexxy-editor>
                    </div>
                    <a href=(path_cancel)>"Close editor and discard changes"</a>
                    <div class="message__edit-btns flex align-center justify-space-between gap full-width pad-block-start-half">
                        <button name="button" type="submit" class="btn btn--reversed">
                            <img aria-hidden="true" src=(check) />
                            <span class="for-screen-reader">"Save changes"</span>
                        </button>
                        <a class="btn btn--negative" data-tip="Delete message" href=(trigger_href)>
                            <img aria-hidden="true" src=(img_trash()) />
                            <span class="for-screen-reader">"Delete message"</span>
                        </a>
                        (delete_dialog)
                    </div>
                </form>
            </div>
            </div>
        }
        .boxed()
    };
    let editing = Slot::new(editing);
    view! {
        cx =>
        <div id=(frame.clone()) style="display: contents;">
            <div class="message__body position-relative">
                (editing)
                <div class="message__actions flex flex-wrap">
                    <a class="message__action-btn message__edit-close-btn txt-small btn btn--borderless" href=(path.clone())>
                        <img class="colorize--black" aria-hidden="true" src=(img_remove()) />
                        <span class="for-screen-reader">"Close editor and discard changes"</span>
                    </a>
                </div>
                <form id=(delete_form) action=(path) accept-charset="UTF-8" method="post">
                    <input type="hidden" name="_method" value="delete" />
                    <input type="hidden" name="authenticity_token" value=(csrf) />
                </form>
            </div>
        </div>
    }
    .boxed()
}

/// `messages#edit`, in the application layout without sidebar/nav.
async fn edit_message(cx: &Cx, room_param: &str, id_param: &str) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let admin = user.role == UserRole::Administrator.value();
    let Some(room) = set_room(db, user.id, room_param).await? else {
        return not_found().into_response(cx);
    };
    let Some(message) = set_message(db, room.id, id_param).await? else {
        return not_found().into_response(cx);
    };
    ensure_can_administer(admin, user.id, message.creator_id)?;
    let stored = message.body.clone().unwrap_or_default();
    let view = message_view_for(db, &room, &message, user.id).await?;
    let csrf_token = crate::csrf::issue(cx);
    let query = query_params::<crate::confirm::ConfirmQuery>(cx)?;
    let delete_form = crate::room_show::message_dom_id(&view.client_message_id, "delete_form");
    let confirming = crate::confirm::confirming(query.confirm.as_deref(), &delete_form);
    let content = Slot::new(edit_view(cx, &view, &stored, &csrf_token, confirming));
    let shell = page_shell(db, user.id, &user.name).await?;
    document_shell(
        cx,
        "Topcamp".to_string(),
        body_classes("", admin),
        content,
        crate::flash::Flash::default(),
        shell,
        None,
        None,
        None,
        None,
    )
    .boxed()
    .async_into_response(cx)
    .await
}

// --- update / destroy --------------------------------------------------------------

/// `messages#update`: store the body, redirect to the message. JSON
/// asks 500 (`Missing template messages/show`, like upstream).
async fn update_message(cx: &Cx, room_param: &str, id_param: &str, body: Body) -> Result<Response> {
    let input = parse_message_form(&to_bytes(body, 1024 * 1024).await?);
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    if !input.present {
        return Err(bad_request("param is missing or the value is empty: message").into());
    }
    let db = &app_context::<AppState>(cx).db;
    let admin = user.role == UserRole::Administrator.value();
    let Some(room) = set_room(db, user.id, room_param).await? else {
        return not_found().into_response(cx);
    };
    let Some(message) = set_message(db, room.id, id_param).await? else {
        return not_found().into_response(cx);
    };
    ensure_can_administer(admin, user.id, message.creator_id)?;
    let stored = crate::richtext::canonicalize_plain(input.body.as_deref().unwrap_or(""));
    MessageRepository::update_body(db, message.id, &stored)
        .await
        .map_err(http_error)?;
    // Live message regions re-emit (UI-05r).
    let app = app_context::<AppState>(cx);
    crate::live::publish_message_update(&app.bus, room.id, message.id);
    if request::headers(cx)
        .get("accept")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("application/json"))
    {
        use topcamp_domain::error::{Error as DomainError, InfrastructureError};
        return Err(http_error(DomainError::Infrastructure(
            InfrastructureError::new("messages"),
        )));
    }
    see_other(crate::room_show::message_path(room.id, message.id)).into_response(cx)
}

/// `messages#destroy`: drop the message, land back on the room.
async fn destroy_message(
    cx: &Cx,
    room_param: &str,
    id_param: &str,
    body: Body,
) -> Result<Response> {
    let input = parse_message_form(&to_bytes(body, 1024 * 1024).await?);
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    let db = &app_context::<AppState>(cx).db;
    let admin = user.role == UserRole::Administrator.value();
    let Some(room) = set_room(db, user.id, room_param).await? else {
        return not_found().into_response(cx);
    };
    let Some(message) = set_message(db, room.id, id_param).await? else {
        return not_found().into_response(cx);
    };
    ensure_can_administer(admin, user.id, message.creator_id)?;
    MessageRepository::destroy(db, message.id)
        .await
        .map_err(http_error)?;
    // Live message regions emit empty (UI-05r).
    let app = app_context::<AppState>(cx);
    crate::live::publish_message_remove(&app.bus, room.id, &message.client_message_id);
    see_other(format!("/rooms/{}", room.id)).into_response(cx)
}

// --- routes ------------------------------------------------------------------------

/// `GET /rooms/:room_id/messages/:id`.
#[route(GET "/rooms/{room_id}/messages/{id}")]
pub async fn show(cx: &Cx) -> Result<Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    let id = path_param_segment(cx, "id").to_owned();
    show_message(cx, &room, &id).await
}

/// `GET /rooms/:room_id/messages/new`: no such page upstream (404).
#[route(GET "/rooms/{room_id}/messages/new")]
pub async fn new_message(cx: &Cx) -> Result<Response> {
    let _ = crate::auth::current_user(cx).await?;
    not_found().into_response(cx)
}

/// `GET /rooms/:room_id/messages/:id/edit`.
#[route(GET "/rooms/{room_id}/messages/{id}/edit")]
pub async fn edit(cx: &Cx) -> Result<Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    let id = path_param_segment(cx, "id").to_owned();
    edit_message(cx, &room, &id).await
}

#[query_params(error = bad_request)]
struct AttachmentQuery {
    disposition: Option<String>,
}

/// `GET /rooms/:room_id/messages/:id/attachment`: the blob's bytes
/// for room members (our blob redirect path), inline by default or
/// `?disposition=attachment` to download. Active content serves as
/// octet-stream, as upstream's blob controller does.
async fn show_attachment(cx: &Cx, room_param: &str, id_param: &str) -> Result<Response> {
    let query = query_params::<AttachmentQuery>(cx)?;
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let Some(room) = set_room(db, user.id, room_param).await? else {
        return not_found().into_response(cx);
    };
    let Some(message) = set_message(db, room.id, id_param).await? else {
        return not_found().into_response(cx);
    };
    let Some(blob) = AttachmentRepository::blob_for_record(db, "Message", message.id, "attachment")
        .await
        .map_err(http_error)?
    else {
        return not_found().into_response(cx);
    };
    let etag = format!("\"blob-{}-{}\"", blob.id, blob.byte_size);
    if request::headers(cx)
        .get("if-none-match")
        .and_then(|value| value.to_str().ok())
        == Some(etag.as_str())
    {
        return Ok(Response::builder()
            .status(304)
            .body(Body::empty())
            .expect("304 builds"));
    }
    let Some(bytes) = crate::first_run::store_bytes(&blob.key).await? else {
        return not_found().into_response(cx);
    };
    let stored = blob
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    let content_type = crate::richtext::content_type_for_serving(stored);
    let kind = if query.disposition.as_deref() == Some("attachment") {
        "attachment"
    } else {
        "inline"
    };
    let filename: String = blob
        .filename
        .chars()
        .map(|c| {
            if c.is_control() || c == '"' || c == '\\' {
                '_'
            } else {
                c
            }
        })
        .collect();
    Ok(Response::builder()
        .status(200)
        .header("content-type", content_type)
        .header(
            "content-disposition",
            format!("{kind}; filename=\"{filename}\""),
        )
        .header("cache-control", "private, max-age=86400")
        .header("etag", etag)
        .body(Body::from(bytes))
        .expect("attachment response builds"))
}

/// `GET /rooms/:room_id/messages/:id/attachment`.
#[route(GET "/rooms/{room_id}/messages/{id}/attachment")]
pub async fn attachment(cx: &Cx) -> Result<Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    let id = path_param_segment(cx, "id").to_owned();
    show_attachment(cx, &room, &id).await
}

/// `PATCH/PUT /rooms/:room_id/messages/:id`.
#[route(PATCH "/rooms/{room_id}/messages/{id}")]
pub async fn update(cx: &Cx, body: Body) -> Result<Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    let id = path_param_segment(cx, "id").to_owned();
    update_message(cx, &room, &id, body).await
}

/// `PUT /rooms/:room_id/messages/:id` (same action).
#[route(PUT "/rooms/{room_id}/messages/{id}")]
pub async fn replace(cx: &Cx, body: Body) -> Result<Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    let id = path_param_segment(cx, "id").to_owned();
    update_message(cx, &room, &id, body).await
}

/// `DELETE /rooms/:room_id/messages/:id`.
#[route(DELETE "/rooms/{room_id}/messages/{id}")]
pub async fn destroy(cx: &Cx, body: Body) -> Result<Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    let id = path_param_segment(cx, "id").to_owned();
    destroy_message(cx, &room, &id, body).await
}

/// `POST /rooms/:room_id/messages/:id`: Rack's method override runs
/// before routing upstream, so plain POSTs 404 and only `_method`
/// patch/put/delete dispatch.
#[route(POST "/rooms/{room_id}/messages/{id}")]
pub async fn modify(cx: &Cx, body: Body) -> Result<Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    let id = path_param_segment(cx, "id").to_owned();
    let raw = to_bytes(body, 1024 * 1024).await?;
    let method = parse_message_form(&raw).method_override;
    match method.as_deref() {
        Some("patch") | Some("put") => update_message(cx, &room, &id, Body::from(raw)).await,
        Some("delete") => destroy_message(cx, &room, &id, Body::from(raw)).await,
        _ => not_found().into_response(cx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_parses_bracketed_keys() {
        let form = parse_message_form(
            b"message%5Bbody%5D=hi&message%5Bclient_message_id%5D=&authenticity_token=t&_method=PATCH",
        );
        assert!(form.present);
        assert_eq!(form.body.as_deref(), Some("hi"));
        assert_eq!(form.client_message_id.as_deref(), Some(""));
        assert!(!form.attachment);
        assert_eq!(form.authenticity_token.as_deref(), Some("t"));
        assert_eq!(form.method_override.as_deref(), Some("patch"));
    }

    #[test]
    fn form_absent_without_message_keys() {
        let form = parse_message_form(b"authenticity_token=t");
        assert!(!form.present);
        assert_eq!(form.body, None);
    }

    #[test]
    fn uuids_look_like_v4() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert_eq!(id.chars().filter(|c| *c == '-').count(), 4);
    }
}
