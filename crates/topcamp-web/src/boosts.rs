//! `Messages::BoostsController`: boost chips on a message.
//!
//! `GET /messages/:id/boosts` renders the boosting fragment;
//! `GET .../new` the inline boost form; `POST` creates from
//! `boost[content]`; `DELETE /messages/:id/boosts/:boost_id` — or
//! `POST` there with `_method=delete`, the chip form's shape —
//! destroys the current user's boost (`head :no_content`).
//! Upstream's create redirects to the boosts frame URL; without
//! frames that page strands the browser, so ours lands back on
//! the room. Quick-boost forms POST plainly and land back on the
//! room (303); the `boost_message` procedure stays for Topcoat
//! clients, and every window's message region re-renders on
//! `BoostsChanged`.

use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        Body,
        error::{bad_request, forbidden, not_found, see_other},
        path_param_segment,
        response::{AsyncIntoResponse, IntoResponse, Response},
        route, to_bytes,
    },
    view::{ViewExt as _, view},
};

use topcamp_db::PgDb;
use topcamp_db::repositories::{
    MessageDetail, MessageRepository, RoomRepository, RoomRow, UserRepository,
};

use crate::rooms::cast_integer;
use crate::state::{AppState, http_error};

/// Parsed `boost[...]` submission. `present` mirrors
/// `params.require("boost")`: any `boost[*]` key counts.
struct BoostForm {
    present: bool,
    content: Option<String>,
    authenticity_token: Option<String>,
    method_override: Option<String>,
}

fn parse_boost_form(raw: &[u8]) -> BoostForm {
    let mut form = BoostForm {
        present: false,
        content: None,
        authenticity_token: None,
        method_override: None,
    };
    let Ok(pairs) = serde_urlencoded::from_bytes::<Vec<(String, String)>>(raw) else {
        return form;
    };
    for (key, value) in pairs {
        match key.as_str() {
            "boost[content]" => {
                form.present = true;
                form.content = Some(value);
            }
            "authenticity_token" => form.authenticity_token = Some(value),
            "_method" => form.method_override = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    form
}

fn csrf_ok(cx: &Cx, input: &BoostForm) -> bool {
    input
        .authenticity_token
        .as_deref()
        .is_some_and(|token| crate::csrf::verify(cx, token))
}

/// `Current.user.reachable_messages.find(params[:message_id])`: the
/// message plus its room, or `None` when either is out of reach.
async fn set_message(
    db: &PgDb,
    user_id: i64,
    param: &str,
) -> Result<Option<(RoomRow, MessageDetail)>> {
    let Some(id) = cast_integer(param) else {
        return Ok(None);
    };
    let Some(row) = MessageRepository::find_by_id(db, id)
        .await
        .map_err(http_error)?
    else {
        return Ok(None);
    };
    let Some(room) = RoomRepository::find_for_user(db, user_id, row.room_id)
        .await
        .map_err(http_error)?
    else {
        return Ok(None);
    };
    let detail = MessageRepository::find_detail_in_room(db, room.id, row.id)
        .await
        .map_err(http_error)?;
    Ok(detail.map(|message| (room, message)))
}

async fn message_view_for(
    db: &PgDb,
    room: &RoomRow,
    message: &MessageDetail,
    me_id: i64,
) -> Result<crate::room_show::ShowMessageView> {
    let room_name = crate::messages::room_name_for(db, room).await?;
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

/// `boosts#index`: the boosting fragment (chips + inline link).
async fn index_boosts(cx: &Cx, id_param: &str) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let Some((room, message)) = set_message(db, user.id, id_param).await? else {
        return not_found().into_response(cx);
    };
    let view = message_view_for(db, &room, &message, user.id).await?;
    crate::room_show::boosts_view(cx, &view, &crate::csrf::issue(cx), "")
        .async_into_response(cx)
        .await
}

/// `messages/boosts/new`: the inline boost form (custom emoji/text).
fn new_boost_view(
    cx: &Cx,
    view: &crate::room_show::ShowMessageView,
    user_id: i64,
    user_name: &str,
    user_title: &str,
    avatar_url: &str,
    csrf_token: &str,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let frame = crate::room_show::message_dom_id(&view.client_message_id, "new_boost");
    let action = format!("/messages/{}/boosts", view.id);
    let cancel = action.clone();
    let user_path = format!("/users/{user_id}");
    let title = user_title.to_string();
    let name = user_name.to_string();
    let avatar = avatar_url.to_string();
    let csrf = csrf_token.to_string();
    view! {
        cx =>
        <div id=(frame)>
            <div class="boost flex-inline postion--relative max-width fill-white" style="--column-gap: var(--inline-space-half)">
                <form class="boost__form flex align-center gap expanded" action=(action) accept-charset="UTF-8" method="post">
                    <input type="hidden" name="authenticity_token" value=(csrf) />
                    <label class="boost__form-label flex gap" style="--column-gap: 0.7ch;" role="button" tabindex="0" aria-label="Add a boost">
                        <figure class="avatar boost__avatar flex-item-no-shrink">
                            <a title=(title) class="btn avatar" href=(user_path)><img aria-hidden="true" src=(avatar) width="48" height="48" /></a>
                            <span class="for-screen-reader">(name)</span>
                        </figure>
                        <input autofocus="autofocus" autocomplete="off" autocorrect="off" maxlength="16" required="required" pattern="\\S+.*" class="input input--boost txt-small" size="16" type="text" name="boost[content]" />
                    </label>
                    <button name="button" type="submit" class="btn btn--reversed">
                        <img aria-hidden="true" src=(img_check()) />
                        <span class="for-screen-reader">"Submit"</span>
                    </button>
                    <a class="btn btn--negative" href=(cancel)>
                        <img aria-hidden="true" src=(img_minus()) />
                        <span class="for-screen-reader">"Cancel"</span>
                    </a>
                </form>
            </div>
        </div>
    }
    .boxed()
}

/// `boosts#new`: the inline boost form fragment.
async fn new_boost(cx: &Cx, id_param: &str) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let Some((room, message)) = set_message(db, user.id, id_param).await? else {
        return not_found().into_response(cx);
    };
    let view = message_view_for(db, &room, &message, user.id).await?;
    let users = UserRepository::form_users(db, &[user.id])
        .await
        .map_err(http_error)?;
    let me = users.first().expect("current user exists");
    let actor = crate::room_show::actor_view(me);
    new_boost_view(
        cx,
        &view,
        actor.id,
        &actor.name,
        &actor.title,
        &actor.avatar_url,
        &crate::csrf::issue(cx),
    )
    .async_into_response(cx)
    .await
}

/// `boosts#create`: store the boost, land back on the room. A
/// missing `boost[content]` 500s (the column is `NOT NULL`), like
/// upstream's `NotNullViolation`.
async fn create_boost(cx: &Cx, id_param: &str, body: Body) -> Result<Response> {
    let input = parse_boost_form(&to_bytes(body, 1024 * 1024).await?);
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    if !input.present {
        return Err(bad_request("param is missing or the value is empty: boost").into());
    }
    let app = app_context::<AppState>(cx);
    let Some((room, message)) = set_message(&app.db, user.id, id_param).await? else {
        return not_found().into_response(cx);
    };
    let Some(content) = input.content else {
        use topcamp_domain::error::{Error as DomainError, InfrastructureError};
        return Err(http_error(DomainError::Infrastructure(
            InfrastructureError::new("boosts"),
        )));
    };
    MessageRepository::create_boost(&app.db, message.id, user.id, &content)
        .await
        .map_err(http_error)?;
    crate::live::publish_boosts_changed(&app.bus, room.id, message.id);
    see_other(format!("/rooms/{}", room.id)).into_response(cx)
}

/// `boosts#destroy`: drop the current user's boost, `head
/// :no_content`. A boost by anyone else (or on another message)
/// 404s, like `find_by!`.
async fn destroy_boost(cx: &Cx, id_param: &str, boost_param: &str, body: Body) -> Result<Response> {
    let input = parse_boost_form(&to_bytes(body, 1024 * 1024).await?);
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    let app = app_context::<AppState>(cx);
    let Some((room, message)) = set_message(&app.db, user.id, id_param).await? else {
        return not_found().into_response(cx);
    };
    let boost_id = cast_integer(boost_param).unwrap_or(0);
    let found = MessageRepository::find_boost(&app.db, message.id, boost_id, user.id)
        .await
        .map_err(http_error)?;
    if found.is_none() {
        return not_found().into_response(cx);
    }
    MessageRepository::delete_boost(&app.db, boost_id)
        .await
        .map_err(http_error)?;
    crate::live::publish_boosts_changed(&app.bus, room.id, message.id);
    Ok(Response::builder()
        .status(204)
        .body(Body::empty())
        .expect("204 builds"))
}

// --- routes ------------------------------------------------------------------------

/// `GET /messages/:id/boosts`.
#[route(GET "/messages/{id}/boosts")]
pub async fn index(cx: &Cx) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    index_boosts(cx, &id).await
}

