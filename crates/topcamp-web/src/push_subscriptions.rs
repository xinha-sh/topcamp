//! `Users::PushSubscriptionsController` + `TestNotificationsController`
//! (`/users/me/push_subscriptions`): the "Push Notifications Dev
//! Mode" index, the JSON/form create the notifications bell syncs
//! to, per-subscription destroy, and test delivery. Sign-out removes
//! the session's endpoint (`auth::destroy`).
//!
//! Endpoint validation mirrors `Push::Subscription`: presence,
//! HTTPS on 443, the five permitted vendor hosts (subdomains
//! included), and DNS resolution to a public address. Tests stub
//! resolution with `TOPCAMP_TEST_PUSH_DNS` (debug builds only),
//! like upstream's `stub_web_push_dns_resolution`.

use std::net::IpAddr;

use serde::Deserialize;
use topcoat::{
    Result,
    context::{Cx, app_context},
    cookie::{Cookies, cookies},
    router::{
        Body, Slot,
        content::Form,
        error::{bad_request, forbidden, not_found, see_other},
        path_param_segment, request,
        response::{AsyncIntoResponse, IntoResponse, Response},
        route, to_bytes,
    },
    view::{BoxView, ViewExt as _, view},
};

use topcamp_db::repositories::{
    AccountRepository, PushSubscriptionRepository, PushSubscriptionRow,
};

use crate::pages::{ShellContext, body_classes, document_shell};
use crate::state::{AppState, http_error};

/// `Push::Subscription::PERMITTED_ENDPOINT_HOSTS`.
const PERMITTED_ENDPOINT_HOSTS: [&str; 5] = [
    "jmt17.google.com",
    "fcm.googleapis.com",
    "updates.push.services.mozilla.com",
    "web.push.apple.com",
    "notify.windows.com",
];

/// `VAPID_PUBLIC_KEY` (`vapid.rb` reads the same env first). `None`
/// renders the meta tag without content, like upstream's nil key.
pub(crate) fn vapid_public_key() -> Option<String> {
    std::env::var("VAPID_PUBLIC_KEY")
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

/// Delivery needs both halves; without them the test send (like the
/// `send_notification` workflow) is skipped.
fn vapid_configured() -> bool {
    vapid_public_key().is_some()
        && std::env::var("VAPID_PRIVATE_KEY")
            .ok()
            .is_some_and(|key| !key.trim().is_empty())
}

/// Shape validation (`validate_endpoint_url` minus DNS): presence is
/// the caller's `Option`, here the URL must parse, use HTTPS on the
/// default port, and belong to a permitted push service. Returns the
/// lowercase host for the resolution check.
fn endpoint_host(endpoint: &str) -> Result<String, &'static str> {
    let url = url::Url::parse(endpoint).map_err(|_| "is not a valid URL")?;
    if url.scheme() != "https" {
        return Err("must use HTTPS");
    }
    if url.port_or_known_default() != Some(443) {
        return Err("must use the default HTTPS port");
    }
    let host = url.host_str().unwrap_or("").to_lowercase();
    let permitted = PERMITTED_ENDPOINT_HOSTS
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")));
    if !permitted {
        return Err("is not a permitted push service");
    }
    Ok(host)
}

/// Test seam for resolution (`TOPCAMP_TEST_PUSH_DNS` as
/// `host=ip,host=ip`), debug builds only. `None` means real DNS.
fn stubbed_ips(host: &str) -> Option<Vec<IpAddr>> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let mapping = std::env::var("TOPCAMP_TEST_PUSH_DNS").ok()?;
    let mut ips = Vec::new();
    for pair in mapping.split(',') {
        let (name, ip) = pair.split_once('=')?;
        if name.trim().eq_ignore_ascii_case(host)
            && let Ok(ip) = ip.trim().parse::<IpAddr>()
        {
            ips.push(ip);
        }
    }
    (!ips.is_empty()).then_some(ips)
}

/// `resolved_endpoint_ip`: the host resolves and at least one answer
/// is a public address. Classification reuses the unfurl guard's
/// `blocked_address` (private/loopback/link-local resolutions fail
/// the subscription, like the guard's `Violation`).
async fn resolves_public(host: &str) -> bool {
    use crate::unfurl::blocked_address;
    if let Some(ips) = stubbed_ips(host) {
        return ips.iter().any(|ip| !blocked_address(*ip));
    }
    // A numeric host never reaches DNS (classified directly, like the
    // unfurl guard); a permitted vendor host is never numeric.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return !blocked_address(ip);
    }
    let Ok(resolved) = tokio::net::lookup_host((host, 443)).await else {
        return false;
    };
    resolved.into_iter().any(|addr| !blocked_address(addr.ip()))
}

