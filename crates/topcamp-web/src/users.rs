//! User avatars (`GET`+`DELETE /users/:token/avatar`).
//!
//! Upstream `Users::AvatarsController`: the signed avatar token names
//! the user (bad signature → 404); the response is the uploaded image,
//! the default bot avatar, or an initials SVG. Freshness matches
//! upstream (`users/<id>-<v>` ETag, 30-minute public cache).
//!
//! Tokens are HMAC-SHA256 over the user id, keyed by `SECRET_KEY_BASE`
//! (dev default + loud warning in `serve`; tests install a fixed key).
//! The format is ours (`<id>.<hex>`): only this app mints and verifies
//! them (Rails-compat would buy nothing across separate databases).

use std::sync::OnceLock;

use subtle::ConstantTimeEq;
use topcamp_db::repositories::{AttachmentRepository, AvatarUser, SidebarUser, UserRepository};
use topcamp_domain::auth::UserRole;

static SECRET: OnceLock<Vec<u8>> = OnceLock::new();

/// Install the avatar-token key. The second install loses.
pub fn init_secret(key: Vec<u8>) -> std::result::Result<(), Vec<u8>> {
    SECRET.set(key)
}

fn secret() -> Vec<u8> {
    SECRET
        .get_or_init(|| b"topcamp-dev-insecure-secret".to_vec())
        .clone()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn signature(user_id: i64) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(&secret()).expect("HMAC takes any key size");
    mac.update(format!("user-avatar:{user_id}").as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Mint an avatar token for sidebar `<img>` URLs.
pub fn avatar_token(user_id: i64) -> String {
    format!("{user_id}.{}", hex(&signature(user_id)))
}

/// Verify an avatar token back to its user id.
pub fn verify_avatar_token(token: &str) -> Option<i64> {
    let (id, presented) = token.split_once('.')?;
    let user_id: i64 = id.parse().ok()?;
    let expected = hex(&signature(user_id));
    (presented.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() == 1).then_some(user_id)
}

/// `User#attachable_sgid` for mentions: `User/<id>.<hex-mac>`
/// (ours, like the avatar tokens: only this app mints them).
pub fn mention_sgid(user_id: i64) -> String {
    format!("User/{user_id}.{}", hex(&mention_signature(user_id)))
}

/// Verify a mention sgid back to its user id.
pub fn verify_mention_sgid(sgid: &str) -> Option<i64> {
    let rest = sgid.strip_prefix("User/")?;
    let (id, presented) = rest.split_once('.')?;
    let user_id: i64 = id.parse().ok()?;
    let expected = hex(&mention_signature(user_id));
    (presented.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() == 1).then_some(user_id)
}

fn mention_signature(user_id: i64) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(&secret()).expect("HMAC takes any key size");
    mac.update(format!("user-mention:{user_id}").as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// `fresh_user_avatar`: versioned avatar URL for a sidebar user.
pub fn avatar_url(user: &SidebarUser) -> String {
    avatar_url_for(user.id, &user.updated_number)
}

/// Versioned avatar URL from bare identity (room-form users).
pub fn avatar_url_for(user_id: i64, updated_number: &str) -> String {
    format!("/users/{}/avatar?v={updated_number}", avatar_token(user_id))
}

/// `User#initials`: first ASCII word char of each word boundary run.
pub fn initials(name: &str) -> String {
    let mut initials = String::new();
    let mut previous: Option<char> = None;
    for c in name.chars() {
        let ascii_word = c.is_ascii_alphanumeric() || c == '_';
        let boundary = previous.is_none_or(|p| !(p.is_alphanumeric() || p == '_'));
        if ascii_word && boundary {
            initials.push(c);
        }
        previous = Some(c);
    }
    initials
}

/// `Users::AvatarsHelper::AVATAR_COLORS`.
const AVATAR_COLORS: [&str; 18] = [
    "#AF2E1B", "#CC6324", "#3B4B59", "#BFA07A", "#ED8008", "#ED3F1C", "#BF1B1B", "#736B1E",
    "#D07B53", "#736356", "#AD1D1D", "#BF7C2A", "#C09C6F", "#698F9C", "#7C956B", "#5D618F",
    "#3B3633", "#67695E",
];

/// Zlib's CRC-32 (IEEE, reflected).
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// `avatar_background_color(user)`: `Zlib.crc32(user.to_param)` picks it.
pub fn avatar_background_color(user_id: i64) -> &'static str {
    let crc = crc32(user_id.to_string().as_bytes());
    AVATAR_COLORS[(crc % AVATAR_COLORS.len() as u32) as usize]
}

/// `users/avatars/show.svg.erb`, byte for byte (note the blank line the
/// `{% if %}` leaves behind when initials are short, and the trailing
/// newline difference vs the golden file's own ending).
pub fn avatar_svg(user_id: i64, initials: &str) -> String {
    let color = avatar_background_color(user_id);
    let squeeze = initials.chars().count() >= 3;
    format!(
        "<svg version=\"1.1\" xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\"\n  \
         viewBox=\"0 0 512 512\" class=\"avatar\" aria-hidden=\"true\">\n  \
         <defs>\n    \
         <clipPath id=\"porthole\">\n      \
         <circle cx=\"50%\" cy=\"50%\" r=\"50%\" />\n    \
         </clipPath>\n  \
         </defs>\n\n  \
         <g>\n    \
         <rect width=\"100%\" height=\"100%\" rx=\"50\" fill=\"{color}\" />\n\n    \
         <text x=\"50%\" y=\"50%\" fill=\"#FFFFFF\"\n      \
         text-anchor=\"middle\" dy=\"0.35em\"\n      \
         {squeeze}\
         font-family=\"-apple-system, BlinkMacSystemFont, Segoe UI, Roboto, Helvetica, Arial, sans-serif\"\n      \
         font-size=\"230\"\n      \
         font-weight=\"800\"\n      \
         letter-spacing=\"-5\">\n      \
         {initials}\n    \
         </text>\n  \
         </g>\n\
         </svg>\n",
        squeeze = if squeeze {
            "textLength=\"85%\" lengthAdjust=\"spacingAndGlyphs\"\n      "
        } else {
            "\n      "
        },
    )
}

/// `expires_in 30.minutes, public: true, stale_while_revalidate: 1.week`.
const AVATAR_CACHE_CONTROL: &str = "public, max-age=1800, stale-while-revalidate=604800";

/// `default-bot-avatar.svg` bytes for bot-role users without uploads.
const DEFAULT_BOT_AVATAR: &[u8] = include_bytes!("../assets/images/default-bot-avatar.svg");

/// `Users::AvatarsController#show`. Signed URLs still require a session
/// (upstream's default `before_actions`); bad signatures and missing
/// users are 404s, matching `InvalidSignature`/`RecordNotFound`.
#[topcoat::router::route(GET "/users/{avatar_token}/avatar")]
pub async fn avatar_show(
    cx: &topcoat::context::Cx,
) -> topcoat::Result<topcoat::router::response::Response> {
    use topcoat::router::response::{IntoResponse, Response};
    if crate::auth::current_user_or_deny_bot(cx).await?.is_none() {
        return topcoat::router::error::see_other("/session/new").into_response(cx);
    }
    let token = topcoat::router::path_param_segment(cx, "avatar_token").to_owned();
    let db = &topcoat::context::app_context::<crate::state::AppState>(cx).db;
    let user = match verify_avatar_token(&token) {
        Some(id) => UserRepository::find_avatar_user(db, id)
            .await
            .map_err(crate::state::http_error)?,
        None => None,
    };
    let Some(user) = user else {
        return not_found(cx).into_response(cx);
    };
    let etag = format!("users/{}-{}", user.id, user.updated_number);
    if let Some(requested) = topcoat::router::request::headers(cx)
        .get("if-none-match")
        .and_then(|value| value.to_str().ok())
        && (requested == etag || requested == format!("\"{etag}\""))
    {
        return Ok(Response::builder()
            .status(304)
            .body(topcoat::router::Body::empty())
            .expect("304 builds"));
    }
    let (content_type, bytes) = avatar_bytes(db, &user).await?;
    Ok(Response::builder()
        .status(200)
        .header("content-type", content_type)
        .header("cache-control", AVATAR_CACHE_CONTROL)
        .header("etag", format!("\"{etag}\""))
        .body(topcoat::router::Body::from(bytes))
        .expect("avatar response builds"))
}

fn not_found(cx: &topcoat::context::Cx) -> topcoat::Result<topcoat::router::response::Response> {
    use topcoat::router::response::{IntoResponse, Response};
    // Plain 404: the global placeholder page would leak layout weight
    // into every broken avatar (upstream sends `head :not_found`).
    Response::builder()
        .status(404)
        .body(topcoat::router::Body::empty())
        .expect("404 builds")
        .into_response(cx)
}

/// The avatar's `:square` webp variant, the bot default, or an
/// initials SVG. A missing variant row self-heals from the original;
/// a missing or non-raster upload falls through to bot/initials,
/// matching upstream's `if avatar.variable?` gate.
async fn avatar_bytes(
    db: &topcamp_db::PgDb,
    user: &AvatarUser,
) -> topcoat::Result<(String, Vec<u8>)> {
    if let Some(blob) = AttachmentRepository::avatar_for_user(db, user.id)
        .await
        .map_err(crate::state::http_error)?
        && let Some(webp) = avatar_variant(db, &blob).await?
    {
        return Ok(("image/webp".to_string(), webp));
    }
    if user.role == UserRole::Bot.value() {
        return Ok(("image/svg+xml".to_string(), DEFAULT_BOT_AVATAR.to_vec()));
    }
    Ok((
        "image/svg+xml; charset=utf-8".to_string(),
        avatar_svg(user.id, &initials(&user.name)).into_bytes(),
    ))
}

/// Fetch the recorded webp variant, deriving + storing + recording it
/// on a miss. `None` when the upload is not a decodable raster.
async fn avatar_variant(
    db: &topcamp_db::PgDb,
    blob: &topcamp_db::repositories::BlobRow,
) -> topcoat::Result<Option<Vec<u8>>> {
    let digest = crate::variants::avatar_digest();
    let key = topcamp_storage::variant_key(&blob.key, &digest);
    let recorded = AttachmentRepository::find_variant(db, blob.id, &digest)
        .await
        .map_err(crate::state::http_error)?;
    if recorded.is_none()
        && let Some(store) = crate::first_run::store()
        && let Ok(Some(original)) = topcamp_storage::BlobStore::get(&store, &blob.key).await
        && let Some(webp) = crate::variants::square_webp(&original)
        && topcamp_storage::BlobStore::put(&store, &key, webp, Some("image/webp"))
            .await
            .is_ok()
    {
        let _ = AttachmentRepository::record_variant(db, blob.id, &digest).await;
    }
    crate::first_run::store_bytes(&key).await
}

/// Destroy form: `POST` carries the Rails `_method` override.
#[derive(Debug, serde::Deserialize)]
struct AvatarDestroyForm {
    authenticity_token: String,
    #[serde(rename = "_method", default)]
    method_override: Option<String>,
}

/// `Users::AvatarsController#destroy`: the *current* user's avatar goes
/// (the path id is decorative upstream too), then back to the profile.
#[topcoat::router::route([POST, DELETE] "/users/{avatar_token}/avatar")]
pub async fn avatar_destroy(
    cx: &topcoat::context::Cx,
    topcoat::router::content::Form(input): topcoat::router::content::Form<AvatarDestroyForm>,
) -> topcoat::Result<topcoat::router::response::Response> {
    use topcoat::router::response::IntoResponse;
    if topcoat::router::request::method(cx) == http::Method::POST
        && input.method_override.as_deref() != Some("delete")
    {
        return Err(topcoat::router::error::not_found().into());
    }
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return topcoat::router::error::see_other("/session/new").into_response(cx);
    };
    if !crate::csrf::verify(cx, &input.authenticity_token) {
        return Err(topcoat::router::error::forbidden().into());
    }
    let db = &topcoat::context::app_context::<crate::state::AppState>(cx).db;
    if let Some(blob_id) = AttachmentRepository::detach_from_record(db, "User", user.id, "avatar")
        .await
        .map_err(crate::state::http_error)?
    {
        let mut tx = db.pool().begin().await.map_err(topcamp_db::DbError::Sqlx)?;
        topcamp_db::outbox::publish(&mut tx, "purge_blob", &format!("{{\"blob_id\":{blob_id}}}"))
            .await
            .map_err(topcamp_db::DbError::Sqlx)?;
        tx.commit().await.map_err(topcamp_db::DbError::Sqlx)?;
    }
    topcoat::router::error::see_other("/users/me/profile").into_response(cx)
}

// --- show --------------------------------------------------------------------

use topcamp_db::repositories::{
    AccountRepository, MembershipRepository, MessageRepository, ProfileUser, RoomRepository,
};
use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        Body, Slot,
        error::{forbidden, see_other},
        path_param_segment, request,
        response::{AsyncIntoResponse, IntoResponse, Response},
        route, to_bytes,
    },
    view::{ViewExt as _, view},
};

