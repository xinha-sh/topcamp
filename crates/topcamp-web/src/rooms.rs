//! Rooms home: welcome root, rooms index redirect, room destroy.
//!
//! Upstream `WelcomeController` + `RoomsController#index/destroy`:
//! `GET /` sends members to their last-visited room and shows roomless
//! users the empty state; `GET /rooms` redirects to the newest room;
//! destroy removes memberships + messages + the row (admin/creator).
//! Upstream draws but never implements `rooms#new/create/edit/update`,
//! so those paths 404 (`rooms_new` pins the one that would otherwise
//! parse as an id).

use serde::Deserialize;
use topcamp_db::repositories::{
    AccountRepository, MembershipRepository, MessageRepository, RoomRepository, RoomRow,
    UserRepository,
};
use topcamp_domain::auth::UserRole;
use topcoat::{
    Result,
    context::{Cx, app_context},
    cookie::{Cookies, cookies},
    router::{
        Slot,
        content::Form,
        error::{forbidden, see_other},
        path_param_segment,
        response::{AsyncIntoResponse, IntoResponse, Response},
        route,
    },
    view::{BoxView, ViewExt as _, view},
};

use crate::pages::{ShellContext, body_classes, document_shell};
use crate::state::{AppState, http_error};

/// Rails integer-column cast: leading digits after an optional sign,
/// `None` when no digits (`"new"` → `None`, `"12abc"` → `12`).
pub(crate) fn cast_integer(value: &str) -> Option<i64> {
    let value = value.trim_start();
    let (sign, digits) = match value.as_bytes().first() {
        Some(b'-') => (-1, &value[1..]),
        Some(b'+') => (1, &value[1..]),
        _ => (1, value),
    };
    let digits: String = digits.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<i64>().ok().map(|n| sign * n)
}

/// `last_room_visited`: the `last_room` cookie's room while the user is
/// still in it, else their original room.
async fn last_room_visited(
    cx: &Cx,
    db: &topcamp_db::PgDb,
    user_id: i64,
) -> Result<Option<RoomRow>> {
    if let Some(cookie) = cookies(cx).get("last_room")
        && let Some(room_id) = cast_integer(cookie.value())
        && let Some(room) = RoomRepository::find_for_user(db, user_id, room_id)
            .await
            .map_err(http_error)?
    {
        return Ok(Some(room));
    }
    RoomRepository::original_for_user(db, user_id)
        .await
        .map_err(http_error)
}

/// `WelcomeController#show`: signed-out visitors go to sign-in; members
/// go to their last-visited room; roomless users get the empty state.
#[route(GET "/")]
pub async fn welcome(cx: &Cx) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    if let Some(room) = last_room_visited(cx, db, user.id).await? {
        return see_other(format!("/rooms/{}", room.id)).into_response(cx);
    }
    let admin = user.role == UserRole::Administrator.value();
    let shell = ShellContext {
        current_user: Some((user.id, user.name.clone())),
        logo_version: AccountRepository::first(db)
            .await
            .map_err(http_error)?
            .map(|account| account.updated_number),
    };
    let Some(me) = UserRepository::find_avatar_user(db, user.id)
        .await
        .map_err(http_error)?
    else {
        return see_other("/session/new").into_response(cx);
    };
    let app = app_context::<AppState>(cx);
    let ctx = crate::sidebar::SidebarCtx {
        db: app.db.clone(),
        bus: app.bus.clone(),
        me: me.clone(),
        admin,
    };
    let csrf_token = crate::csrf::issue(cx);
    let frame = crate::sidebar::sidebar_frame(cx, ctx, csrf_token, true).await?;
    let content = welcome_view(cx, user.name);
    document_shell(
        cx,
        "No rooms yet".to_string(),
        body_classes("sidebar", admin),
        Slot::new(content),
        crate::flash::Flash::default(),
        shell,
        Some(Slot::new(frame)),
        None,
        None,
        None,
    )
    .boxed()
    .async_into_response(cx)
    .await
}

/// Empty state: same `message-area` shell the room page fills, with the
/// user's name for screen readers.
fn welcome_view(cx: &Cx, user_name: String) -> BoxView<'static> {
    use crate::assets::*;
    view! {
        cx =>
        <div id="message-area" class="message-area">
            <div class="message-area--empty min-width center">
                <figure class="center pad">
                    <img aria-hidden="true" class="colorize--black translucent" src=(img_messages_empty()) />
                    <span class="for-screen-reader">(user_name)</span>
                </figure>
            </div>
        </div>
    }
    .boxed()
}