/// `GET /messages/:id/boosts/new`.
#[route(GET "/messages/{id}/boosts/new")]
pub async fn new(cx: &Cx) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    new_boost(cx, &id).await
}

/// `POST /messages/:id/boosts`.
#[route(POST "/messages/{id}/boosts")]
pub async fn create(cx: &Cx, body: Body) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    create_boost(cx, &id, body).await
}

/// `DELETE /messages/:id/boosts/:boost_id`.
#[route(DELETE "/messages/{id}/boosts/{boost_id}")]
pub async fn destroy(cx: &Cx, body: Body) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    let boost_id = path_param_segment(cx, "boost_id").to_owned();
    destroy_boost(cx, &id, &boost_id, body).await
}

/// `POST /messages/:id/boosts/:boost_id`: the chip delete form
/// carries `_method=delete`; anything else 404s.
#[route(POST "/messages/{id}/boosts/{boost_id}")]
pub async fn modify(cx: &Cx, body: Body) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    let boost_id = path_param_segment(cx, "boost_id").to_owned();
    let raw = to_bytes(body, 1024 * 1024).await?;
    let method = parse_boost_form(&raw).method_override;
    match method.as_deref() {
        Some("delete") => destroy_boost(cx, &id, &boost_id, Body::from(raw)).await,
        _ => not_found().into_response(cx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_parses_boost_content() {
        let form = parse_boost_form(b"boost%5Bcontent%5D=%F0%9F%91%8F&authenticity_token=t");
        assert!(form.present);
        assert_eq!(form.content.as_deref(), Some("\u{1f44f}"));
        assert_eq!(form.authenticity_token.as_deref(), Some("t"));
        assert_eq!(form.method_override, None);
    }

    #[test]
    fn form_parses_method_override() {
        let form = parse_boost_form(b"_method=DELETE&authenticity_token=t");
        assert!(!form.present);
        assert_eq!(form.method_override.as_deref(), Some("delete"));
    }
}
