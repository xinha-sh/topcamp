//! Session transfer (`Sessions::TransfersController`): sign in on
//! another device with a user's transfer link, plus the
//! incompatible-browser page (`AllowBrowser`).
//!
//! `GET /session/transfers/:id` renders a form with a submit button
//! that PUTs back to its own URL; `PUT/PATCH` (or `POST` with
//! `_method=put/patch`) verifies the `transfer_id` signed id, starts
//! a session for the active user behind it, and goes `/`. Anything
//! else is `head :bad_request`. The transfer link itself lives on
//! the profile pages ([`transfer_fieldset`]); the QR route that
//! renders it scannable is [`crate::qr_code`].
//!
//! Old browsers never reach any of this: [`browser_gate`] (wired in
//! [`crate::router`]) renders the incompatible-browser page first.
//! Upstream runs authentication before the browser check, so a
//! signed-out old browser there 302s to sign-in and only then sees
//! the page; the layer answers the page directly instead (one fewer
//! hop to the same screen).

use serde::Deserialize;
use topcamp_db::repositories::UserRepository;
use topcamp_domain::auth::UserStatus;
use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        Body, Slot,
        error::{forbidden, not_found, see_other},
        path_param_segment,
        request::{headers, uri},
        response::{AsyncIntoResponse, IntoResponse, Response},
        route, to_bytes,
    },
    view::{BoxView, ViewExt as _, view},
};

use crate::state::{AppState, http_error};

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// `session_transfer_path(transfer_id)`.
pub(crate) fn transfer_path(transfer_id: &str) -> String {
    format!("/session/transfers/{transfer_id}")
}

/// Absolute transfer URL from the request Host (`http` unless a
/// forwarded proto says otherwise); relative when Host is absent.
/// Mirrors `invite_url`: the link leaves this device, so it needs
/// the origin.
pub(crate) fn transfer_url(cx: &Cx, transfer_id: &str) -> String {
    let path = transfer_path(transfer_id);
    let host = headers(cx)
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim();
    if host.is_empty() {
        return path;
    }
    let proto = headers(cx)
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http")
        .split(',')
        .next()
        .unwrap_or("http")
        .trim();
    format!("{proto}://{host}{path}")
}

/// `user.transfer_id`: freshly minted for the profile pages (each
/// render gets its own 4-hour token, like upstream).
pub(crate) fn mint_transfer_id(secret_key_base: &[u8], user_id: i64) -> String {
    crate::signed_id::transfer_id(secret_key_base, user_id, now_millis())
}

/// `GET /session/transfers/:id`: the PUT form (plain submit button).
/// `allow_unauthenticated_access`, signed in or not.
#[route(GET "/session/transfers/{id}")]
pub async fn show(cx: &Cx) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    let action = transfer_path(&id);
    let csrf_token = crate::csrf::issue(cx);
    let content = Slot::new(transfer_show_view(cx, &action, &csrf_token));
    let me = crate::auth::current_user(cx).await?;
    let admin = me
        .as_ref()
        .is_some_and(|user| user.role == topcamp_domain::auth::UserRole::Administrator.value());
    let shell = match me {
        Some(user) => {
            let db = &app_context::<AppState>(cx).db;
            crate::users::page_shell(db, user.id, &user.name).await?
        }
        None => crate::pages::ShellContext::anonymous(),
    };
    crate::pages::document_shell(
        cx,
        "Topcamp".to_string(),
        crate::pages::body_classes("", admin),
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

/// Block-less `form_with(url, method: :put)`: the opening tag plus
/// hidden fields. Upstream auto-submits via JS; without it the page
/// carries a plain submit button instead (the posted fields are
/// identical either way).
fn transfer_show_view(cx: &Cx, action: &str, csrf_token: &str) -> BoxView<'static> {
    let action = action.to_string();
    let csrf = csrf_token.to_string();
    view! {
        cx =>
        <form action=(action) accept-charset="UTF-8" method="post">
            <input type="hidden" name="_method" value="put" />
            <input type="hidden" name="authenticity_token" value=(csrf) />
            <button class="btn" type="submit">"Sign in"</button>
        </form>
    }
    .boxed()
}

