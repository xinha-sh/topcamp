//! `Autocompletable::UsersController`: mention/autocomplete
//! candidates, 20 per page, as bare `<lexxy-prompt-item>` HTML or
//! JSON (`Accept: application/json`). The mentions prompt filters
//! with `filter`, autocomplete inputs with `query`; `room_id`
//! scopes to room members (non-member rooms 404, like `.find`).

use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        content::Json,
        error::{not_found, see_other},
        query_params,
        response::{AsyncIntoResponse, IntoResponse, Response},
        route,
    },
    view::{ViewExt as _, view},
};

use topcamp_db::repositories::{RoomRepository, UserRepository};

use crate::state::{AppState, http_error};

#[query_params(error = bad_request)]
struct AutocompleteQuery {
    filter: Option<String>,
    query: Option<String>,
    room_id: Option<String>,
    page: Option<String>,
}

/// Absolute avatar URL for the JSON answer (request host + scheme,
/// like `fresh_user_avatar_url`).
fn absolute_avatar_url(cx: &Cx, user_id: i64, updated_number: &str) -> String {
    use topcoat::router::request;
    let headers = request::headers(cx);
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http");
    let path = crate::users::avatar_url_for(user_id, updated_number);
    format!("{scheme}://{host}{path}")
}

fn wants_json(cx: &Cx) -> bool {
    use topcoat::router::request;
    request::headers(cx)
        .get("accept")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("application/json"))
}

/// `avatar_tag`: avatar link, 48px image.
fn avatar_tag_view(
    cx: &Cx,
    user_id: i64,
    name: &str,
    bio: Option<&str>,
    updated_number: &str,
) -> topcoat::view::BoxView<'static> {
    let href = format!("/users/{user_id}");
    let title = match bio.filter(|bio| !bio.trim().is_empty()) {
        Some(bio) => format!("{name} \u{2013} {bio}"),
        None => name.to_string(),
    };
    let src = crate::users::avatar_url_for(user_id, updated_number);
    view! {
        cx =>
        <a title=(title) class="btn avatar" href=(href)>
            <img aria-hidden="true" width="48" height="48" src=(src) />
        </a>
    }
    .boxed()
}

/// `users/_mention`: the editor template (a span: mentions sit
/// inline in the editor's paragraphs).
fn mention_view(
    cx: &Cx,
    user_id: i64,
    name: &str,
    bio: Option<&str>,
    updated_number: &str,
) -> topcoat::view::BoxView<'static> {
    let sgid = crate::users::mention_sgid(user_id);
    let avatar =
        topcoat::router::Slot::new(avatar_tag_view(cx, user_id, name, bio, updated_number));
    let label = format!(" {name}");
    view! {
        cx =>
        <span class="mention" sgid=(sgid)>(avatar)(label)</span>
    }
    .boxed()
}

/// `autocompletable/users/_prompt_item`.
fn prompt_item_view(
    cx: &Cx,
    user: &topcamp_db::repositories::FormUser,
) -> topcoat::view::BoxView<'static> {
    let search = user.name.clone();
    let sgid = crate::users::mention_sgid(user.id);
    let avatar = topcoat::router::Slot::new(avatar_tag_view(
        cx,
        user.id,
        &user.name,
        user.bio.as_deref(),
        &user.updated_number,
    ));
    let name = user.name.clone();
    let editor = topcoat::router::Slot::new(mention_view(
        cx,
        user.id,
        &user.name,
        user.bio.as_deref(),
        &user.updated_number,
    ));
    view! {
        cx =>
        <lexxy-prompt-item search=(search) sgid=(sgid)>
            <template type="menu">
                <span class="autocomplete__item flex align-center gap unpad">
                    (avatar)
                    <span class="autocompletable__name">(name)</span>
                </span>
            </template>
            <template type="editor">
                (editor)
            </template>
        </lexxy-prompt-item>
    }
    .boxed()
}

/// `GET /autocompletable/users`.
async fn index_users(cx: &Cx) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let params = query_params::<AutocompleteQuery>(cx).ok();
    let filter = params
        .as_ref()
        .and_then(|query| query.filter.clone())
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            params
                .as_ref()
                .and_then(|query| query.query.clone())
                .filter(|text| !text.trim().is_empty())
        });
    let room_id = params
        .as_ref()
        .and_then(|query| query.room_id.clone())
        .and_then(|value| crate::rooms::cast_integer(&value));
    let page = params
        .as_ref()
        .and_then(|query| query.page.clone())
        .and_then(|value| crate::rooms::cast_integer(&value))
        .unwrap_or(1)
        .max(1);
    let db = &app_context::<AppState>(cx).db;
    if let Some(room_id) = room_id
        && RoomRepository::find_for_user(db, user.id, room_id)
            .await
            .map_err(http_error)?
            .is_none()
    {
        return not_found().into_response(cx);
    }
    let users =
        UserRepository::autocompletable(db, room_id, filter.as_deref(), 20, (page - 1) * 20)
            .await
            .map_err(http_error)?;
    if wants_json(cx) {
        let items: Vec<serde_json::Value> = users
            .iter()
            .map(|user| {
                serde_json::json!({
                    "name": crate::richtext::escape(&user.name),
                    "value": user.id,
                    "avatar_url": absolute_avatar_url(cx, user.id, &user.updated_number),
                    "sgid": crate::users::mention_sgid(user.id),
                })
            })
            .collect();
        return Json(items).into_response(cx);
    }
    let items: Vec<topcoat::router::Slot> = users
        .iter()
        .map(|user| topcoat::router::Slot::new(prompt_item_view(cx, user)))
        .collect();
    view! {
        cx =>
        <div>
            for item in items {
                (item)
            }
        </div>
    }
    .boxed()
    .async_into_response(cx)
    .await
}

/// `GET /autocompletable/users`.
#[route(GET "/autocompletable/users")]
pub async fn index(cx: &Cx) -> Result<Response> {
    index_users(cx).await
}
