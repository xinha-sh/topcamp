//! `PwaController`: the web app manifest and the service worker,
//! plus the install-instructions partial on the profile page.
//!
//! `GET /webmanifest[.json]` renders the manifest (account name,
//! logo icons, install shortcuts, screenshots); `GET
//! /service-worker[.js]` serves the push worker verbatim. Both are
//! `allow_unauthenticated_access`. Upstream answers 406 for other
//! formats (`/webmanifest.html`); those fall through to our 404 page
//! instead — nothing links them.
//!
//! There is deliberately no `/offline.html`: upstream ships none
//! (the worker handles push only, no fetch caching), so the tracker
//! line overstated. Likewise the install prompt's `chrome &&
//! android` elif is dead upstream (Chrome is excluded at the top) —
//! mirrored anyway.

use topcamp_db::repositories::AccountRepository;
use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{Slot, request::headers, response::Response, route},
    view::{BoxView, ViewExt as _, view},
};

use crate::state::{AppState, http_error};
use crate::user_agent::ApplicationPlatform;

/// `pwa/service_worker.js`, served verbatim.
pub const SERVICE_WORKER_JS: &str = include_str!("../assets/js/service_worker.js");

/// `request.base_url` for `image_url`: absolute when Host is known,
/// empty (relative manifest URLs) otherwise.
fn base_url(cx: &Cx) -> String {
    let host = headers(cx)
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim();
    if host.is_empty() {
        return String::new();
    }
    let proto = headers(cx)
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http")
        .split(',')
        .next()
        .unwrap_or("http")
        .trim();
    format!("{proto}://{host}")
}

/// `image_url(source)`: the asset's bundled URL against the base.
fn image_url(
    config: &topcoat::asset::AssetConfig,
    base: &str,
    asset: topcoat::asset::Asset,
) -> String {
    format!("{base}{}", config.resolve(asset))
}

/// `value` as a JSON string, quotes included.
fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string serializes")
}

/// `direct :fresh_account_logo`: `size` first, then the account's
/// `updated_at` number as `v`.
fn fresh_account_logo(size: Option<&str>, updated_number: Option<&str>) -> String {
    let mut query = Vec::new();
    if let Some(size) = size {
        query.push(format!("size={size}"));
    }
    if let Some(v) = updated_number {
        query.push(format!("v={v}"));
    }
    if query.is_empty() {
        "/account/logo".to_string()
    } else {
        format!("/account/logo?{}", query.join("&"))
    }
}

/// `pwa/manifest.json.erb`, byte-for-byte with upstream's render
/// (whose golden is Rails' HTML-escaped output: `&amp;` there is a
/// plain `&` here, and the file is valid JSON).
#[allow(clippy::too_many_arguments)]
fn render_manifest(
    account_name: &str,
    logo_small: &str,
    logo: &str,
    add_icon: &str,
    person_icon: &str,
    shot_chat: &str,
    shot_sidebar: &str,
    shot_dark: &str,
) -> String {
    format!(
        "{{\n  \"name\": {},\n  \"icons\": [\n    {{\n      \"src\": {},\n      \"type\": \"image/png\",\n      \"sizes\": \"192x192\"\n    }},\n    {{\n      \"src\": {},\n      \"type\": \"image/png\",\n      \"sizes\": \"512x512\"\n    }},\n    {{\n      \"src\": {},\n      \"type\": \"image/png\",\n      \"sizes\": \"512x512\",\n      \"purpose\": \"maskable\"\n    }}\n  ],\n  \"start_url\": \"/\",\n  \"display\": \"standalone\",\n  \"scope\": \"/\",\n  \"description\": \"A chat app from the makers of Basecamp and HEY.\",\n  \"categories\": [\"social\", \"business\", \"productivity\"],\n  \"theme_color\": \"#ffffff\",\n  \"background_color\": \"#ffffff\",\n  \"shortcuts\": [\n    {{\n      \"name\": \"New chat room\",\n      \"description\": \"Open Topcamp and start a new chat room\",\n      \"url\": \"rooms/opens/new\",\n      \"icons\": [{{ \"src\": {}, \"sizes\": \"any\" }}]\n    }},\n    {{\n      \"name\": \"My profile\",\n      \"description\": \"Open Topcamp and view your profile\",\n      \"url\": \"/users/me/profile\",\n      \"icons\": [{{ \"src\": {}, \"sizes\": \"any\" }}]\n    }}\n  ],\n  \"screenshots\": [\n    {{\n      \"src\": {},\n      \"sizes\": \"1080x2400\",\n      \"form_factor\": \"narrow\",\n      \"label\": \"Topcamp is an installable, self-hosted group chat system.\"\n    }},\n    {{\n      \"src\": {},\n      \"sizes\": \"1080x2400\",\n      \"form_factor\": \"narrow\",\n      \"label\": \"Easily invite people. Make rooms. @mentions, DMs, and mobile support.\"\n    }},\n    {{\n      \"src\": {},\n      \"sizes\": \"1080x2400\",\n      \"form_factor\": \"narrow\",\n      \"label\": \"Full support for dark mode, customizable to your brand.\"\n    }}\n  ]\n}}\n",
        json_string(account_name),
        json_string(logo_small),
        json_string(logo),
        json_string(logo),
        json_string(add_icon),
        json_string(person_icon),
        json_string(shot_chat),
        json_string(shot_sidebar),
        json_string(shot_dark),
    )
}