/// The transfer form posts urlencoded (`_method` + token); every
/// field is optional so a bare PUT rejects as a forgery (403),
/// never a parse error.
#[derive(Debug, Default, Deserialize)]
struct TransferForm {
    #[serde(rename = "_method", default)]
    method_override: Option<String>,
    #[serde(default)]
    authenticity_token: Option<String>,
}

fn parse_form(raw: &[u8]) -> TransferForm {
    serde_urlencoded::from_bytes(raw).unwrap_or_default()
}

/// `PUT/PATCH /session/transfers/:id`: log in as the transfer's
/// active user and go `/` (`post_authenticating_url` without a
/// stored return address, which this app never sets); anything else
/// is `head :bad_request`.
#[route([PUT, PATCH] "/session/transfers/{id}")]
pub async fn update(cx: &Cx, body: Body) -> Result<Response> {
    let raw = to_bytes(body, 64 * 1024).await?;
    update_transfer(cx, &parse_form(&raw)).await
}

/// `POST /session/transfers/:id` (`_method=put/patch`); anything
/// else 404s like upstream's missing POST action.
#[route(POST "/session/transfers/{id}")]
pub async fn modify(cx: &Cx, body: Body) -> Result<Response> {
    let raw = to_bytes(body, 64 * 1024).await?;
    let form = parse_form(&raw);
    match form.method_override.as_deref() {
        Some("put") | Some("patch") => update_transfer(cx, &form).await,
        _ => not_found().into_response(cx),
    }
}

async fn update_transfer(cx: &Cx, form: &TransferForm) -> Result<Response> {
    if !crate::csrf::verify(cx, &form.authenticity_token.clone().unwrap_or_default()) {
        return Err(forbidden().into());
    }
    let id = path_param_segment(cx, "id").to_owned();
    let app = app_context::<AppState>(cx);
    // `User.active.find_by_transfer_id(params[:id])`.
    let user =
        match crate::signed_id::user_id_from_transfer_id(app.stream_key.bytes(), &id, now_millis())
        {
            Some(user_id) => UserRepository::find_by_id(&app.db, user_id)
                .await
                .map_err(http_error)?
                .filter(|user| user.status == UserStatus::Active.value()),
            None => None,
        };
    match user {
        Some(user) => {
            crate::auth::start_session(cx, &app.db, user.id).await?;
            see_other("/").into_response(cx)
        }
        None => Ok(Response::builder()
            .status(400)
            .body(Body::empty())
            .expect("400 builds")),
    }
}

/// `users/profiles/_transfer`: the auto-login link (a readonly
/// selectable input) with its QR zoom link. Administrators viewing
/// someone else get the recovery label; everyone else gets the
/// screen-reader one.
pub(crate) fn transfer_fieldset(
    cx: &Cx,
    url: &str,
    user_id: i64,
    current_user_id: i64,
) -> BoxView<'static> {
    use crate::assets::*;
    let url_value = url.to_string();
    let qr_path = format!("/qr_code/{}", crate::room_show::urlsafe_encode64(url));
    let qr_href = qr_path.clone();
    let other = user_id != current_user_id;
    view! {
        cx =>
        <fieldset>
            <legend class="gap">
                <img aria-hidden="true" class="colorize--black" src=(img_laptop()) width="36" height="36" />
                <img aria-hidden="true" class="colorize--black" src=(img_transfer()) width="36" height="36" />
                <img aria-hidden="true" class="colorize--black" src=(img_mobile_phone()) width="36" height="36" />
            </legend>
            <div class="flex flex-column gap">
                if other {
                    <div class="flex align-center gap justify-center">
                        <img aria-hidden="true" class="flex-item-no-shrink colorize--black" src=(img_crown()) width="16" height="16" />
                        <label for="session_transfer_url">"Share to get them back into their account"</label>
                    </div>
                } else {
                    <label for="session_transfer_url" class="for-screen-reader">"Use this link to login automatically on another device"</label>
                }
                <input type="text" class="input" value=(url_value) id="session_transfer_url" readonly="readonly" />
                <div class="flex align-center center gap">
                    <a class="btn" href=(qr_href)>
                        <span class="for-screen-reader">"Show auto-login QR code"</span>
                        <img aria-hidden="true" class="colorize--black" src=(img_qr_code()) width="20" height="20" />
                    </a>
                </div>
            </div>
        </fieldset>
    }
    .boxed()
}

