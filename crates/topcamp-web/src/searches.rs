//! `SearchesController`: the search page (`GET /searches`),
//! recording (`POST /searches`), and recents clearing
//! (`DELETE /searches/clear`, dispatched from POST by browsers).
//!
//! The query sanitizer (`[^[:word:]]` → space) and the newest-100
//! chronological hits mirror upstream; the page reuses the room
//! message rows with threading off (upstream's search-results
//! formatter uses `ThreadStyle.none`).

use std::collections::HashMap;

use topcoat::{
    Result,
    context::{Cx, app_context},
    cookie::{Cookies as _, cookies},
    router::{
        Body, Slot,
        error::{forbidden, not_found, see_other},
        query_params, request,
        response::{AsyncIntoResponse, IntoResponse, Response},
        route, to_bytes,
    },
    view::{ViewExt as _, view},
};

use topcamp_db::repositories::{
    MembershipRepository, MessageRepository, RoomRepository, SearchRepository, UserRepository,
};
use topcamp_domain::search::sanitize_query;

use crate::state::{AppState, http_error};

#[query_params(error = bad_request)]
struct SearchesQuery {
    q: Option<String>,
}

/// `searches_url(q:)`: urlencoded query pair, for recents links and
/// the post-search redirect.
fn searches_url(query: &str) -> String {
    let pair = serde_urlencoded::to_string([("q", query)]).unwrap_or_default();
    format!("/searches?{pair}")
}

/// Exit-search target: the remembered room, else the root.
fn exit_href(cx: &Cx) -> String {
    cookies(cx)
        .get("last_room")
        .and_then(|cookie| crate::rooms::cast_integer(cookie.value()))
        .map(|id| format!("/rooms/{id}"))
        .unwrap_or_else(|| "/".to_string())
}

async fn page_shell(
    db: &topcamp_db::PgDb,
    user_id: i64,
    name: &str,
) -> Result<crate::pages::ShellContext> {
    use topcamp_db::repositories::AccountRepository;
    Ok(crate::pages::ShellContext {
        current_user: Some((user_id, name.to_string())),
        logo_version: AccountRepository::first(db)
            .await
            .map_err(http_error)?
            .map(|account| account.updated_number),
    })
}

/// One recent-search link (`"query"`), shared by nav + sidebar.
fn recent_link(cx: &Cx, query: &str) -> topcoat::view::BoxView<'static> {
    let href = searches_url(query);
    let label = format!("\u{201c}{query}\u{201d}");
    view! {
        cx =>
        <a href=(href) class="align-center gap room btn txt-nowrap">
            <span class="overflow-ellipsis">(label)</span>
        </a>
    }
    .boxed()
}

/// The clear-recents broom (`button_to`, DELETE via POST).
fn clear_button(cx: &Cx, csrf_token: &str) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let csrf = csrf_token.to_string();
    view! {
        cx =>
        <form class="button_to" action="/searches/clear" accept-charset="UTF-8" method="post" data-confirm="Are you sure you want to clear your recent searches?">
            <input type="hidden" name="_method" value="delete" />
            <input type="hidden" name="authenticity_token" value=(csrf) />
            <button class="btn searches__btn" type="submit">
                <img aria-hidden="true" src=(img_broom()) />
                <span class="for-screen-reader">"Clear recent searches"</span>
            </button>
        </form>
    }
    .boxed()
}

/// `content_for :nav`: the query chip (when searching) + recents.
fn nav_view(
    cx: &Cx,
    query: Option<&str>,
    hits: usize,
    recents: &[topcamp_db::repositories::SearchRow],
    csrf_token: &str,
) -> topcoat::view::BoxView<'static> {
    let chip = query.map(|text| (format!("\u{201c}{text}\u{201d}"), hits.to_string()));
    let mut links: Vec<Slot> = recents
        .iter()
        .map(|search| Slot::new(recent_link(cx, &search.query)))
        .collect();
    if !recents.is_empty() {
        links.push(Slot::new(clear_button(cx, csrf_token)));
    }
    view! {
        cx =>
        if let Some((label, count)) = chip {
            <div class="searches__query flex align-center gap pad-block-start-half">
                <div class="btn btn--reversed btn--faux align-center gap txt-nowrap">
                    <span class="overflow-ellipsis">(label)</span>
                    <span class="flex-item-no-shrink">(count)</span>
                </div>
            </div>
        }
        <div class="searches__recents align-center gap pad-block-half overflow-y overflow-hide-scrollbar">
            for link in links {
                (link)
            }
        </div>
    }
    .boxed()
}