async fn manifest_body(cx: &Cx) -> Result<Response> {
    use crate::assets::*;
    let db = &app_context::<AppState>(cx).db;
    let account = AccountRepository::first(db).await.map_err(http_error)?;
    let name = account
        .as_ref()
        .map(|a| a.name.as_str())
        .unwrap_or("Topcamp");
    let version = account.as_ref().map(|a| a.updated_number.as_str());
    let base = base_url(cx);
    let config = app_context::<topcoat::asset::AssetConfig>(cx);
    let body = render_manifest(
        name,
        &fresh_account_logo(Some("small"), version),
        &fresh_account_logo(None, version),
        &image_url(config, &base, img_add()),
        &image_url(config, &base, img_person()),
        &image_url(config, &base, img_screenshot_chat()),
        &image_url(config, &base, img_screenshot_sidebar()),
        &image_url(config, &base, img_screenshot_dark_mode()),
    );
    Ok(Response::builder()
        .status(200)
        .header("content-type", "application/json; charset=utf-8")
        .body(topcoat::router::Body::from(body))
        .expect("manifest builds"))
}

/// `GET /webmanifest[.json]`.
#[route(GET "/webmanifest")]
pub async fn manifest(cx: &Cx) -> Result<Response> {
    manifest_body(cx).await
}

/// `GET /webmanifest.json` (what the layout links).
#[route(GET "/webmanifest.json")]
pub async fn manifest_json(cx: &Cx) -> Result<Response> {
    manifest_body(cx).await
}

/// `GET /service-worker[.js]`.
#[route(GET "/service-worker")]
pub async fn service_worker(cx: &Cx) -> Result<Response> {
    service_worker_body(cx).await
}

/// `GET /service-worker.js` (what push registration fetches).
#[route(GET "/service-worker.js")]
pub async fn service_worker_js(cx: &Cx) -> Result<Response> {
    service_worker_body(cx).await
}

async fn service_worker_body(_cx: &Cx) -> Result<Response> {
    Ok(Response::builder()
        .status(200)
        .header("content-type", "text/javascript; charset=utf-8")
        .body(topcoat::router::Body::from(SERVICE_WORKER_JS))
        .expect("worker builds"))
}

/// `String#capitalize`: first character upcased, the rest downcased.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
    }
}

/// Which install instructions this platform gets, in the template's
/// elif order (`ChromeAndroid` is dead: Chrome never gets here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallBranch {
    Edge,
    ChromeAndroid,
    FirefoxAndroid,
    SafariDesktop,
    Ios,
    Other,
}

fn install_branch(platform: &ApplicationPlatform) -> Option<InstallBranch> {
    if platform.chrome() || (platform.firefox() && !platform.android()) {
        return None;
    }
    Some(if platform.edge() {
        InstallBranch::Edge
    } else if platform.chrome() && platform.android() {
        InstallBranch::ChromeAndroid
    } else if platform.firefox() && platform.android() {
        InstallBranch::FirefoxAndroid
    } else if platform.safari() && platform.desktop() {
        InstallBranch::SafariDesktop
    } else if (platform.safari() || platform.chrome()) && platform.ios() {
        InstallBranch::Ios
    } else {
        InstallBranch::Other
    })
}