/// Full `valid?` for an endpoint: shape + public resolution.
async fn subscription_valid(endpoint: &str) -> bool {
    let Ok(host) = endpoint_host(endpoint) else {
        return false;
    };
    resolves_public(&host).await
}

fn ok_head(cx: &Cx) -> Result<Response> {
    Response::builder()
        .status(200)
        .body(Body::empty())
        .expect("200 builds")
        .into_response(cx)
}

fn unprocessable_head(cx: &Cx) -> Result<Response> {
    Response::builder()
        .status(422)
        .body(Body::empty())
        .expect("422 builds")
        .into_response(cx)
}

/// Parsed `push_subscription` params (`present` mirrors
/// `params.require`: any `push_subscription[*]` key counts).
#[derive(Default)]
struct SubscriptionForm {
    present: bool,
    endpoint: Option<String>,
    p256dh_key: Option<String>,
    auth_key: Option<String>,
    authenticity_token: Option<String>,
}

fn parse_subscription_json(value: &serde_json::Value) -> SubscriptionForm {
    let mut form = SubscriptionForm::default();
    let Some(nested) = value.get("push_subscription") else {
        return form;
    };
    form.present = true;
    let text = |key: &str| {
        nested
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    form.endpoint = text("endpoint");
    form.p256dh_key = text("p256dh_key");
    form.auth_key = text("auth_key");
    form
}

fn parse_subscription_form(raw: &[u8]) -> SubscriptionForm {
    let mut form = SubscriptionForm::default();
    let Ok(pairs) = serde_urlencoded::from_bytes::<Vec<(String, String)>>(raw) else {
        return form;
    };
    for (key, value) in pairs {
        match key.as_str() {
            "push_subscription[endpoint]" => {
                form.present = true;
                form.endpoint = Some(value);
            }
            "push_subscription[p256dh_key]" => {
                form.present = true;
                form.p256dh_key = Some(value);
            }
            "push_subscription[auth_key]" => {
                form.present = true;
                form.auth_key = Some(value);
            }
            "authenticity_token" => form.authenticity_token = Some(value),
            _ => {}
        }
    }
    form
}

/// `POST /users/me/push_subscriptions`: find-or-create by the full
/// params. An existing row must still validate (legacy sinks stay
/// dead); otherwise touch. Fresh rows persist only when valid.
/// 200/422 heads either way, like upstream.
async fn create_subscription(cx: &Cx, body: Body) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let headers = request::headers(cx);
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let header_token = headers
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let raw = to_bytes(body, 64 * 1024).await?;
    let (form, token) = if content_type.starts_with("application/json") {
        let parsed: serde_json::Value = serde_json::from_slice(&raw).unwrap_or_default();
        (parse_subscription_json(&parsed), header_token)
    } else {
        let form = parse_subscription_form(&raw);
        let token = form.authenticity_token.clone().unwrap_or(header_token);
        (form, token)
    };
    if !crate::csrf::verify(cx, &token) {
        return Err(forbidden().into());
    }
    if !form.present {
        return Err(
            bad_request("param is missing or the value is empty: push_subscription").into(),
        );
    }
    let db = &app_context::<AppState>(cx).db;
    // `validates :endpoint, presence: true`: blank is 422, never stored.
    let Some(endpoint) = form.endpoint.filter(|value| !value.trim().is_empty()) else {
        return unprocessable_head(cx);
    };
    let existing = PushSubscriptionRepository::find_push_subscription_by_params(
        db,
        user.id,
        &endpoint,
        form.p256dh_key.as_deref(),
        form.auth_key.as_deref(),
    )
    .await
    .map_err(http_error)?;
    if let Some(row) = existing {
        // Existing endpoints must pass current validations.
        if !subscription_valid(&endpoint).await {
            return unprocessable_head(cx);
        }
        PushSubscriptionRepository::touch_push_subscription(db, row.id)
            .await
            .map_err(http_error)?;
        return ok_head(cx);
    }
    if !subscription_valid(&endpoint).await {
        return unprocessable_head(cx);
    }
    let user_agent = headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok());
    PushSubscriptionRepository::create_push_subscription(
        db,
        user.id,
        &endpoint,
        form.p256dh_key.as_deref(),
        form.auth_key.as_deref(),
        user_agent,
    )
    .await
    .map_err(http_error)?;
    ok_head(cx)
}

/// `POST /users/me/push_subscriptions` (the bell syncs JSON here).
#[route(POST "/users/me/push_subscriptions")]
pub async fn create(cx: &Cx, body: Body) -> Result<Response> {
    create_subscription(cx, body).await
}

/// `button_to ..., method: :delete` posts this form; raw DELETEs
/// carry no body, so everything is optional and the token check
/// fails closed.
#[derive(Debug, Deserialize, Default)]
struct DestroyForm {
    #[serde(default)]
    authenticity_token: String,
    #[serde(rename = "_method", default)]
    method_override: Option<String>,
}