/// `content_for :sidebar`: recents take the room list's place.
fn sidebar_view(
    cx: &Cx,
    recents: &[topcamp_db::repositories::SearchRow],
    csrf_token: &str,
) -> topcoat::view::BoxView<'static> {
    let mut links: Vec<Slot> = recents
        .iter()
        .map(|search| Slot::new(recent_link(cx, &search.query)))
        .collect();
    if !recents.is_empty() {
        links.push(Slot::new(clear_button(cx, csrf_token)));
    }
    view! {
        cx =>
        <div class="rooms position-relative flex flex-column gap overflow-y overflow-hide-scrollbar">
            for link in links {
                (link)
            }
        </div>
    }
    .boxed()
}

/// `content_for :footer`: exit-search + the search composer.
fn footer_view(
    cx: &Cx,
    exit: &str,
    raw: &str,
    csrf_token: &str,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let exit_href = exit.to_string();
    let value = raw.to_string();
    let csrf = csrf_token.to_string();
    view! {
        cx =>
        <div class="composer flex align-end gap">
            <a href=(exit_href) class="btn flex-item-no-shrink margin-block-end" style="view-transition-name: input-switcher; --btn-border-radius: 0.5em">
                <img aria-hidden="true" src=(img_arrow_left()) />
                <span class="for-screen-reader">"Exit search "</span>
            </a>
            <form action="/searches" accept-charset="UTF-8" method="post" class="margin-block flex-item-grow contain flex align-center gap">
                <input type="hidden" name="authenticity_token" value=(csrf) />
                <div class="composer__input flex align-center flex-item-grow gap full-width input input--actor min-width">
                    <img aria-hidden="true" width="20" height="20" src=(img_search()) class="composer__input-hint colorize--black" style="view-transition-name: input-btn;" />
                    <input type="text" name="q" value=(value) class="searches__input input flex-item-grow" role="searchbox" aria-label="search" autofocus="autofocus" required="required" />
                    <a href="/searches" role="button" class="searches__reset">
                        <img aria-hidden="true" width="14" height="14" src=(img_remove()) class="colorize--black" />
                        <span class="for-screen-reader">"Clear search field"</span>
                    </a>
                    <button type="submit" class="btn btn--reversed flex-item-no-shrink txt-small" style="--btn-border-radius: 0.5em">
                        <img aria-hidden="true" src=(img_arrow_up()) />
                        <span class="for-screen-reader">"Search"</span>
                    </button>
                </div>
            </form>
        </div>
    }
    .boxed()
}

/// `GET /searches`.
async fn index_searches(cx: &Cx) -> Result<Response> {
    use crate::pages::{body_classes, document_shell};
    use topcamp_domain::auth::UserRole;
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let admin = user.role == UserRole::Administrator.value();
    let raw = query_params::<SearchesQuery>(cx)
        .ok()
        .and_then(|query| query.q.clone())
        .unwrap_or_default();
    let cleaned = sanitize_query(&raw);
    let active = (!cleaned.trim().is_empty()).then_some(cleaned);
    let recents = SearchRepository::ordered(db, user.id)
        .await
        .map_err(http_error)?;
    let details = match &active {
        Some(query) => MessageRepository::search_details(db, user.id, query, 100)
            .await
            .map_err(http_error)?,
        None => Vec::new(),
    };
    let mut room_ids: Vec<i64> = details.iter().map(|message| message.room_id).collect();
    room_ids.sort_unstable();
    room_ids.dedup();
    let mut rooms = HashMap::new();
    for room_id in room_ids {
        let Some(room) = RoomRepository::find_for_user(db, user.id, room_id)
            .await
            .map_err(http_error)?
        else {
            continue;
        };
        let name = if room.kind == "Rooms::Direct" {
            let ids = MembershipRepository::member_user_ids(db, room_id)
                .await
                .map_err(http_error)?;
            let members = UserRepository::form_users(db, &ids)
                .await
                .map_err(http_error)?;
            crate::room_show::room_display_name(&room, &members, user.id, &user.name)
        } else {
            room.name.clone().unwrap_or_default()
        };
        rooms.insert(room_id, name);
    }
    let views = crate::room_show::message_views(db, &rooms, &details, user.id, false).await?;
    let csrf_token = crate::csrf::issue(cx);
    let rows: Vec<Slot> = views
        .iter()
        .map(|message| Slot::new(crate::room_show::message_view(cx, message, &csrf_token, "")))
        .collect();
    let hits = rows.len();
    let content = Slot::new(content_view(cx, rows));
    let nav = Slot::new(nav_view(cx, active.as_deref(), hits, &recents, &csrf_token));
    let sidebar = Slot::new(sidebar_view(cx, &recents, &csrf_token));
    let footer = Slot::new(footer_view(cx, &exit_href(cx), &raw, &csrf_token));
    let shell = page_shell(db, user.id, &user.name).await?;
    document_shell(
        cx,
        "Search".to_string(),
        body_classes("sidebar searches", admin),
        content,
        crate::flash::Flash::default(),
        shell,
        Some(sidebar),
        Some(nav),
        None,
        Some(footer),
    )
    .boxed()
    .async_into_response(cx)
    .await
}

