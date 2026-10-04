//! HTML pages matching upstream's views pixel-for-markup.
//!
//! [`document`] is the site-wide layout (upstream application layout:
//! skip link, nav, flash, main + footer, sidebar, lightbox, app logo)
//! over the vendored design-system stylesheets (`src/assets`, MIT — see
//! `assets/NOTICE`). Pages return content only; routes answering markup
//! directly shell it themselves since layouts only wrap pages.

use std::future::Future;

use topcamp_db::PgDb;
use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        Slot,
        error::see_other,
        layout, query_params, request,
        response::{AsyncIntoResponse, IntoResponse, Response},
    },
    view::{BoxView, HoistView, View, ViewExt as _, view},
};

use crate::state::{AppState, http_error};

/// Request-scoped shell context: signed-in identity for the head
/// metas plus the account logo cache key. `None` fields render nothing
/// (signed-out pages) or the bare logo URL (pre-install pages, where no
/// account row exists yet to version against).
pub(crate) struct ShellContext {
    pub current_user: Option<(i64, String)>,
    pub logo_version: Option<String>,
}

impl ShellContext {
    pub(crate) fn anonymous() -> Self {
        Self {
            current_user: None,
            logo_version: None,
        }
    }
}

/// `body_classes`: page hook + `admin` for administrators (the
/// `account-has-logo` hook arrives with custom logos).
pub(crate) fn body_classes(page: &'static str, admin: bool) -> &'static str {
    match (page, admin) {
        ("", false) => "",
        ("", true) => "admin",
        ("signup", false) => "signup",
        ("sidebar", false) => "sidebar",
        ("sidebar", true) => "sidebar admin",
        ("sidebar searches", false) => "sidebar searches",
        ("sidebar searches", true) => "sidebar searches admin",
        (page, _) => page,
    }
}

/// The persistent theme cookie (`light` / `dark` / `system`).
pub(crate) const THEME_COOKIE: &str = "topcamp_theme";

/// Request theme: the cookie's value, or `system` when missing or
/// unknown (the stylesheet then follows the OS color scheme). Reads
/// the raw `Cookie` header: the shell also renders contexts where the
/// cookie layer is not installed, and `cookies(cx)` panics without it.
pub(crate) fn theme(cx: &Cx) -> &'static str {
    let value = request::headers(cx)
        .get("cookie")
        .and_then(|header| header.to_str().ok())
        .iter()
        .flat_map(|header| header.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(name, value)| (name.trim() == THEME_COOKIE).then(|| value.trim()));
    match value {
        Some("light") => "light",
        Some("dark") => "dark",
        _ => "system",
    }
}