use crate::pages::{ShellContext, body_classes, document_shell};
use crate::rooms::cast_integer;
use crate::state::{AppState, http_error};

/// Shell identity + logo for user pages.
pub(crate) async fn page_shell(
    db: &topcamp_db::PgDb,
    user_id: i64,
    name: &str,
) -> Result<ShellContext> {
    Ok(ShellContext {
        current_user: Some((user_id, name.to_string())),
        logo_version: AccountRepository::first(db)
            .await
            .map_err(http_error)?
            .map(|account| account.updated_number),
    })
}

fn ensure_admin(admin: bool) -> Result<()> {
    if admin {
        Ok(())
    } else {
        Err(forbidden().into())
    }
}

/// `link_back`: the referrer, else root (upstream checks the request
/// URL too; the show page is never its own referrer in practice).
fn back_href(cx: &Cx) -> String {
    request::headers(cx)
        .get("referer")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("/")
        .to_string()
}

/// `users/_ban_button`: ban (POST) when active, unban (DELETE) otherwise.
fn ban_button_view(
    cx: &Cx,
    user_id: i64,
    name: &str,
    active: bool,
    csrf_token: &str,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let action = format!("/users/{user_id}/ban");
    let csrf = csrf_token.to_string();
    if active {
        let label = format!("Ban {name}");
        let text = format!("Ban {name}");
        let confirm = "Are you sure you want to ban this user? This will log them out, delete their messages, and block their IP addresses.".to_string();
        view! {
            cx =>
            <form class="button_to" data-confirm=(confirm) action=(action) accept-charset="UTF-8" method="post">
                <input type="hidden" name="authenticity_token" value=(csrf) />
                <button class="btn full-width" type="submit">
                    <img aria-hidden="true" aria-label=(label) src=(img_cancel()) />
                    <span>(text)</span>
                </button>
            </form>
        }
        .boxed()
    } else {
        let label = format!("Remove Ban {name}");
        let confirm = "Are you sure you want to remove the ban on this user?".to_string();
        view! {
            cx =>
            <form class="button_to" data-confirm=(confirm) action=(action) accept-charset="UTF-8" method="post">
                <input type="hidden" name="_method" value="delete" />
                <input type="hidden" name="authenticity_token" value=(csrf) />
                <button class="btn btn--negative full-width" type="submit">
                    <img aria-hidden="true" aria-label=(label) src=(img_cancel()) />
                    <span>"Remove ban"</span>
                </button>
            </form>
        }
        .boxed()
    }
}