/// `pwa/_install_instructions`: the install prompt atop the profile,
/// `None` where the browser installs without instructions.
pub(crate) fn install_instructions_view(
    cx: &Cx,
    platform: &ApplicationPlatform,
) -> Option<BoxView<'static>> {
    use crate::assets::*;
    let branch = install_branch(platform)?;
    let browser = capitalize(&platform.browser());
    let system = platform.operating_system().unwrap_or_default();
    let edge_icon = img_install_edge();
    let dots_icon = img_menu_dots_vertical();
    let dots_icon_again = img_menu_dots_vertical();
    let share_icon = img_external_share();
    let install_icon = img_external_install();
    let install_icon_again = img_external_install();
    let disclosure_icon = img_disclosure();
    // The iOS branch leads with a paragraph; every branch has one root otherwise.
    let lead: Option<BoxView<'static>> = match branch {
        InstallBranch::Ios => Some(
            view! {
                cx =>
                <p>"To receive push notifications in "(browser)" for "(system)", you must install Topcamp as a web app."</p>
            }
            .boxed(),
        ),
        _ => None,
    };
    let steps: BoxView<'static> = match branch {
        InstallBranch::Edge => view! {
            cx =>
            <ol>
                <li>"Click "<em><img alt="the app available - install Topcamp chat button" src=(edge_icon) width="16" height="16" /></em>"in the address bar."</li>
                <li>"Click "<em>"Install"</em>"."</li>
            </ol>
        }
        .boxed(),
        InstallBranch::ChromeAndroid => view! {
            cx =>
            <ol>
                <li>"Tap the "<em><img alt="More options" src=(dots_icon) width="16" height="16" /></em>" menu button."</li>
                <li>"Tap "<em>"Install app"</em>" in the menu."</li>
            </ol>
        }
        .boxed(),
        InstallBranch::FirefoxAndroid => view! {
            cx =>
            <ol>
                <li>"Tap the "<em><img alt="More options" src=(dots_icon_again) width="16" height="16" /></em>" menu button."</li>
                <li>"Tap "<em>"Install"</em>" in the menu."</li>
            </ol>
        }
        .boxed(),
        InstallBranch::SafariDesktop => view! {
            cx =>
            <ol>
                <li>"Click "<em>"File"</em>" in the top left."</li>
                <li>"Click "<em>"Add to Dock…"</em>"."</li>
            </ol>
        }
        .boxed(),
        InstallBranch::Ios => view! {
            cx =>
            <ol>
                <li>"Tap "<em><img alt="the share button" src=(share_icon) width="20" height="20" /></em></li>
                <li>"Tap "<em>"Add to Home Screen"</em>"."</li>
            </ol>
        }
        .boxed(),
        InstallBranch::Other => view! {
            cx =>
            <p>"Some platforms require you to install Topcamp as a web app to receive push notifications."</p>
        }
        .boxed(),
    };
    let steps = topcoat::router::Slot::new(steps);
    let lead = lead.map(topcoat::router::Slot::new);
    Some(
        view! {
            cx =>
            <details class="notifications-help pwa__instructions hide-in-pwa">
                <summary class="btn">
                    <img aria-hidden="true" src=(install_icon) width="20" height="20" />
                    <strong>"Install Topcamp as a web app."</strong>
                    <img aria-hidden="true" class="disclosure" src=(disclosure_icon) width="10" height="10" />
                </summary>
                if let Some(lead) = lead {
                    (lead)
                }
                (steps)
                <div class="margin-block-start txt-align-center pwa__installer">
                    <hr class="separator margin-block" />
                    <button class="btn btn--reversed center" type="button">
                        <img aria-hidden="true" src=(install_icon_again) />
                        "Install now"
                    </button>
                </div>
            </details>
        }
        .boxed(),
    )
}

/// `pwa/_browser_settings` branches, in the template's elif order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserSettingsBranch {
    FirefoxAndroid,
    EdgeDesktop,
    FirefoxDesktop,
    ChromeDesktop,
    ChromeAndroid,
    SafariDesktop,
    Other,
}