/// `ALLOW_BROWSER_VERSIONS`, in template order.
pub(crate) const ALLOW_BROWSER_VERSIONS: [(&str, &str); 4] = [
    ("safari", "17.2"),
    ("chrome", "120"),
    ("firefox", "121"),
    ("opera", "104"),
];

/// `translations_for(:incompatible_browser_messsage)` (sic).
pub(crate) const INCOMPATIBLE_BROWSER_TRANSLATIONS: &[(&str, &str)] = &[
    (
        "🇺🇸",
        "Upgrade to a supported web browser. Topcamp requires a modern web browser. Please use one of the browsers listed below and make sure auto-updates are enabled.",
    ),
    (
        "🇪🇸",
        "Actualiza a un navegador web compatible. Topcamp requiere un navegador web moderno. Utiliza uno de los navegadores listados a continuación y asegúrate de que las actualizaciones automáticas estén habilitadas.",
    ),
    (
        "🇫🇷",
        "Mettez à jour vers un navigateur web pris en charge. Topcamp nécessite un navigateur web moderne. Veuillez utiliser l'un des navigateurs répertoriés ci-dessous et assurez-vous que les mises à jour automatiques sont activées.",
    ),
    (
        "🇮🇳",
        "समर्थित वेब ब्राउज़र में अपग्रेड करें। Topcamp को एक आधुनिक वेब ब्राउज़र की आवश्यकता है। कृपया नीचे सूचीबद्ध ब्राउज़रों में से कोई एक का उपयोग करें और सुनिश्चित करें कि स्वचालित अपडेट्स सक्षम हैं।",
    ),
    (
        "🇩🇪",
        "Aktualisieren Sie auf einen unterstützten Webbrowser. Topcamp erfordert einen modernen Webbrowser. Verwenden Sie bitte einen der unten aufgeführten Browser und stellen Sie sicher, dass automatische Updates aktiviert sind.",
    ),
    (
        "🇧🇷",
        "Atualize para um navegador compatível. O Topcamp requer um navegador moderno. Por favor, use um dos navegadores listados abaixo e certifique-se de que as atualizações automáticas estão ativadas.",
    ),
    (
        "🇯🇵",
        "サポートされたウェブブラウザーにアップグレードしてください。Topcampはモダンなウェブブラウザーが必要です。下記のブラウザーのいずれかを使用し、自動更新が有効になっていることを確認してください。",
    ),
];

/// `String#capitalize`: first character upcased, the rest downcased.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
    }
}

/// `sessions/incompatible_browser`: the `allow_browser` block page
/// (200, HTML whatever the request asked for). The shell is always
/// the anonymous one: layers run before the session middleware, so
/// identity is unresolvable here (and nothing on this dead-end page
/// — no forms, no live regions — reads it).
pub(crate) async fn blocked_response(cx: &Cx) -> Result<Response> {
    let user_agent = headers(cx).get("user-agent").and_then(|v| v.to_str().ok());
    let title = if crate::user_agent::apple_messages(user_agent) {
        "Topcamp"
    } else {
        "Unsupported browser"
    }
    .to_string();
    let content = Slot::new(incompatible_browser_view(cx));
    crate::pages::document_shell(
        cx,
        title,
        crate::pages::body_classes("", false),
        content,
        crate::flash::Flash::default(),
        crate::pages::ShellContext::anonymous(),
        None,
        None,
        None,
        None,
    )
    .boxed()
    .async_into_response(cx)
    .await
}