/// Ping button: `POST /rooms/directs` with `user_ids[]`.
fn ping_button_view(
    cx: &Cx,
    user_id: i64,
    name: &str,
    bot: bool,
    csrf_token: &str,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let csrf = csrf_token.to_string();
    let user_id_value = user_id.to_string();
    if bot {
        view! {
            cx =>
            <form class="button_to" action="/rooms/directs" accept-charset="UTF-8" method="post">
                <input type="hidden" name="authenticity_token" value=(csrf) />
                <input type="hidden" name="user_ids[]" value=(user_id_value) />
                <button class="btn btn--primary full-width txt--large" type="submit">
                    <img aria-hidden="true" src=(img_messages()) />
                </button>
            </form>
        }
        .boxed()
    } else {
        let label = format!("Ping {name}");
        view! {
            cx =>
            <form class="button_to" action="/rooms/directs" accept-charset="UTF-8" method="post">
                <input type="hidden" name="authenticity_token" value=(csrf) />
                <input type="hidden" name="user_ids[]" value=(user_id_value) />
                <button class="btn btn--reversed full-width txt-large" type="submit">
                    <img aria-hidden="true" aria-label=(label) src=(img_messages()) />
                </button>
            </form>
        }
        .boxed()
    }
}

/// `users/show`: the profile panel. Administrators see the
/// transfer fieldset on active users (their own page included).
#[allow(clippy::too_many_arguments)]
fn show_view(
    cx: &Cx,
    profile: &ProfileUser,
    me_id: i64,
    admin: bool,
    avatar_url: &str,
    csrf_token: &str,
    transfer_url: &str,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    use topcamp_domain::auth::UserStatus;
    let name = profile.name.clone();
    let bio = profile.bio.clone().unwrap_or_default();
    let email = profile.email_address.clone().unwrap_or_default();
    let avatar_src = avatar_url.to_string();
    let csrf = csrf_token.to_string();
    let is_me = profile.id == me_id;
    let is_bot = profile.role == UserRole::Bot.value();
    let active = profile.status == UserStatus::Active.value();
    let deactivated = profile.status == UserStatus::Deactivated.value();
    let banned = profile.status == UserStatus::Banned.value();
    let back = back_href(cx);
    let pencil = Slot::new(
        view! {
            cx =>
            <a class="btn" href="/users/me/profile">
                <img aria-hidden="true" src=(img_pencil()) />
                <span class="for-screen-reader">"Edit my profile"</span>
            </a>
        }
        .boxed(),
    );
    let pencil = Slot::new(pencil);
    // `view!` moves values into branches: clone per use.
    let ping_bot = Slot::new(ping_button_view(cx, profile.id, &name, is_bot, &csrf));
    let ping = Slot::new(ping_button_view(cx, profile.id, &name, is_bot, &csrf));
    let ban_button = Slot::new(ban_button_view(cx, profile.id, &name, active, &csrf));
    let show_pencil = is_me;
    let bot_active = is_bot && active;
    let bot_gone = is_bot && !active;
    let gone_bot = format!("{name} is no longer on this account");
    let gone = format!("{name} is no longer on this account");
    let name_gone = name.clone();
    let show_person = !is_bot && !deactivated;
    let show_gone_person = !is_bot && deactivated;
    let show_email = admin && !email.is_empty();
    let email_href = format!("mailto:{email}");
    // Bots ping from their own branch; deactivated users keep the ban
    // row (upstream's unban reactivates them).
    let show_ping = active && !is_bot;
    let show_transfer = admin && active && !is_bot;
    let transfer = Slot::new(crate::transfer::transfer_fieldset(
        cx,
        transfer_url,
        profile.id,
        me_id,
    ));
    let show_ban = admin && !is_me && !is_bot;
    let panel_class = if banned {
        "flex flex-column gap banned"
    } else {
        "flex flex-column gap"
    };
    let back_link = back.clone();
    view! {
        cx =>
        <div>
            <div class="flex-item-justify-start">
                <a class="btn" href=(back_link)>
                    <img aria-hidden="true" width="20" height="20" src=(img_arrow_left()) />
                    <span class="for-screen-reader">"Back"</span>
                </a>
            </div>
            if show_pencil {
                <div class="flex align-center gap flex-item-justify-end">
                    (pencil)
                </div>
            }
            <section class="panel txt-align-center">
                <div class=(panel_class)>
                    <div class="avatar txt-xx-large center" style="background: white">
                        <img alt="Profile avatar" class="avatar" src=(avatar_src) />
                    </div>
                    if bot_active {
                        <div class="pad-double--inline push--inline push--block-start">
                            (ping_bot)
                        </div>
                    }
                    if bot_gone {
                        <div class="pad-double--inline push--inline push--block-start">
                            <div>(gone_bot)</div>
                        </div>
                    }
                    if show_gone_person {
                        <div>
                            <h1 class="txt-x-large margin-none">(name_gone)</h1>
                            <div>(gone)</div>
                        </div>
                    }
                    if show_person {
                        <div class="flex flex-column gap" style="--row-gap: calc(var(--block-space) / 3)">
                            <h1 class="txt-x-large txt-tight-lines margin-none">(name)</h1>
                            if show_email {
                                <div><a href=(email_href)>(email)</a></div>
                            }
                            <div>(bio)</div>
                        </div>
                    }
                    if show_ping {
                        <div class="pad-inline-double margin-inline margin-block-start">
                            (ping)
                        </div>
                    }
                    if show_transfer {
                        <hr class="margin-block-start borderless" />
                        (transfer)
                    }
                    if show_ban {
                        <div class="margin-block-start">
                            (ban_button)
                        </div>
                    }
                </div>
            </section>
        </div>
    }
    .boxed()
}