fn browser_settings_branch(platform: &ApplicationPlatform) -> Option<BrowserSettingsBranch> {
    if (platform.safari() || platform.chrome()) && platform.ios() {
        return None;
    }
    Some(if platform.firefox() && platform.android() {
        BrowserSettingsBranch::FirefoxAndroid
    } else if platform.edge() && platform.desktop() {
        BrowserSettingsBranch::EdgeDesktop
    } else if platform.firefox() && platform.desktop() {
        BrowserSettingsBranch::FirefoxDesktop
    } else if platform.chrome() && platform.desktop() {
        BrowserSettingsBranch::ChromeDesktop
    } else if platform.chrome() && platform.android() {
        BrowserSettingsBranch::ChromeAndroid
    } else if platform.safari() && platform.desktop() {
        BrowserSettingsBranch::SafariDesktop
    } else {
        BrowserSettingsBranch::Other
    })
}

/// `pwa/_browser_settings`: per-browser notification steps. `None`
/// on iOS Safari/Chrome (no browser-side switch exists there).
/// `root_url` is the origin with a trailing slash, like Rails'.
pub(crate) fn browser_settings_view(
    cx: &Cx,
    platform: &ApplicationPlatform,
    root_url: &str,
) -> Option<BoxView<'static>> {
    use crate::assets::*;
    let branch = browser_settings_branch(platform)?;
    let browser = capitalize(&platform.browser());
    let root = root_url.to_string();
    let web_icon = img_external_web();
    let disclosure_icon = img_disclosure();
    let lock_view_site = img_lock();
    let switch = img_external_switch();
    let switch_toggle = img_external_switch();
    let sliders = img_external_sliders();
    let dots = img_menu_dots_vertical();
    let bell = img_notification_bell_alert();
    // `view!` moves its captures; the summary below reuses the name.
    let browser_chrome_android = browser.clone();
    let browser_safari = browser.clone();
    // Desktop arms interleave headings and lists with no wrapper,
    // so arms produce sibling segments (no fragment syntax).
    let segments: Vec<BoxView<'static>> = match branch {
        BrowserSettingsBranch::FirefoxAndroid => vec![view! {
            cx =>
            <ol>
                <li>"Tap "<em><img alt="the View site information button" width="20" height="20" src=(lock_view_site) /></em>" in the address bar."</li>
                <li>"Tap "<em>"Notification"</em>" to change to "<em>"Allowed"</em>"."</li>
            </ol>
        }
        .boxed()],
        BrowserSettingsBranch::EdgeDesktop => edge_desktop_browser_steps(
            cx,
            platform.windows(),
            &browser,
            lock_view_site,
            switch,
            switch_toggle,
        ),
        BrowserSettingsBranch::FirefoxDesktop => firefox_desktop_browser_steps(
            cx,
            platform.windows(),
            &browser,
            &root,
            switch,
            switch_toggle,
        ),
        BrowserSettingsBranch::ChromeDesktop => chrome_desktop_browser_steps(
            cx,
            platform.windows(),
            &browser,
            sliders,
            switch,
            switch_toggle,
        ),
        BrowserSettingsBranch::ChromeAndroid => vec![view! {
            cx =>
            <ol>
                <li>"Tap the "<em><img alt="More options" width="16" height="16" src=(dots) /></em>" menu button."</li>
                <li>"Tap "<em>"Settings"</em>"."</li>
                <li>"Tap "<em>"Notifications"</em>"."</li>
                <li>"Tap "<em><img alt="the switch" width="22" height="22" src=(switch) /></em>" to "<em>"Allow "(browser_chrome_android)" notifications"</em>"."</li>
                <li>"Tap "<em><img alt="the switch" width="22" height="22" src=(switch_toggle) /></em>" next to "<em>"Web apps"</em>"."</li>
                <li>"Tap "<em><img alt="the notification bell" width="16" height="16" src=(bell) /></em>" and select "<em>"Allow"</em>"."</li>
            </ol>
        }
        .boxed()],
        BrowserSettingsBranch::SafariDesktop => vec![view! {
            cx =>
            <ol>
                <li>"Click "<em>(browser_safari)</em>" in the top left."</li>
                <li>"Click "<em>"Settings…"</em>"."</li>
                <li>"Click the "<em>"Websites"</em>" tab."</li>
                <li>"Click "<em>"Notifications"</em>" in the sidebar."</li>
                <li>"Click "<em>(root)</em>" in the list."</li>
                <li>"Select "<em>"Allow"</em>"."</li>
            </ol>
        }
        .boxed()],
        BrowserSettingsBranch::Other => vec![view! {
            cx =>
            <p>"Ensure notifications are enabled for "<em>(root)</em>" in your web browser settings."</p>
        }
        .boxed()],
    };
    let steps: Vec<Slot> = segments.into_iter().map(Slot::new).collect();
    Some(
        view! {
            cx =>
            <details class="notifications-help">
                <summary class="btn">
                    <img aria-hidden="true" width="20" height="20" src=(web_icon) />
                    <strong>"Check your "(browser)" settings"</strong>
                    <img aria-hidden="true" width="10" height="10" class="disclosure" src=(disclosure_icon) />
                </summary>
                for step in steps {
                    (step)
                }
            </details>
        }
        .boxed(),
    )
}