/// `RoomsController#index`: redirect to the newest room. Roomless users
/// hit upstream's routing error (`room_url(nil)`), a 500 here too. The
/// typed controllers inherit this action (`GET /rooms/opens`, ...).
#[route(GET "/rooms")]
pub async fn index(cx: &Cx) -> Result<Response> {
    index_redirect(cx).await
}

/// Shared index body behind `/rooms` and the typed collection paths.
pub(crate) async fn index_redirect(cx: &Cx) -> Result<Response> {
    use topcamp_domain::error::{Error as DomainError, InfrastructureError};
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    match RoomRepository::last_for_user(db, user.id)
        .await
        .map_err(http_error)?
    {
        Some(room) => see_other(format!("/rooms/{}", room.id)).into_response(cx),
        None => Err(http_error(DomainError::Infrastructure(
            InfrastructureError::new("rooms"),
        ))),
    }
}

/// No `rooms#new` action upstream: the drawn route 404s like
/// `AbstractController::ActionNotFound`. Registered before the room
/// routes so the literal wins over `{room_id}`.
#[route(GET "/rooms/new")]
pub async fn rooms_new(cx: &Cx) -> Result<Response> {
    crate::not_found::response(cx)
}

/// Room destroy form. Plain HTML forms cannot send DELETE, so like the
/// session routes this accepts `POST` with `_method=delete` (what
/// Rails' `button_to ..., method: :delete` submits).
#[derive(Debug, Deserialize)]
pub(crate) struct DestroyForm {
    pub authenticity_token: String,
    #[serde(rename = "_method", default)]
    pub method_override: Option<String>,
}

/// `RoomsController#destroy`: admin or creator removes memberships +
/// messages + the row, then goes home. Unknown or inaccessible rooms
/// redirect home (`set_room`); the alert flash text lands with UI-14.
/// Room-remove broadcasts land with the live updates (UI-05).
#[route([POST, DELETE] "/rooms/{room_id}")]
pub async fn destroy(cx: &Cx, Form(input): Form<DestroyForm>) -> Result<Response> {
    if topcoat::router::request::method(cx) == http::Method::POST
        && input.method_override.as_deref() != Some("delete")
    {
        return Err(topcoat::router::error::not_found().into());
    }
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !crate::csrf::verify(cx, &input.authenticity_token) {
        return Err(forbidden().into());
    }
    let db = &app_context::<AppState>(cx).db;
    let param = path_param_segment(cx, "room_id").to_owned();
    let room = match cast_integer(&param) {
        Some(id) => RoomRepository::find_for_user(db, user.id, id)
            .await
            .map_err(http_error)?,
        None => None,
    };
    let Some(room) = room else {
        return crate::flash::redirect_with_alert(cx, "/", "Room not found or inaccessible");
    };
    if !(user.role == UserRole::Administrator.value() || room.creator_id == user.id) {
        return Err(forbidden().into());
    }
    MembershipRepository::delete_for_room(db, room.id)
        .await
        .map_err(http_error)?;
    for message_id in MessageRepository::ids_for_room(db, room.id)
        .await
        .map_err(http_error)?
    {
        MessageRepository::destroy(db, message_id)
            .await
            .map_err(http_error)?;
    }
    // Publish BEFORE the destroy (memberships still addressable).
    let app = app_context::<AppState>(cx);
    if room.kind == "Rooms::Open" {
        crate::live::publish_open_rooms(&app.bus);
    } else {
        crate::live::publish_member_rooms(&app.db, &app.bus, room.id).await?;
    }
    RoomRepository::destroy(db, room.id)
        .await
        .map_err(http_error)?;
    see_other("/").into_response(cx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cast_integer_reads_leading_digits() {
        assert_eq!(cast_integer("42"), Some(42));
        assert_eq!(cast_integer("12abc"), Some(12));
        assert_eq!(cast_integer("  9"), Some(9));
        assert_eq!(cast_integer("-7"), Some(-7));
        assert_eq!(cast_integer("+8x"), Some(8));
        assert_eq!(cast_integer("new"), None);
        assert_eq!(cast_integer(""), None);
        assert_eq!(cast_integer("-"), None);
        assert_eq!(cast_integer("abc"), None);
    }

    #[test]
    fn body_class_combos_match_upstream() {
        assert_eq!(body_classes("", false), "");
        assert_eq!(body_classes("", true), "admin");
        assert_eq!(body_classes("signup", false), "signup");
        assert_eq!(body_classes("sidebar", false), "sidebar");
        assert_eq!(body_classes("sidebar", true), "sidebar admin");
    }
}