/// `users#show`.
async fn show_user(cx: &Cx, id_param: &str) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let Some(id) = cast_integer(id_param) else {
        return not_found(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let admin = user.role == UserRole::Administrator.value();
    let Some(profile) = UserRepository::find_profile(db, id)
        .await
        .map_err(http_error)?
    else {
        return not_found(cx);
    };
    let avatar_url = avatar_url_for(profile.id, &profile.updated_number);
    let csrf_token = crate::csrf::issue(cx);
    let app = app_context::<AppState>(cx);
    let transfer_url = crate::transfer::transfer_url(
        cx,
        &crate::transfer::mint_transfer_id(app.stream_key.bytes(), profile.id),
    );
    let content = Slot::new(show_view(
        cx,
        &profile,
        user.id,
        admin,
        &avatar_url,
        &csrf_token,
        &transfer_url,
    ));
    let shell = page_shell(db, user.id, &user.name).await?;
    document_shell(
        cx,
        profile.name.clone(),
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

// --- bans --------------------------------------------------------------------

/// `users/bans#create`: ban (rows + disconnect + inline live removes
/// + durable workflow enqueue), back to the user.
async fn create_ban(cx: &Cx, id_param: &str, body: Body) -> Result<Response> {
    let pairs =
        serde_urlencoded::from_bytes::<Vec<(String, String)>>(&to_bytes(body, 64 * 1024).await?)
            .unwrap_or_default();
    let token = pairs
        .iter()
        .find(|(key, _)| key == "authenticity_token")
        .map(|(_, value)| value.as_str());
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !token.is_some_and(|token| crate::csrf::verify(cx, token)) {
        return Err(forbidden().into());
    }
    ensure_admin(user.role == UserRole::Administrator.value())?;
    let Some(id) = cast_integer(id_param) else {
        return not_found(cx);
    };
    let app = app_context::<AppState>(cx);
    if UserRepository::find_by_id(&app.db, id)
        .await
        .map_err(http_error)?
        .is_none()
    {
        return not_found(cx);
    }
    UserRepository::ban(&app.db, id).await.map_err(http_error)?;
    crate::cable::disconnect_user(app, id, false);
    // Inline live removes (the workflow destroys durable-side).
    let ids = MessageRepository::ids_for_creator(&app.db, id)
        .await
        .map_err(http_error)?;
    if !ids.is_empty() {
        let keys = MessageRepository::removal_keys(&app.db, &ids)
            .await
            .map_err(http_error)?;
        for (_, room_id, client_message_id) in &keys {
            crate::live::publish_message_remove(&app.bus, *room_id, client_message_id);
        }
    }
    let mut tx = app
        .db
        .pool()
        .begin()
        .await
        .map_err(topcamp_db::DbError::Sqlx)?;
    topcamp_db::outbox::publish(
        &mut tx,
        "remove_banned_content",
        &format!("{{\"user_id\":{id}}}"),
    )
    .await
    .map_err(topcamp_db::DbError::Sqlx)?;
    tx.commit().await.map_err(topcamp_db::DbError::Sqlx)?;
    see_other(format!("/users/{id}")).into_response(cx)
}

/// `users/bans#destroy`: unban, back to the user.
async fn destroy_ban(cx: &Cx, id_param: &str, body: Body) -> Result<Response> {
    let pairs =
        serde_urlencoded::from_bytes::<Vec<(String, String)>>(&to_bytes(body, 64 * 1024).await?)
            .unwrap_or_default();
    let mut token = None;
    let mut override_method = None;
    for (key, value) in &pairs {
        match key.as_str() {
            "authenticity_token" => token = Some(value.as_str()),
            "_method" => override_method = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    if request::method(cx) == http::Method::POST && override_method.as_deref() != Some("delete") {
        return not_found(cx);
    }
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !token.is_some_and(|token| crate::csrf::verify(cx, token)) {
        return Err(forbidden().into());
    }
    ensure_admin(user.role == UserRole::Administrator.value())?;
    let Some(id) = cast_integer(id_param) else {
        return not_found(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    if UserRepository::find_by_id(db, id)
        .await
        .map_err(http_error)?
        .is_none()
    {
        return not_found(cx);
    }
    UserRepository::unban(db, id).await.map_err(http_error)?;
    see_other(format!("/users/{id}")).into_response(cx)
}

/// `GET /users/:id`.
#[route(GET "/users/{id}")]
pub async fn show(cx: &Cx) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    show_user(cx, &id).await
}

/// `POST /users/:id/ban`: ban, or `_method=delete` dispatch to
/// unban (browsers can't DELETE from a form).
#[route(POST "/users/{id}/ban")]
pub async fn ban(cx: &Cx, body: Body) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    let raw = to_bytes(body, 64 * 1024).await?;
    let pairs = serde_urlencoded::from_bytes::<Vec<(String, String)>>(&raw).unwrap_or_default();
    let override_method = pairs
        .iter()
        .find(|(key, _)| key == "_method")
        .map(|(_, value)| value.to_ascii_lowercase());
    if override_method.as_deref() == Some("delete") {
        destroy_ban(cx, &id, Body::from(raw)).await
    } else {
        create_ban(cx, &id, Body::from(raw)).await
    }
}

/// `DELETE /users/:id/ban`.
#[route(DELETE "/users/{id}/ban")]
pub async fn unban(cx: &Cx, body: Body) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    destroy_ban(cx, &id, body).await
}

// --- profile -------------------------------------------------------------------

/// `translation_button(key)`: the globe popup with per-language labels.
pub(crate) fn translations_view(
    cx: &Cx,
    entries: &[(&str, &str)],
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let rows: Vec<(String, String)> = entries
        .iter()
        .map(|(language, text)| (language.to_string(), text.to_string()))
        .collect();
    let mut items: Vec<Slot<'static>> = Vec::with_capacity(rows.len() * 2);
    for (language, text) in rows {
        items.push(Slot::new(
            view! {
                cx =>
                <dt>(language)</dt>
            }
            .boxed(),
        ));
        items.push(Slot::new(
            view! {
                cx =>
                <dd class="margin-none">(text)</dd>
            }
            .boxed(),
        ));
    }
    view! {
        cx =>
        <details class="position-relative">
            <summary class="btn" tabindex="-1">
                <img src=(img_globe()) width="20" height="20" aria-hidden="true" class="color-icon" />
                <span class="for-screen-reader">"Translate"</span>
            </summary>
            <div class="language-list-menu shadow">
                <dl class="language-list">
                    for item in items {
                        (item)
                    }
                </dl>
            </div>
        </details>
    }
    .boxed()
}

pub(crate) const NAME_TRANSLATIONS: &[(&str, &str)] = &[
    ("🇺🇸", "Enter your name"),
    ("🇪🇸", "Introduce tu nombre"),
    ("🇫🇷", "Entrez votre nom"),
    ("🇮🇳", "अपना नाम दर्ज करें"),
    ("🇩🇪", "Geben Sie Ihren Namen ein"),
    ("🇧🇷", "Insira seu nome"),
    ("🇯🇵", "お名前を入力してください"),
];

const BIO_TRANSLATIONS: &[(&str, &str)] = &[
    ("🇺🇸", "Enter a few words about yourself."),
    ("🇪🇸", "Ingresa algunas palabras sobre ti mismo."),
    ("🇫🇷", "Saisissez quelques mots à propos de vous-même."),
    ("🇮🇳", "अपने बारे में कुछ शब्द लिखें."),
    ("🇩🇪", "Geben Sie ein paar Worte über sich selbst ein."),
    ("🇧🇷", "Insira alguma palavras sobre você."),
    ("🇯🇵", "ご自分について簡単に記入してください。"),
];

pub(crate) const EMAIL_TRANSLATIONS: &[(&str, &str)] = &[
    ("🇺🇸", "Enter your email address"),
    ("🇪🇸", "Introduce tu correo electrónico"),
    ("🇫🇷", "Entrez votre adresse courriel"),
    ("🇮🇳", "अपना ईमेल पता दर्ज करें"),
    ("🇩🇪", "Geben Sie Ihre E-Mail-Adresse ein"),
    ("🇧🇷", "Insira seu endereço de email"),
    ("🇯🇵", "メールアドレスを入力してください"),
];

const UPDATE_PASSWORD_TRANSLATIONS: &[(&str, &str)] = &[
    ("🇺🇸", "Change password"),
    ("🇪🇸", "Cambiar contraseña"),
    ("🇫🇷", "Changer le mot de passe"),
    ("🇮🇳", "पासवर्ड बदलें"),
    ("🇩🇪", "Passwort ändern"),
    ("🇧🇷", "Alterar senha"),
    ("🇯🇵", "パスワードを変更"),
];

/// One profile memberships-menu row: room link + involvement bell.
fn membership_view(
    cx: &Cx,
    room_id: i64,
    direct: bool,
    display_name: &str,
    involvement: &str,
    csrf_token: &str,
) -> topcoat::view::BoxView<'static> {
    let room_href = format!("/rooms/{room_id}");
    let name = display_name.to_string();
    let bell = Slot::new(crate::involvements::bell_view(
        cx,
        room_id,
        direct,
        involvement,
        csrf_token,
    ));
    view! {
        cx =>
        <li class="flex align-center gap margin-none min-width membership-item">
            <a href=(room_href) class="overflow-ellipsis fill-shade txt-primary txt-undecorated">
                <strong>(name)</strong>
            </a>
            <hr class="separator" aria-hidden="true" />
            <span class="txt-small">
                (bell)
            </span>
        </li>
    }
    .boxed()
}

/// `users/profiles/show` (the push-subscription dev fieldset
/// renders in debug builds, like upstream's development-only link).
#[allow(clippy::too_many_arguments)]
fn profile_view(
    cx: &Cx,
    profile: &ProfileUser,
    shared: Vec<Slot<'static>>,
    directs: Vec<Slot<'static>>,
    avatar_url: &str,
    csrf_token: &str,
    transfer_url: &str,
    platform: &crate::user_agent::ApplicationPlatform,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let csrf_camera = csrf_token.to_string();
    let csrf_avatar = csrf_camera.clone();
    let csrf_remove = csrf_camera.clone();
    let csrf_fields = csrf_camera.clone();
    let name = profile.name.clone();
    let email = profile.email_address.clone().unwrap_or_default();
    let bio = profile.bio.clone().unwrap_or_default();
    let avatar_src = avatar_url.to_string();
    let avatar_token = avatar_token(profile.id);
    let avatar_action = format!("/users/{avatar_token}/avatar");
    let transition = format!("view-transition-name: avatar-{}", profile.id);
    let avatar_attached = profile.avatar_attached;
    let name_translations = Slot::new(translations_view(cx, NAME_TRANSLATIONS));
    let email_translations = Slot::new(translations_view(cx, EMAIL_TRANSLATIONS));
    let password_translations = Slot::new(translations_view(cx, UPDATE_PASSWORD_TRANSLATIONS));
    let bio_translations = Slot::new(translations_view(cx, BIO_TRANSLATIONS));
    let both_lists = !shared.is_empty() && !directs.is_empty();
    let shared_rows = shared;
    let direct_rows = directs;
    let install = crate::pwa::install_instructions_view(cx, platform).map(Slot::new);
    // The profile is always the viewer's own: the screen-reader label.
    let transfer = Slot::new(crate::transfer::transfer_fieldset(
        cx,
        transfer_url,
        profile.id,
        profile.id,
    ));
    view! {
        cx =>
        <section class="panel flex flex-column gap" style=(transition)>
            if let Some(install) = install {
                (install)
            }
            <div class="align-center center avatar__form gap">
                <form class="txt-medium" action="/users/me/profile" accept-charset="UTF-8" method="post" enctype="multipart/form-data">
                    <input type="hidden" name="_method" value="patch" />
                    <input type="hidden" name="authenticity_token" value=(csrf_camera) />
                    <label class="btn input--file">
                        <img aria-hidden="true" width="20" height="20" src=(img_camera()) />
                        <input type="file" name="user[avatar]" id="file" class="input" accept="image/*" />
                        <span class="for-screen-reader">"Upload avatar"</span>
                    </label>
                    <button class="btn txt-small" type="submit">"Upload"</button>
                </form>
                <form action="/users/me/profile" accept-charset="UTF-8" method="post" enctype="multipart/form-data">
                    <input type="hidden" name="_method" value="patch" />
                    <input type="hidden" name="authenticity_token" value=(csrf_avatar) />
                    <label class="btn avatar input--file txt-xx-large">
                        <img alt="Avatar" aria-hidden="true" width="300" height="300" src=(avatar_src) />
                        <input type="file" name="user[avatar]" id="file" class="input" accept="image/*" />
                        <span class="for-screen-reader">"Avatar"</span>
                    </label>
                    <button class="btn txt-small" type="submit">"Upload"</button>
                </form>
                if avatar_attached {
                    <form class="button_to" action=(avatar_action) accept-charset="UTF-8" method="post">
                        <input type="hidden" name="_method" value="delete" />
                        <input type="hidden" name="authenticity_token" value=(csrf_remove) />
                        <button class="btn btn--negative txt-small avatar__delete-btn" type="submit">
                            <img aria-hidden="true" width="20" height="20" src=(img_minus()) />
                            <span class="for-screen-reader">"Delete avatar"</span>
                        </button>
                    </form>
                }
            </div>
            <form action="/users/me/profile" accept-charset="UTF-8" method="post">
                <input type="hidden" name="_method" value="patch" />
                <input type="hidden" name="authenticity_token" value=(csrf_fields) />
                <div class="flex flex-column gap">
                    <div class="flex align-center gap">
                        (name_translations)
                        <label class="flex align-center gap flex-item-grow input input--actor">
                            <input class="input txt-large" type="text" value=(name) name="user[name]" autocomplete="name" placeholder="Enter your name" autofocus="autofocus" required="required" data-1p-ignore="true" />
                            <img aria-hidden="true" width="24" height="24" src=(img_person()) class="colorize--black" />
                        </label>
                    </div>
                    <div class="flex align-center gap">
                        (email_translations)
                        <label class="flex align-center gap flex-item-grow input input--actor">
                            <input class="input txt-large" type="email" value=(email) name="user[email_address]" autocomplete="username" placeholder="Enter your email address" />
                            <img aria-hidden="true" width="24" height="24" src=(img_email()) class="colorize--black" />
                        </label>
                    </div>
                    <div class="flex align-center gap">
                        (password_translations)
                        <label class="flex align-center gap flex-item-grow input input--actor">
                            <input class="input txt-large" type="password" name="user[password]" autocomplete="new-password" placeholder="Change password" maxlength="72" />
                            <img aria-hidden="true" width="24" height="24" src=(img_password()) class="colorize--black" />
                        </label>
                    </div>
                    <div class="flex align-start gap">
                        (bio_translations)
                        <label class="flex align--center gap flex-item--grow input input--actor">
                            <textarea class="input txt-large" name="user[bio]" placeholder="A few words about yourself…" maxlength="200" rows="3">(bio)</textarea>
                            <img aria-hidden="true" width="24" height="24" src=(img_bio()) class="colorize--black" />
                        </label>
                    </div>
                    <button class="btn btn--reversed center txt-large" type="submit">
                        <img aria-hidden="true" width="20" height="20" src=(img_check()) />
                        <span class="for-screen-reader">"Save changes"</span>
                    </button>
                </div>
            </form>
            <div class="margin-block pad-inline pad-block fill-shade border-radius">
                <menu class="flex flex-column gap margin-none pad">
                    for row in shared_rows {
                        (row)
                    }
                    if both_lists {
                        <hr class="separator full-width" style="--border-style: solid" />
                    }
                    for row in direct_rows {
                        (row)
                    }
                </menu>
            </div>
            (transfer)
            if cfg!(debug_assertions) {
                <fieldset>
                    <legend>
                        <img aria-hidden="true" width="36" height="36" class="colorize--black" src=(img_key()) />
                    </legend>
                    <a class="btn txt-small center" href="/users/me/push_subscriptions">"Push Notifications Dev Mode"</a>
                </fieldset>
            }
        </section>
    }
    .boxed()
}

/// Profile form fields (`user[name]` etc. + optional avatar file).
#[derive(Default)]
struct ProfileForm {
    name: Option<String>,
    email_address: Option<String>,
    password: Option<String>,
    bio: Option<String>,
    authenticity_token: Option<String>,
    method_override: Option<String>,
    /// `user[avatar]` seen (any value): the 30-minute notice, like
    /// upstream's non-nil check (the value itself is ignored outside
    /// multipart file parts).
    avatar_present: bool,
}

pub(crate) struct AvatarFile {
    pub(crate) filename: String,
    pub(crate) content_type: Option<String>,
    pub(crate) bytes: Vec<u8>,
}

fn parse_profile_form(raw: &[u8]) -> ProfileForm {
    let mut form = ProfileForm::default();
    for (key, value) in
        serde_urlencoded::from_bytes::<Vec<(String, String)>>(raw).unwrap_or_default()
    {
        match key.as_str() {
            "user[name]" => form.name = Some(value),
            "user[email_address]" => form.email_address = Some(value),
            "user[password]" => form.password = Some(value),
            "user[bio]" => form.bio = Some(value),
            "user[avatar]" => form.avatar_present = true,
            "authenticity_token" => form.authenticity_token = Some(value),
            "_method" => form.method_override = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    form
}

async fn parse_multipart_profile(
    content_type: &str,
    raw: &[u8],
) -> Result<(ProfileForm, Option<AvatarFile>)> {
    use futures_util::stream::{self};
    use topcoat::router::error::bad_request;
    let boundary =
        multer::parse_boundary(content_type).map_err(|_| bad_request("malformed multipart"))?;
    let bytes = bytes::Bytes::copy_from_slice(raw);
    let stream = stream::once(async move { Ok::<_, multer::Error>(bytes) });
    let mut multipart = multer::Multipart::new(stream, boundary);
    let mut form = ProfileForm::default();
    let mut avatar = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| bad_request("malformed multipart"))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "user[avatar]" {
            let filename = field.file_name().unwrap_or("avatar").to_string();
            let content_type = field.content_type().map(|mime| mime.to_string());
            let bytes = field
                .bytes()
                .await
                .map_err(|_| bad_request("malformed multipart"))?;
            if !bytes.is_empty() {
                if bytes.len() > 5 * 1024 * 1024 {
                    return Err(bad_request("avatar too large").into());
                }
                avatar = Some(AvatarFile {
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
            "user[name]" => form.name = Some(value),
            "user[email_address]" => form.email_address = Some(value),
            "user[password]" => form.password = Some(value),
            "user[bio]" => form.bio = Some(value),
            "user[avatar]" => form.avatar_present = true,
            "authenticity_token" => form.authenticity_token = Some(value),
            "_method" => form.method_override = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    Ok((form, avatar))
}

fn storage_unavailable() -> topcoat::Error {
    use topcamp_domain::error::{Error as DomainError, InfrastructureError};
    http_error(DomainError::Infrastructure(InfrastructureError::new(
        "storage",
    )))
}

/// Replace the user's avatar (objects + rows + square WebP variant).
pub(crate) async fn replace_avatar(
    db: &topcamp_db::PgDb,
    user_id: i64,
    file: AvatarFile,
) -> Result<()> {
    use topcamp_db::repositories::NewBlob;
    use topcamp_storage::BlobStore;
    let Some(store) = crate::first_run::store() else {
        return Err(storage_unavailable());
    };
    if let Some(old) = AttachmentRepository::blob_for_record(db, "User", user_id, "avatar")
        .await
        .map_err(http_error)?
    {
        let digests = AttachmentRepository::variant_digests(db, old.id)
            .await
            .map_err(http_error)?;
        for digest in &digests {
            store
                .delete(&topcamp_storage::variant_key(&old.key, digest))
                .await
                .map_err(|_| storage_unavailable())?;
        }
        store
            .delete(&old.key)
            .await
            .map_err(|_| storage_unavailable())?;
        AttachmentRepository::delete_blob(db, old.id)
            .await
            .map_err(http_error)?;
    }
    let key = crate::first_run::generate_blob_key();
    store
        .put(&key, file.bytes.clone(), file.content_type.as_deref())
        .await
        .map_err(|_| storage_unavailable())?;
    let blob = AttachmentRepository::insert_blob(
        db,
        NewBlob {
            key: key.clone(),
            filename: file.filename,
            content_type: file.content_type,
            byte_size: file.bytes.len() as i64,
            checksum: Some(crate::first_run::checksum(&file.bytes)),
            service_name: "rustfs".to_string(),
        },
    )
    .await
    .map_err(http_error)?;
    AttachmentRepository::attach_to_record(db, "User", user_id, "avatar", blob.id)
        .await
        .map_err(http_error)?;
    let digest = crate::variants::avatar_digest();
    if let Some(webp) = crate::variants::square_webp(&file.bytes) {
        store
            .put(
                &topcamp_storage::variant_key(&key, &digest),
                webp,
                Some("image/webp"),
            )
            .await
            .map_err(|_| storage_unavailable())?;
        AttachmentRepository::record_variant(db, blob.id, &digest)
            .await
            .map_err(http_error)?;
    }
    Ok(())
}

/// `users/profiles#show`.
async fn show_profile(cx: &Cx) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let admin = user.role == UserRole::Administrator.value();
    let Some(profile) = UserRepository::find_profile(db, user.id)
        .await
        .map_err(http_error)?
    else {
        return not_found(cx);
    };
    let csrf_token = crate::csrf::issue(cx);
    let memberships = UserRepository::memberships_for_profile(db, user.id)
        .await
        .map_err(http_error)?;
    let mut shared = Vec::new();
    let mut directs = Vec::new();
    for membership in &memberships {
        let direct = membership.kind == "Rooms::Direct";
        let display = if direct {
            let ids = MembershipRepository::member_user_ids(db, membership.room_id)
                .await
                .map_err(http_error)?;
            let members = UserRepository::form_users(db, &ids)
                .await
                .map_err(http_error)?;
            let room = RoomRepository::find_for_user(db, user.id, membership.room_id)
                .await
                .map_err(http_error)?;
            room.map(|room| {
                crate::room_show::room_display_name(&room, &members, user.id, &user.name)
            })
            .unwrap_or_default()
        } else {
            membership.name.clone().unwrap_or_default()
        };
        let row = Slot::new(membership_view(
            cx,
            membership.room_id,
            direct,
            &display,
            &membership.involvement,
            &csrf_token,
        ));
        if direct {
            directs.push(row)
        } else {
            shared.push(row)
        }
    }
    let avatar_url = avatar_url_for(profile.id, &profile.updated_number);
    let app = app_context::<AppState>(cx);
    let transfer_url = crate::transfer::transfer_url(
        cx,
        &crate::transfer::mint_transfer_id(app.stream_key.bytes(), profile.id),
    );
    let user_agent = request::headers(cx)
        .get("user-agent")
        .and_then(|value| value.to_str().ok());
    let platform = crate::user_agent::ApplicationPlatform::new(user_agent);
    let content = Slot::new(profile_view(
        cx,
        &profile,
        shared,
        directs,
        &avatar_url,
        &csrf_token,
        &transfer_url,
        &platform,
    ));
    let shell = page_shell(db, user.id, &user.name).await?;
    let nav = Slot::new(profile_nav_view(cx, &back_href(cx), &csrf_token));
    document_shell(
        cx,
        profile.name.clone(),
        body_classes("", admin),
        content,
        crate::flash::Flash::default(),
        shell,
        None,
        Some(nav),
        None,
        None,
    )
    .boxed()
    .async_into_response(cx)
    .await
}

/// Profile `content_for :nav`: back link + session destroy. The
/// sessions-controller push handoff lands with push (UI-15); until
/// then the form logs out directly.
fn profile_nav_view(cx: &Cx, back: &str, csrf_token: &str) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let csrf = csrf_token.to_string();
    let back_link = back.to_string();
    view! {
        cx =>
        <div class="flex-item-justify-start">
            <a class="btn" href=(back_link)>
                <img aria-hidden="true" width="20" height="20" src=(img_arrow_left()) />
                <span class="for-screen-reader">"Back"</span>
            </a>
        </div>
        <div class="flex-item-justify-end">
            <form action="/session" accept-charset="UTF-8" method="post">
                <input type="hidden" name="_method" value="delete" />
                <input type="hidden" name="authenticity_token" value=(csrf) />
                <input type="hidden" name="push_subscription_endpoint" value="" />
                <button class="btn" type="submit">
                    <img aria-hidden="true" src=(img_logout()) />
                    <span class="for-screen-reader">"Log out"</span>
                </button>
            </form>
        </div>
    }
    .boxed()
}

/// `users/profiles#update`: fields (+ avatar file), then back. Blank
/// passwords keep the digest, as `has_secure_password` does.
async fn update_profile(cx: &Cx, body: Body) -> Result<Response> {
    let content_type = request::headers(cx)
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let (form, avatar) = if content_type.starts_with("multipart/form-data") {
        parse_multipart_profile(&content_type, &to_bytes(body, 8 * 1024 * 1024).await?).await?
    } else {
        (
            parse_profile_form(&to_bytes(body, 1024 * 1024).await?),
            None,
        )
    };
    if request::method(cx) == http::Method::POST
        && !matches!(form.method_override.as_deref(), Some("patch") | Some("put"))
    {
        return not_found(cx);
    }
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !form
        .authenticity_token
        .as_deref()
        .is_some_and(|token| crate::csrf::verify(cx, token))
    {
        return Err(forbidden().into());
    }
    let db = &app_context::<AppState>(cx).db;
    // Any avatar value (file or field) earns the 30-minute notice.
    let avatar_given = avatar.is_some() || form.avatar_present;
    if let Some(avatar) = avatar {
        replace_avatar(db, user.id, avatar).await?;
    }
    // Upstream redirects regardless of validity (no bang); blank name
    // keeps the stored one, mirroring the failed validation.
    let name = form.name.filter(|name| !name.trim().is_empty());
    let password_digest = form
        .password
        .filter(|password| !password.is_empty())
        .map(|password| {
            bcrypt::hash(password, bcrypt::DEFAULT_COST).map_err(|_| {
                http_error(topcamp_domain::error::Error::Infrastructure(
                    topcamp_domain::error::InfrastructureError::new("password"),
                ))
            })
        })
        .transpose()?;
    UserRepository::update_profile(
        db,
        user.id,
        topcamp_db::repositories::ProfileUpdate {
            name,
            email_address: form.email_address,
            bio: form.bio,
            password_digest,
        },
    )
    .await
    .map_err(http_error)?;
    // `redirect_to user_profile_url, notice: update_notice`.
    let notice = if avatar_given {
        "It may take up to 30 minutes to change everywhere."
    } else {
        "✓"
    };
    crate::flash::redirect_with_notice(cx, "/users/me/profile", notice)
}

/// `GET /users/me/profile`.
#[route(GET "/users/me/profile")]
pub async fn profile_show(cx: &Cx) -> Result<Response> {
    show_profile(cx).await
}

/// `PATCH /users/me/profile`.
#[route([PATCH, PUT] "/users/me/profile")]
pub async fn profile_update(cx: &Cx, body: Body) -> Result<Response> {
    update_profile(cx, body).await
}

/// `POST /users/me/profile` (`_method=patch/put`).
#[route(POST "/users/me/profile")]
pub async fn profile_modify(cx: &Cx, body: Body) -> Result<Response> {
    update_profile(cx, body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip_and_reject_forgeries() {
        let _ = init_secret(b"test-secret".to_vec());
        let token = avatar_token(7);
        assert_eq!(verify_avatar_token(&token), Some(7));
        assert_eq!(verify_avatar_token("7.deadbeef"), None);
        assert_eq!(verify_avatar_token("nope"), None);
        assert_eq!(verify_avatar_token(""), None);
        // Cross-id splices fail: the MAC binds the id.
        let other = avatar_token(8);
        let spliced = format!("7.{}", other.split_once('.').unwrap().1);
        assert_eq!(verify_avatar_token(&spliced), None);
    }

    #[test]
    fn initials_match_upstream_word_scan() {
        assert_eq!(initials("David"), "D");
        assert_eq!(initials("David Heinemeier Hansson"), "DHH");
        assert_eq!(initials("  spaced  out "), "so");
        assert_eq!(initials("under_score"), "u");
        assert_eq!(initials(""), "");
    }

    #[test]
    fn avatar_color_is_crc32_indexed() {
        // David (id 127326141 in the golden fixture) renders #736356.
        assert_eq!(avatar_background_color(127326141), "#736356");
        assert!(AVATAR_COLORS.contains(&avatar_background_color(12345)));
    }

    #[test]
    fn avatar_svg_matches_golden_shape() {
        let svg = avatar_svg(127326141, "D");
        assert!(svg.contains("fill=\"#736356\""), "{svg}");
        assert!(svg.contains(">D<") || svg.contains("\n      D\n"), "{svg}");
        assert!(!svg.contains("textLength"), "{svg}");
        let long = avatar_svg(127326141, "DHH");
        assert!(long.contains("textLength=\"85%\""), "{long}");
    }
}