/// Shared "turn on notifications for the browser app" steps (Edge,
/// Firefox, and Chrome desktop render the same two variants, except
/// Firefox's Windows switch alt reads "the toggle button").
fn browser_app_steps_view(
    cx: &Cx,
    windows: bool,
    browser: &str,
    windows_switch_alt: &str,
    switch: topcoat::asset::Asset,
    switch_toggle: topcoat::asset::Asset,
) -> BoxView<'static> {
    let browser = browser.to_string();
    let alt = windows_switch_alt.to_string();
    if windows {
        view! {
            cx =>
            <ol>
                <li>"Click "<em>"Start"</em>", then "<em>"Settings"</em>"."</li>
                <li>"Go to "<em>"System > Notification"</em>"."</li>
                <li>"Click "<em><img alt=(alt) width="22" height="22" src=(switch) /></em>" "<em>"ON"</em>" for "(browser)"."</li>
            </ol>
        }
        .boxed()
    } else {
        view! {
            cx =>
            <ol>
                <li>"Click "<em aria-label="the Apple menu">""</em>" in the top left."</li>
                <li>"Click "<em>"System Settings…"</em>"."</li>
                <li>"Click "<em>"Notifications"</em>"."</li>
                <li>"Click "<em>(browser)</em>"."</li>
                <li>"Click "<em><img alt="the switch" width="22" height="22" src=(switch_toggle) /></em>" to "<em>"Allow notifications"</em>"."</li>
            </ol>
        }
        .boxed()
    }
}

fn website_heading_view(cx: &Cx) -> BoxView<'static> {
    view! {
        cx =>
        <h2 class="txt-normal txt-medium margin-block-start">"Turn on notifications for this website."</h2>
    }
    .boxed()
}

fn browser_heading_view(cx: &Cx, browser: &str) -> BoxView<'static> {
    let browser = browser.to_string();
    view! {
        cx =>
        <h2 class="txt-normal txt-medium margin-block-start">"Turn on notifications for "(browser)"."</h2>
    }
    .boxed()
}

fn edge_desktop_browser_steps(
    cx: &Cx,
    windows: bool,
    browser: &str,
    lock: topcoat::asset::Asset,
    switch: topcoat::asset::Asset,
    switch_toggle: topcoat::asset::Asset,
) -> Vec<BoxView<'static>> {
    let site_steps = view! {
        cx =>
        <ol>
            <li>"Click "<em><img alt="the View site information button" width="20" height="20" src=(lock) /></em>" left of the address bar."</li>
            <li>"Under "<em>"Permissions for this site > Notifications"</em>", choose "<em>"Allow"</em>"."</li>
        </ol>
    }
    .boxed();
    vec![
        website_heading_view(cx),
        site_steps,
        browser_heading_view(cx, browser),
        browser_app_steps_view(cx, windows, browser, "the switch", switch, switch_toggle),
    ]
}

fn firefox_desktop_browser_steps(
    cx: &Cx,
    windows: bool,
    browser: &str,
    root_url: &str,
    switch: topcoat::asset::Asset,
    switch_toggle: topcoat::asset::Asset,
) -> Vec<BoxView<'static>> {
    let browser_name = browser.to_string();
    let root = root_url.to_string();
    let site_steps = view! {
        cx =>
        <ol>
            <li>"Click "<em>(browser_name)</em>" in the top left."</li>
            <li>"Click "<em>"Settings…"</em>"."</li>
            <li>"Click "<em>"Privacy & Security"</em>" in the sidebar."</li>
            <li>"Scroll down to "<em>"Permissions"</em>"."</li>
            <li>"Click "<em>"Settings"</em>" next to "<em>"Notifications"</em>"."</li>
            <li>"Select "<em>"Allow"</em>" next to "<em>(root)</em>"."</li>
        </ol>
    }
    .boxed();
    vec![
        website_heading_view(cx),
        site_steps,
        browser_heading_view(cx, browser),
        browser_app_steps_view(
            cx,
            windows,
            browser,
            "the toggle button",
            switch,
            switch_toggle,
        ),
    ]
}