fn incompatible_browser_view(cx: &Cx) -> BoxView<'static> {
    use crate::assets::*;
    let translations = Slot::new(crate::users::translations_view(
        cx,
        INCOMPATIBLE_BROWSER_TRANSLATIONS,
    ));
    let browsers: Vec<(String, String, Slot<'static>)> = ALLOW_BROWSER_VERSIONS
        .iter()
        .map(|(browser, version)| {
            let icon = match *browser {
                "safari" => img_browser_safari(),
                "chrome" => img_browser_chrome(),
                "firefox" => img_browser_firefox(),
                _ => img_browser_opera(),
            };
            let badge = Slot::new(
                view! {
                    cx =>
                    <img aria-hidden="true" class="center" src=(icon) />
                }
                .boxed(),
            );
            (capitalize(browser), format!(" {version}+"), badge)
        })
        .collect();
    view! {
        cx =>
        <div class="panel center">
            <header>
                <h1 class="txt-x-large txt-tight-lines txt-align-center margin-none-block-start margin-block-end">"Upgrade to a supported web browser"</h1>
                <div class="flex align-start gap">
                    (translations)
                    <p class="margin-none-block-start">"Topcamp requires a modern web browser. Please use one of the browsers listed below and make sure auto-updates are enabled."</p>
                </div>
            </header>
            <div class="browser-list flex align-center flex-wrap gap justify-center margin-block">
                for (browser, version, badge) in browsers {
                    <div class="browser flex flex-column">
                        (badge)
                        <div class="flex flex-column align-center margin-block-start-half">
                            <strong>(browser)</strong>
                            <span>(version)</span>
                        </div>
                    </div>
                }
            </div>
        </div>
    }
    .boxed()
}

/// Paths the browser gate skips: the framework's own assets,
/// runtime, fonts, and live procedures, the cable socket, and the
/// health check. Everything else is an app request upstream would
/// run `allow_browser` on.
pub(crate) fn gate_skipped(path: &str) -> bool {
    path == "/up"
        || path == "/cable"
        || path.starts_with("/_topcoat/")
        || path.starts_with("/live/")
}

/// The gate half of `allow_browser`: `Some(blocked page)` when the
/// request's browser is too old to run Topcamp, `None` otherwise.
/// The caller (a layer) skips infrastructure paths first.
pub(crate) async fn gate_response(cx: &Cx) -> Result<Option<Response>> {
    let user_agent = headers(cx)
        .get("user-agent")
        .and_then(|value| value.to_str().ok());
    if !crate::user_agent::browser_blocked(user_agent) {
        return Ok(None);
    }
    Ok(Some(blocked_response(cx).await?))
}

/// Current request path, for the gate's exclusion list.
pub(crate) fn request_path(cx: &Cx) -> &str {
    uri(cx).path()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_paths_join() {
        assert_eq!(transfer_path("abc"), "/session/transfers/abc");
    }

    #[test]
    fn gate_skips_infrastructure() {
        for path in [
            "/up",
            "/cable",
            "/_topcoat/assets/app.css",
            "/_topcoat/runtime/procedures",
            "/_topcoat/fonts/x",
            "/live/messages/post",
        ] {
            assert!(gate_skipped(path), "{path}");
        }
        for path in [
            "/",
            "/session/new",
            "/session/transfers/abc",
            "/qr_code/abc",
            "/users/me/profile",
            "/search",
            "/unfurl_link",
            "/webmanifest.json",
        ] {
            assert!(!gate_skipped(path), "{path}");
        }
    }

    #[test]
    fn capitalize_matches_ruby() {
        assert_eq!(capitalize("safari"), "Safari");
        assert_eq!(capitalize("CHROME"), "Chrome");
        assert_eq!(capitalize(""), "");
    }
}