/// `DELETE /users/me/push_subscriptions/:id`: user-scoped
/// `destroy_by`, then back to the index (missing rows redirect all
/// the same, like upstream).
#[route([POST, DELETE] "/users/me/push_subscriptions/{id}")]
pub async fn destroy(cx: &Cx, Form(input): Form<DestroyForm>) -> Result<Response> {
    if request::method(cx) == http::Method::POST
        && input.method_override.as_deref() != Some("delete")
    {
        return Err(not_found().into());
    }
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !crate::csrf::verify(cx, &input.authenticity_token) {
        return Err(forbidden().into());
    }
    if let Some(id) = crate::rooms::cast_integer(path_param_segment(cx, "id")) {
        let db = &app_context::<AppState>(cx).db;
        PushSubscriptionRepository::destroy_push_subscription(db, user.id, id)
            .await
            .map_err(http_error)?;
    }
    see_other("/users/me/push_subscriptions").into_response(cx)
}

#[derive(Debug, Deserialize, Default)]
struct TestForm {
    #[serde(default)]
    authenticity_token: String,
}

/// Best-effort test delivery (`notification(...).deliver`), skipped
/// without VAPID keys. Plaintext like the `send_notification`
/// workflow's stub transport (documented there); the outcome never
/// blocks the redirect.
async fn deliver_test(subscription: &PushSubscriptionRow) {
    if !vapid_configured() {
        return;
    }
    let Some(endpoint) = subscription.endpoint.as_deref().filter(|e| !e.is_empty()) else {
        return;
    };
    let body = serde_json::json!({
        "title": "Topcamp Test",
        "options": {
            "body": format!("test-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default()),
            "icon": "/account/logo",
            "data": { "path": "/users/me/push_subscriptions" },
        },
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let Ok(client) = client else { return };
    let Ok(payload) = serde_json::to_vec(&body) else {
        return;
    };
    match client
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(payload)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {
            tracing::info!(subscription_id = subscription.id, "push test delivered");
        }
        Ok(response) => {
            tracing::warn!(
                subscription_id = subscription.id,
                status = response.status().as_u16(),
                "push test rejected"
            );
        }
        Err(err) => {
            tracing::warn!(
                subscription_id = subscription.id,
                error = err.to_string(),
                "push test failed"
            );
        }
    }
}

/// `POST /users/me/push_subscriptions/:id/test_notifications`:
/// `find` (user-scoped; missing or foreign is the 404 page), deliver,
/// back to the index.
#[route(POST "/users/me/push_subscriptions/{push_subscription_id}/test_notifications")]
pub async fn create_test(cx: &Cx, Form(input): Form<TestForm>) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !crate::csrf::verify(cx, &input.authenticity_token) {
        return Err(forbidden().into());
    }
    let db = &app_context::<AppState>(cx).db;
    let subscription =
        match crate::rooms::cast_integer(path_param_segment(cx, "push_subscription_id")) {
            Some(id) => PushSubscriptionRepository::find_push_subscription(db, user.id, id)
                .await
                .map_err(http_error)?,
            None => None,
        };
    let Some(subscription) = subscription else {
        return crate::not_found::response(cx);
    };
    deliver_test(&subscription).await;
    see_other("/users/me/push_subscriptions").into_response(cx)
}

// --- views -----------------------------------------------------------

/// `link_back_to_last_room_visited`: the remembered room, else root.
fn back_nav(cx: &Cx) -> BoxView<'static> {
    use crate::assets::*;
    let href = cookies(cx)
        .get("last_room")
        .and_then(|cookie| crate::rooms::cast_integer(cookie.value()))
        .map(|id| format!("/rooms/{id}"))
        .unwrap_or_else(|| "/".to_string());
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

/// `users/push_subscriptions/_push_subscription`: the UA line, the
/// endpoint, and the test/delete buttons.
fn subscription_row_view(
    cx: &Cx,
    subscription: &PushSubscriptionRow,
    csrf_token: &str,
) -> BoxView<'static> {
    use crate::assets::*;
    let agent = crate::user_agent::parse(subscription.user_agent.as_deref().unwrap_or(""));
    let agent_line = format!(
        "{} {} on {}",
        agent.browser(),
        agent.version().as_str(),
        agent.platform().unwrap_or_default()
    );
    let endpoint = subscription.endpoint.clone().unwrap_or_default();
    let test_action = format!(
        "/users/me/push_subscriptions/{}/test_notifications",
        subscription.id
    );
    let delete_action = format!("/users/me/push_subscriptions/{}", subscription.id);
    let csrf = csrf_token.to_string();
    let csrf_delete = csrf_token.to_string();
    view! {
        cx =>
        <li class="flex flex-column margin-none membership-item">
            <span class="overflow-ellipsis txt-primary txt-undecorated">
                <strong>(agent_line)</strong><br />
            </span>
            <span class="flex align-start gap txt-small">
                <span>(endpoint)</span>
                <span class="flex align-center gap">
                    <form class="button_to" method="post" action=(test_action)>
                        <input type="hidden" name="authenticity_token" value=(csrf) />
                        <button class="btn btn--reversed" type="submit">
                            <img aria-hidden="true" width="20" height="20" src=(img_notification_bell_everything()) />
                            <span class="for-screen-reader">"Send test notification"</span>
                        </button>
                    </form>
                    <form class="button_to" method="post" action=(delete_action)>
                        <input type="hidden" name="_method" value="delete" />
                        <input type="hidden" name="authenticity_token" value=(csrf_delete) />
                        <button class="btn btn--negative" type="submit">
                            <img aria-hidden="true" width="20" height="20" src=(img_minus()) />
                            <span class="for-screen-reader">"Delete subscription"</span>
                        </button>
                    </form>
                </span>
            </span>
        </li>
    }
    .boxed()
}