fn chrome_desktop_browser_steps(
    cx: &Cx,
    windows: bool,
    browser: &str,
    sliders: topcoat::asset::Asset,
    switch: topcoat::asset::Asset,
    switch_toggle: topcoat::asset::Asset,
) -> Vec<BoxView<'static>> {
    let site_steps = view! {
        cx =>
        <ol>
            <li>"Click the "<em><img alt="View site information" width="20" height="20" src=(sliders) /></em>" icon in the address bar."</li>
            <li>"Click "<em>"Site Settings"</em>"."</li>
            <li>"Ensure notifications are "<em>"Allowed"</em>"."</li>
        </ol>
    }
    .boxed();
    vec![
        website_heading_view(cx),
        site_steps,
        browser_heading_view(cx, browser),
        browser_app_steps_view(cx, windows, browser, "the switch", switch, switch_toggle),
    ]
}

/// `pwa/_system_settings` branches, in the template's elif order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemSettingsBranch {
    FirefoxAndroid,
    EdgeDesktop,
    FirefoxOrChromeDesktop,
    SafariDesktop,
    SafariOrChromeIos,
    ChromeAndroid,
    Other,
}

fn system_settings_branch(platform: &ApplicationPlatform) -> SystemSettingsBranch {
    if platform.firefox() && platform.android() {
        SystemSettingsBranch::FirefoxAndroid
    } else if platform.edge() && platform.desktop() {
        SystemSettingsBranch::EdgeDesktop
    } else if (platform.firefox() || platform.chrome()) && platform.desktop() {
        SystemSettingsBranch::FirefoxOrChromeDesktop
    } else if platform.safari() && platform.desktop() {
        SystemSettingsBranch::SafariDesktop
    } else if (platform.safari() || platform.chrome()) && platform.ios() {
        SystemSettingsBranch::SafariOrChromeIos
    } else if platform.chrome() && platform.android() {
        SystemSettingsBranch::ChromeAndroid
    } else {
        SystemSettingsBranch::Other
    }
}

/// `pwa/_system_settings`: per-OS notification steps.
pub(crate) fn system_settings_view(cx: &Cx, platform: &ApplicationPlatform) -> BoxView<'static> {
    use crate::assets::*;
    let branch = system_settings_branch(platform);
    let browser = capitalize(&platform.browser());
    let system = platform.operating_system().unwrap_or_default();
    let gear_icon = img_external_gear();
    let disclosure_icon = img_disclosure();
    let dots = img_menu_dots_vertical();
    let switch = img_external_switch();
    let gear = img_external_gear();
    let steps: BoxView<'static> = match branch {
        SystemSettingsBranch::FirefoxAndroid => view! {
            cx =>
            <ol>
                <li>"Tap the "<em><img alt="More options" width="16" height="16" src=(dots) /></em>" menu button."</li>
                <li>"Tap "<em>"Settings"</em>"."</li>
                <li>"Tap "<em>"Notifications"</em>"."</li>
                <li>"Tap "<em><img alt="the toggle button" width="22" height="22" src=(switch) /></em>" to "<em>"Allow "(browser)" notifications"</em>"."</li>
            </ol>
        }
        .boxed(),
        SystemSettingsBranch::EdgeDesktop => view! {
            cx =>
            <ol>
                <li>"Click "<em>"Start"</em>", then "<em>"Settings"</em>"."</li>
                <li>"Go to "<em>"System > Notification"</em>"."</li>
                <li>"Click "<em><img alt="the toggle button" width="22" height="22" src=(switch) /></em>" "<em>"ON"</em>" for Topcamp."</li>
            </ol>
        }
        .boxed(),
        SystemSettingsBranch::FirefoxOrChromeDesktop => {
            system_topcamp_steps_view(cx, platform.windows(), switch)
        }
        SystemSettingsBranch::SafariDesktop => {
            system_topcamp_steps_view(cx, false, switch)
        }
        SystemSettingsBranch::SafariOrChromeIos => view! {
            cx =>
            <ol>
                <li>"Open the "<em><img aria-hidden="true" width="20" height="20" src=(gear) /></em>" Settings app."</li>
                <li>"Scroll to and tap "<em>"Topcamp"</em>"."</li>
                <li>"Tap "<em>"Notifications"</em>"."</li>
                <li>"Tap "<em><img alt="the allow notifications switch button" width="22" height="22" src=(switch) /></em>" to "<em>"Allow Notifications"</em>"."</li>
            </ol>
        }
        .boxed(),
        SystemSettingsBranch::ChromeAndroid => view! {
            cx =>
            <ol>
                <li>"Open the "<em><img aria-hidden="true" width="20" height="20" src=(gear) /></em>" Settings app."</li>
                <li>"Tap "<em>"Notifications"</em>"."</li>
                <li>"Tap "<em>"App notifications"</em>"."</li>
                <li>"Scroll to "<em>"Topcamp"</em>"."</li>
                <li>"Tap "<em><img alt="the switch" width="22" height="22" src=(switch) /></em>" to "<em>"Allow Notifications"</em>"."</li>
            </ol>
        }
        .boxed(),
        SystemSettingsBranch::Other => view! {
            cx =>
            <p>"Ensure notifications are allowed for "(browser)" in your system settings."</p>
        }
        .boxed(),
    };
    view! {
        cx =>
        <details class="notifications-help hide-in-browser">
            <summary class="btn">
                <img aria-hidden="true" width="20" height="20" src=(gear_icon) />
                <strong>"Check your "(system)" settings"</strong>
                <img aria-hidden="true" width="10" height="10" class="disclosure" src=(disclosure_icon) />
            </summary>
            (steps)
        </details>
    }
    .boxed()
}