/// `POST /searches`: record the query, redirect to the results.
async fn create_search(cx: &Cx, body: Body) -> Result<Response> {
    let raw = to_bytes(body, 64 * 1024).await?;
    let pairs = serde_urlencoded::from_bytes::<Vec<(String, String)>>(&raw).unwrap_or_default();
    let mut token = None;
    let mut query = None;
    for (key, value) in &pairs {
        match key.as_str() {
            "authenticity_token" => token = Some(value.as_str()),
            "q" => query = Some(value.as_str()),
            _ => {}
        }
    }
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !token.is_some_and(|token| crate::csrf::verify(cx, token)) {
        return Err(forbidden().into());
    }
    let cleaned = sanitize_query(query.unwrap_or(""));
    if !cleaned.trim().is_empty() {
        let db = &app_context::<AppState>(cx).db;
        SearchRepository::record(db, user.id, &cleaned)
            .await
            .map_err(http_error)?;
        return see_other(searches_url(&cleaned)).into_response(cx);
    }
    see_other("/searches").into_response(cx)
}

/// `DELETE /searches/clear` (+ POST `_method` dispatch): drop recents.
async fn clear_searches(cx: &Cx, body: Body) -> Result<Response> {
    let raw = to_bytes(body, 64 * 1024).await?;
    let pairs = serde_urlencoded::from_bytes::<Vec<(String, String)>>(&raw).unwrap_or_default();
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
        return not_found().into_response(cx);
    }
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !token.is_some_and(|token| crate::csrf::verify(cx, token)) {
        return Err(forbidden().into());
    }
    let db = &app_context::<AppState>(cx).db;
    SearchRepository::clear(db, user.id)
        .await
        .map_err(http_error)?;
    see_other("/searches").into_response(cx)
}

/// `GET /searches`.
#[route(GET "/searches")]
pub async fn index(cx: &Cx) -> Result<Response> {
    index_searches(cx).await
}

/// `POST /searches`.
#[route(POST "/searches")]
pub async fn create(cx: &Cx, body: Body) -> Result<Response> {
    create_search(cx, body).await
}

/// `DELETE /searches/clear`.
#[route(DELETE "/searches/clear")]
pub async fn clear(cx: &Cx, body: Body) -> Result<Response> {
    clear_searches(cx, body).await
}

/// `POST /searches/clear`: `_method=delete` dispatch (the broom posts).
#[route(POST "/searches/clear")]
pub async fn modify_clear(cx: &Cx, body: Body) -> Result<Response> {
    clear_searches(cx, body).await
}

/// `search_results_tag` + the hit rows (threading off).
fn content_view(cx: &Cx, rows: Vec<Slot<'static>>) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    view! {
        cx =>
        <div id="message-area" class="message-area">
            <div class="message-area--empty min-width center">
                <figure class="center pad">
                    <img aria-hidden="true" src=(img_search()) class="colorize--black translucent" />
                </figure>
            </div>
            <div id="search-results" class="messages searches__results">
                for row in rows {
                    (row)
                }
            </div>
        </div>
    }
    .boxed()
}