/// Shared document shell, matching upstream's application layout
/// (skip link, nav, flash, main + footer, sidebar, lightbox, app logo).
/// DB-free so error pages render without a database. `now` carries
/// `flash.now` values for direct renders; every render also consumes
/// the carried flash cookie exactly once. `body_class` carries
/// page-local styling hooks (upstream's `signup` on first-run).
#[allow(clippy::too_many_arguments)]
pub(crate) fn document_shell<'a>(
    cx: &Cx,
    title: String,
    body_class: &'static str,
    content: Slot<'a>,
    now: crate::flash::Flash,
    shell: ShellContext,
    sidebar: Option<Slot<'a>>,
    nav: Option<Slot<'a>>,
    head_extra: Option<Slot<'a>>,
    footer: Option<Slot<'a>>,
) -> impl View + use<'a> {
    use crate::assets::*;
    let csrf_token = crate::csrf::issue(cx);
    let flash = crate::flash::take(cx, now);
    let logo_url = match shell.logo_version {
        Some(version) => format!("/account/logo?v={version}"),
        None => "/account/logo".to_string(),
    };
    // Live regions hoist parts while polling; the shell collects them
    // (UI-05r). Without this wrapper `emit!` panics: only pages,
    // layouts, components, and shards collect hoisted parts.
    let theme = theme(cx);
    HoistView::new(view! {
        cx =>
        <!DOCTYPE html>
        <html lang="en" data-theme=(theme)>
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1, user-scalable=no, interactive-widget=resizes-content" />
                <meta name="view-transition" content="same-origin" />
                <meta name="color-scheme" content="light dark" />
                <meta name="theme-color" content="#ffffff" media="(prefers-color-scheme: light)" />
                <meta name="theme-color" content="#000000" media="(prefers-color-scheme: dark)" />
                <meta name="apple-mobile-web-app-capable" content="yes" />
                <meta name="csrf-param" content="authenticity_token" />
                <meta name="csrf-token" content=(csrf_token) />
                if let Some((id, _)) = shell.current_user.as_ref() {
                    <meta name="current-user-id" content=(id.to_string()) />
                }
                if let Some((_, name)) = shell.current_user.as_ref() {
                    <meta name="current-user-name" content=(name.clone()) />
                }
                <meta name="action-cable-url" content="/cable" />
                if let Some(key) = crate::push_subscriptions::vapid_public_key() {
                    <meta name="vapid-public-key" content=(key) />
                } else {
                    <meta name="vapid-public-key" />
                }
                <link rel="manifest" href="/webmanifest.json" />
                <link rel="icon" href=(logo_url.clone()) type="image/png" />
                <link rel="apple-touch-icon" href=(logo_url) />
                <link rel="stylesheet" href=(css_reset()) />
                <link rel="stylesheet" href=(css_actiontext()) />
                <link rel="stylesheet" href=(css_animation()) />
                <link rel="stylesheet" href=(css_autocomplete()) />
                <link rel="stylesheet" href=(css_avatars()) />
                <link rel="stylesheet" href=(css_base()) />
                <link rel="stylesheet" href=(css_boosts()) />
                <link rel="stylesheet" href=(css_buttons()) />
                <link rel="stylesheet" href=(css_code()) />
                <link rel="stylesheet" href=(css_colorize()) />
                <link rel="stylesheet" href=(css_colors()) />
                <link rel="stylesheet" href=(css_composer()) />
                <link rel="stylesheet" href=(css_embeds()) />
                <link rel="stylesheet" href=(css_filters()) />
                <link rel="stylesheet" href=(css_flash()) />
                <link rel="stylesheet" href=(css_inputs()) />
                <link rel="stylesheet" href=(css_layout()) />
                <link rel="stylesheet" href=(css_lightbox()) />
                <link rel="stylesheet" href=(css_messages()) />
                <link rel="stylesheet" href=(css_nav()) />
                <link rel="stylesheet" href=(css_panels()) />
                <link rel="stylesheet" href=(css_separators()) />
                <link rel="stylesheet" href=(css_sidebar()) />
                <link rel="stylesheet" href=(css_signup()) />
                <link rel="stylesheet" href=(css_spinner()) />
                <link rel="stylesheet" href=(css_utilities()) />
                if let Some(extra) = head_extra {
                    (extra)
                }
                <title>(title)</title>
            </head>
            <body class=(body_class)>
                <a href="#main-content" class="skip-navigation btn">"Skip to main content"</a>
                <nav id="nav">
                    if let Some(items) = nav {
                        (items)
                    }
                </nav>
                // `notice = flash_notice.or(flash_alert)`: one flash at most;
                // the alert restyles it negative (even under a notice — the
                // layout's exact precedence, stray-`</span>` typo aside).
                if let Some(text) = flash.notice.clone().or_else(|| flash.alert.clone()) {
                    <div class="flash">
                        if flash.alert.is_some() {
                            <div class="flash__inner shadow" style="--flash-background: var(--color-negative)">
                                <img aria-hidden="true" class="colorize--white" src=(img_alert()) width="24" height="24" />
                            </div>
                        } else {
                            <div class="flash__inner shadow" style="">
                                <img aria-hidden="true" class="colorize--white" src=(img_check()) width="24" height="24" />
                            </div>
                        }
                        <span class="for-screen-reader" role="alert" aria-atomic="true">(text)</span>
                    </div>
                }
                <main id="main-content">
                    (content)
                    <footer id="footer">
                        if let Some(items) = footer {
                            (items)
                        }
                    </footer>
                </main>
                // Mobile nav drawer, no JS: the toggle opens the `#sidebar`
                // popover (top layer, Esc + outside-click dismiss free).
                // Desktop force-shows the unopened popover via CSS.
                if sidebar.is_some() {
                    <button class="btn sidebar__toggle" popovertarget="sidebar" aria-label="Open menu" data-tip="Open menu">
                        <img aria-hidden="true" src=(img_menu()) width="20" height="20" />
                        <span class="for-screen-reader">"Open menu"</span>
                    </button>
                }
                <aside id="sidebar" popover="auto">
                    if let Some(frame) = sidebar {
                        (frame)
                    }
                </aside>
                <a href="https://once.com" id="app-logo" target="_blank" aria-label="Once software from 37signals home page">
                    <img alt="Topcamp logo" width="256" height="216" src=(img_topcamp_icon()) />
                </a>
                <script src=(topcoat::runtime::SCRIPT)></script>
            </body>
        </html>
    })
}