/// Shared "allow Topcamp in the OS" steps (Firefox/Chrome desktop
/// branch on Windows; Safari desktop is always the macOS list).
fn system_topcamp_steps_view(
    cx: &Cx,
    windows: bool,
    switch: topcoat::asset::Asset,
) -> BoxView<'static> {
    if windows {
        view! {
            cx =>
            <ol>
                <li>"Click "<em>"Start"</em>", then "<em>"Settings"</em>"."</li>
                <li>"Go to "<em>"System > Notification"</em>"."</li>
                <li>"Click "<em><img alt="the toggle button" width="22" height="22" src=(switch) /></em>" "<em>"ON"</em>" for Topcamp."</li>
            </ol>
        }
        .boxed()
    } else {
        view! {
            cx =>
            <ol>
                <li>"Click "<em aria-label="the Apple menu">""</em>" in the top left."</li>
                <li>"Click "<em>"System Settings…"</em>"."</li>
                <li>"Click "<em>"Notifications"</em>"."</li>
                <li>"Click "<em>"Topcamp"</em>"."</li>
                <li>"Click "<em><img alt="the allow notifications switch" width="22" height="22" src=(switch) /></em>" to "<em>"Allow notifications"</em>"."</li>
            </ol>
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_matches_upstream_shape() {
        let body = render_manifest(
            "37signals",
            "/account/logo?size=small&v=20260926130020",
            "/account/logo?v=20260926130020",
            "http://topcamp.test/assets/add-f232d8a6.svg",
            "http://topcamp.test/assets/person-da193438.svg",
            "http://topcamp.test/assets/screenshots/android-chat-f8b923c9.png",
            "http://topcamp.test/assets/screenshots/android-sidebar-e9d2b49f.png",
            "http://topcamp.test/assets/screenshots/android-dark-mode-e43dcf59.png",
        );
        let golden = include_str!("testdata/manifest.json").replace("&amp;", "&");
        assert_eq!(body, golden);
        // Valid JSON whatever the account is called.
        let tricky = render_manifest(
            "Back\\slash \"quoted\" <b>&amp;</b>",
            "/account/logo?size=small&v=1",
            "/account/logo?v=1",
            "http://topcamp.test/assets/add.svg",
            "http://topcamp.test/assets/person.svg",
            "http://topcamp.test/assets/1.png",
            "http://topcamp.test/assets/2.png",
            "http://topcamp.test/assets/3.png",
        );
        let parsed: serde_json::Value = serde_json::from_str(&tricky).expect("valid JSON");
        assert_eq!(parsed["name"], "Back\\slash \"quoted\" <b>&amp;</b>");
        assert_eq!(parsed["icons"][0]["src"], "/account/logo?size=small&v=1");
    }

    #[test]
    fn logo_paths_join() {
        assert_eq!(
            fresh_account_logo(Some("small"), Some("7")),
            "/account/logo?size=small&v=7"
        );
        assert_eq!(fresh_account_logo(None, Some("7")), "/account/logo?v=7");
        assert_eq!(fresh_account_logo(None, None), "/account/logo");
    }

    #[test]
    fn install_branches_match_upstream() {
        let branch = |ua: Option<&str>| {
            install_branch(&ApplicationPlatform::new(ua)).map(|b| format!("{b:?}"))
        };
        // (UA, branch) pairs from upstream's golden fixtures.
        let chrome_mac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
        let chrome_android = "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Mobile Safari/537.36";
        let firefox_mac =
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.5; rv:131.0) Gecko/20100101 Firefox/131.0";
        let firefox_android =
            "Mozilla/5.0 (Android 14; Mobile; rv:131.0) Gecko/131.0 Firefox/131.0";
        let edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0";
        let legacy_edge = "Mozilla/5.0 (Windows NT 10.0) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/46.0.2486.0 Safari/537.36 Edge/13.10586";
        let safari_mac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15";
        let safari_ios = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";
        for ua in [chrome_mac, chrome_android, firefox_mac, edge] {
            assert_eq!(branch(Some(ua)), None, "{ua}");
        }
        assert_eq!(branch(Some(legacy_edge)).as_deref(), Some("Edge"));
        assert_eq!(
            branch(Some(firefox_android)).as_deref(),
            Some("FirefoxAndroid")
        );
        assert_eq!(branch(Some(safari_mac)).as_deref(), Some("SafariDesktop"));
        assert_eq!(branch(Some(safari_ios)).as_deref(), Some("Ios"));
        assert_eq!(branch(Some("curl/8.4.0")).as_deref(), Some("Other"));
        assert_eq!(branch(None).as_deref(), Some("Other"));
    }

    #[test]
    fn settings_branches_match_upstream() {
        let chrome_mac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
        let chrome_android = "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Mobile Safari/537.36";
        let firefox_mac =
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.5; rv:131.0) Gecko/20100101 Firefox/131.0";
        let firefox_android =
            "Mozilla/5.0 (Android 14; Mobile; rv:131.0) Gecko/131.0 Firefox/131.0";
        let edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0";
        let safari_mac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15";
        let safari_ios = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";
        let legacy_edge = "Mozilla/5.0 (Windows NT 10.0) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/46.0.2486.0 Safari/537.36 Edge/13.10586";
        let branches = |ua: Option<&str>| {
            let platform = ApplicationPlatform::new(ua);
            (
                browser_settings_branch(&platform).map(|b| format!("{b:?}")),
                format!("{:?}", system_settings_branch(&platform)),
            )
        };
        assert_eq!(
            branches(Some(chrome_mac)),
            (
                Some("ChromeDesktop".to_string()),
                "FirefoxOrChromeDesktop".to_string()
            )
        );
        assert_eq!(
            branches(Some(chrome_android)),
            (
                Some("ChromeAndroid".to_string()),
                "ChromeAndroid".to_string()
            )
        );
        assert_eq!(
            branches(Some(firefox_mac)),
            (
                Some("FirefoxDesktop".to_string()),
                "FirefoxOrChromeDesktop".to_string()
            )
        );
        assert_eq!(
            branches(Some(firefox_android)),
            (
                Some("FirefoxAndroid".to_string()),
                "FirefoxAndroid".to_string()
            )
        );
        // Modern `Edg/` UAs read as Chrome (like the gem); only
        // legacy `Edge/` UAs take the Edge branches.
        assert_eq!(
            branches(Some(edge)),
            (
                Some("ChromeDesktop".to_string()),
                "FirefoxOrChromeDesktop".to_string()
            )
        );
        assert_eq!(
            branches(Some(legacy_edge)),
            (Some("EdgeDesktop".to_string()), "EdgeDesktop".to_string())
        );
        assert_eq!(
            branches(Some(safari_mac)),
            (
                Some("SafariDesktop".to_string()),
                "SafariDesktop".to_string()
            )
        );
        // iOS Safari/Chrome have no browser-side switch.
        assert_eq!(
            branches(Some(safari_ios)),
            (None, "SafariOrChromeIos".to_string())
        );
        assert_eq!(
            branches(Some("curl/8.4.0")),
            (Some("Other".to_string()), "Other".to_string())
        );
        assert_eq!(
            branches(None),
            (Some("Other".to_string()), "Other".to_string())
        );
    }
}
