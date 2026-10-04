//! `Rooms::InvolvementsController`: the notification bell.
//!
//! `GET /rooms/:room_id/involvement` renders the bell button for the
//! current level; `PUT` cycles to the submitted level and redirects
//! back to the bell (sidebar visibility follows via a `RoomList`
//! event). Invalid levels 500, like upstream's enum
//! `ArgumentError`. `POST` carries the Rails `_method` override.

use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        Body, Slot,
        error::{bad_request, forbidden, not_found, see_other},
        path_param_segment, query_params, request,
        response::AsyncIntoResponse,
        route, to_bytes,
    },
    view::{ViewExt as _, view},
};

use topcamp_db::repositories::{MembershipRepository, RoomRepository, UserRepository};

use crate::rooms::cast_integer;
use crate::state::{AppState, http_error};

/// `HUMANIZE_INVOLVEMENT`.
fn humanize(involvement: &str) -> &'static str {
    match involvement {
        "mentions" => "Notifying about @ mentions",
        "everything" => "Notifying about all messages",
        "nothing" => "Notifications are off",
        "invisible" => "Notifications are off and room invisible in sidebar",
        _ => "Notifications",
    }
}

const SHARED_ORDER: &[&str] = &["mentions", "everything", "nothing", "invisible"];
const DIRECT_ORDER: &[&str] = &["everything", "nothing"];

/// `next_involvement_for`: the level after the current one (wraps).
pub(crate) fn next_involvement(direct: bool, involvement: &str) -> &'static str {
    let order = if direct { DIRECT_ORDER } else { SHARED_ORDER };
    let next = order
        .iter()
        .position(|level| *level == involvement)
        .map(|index| (index + 1) % order.len())
        .unwrap_or(0);
    order[next]
}

fn valid_involvement(involvement: &str) -> bool {
    SHARED_ORDER.contains(&involvement)
}

/// `button_to_change_involvement`: the bell form posting the next level.
pub(crate) fn bell_view(
    cx: &Cx,
    room_id: i64,
    direct: bool,
    involvement: &str,
    csrf_token: &str,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let action = format!(
        "/rooms/{room_id}/involvement?involvement={}",
        next_involvement(direct, involvement)
    );
    let label_id = crate::room_show::room_dom_id(
        crate::rooms_typed::Kind::of(if direct {
            "Rooms::Direct"
        } else {
            "Rooms::Open"
        }),
        room_id,
        "involvement_label",
    );
    let label = humanize(involvement).to_string();
    let csrf = csrf_token.to_string();
    let class = format!("btn {involvement}");
    let icon = match involvement {
        "everything" => img_notification_bell_everything(),
        "nothing" => img_notification_bell_nothing(),
        "invisible" => img_notification_bell_invisible(),
        _ => img_notification_bell_mentions(),
    };
    view! {
        cx =>
        <form class="button_to" action=(action) accept-charset="UTF-8" method="post">
            <input type="hidden" name="_method" value="put" />
            <input type="hidden" name="authenticity_token" value=(csrf) />
            <button class=(class) role="checkbox" aria-checked="true" aria-labelledby=(label_id.clone()) tabindex="0" type="submit">
                <img aria-hidden="true" width="20" height="20" src=(icon) />
                <span class="for-screen-reader" id=(label_id)>(label)</span>
            </button>
        </form>
    }
    .boxed()
}

/// `Rooms::InvolvementsController#show`: the bell for this membership.
async fn show_involvement(
    cx: &Cx,
    room_param: &str,
) -> Result<topcoat::router::response::Response> {
    use topcoat::router::response::IntoResponse;
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let room_id = cast_integer(room_param).unwrap_or(0);
    let room = RoomRepository::find_for_user(db, user.id, room_id)
        .await
        .map_err(http_error)?;
    let (Some(room), Some(membership)) = (
        room,
        MembershipRepository::find(db, room_id, user.id)
            .await
            .map_err(http_error)?,
    ) else {
        return not_found().into_response(cx);
    };
    let csrf_token = crate::csrf::issue(cx);
    let bell = Slot::new(bell_view(
        cx,
        room.id,
        room.kind == "Rooms::Direct",
        &membership.involvement,
        &csrf_token,
    ));
    view! {
        cx =>
        <span>(bell)</span>
    }
    .boxed()
    .async_into_response(cx)
    .await
}