/// Site-wide document shell: every page renders inside it.
#[layout("/")]
pub async fn document(cx: &Cx, slot: Slot<'_>) -> Result<impl View> {
    Ok(document_shell(
        cx,
        "Topcamp".to_string(),
        "",
        slot,
        crate::flash::Flash::default(),
        ShellContext::anonymous(),
        None,
        None,
        None,
        None,
    ))
}

/// Sign-in content, matching upstream's `sessions/new` (panel, account
/// logo, credential fieldset, translate widgets, help contact). The
/// rejection shake + flash render when `rejected`.
fn login_view(
    cx: &Cx,
    account_name: String,
    email: Option<String>,
    help: Option<(String, String)>,
    csrf_token: String,
    rejected: bool,
    logo_version: Option<String>,
) -> BoxView<'static> {
    use crate::assets::*;
    // `view!` moves its captures; the demo form below needs its own.
    let demo_token = csrf_token.clone();
    let panel_class = if rejected { "panel shake" } else { "panel" };
    let logo_url = match logo_version {
        Some(version) => format!("/account/logo?v={version}"),
        None => "/account/logo".to_string(),
    };
    view! {
        cx =>
        <section class="txt-align-center">
            <div class=(panel_class)>
                <figure class="account-logo avatar center margin-block-end txt-xx-large">
                    <img alt="Account logo" src=(logo_url) width="300" height="300" />
                </figure>
                <form class="flex flex-column gap" action="/session" accept-charset="UTF-8" method="post">
                    <input type="hidden" name="authenticity_token" value=(csrf_token) />
                    <fieldset class="flex flex-column gap center-block upad">
                        <legend class="txt-large txt-align-center"><strong>(account_name)</strong></legend>
                        <div class="flex align-center gap">
                            <details class="position-relative">
                                <summary class="btn" tabindex="-1">
                                    <img aria-hidden="true" class="color-icon" src=(img_globe()) width="20" height="20" />
                                    <span class="for-screen-reader">"Translate"</span>
                                </summary>
                                <div class="language-list-menu shadow">
                                    <dl class="language-list">
                                        <dt>"🇺🇸"</dt><dd class="margin-none">"Enter your email address"</dd>
                                        <dt>"🇪🇸"</dt><dd class="margin-none">"Introduce tu correo electrónico"</dd>
                                        <dt>"🇫🇷"</dt><dd class="margin-none">"Entrez votre adresse courriel"</dd>
                                        <dt>"🇮🇳"</dt><dd class="margin-none">"अपना ईमेल पता दर्ज करें"</dd>
                                        <dt>"🇩🇪"</dt><dd class="margin-none">"Geben Sie Ihre E-Mail-Adresse ein"</dd>
                                        <dt>"🇧🇷"</dt><dd class="margin-none">"Insira seu endereço de email"</dd>
                                        <dt>"🇯🇵"</dt><dd class="margin-none">"メールアドレスを入力してください"</dd>
                                    </dl>
                                </div>
                            </details>
                            <label class="flex align-center gap input input--actor txt-large">
                                <input required="required" class="input" autofocus="autofocus" autocomplete="username" placeholder="Enter your email address" type="email" name="email_address" id="email_address" value=(email.unwrap_or_default()) />
                                <img aria-hidden="true" class="colorize--black" src=(img_email()) width="24" height="24" />
                            </label>
                        </div>
                        <div class="flex align-center gap">
                            <details class="position-relative">
                                <summary class="btn" tabindex="-1">
                                    <img aria-hidden="true" class="color-icon" src=(img_globe()) width="20" height="20" />
                                    <span class="for-screen-reader">"Translate"</span>
                                </summary>
                                <div class="language-list-menu shadow">
                                    <dl class="language-list">
                                        <dt>"🇺🇸"</dt><dd class="margin-none">"Enter your password"</dd>
                                        <dt>"🇪🇸"</dt><dd class="margin-none">"Introduce tu contraseña"</dd>
                                        <dt>"🇫🇷"</dt><dd class="margin-none">"Saisissez votre mot de passe"</dd>
                                        <dt>"🇮🇳"</dt><dd class="margin-none">"अपना पासवर्ड दर्ज करें"</dd>
                                        <dt>"🇩🇪"</dt><dd class="margin-none">"Geben Sie Ihr Passwort ein"</dd>
                                        <dt>"🇧🇷"</dt><dd class="margin-none">"Insira sua senha"</dd>
                                        <dt>"🇯🇵"</dt><dd class="margin-none">"パスワードを入力してください"</dd>
                                    </dl>
                                </div>
                            </details>
                            <label class="flex align-center gap input input--actor txt-large">
                                <input required="required" class="input" autocomplete="current-password" placeholder="Enter your password" maxlength="72" size="72" type="password" name="password" id="password" />
                                <img aria-hidden="true" class="colorize--black" src=(img_password()) width="24" height="24" />
                            </label>
                        </div>
                        <button name="log_in" type="submit" class="btn btn--reversed center txt-large">
                            <img aria-hidden="true" src=(img_arrow_right()) />
                            <span class="for-screen-reader">"Go"</span>
                        </button>
                    </fieldset>
                </form>
            </div>
            if let Some((name, address)) = help {
                <div class="txt-align-center margin-block-double full-width">
                    // Debug builds sign in as the help-contact account
                    // instead of mailing them; release keeps upstream's
                    // mailto.
                    if cfg!(debug_assertions) {
                        <form class="button_to" method="post" action="/session/demo">
                            <input type="hidden" name="authenticity_token" value=(demo_token) />
                            <button class="btn center" title=(format!("Log in as {name}")) type="submit">
                                <img aria-hidden="true" src=(img_login_keys()) />
                                <span>(address)</span>
                            </button>
                        </form>
                    } else {
                        <a class="btn center" title=(format!("Email {name}")) href=(format!("mailto:\"{name}\" <{address}>"))>
                            <img aria-hidden="true" src=(img_lifebuoy()) />
                            <span>(address)</span>
                        </a>
                    }
                    <div class="txt-align-center center margin-block txt-subtle">
                        "Topcamp™ version "
                        <span class="version-badge">(env!("CARGO_PKG_VERSION"))</span>
                    </div>
                </div>
            }
        </section>
    }
    .boxed()
}