/// `users/push_subscriptions/index`: the dev-mode panel.
fn index_view(cx: &Cx, rows: Vec<BoxView<'static>>) -> BoxView<'static> {
    let items: Vec<Slot> = rows.into_iter().map(Slot::new).collect();
    view! {
        cx =>
        <section class="panel panel--wide flex flex-column gap">
            <h1 class="txt-align-center txt-large margin-none">"Push Notification Subscriptions"</h1>
            <div class="pad-inline fill-shade border-radius" id="push_subscriptions">
                <menu class="pad flex flex-column gap">
                    for item in items {
                        (item)
                    }
                </menu>
            </div>
        </section>
    }
    .boxed()
}

/// `GET /users/me/push_subscriptions`.
#[route(GET "/users/me/push_subscriptions")]
pub async fn index(cx: &Cx) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let subscriptions = PushSubscriptionRepository::push_subscriptions_for_user(db, user.id)
        .await
        .map_err(http_error)?;
    let csrf_token = crate::csrf::issue(cx);
    let rows = subscriptions
        .iter()
        .map(|subscription| subscription_row_view(cx, subscription, &csrf_token))
        .collect();
    let content = index_view(cx, rows);
    let admin = user.role == topcamp_domain::auth::UserRole::Administrator.value();
    let shell = ShellContext {
        current_user: Some((user.id, user.name.clone())),
        logo_version: AccountRepository::first(db)
            .await
            .map_err(http_error)?
            .map(|account| account.updated_number),
    };
    document_shell(
        cx,
        "Push notification subscriptions".to_string(),
        body_classes("", admin),
        Slot::new(content),
        crate::flash::Flash::default(),
        shell,
        None,
        Some(Slot::new(back_nav(cx))),
        None,
        Some(Slot::new(crate::accounts::footer_view(cx))),
    )
    .boxed()
    .async_into_response(cx)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_shape_matches_upstream() {
        assert_eq!(
            endpoint_host("https://fcm.googleapis.com/fcm/send/x").as_deref(),
            Ok("fcm.googleapis.com")
        );
        // Subdomains of a permitted host pass.
        assert_eq!(
            endpoint_host("https://sub.fcm.googleapis.com/x").as_deref(),
            Ok("sub.fcm.googleapis.com")
        );
        // Hosts match case-insensitively.
        assert_eq!(
            endpoint_host("https://FCM.GOOGLEAPIS.COM/x").as_deref(),
            Ok("fcm.googleapis.com")
        );
        // An explicit default port is still the default port.
        assert!(endpoint_host("https://fcm.googleapis.com:443/x").is_ok());
        assert_eq!(
            endpoint_host("http://fcm.googleapis.com/x").unwrap_err(),
            "must use HTTPS"
        );
        assert_eq!(
            endpoint_host("https://fcm.googleapis.com:8443/x").unwrap_err(),
            "must use the default HTTPS port"
        );
        assert_eq!(
            endpoint_host("https://attacker.example.com/steal").unwrap_err(),
            "is not a permitted push service"
        );
        // Suffix tricks fail: the host must equal the vendor or end
        // with a dotted subdomain of it.
        assert_eq!(
            endpoint_host("https://fcm.googleapis.com.evil.com/x").unwrap_err(),
            "is not a permitted push service"
        );
        assert_eq!(
            endpoint_host("https://notfcm.googleapis.com/x").unwrap_err(),
            "is not a permitted push service"
        );
        assert_eq!(
            endpoint_host("not a url").unwrap_err(),
            "is not a valid URL"
        );
    }
}