#[query_params(error = bad_request)]
struct InvolvementQuery {
    involvement: Option<String>,
}

fn invalid_involvement() -> topcoat::Error {
    use topcamp_domain::error::{Error as DomainError, InfrastructureError};
    http_error(DomainError::Infrastructure(InfrastructureError::new(
        "involvement",
    )))
}

/// `Rooms::InvolvementsController#update`: store the level, tell the
/// sidebar, redirect back to the bell.
async fn update_involvement(
    cx: &Cx,
    room_param: &str,
    body: Body,
) -> Result<topcoat::router::response::Response> {
    use topcoat::router::response::IntoResponse;
    let query = query_params::<InvolvementQuery>(cx).ok();
    let raw = to_bytes(body, 64 * 1024).await?;
    let pairs = serde_urlencoded::from_bytes::<Vec<(String, String)>>(&raw).unwrap_or_default();
    let mut form_token = None;
    let mut override_method = None;
    let mut body_level = None;
    for (key, value) in pairs {
        match key.as_str() {
            "authenticity_token" => form_token = Some(value),
            "_method" => override_method = Some(value.to_ascii_lowercase()),
            "involvement" => body_level = Some(value),
            _ => {}
        }
    }
    if request::method(cx) == http::Method::POST && override_method.as_deref() != Some("put") {
        return Err(not_found().into());
    }
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !form_token.is_some_and(|token| crate::csrf::verify(cx, &token)) {
        return Err(forbidden().into());
    }
    // The bell posts the level on the query string; accept a body
    // param too.
    let level = body_level.or_else(|| query.and_then(|query| query.involvement.clone()));
    let Some(level) = level else {
        return Err(bad_request("param is missing or the value is empty: involvement").into());
    };
    if !valid_involvement(&level) {
        return Err(invalid_involvement());
    }
    let app = app_context::<AppState>(cx);
    let room_id = cast_integer(room_param).unwrap_or(0);
    let room = RoomRepository::find_for_user(&app.db, user.id, room_id)
        .await
        .map_err(http_error)?;
    let (Some(room), Some(_)) = (
        room,
        MembershipRepository::find(&app.db, room_id, user.id)
            .await
            .map_err(http_error)?,
    ) else {
        return not_found().into_response(cx);
    };
    UserRepository::set_involvement(&app.db, user.id, room.id, &level)
        .await
        .map_err(http_error)?;
    // Sidebar visibility follows (upstream's visibility broadcasts).
    app.bus
        .user(user.id)
        .send(crate::live::UserEvent::RoomList)
        .ok();
    see_other(format!("/rooms/{}/involvement", room.id)).into_response(cx)
}

/// `GET /rooms/:room_id/involvement`.
#[route(GET "/rooms/{room_id}/involvement")]
pub async fn show(cx: &Cx) -> Result<topcoat::router::response::Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    show_involvement(cx, &room).await
}

/// `PUT /rooms/:room_id/involvement`.
#[route(PUT "/rooms/{room_id}/involvement")]
pub async fn update(cx: &Cx, body: Body) -> Result<topcoat::router::response::Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    update_involvement(cx, &room, body).await
}

/// `POST /rooms/:room_id/involvement` (`_method=put`).
#[route(POST "/rooms/{room_id}/involvement")]
pub async fn modify(cx: &Cx, body: Body) -> Result<topcoat::router::response::Response> {
    let room = path_param_segment(cx, "room_id").to_owned();
    update_involvement(cx, &room, body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn involvement_cycles_per_kind() {
        assert_eq!(next_involvement(false, "mentions"), "everything");
        assert_eq!(next_involvement(false, "invisible"), "mentions");
        assert_eq!(next_involvement(true, "everything"), "nothing");
        assert_eq!(next_involvement(true, "nothing"), "everything");
        assert_eq!(next_involvement(false, "bogus"), "mentions");
    }
}