/// Sign-in page data: install name for the legend, first
/// administrator for the help contact, logo cache key for `?v=`.
async fn signin_data(db: &PgDb) -> Result<(String, Option<(String, String)>, Option<String>)> {
    use topcamp_db::repositories::{AccountRepository, UserRepository};
    let account = AccountRepository::first(db).await.map_err(http_error)?;
    let account_name = account
        .as_ref()
        .map(|account| account.name.clone())
        .unwrap_or_else(|| "Topcamp".to_string());
    let logo_version = account.map(|account| account.updated_number);
    let help = UserRepository::first_administrator(db)
        .await
        .map_err(http_error)?
        .and_then(|admin| Some((admin.name, admin.email_address?)));
    Ok((account_name, help, logo_version))
}

#[query_params]
struct LoginQuery {
    email_address: Option<String>,
}

/// Sign-in form. Fresh installs (no users yet) go to first-run setup.
/// A route (not a page): it can redirect before rendering.
/// `?email_address=` prefills the field (join's duplicate redirect).
#[topcoat::router::route(GET "/session/new")]
pub async fn login(cx: &Cx) -> Result<Response> {
    use topcamp_db::repositories::UserRepository;
    let db = &app_context::<AppState>(cx).db;
    if UserRepository::count(db).await.map_err(http_error)? == 0 {
        return see_other("/first_run").into_response(cx);
    }
    let prefill = query_params::<LoginQuery>(cx)
        .ok()
        .and_then(|query| query.email_address.clone());
    let (account_name, help, logo_version) = signin_data(db).await?;
    let content = login_view(
        cx,
        account_name,
        prefill,
        help,
        crate::csrf::issue(cx),
        false,
        logo_version.clone(),
    );
    let shell = ShellContext {
        current_user: None,
        logo_version,
    };
    document_shell(
        cx,
        "Sign in".to_string(),
        "",
        Slot::new(content),
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

/// Failed sign-in, re-rendered with the rejection flash + panel shake.
/// `status` is 401 (bad credentials) or 429 (rate limited).
pub async fn login_failed(
    cx: &Cx,
    email: Option<String>,
    status: http::StatusCode,
) -> Result<Response> {
    let db = &app_context::<AppState>(cx).db;
    let (account_name, help, logo_version) = signin_data(db).await?;
    let form = login_view(
        cx,
        account_name,
        email,
        help,
        crate::csrf::issue(cx),
        true,
        logo_version.clone(),
    );
    let marked = view! {
        cx =>
        (status)
        (form)
    }
    .boxed();
    let shell = ShellContext {
        current_user: None,
        logo_version,
    };
    document_shell(
        cx,
        "Sign in".to_string(),
        "",
        Slot::new(marked),
        crate::flash::Flash::alert(crate::auth::REJECTION),
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
