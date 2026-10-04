//! Room + message + search routes (§4–§5, §15).
//!
//! Thin handlers: parse params via Topcoat extractors, call [`PgDb`], render
//! JSON views. Authentication still resolves the caller from an explicit id
//! until session auth lands (Phase 3 `auth-flow`); every such spot is marked.

use serde::{Deserialize, Serialize};
use topcamp_db::repositories::{MessageRepository, NewMessage, RoomRepository};
use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        Body,
        content::Json,
        error::{RouterErrorExt as _, bad_request},
        path_param_segment, query_params,
        response::{IntoResponse, Response},
        route,
    },
};

use crate::state::{AppState, http_error};

fn state(cx: &Cx) -> &AppState {
    app_context(cx)
}

// --- views ---------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct RoomView {
    pub id: i64,
    pub name: Option<String>,
    pub kind: String,
}

#[derive(Debug, Serialize)]
pub struct MessageView {
    pub id: i64,
    pub room_id: i64,
    pub creator_id: i64,
    pub client_message_id: String,
}

fn message_view(row: topcamp_db::repositories::MessageRow) -> MessageView {
    MessageView {
        id: row.id,
        room_id: row.room_id,
        creator_id: row.creator_id,
        client_message_id: row.client_message_id,
    }
}

// --- rooms ---------------------------------------------------------------

#[route(GET "/rooms/{room_id}")]
pub async fn show_room(cx: &Cx) -> Result<Response> {
    let raw = path_param_segment(cx, "room_id").to_owned();
    // Browsers get the room page (`rooms#show`); API clients keep JSON.
    if crate::messages::wants_html(cx) {
        return crate::room_show::render_show(cx, &raw, None).await;
    }
    let id: i64 = raw.parse().map_err(|_| bad_request("invalid room id"))?;
    let _user = crate::auth::current_user_or_deny_bot(cx)
        .await?
        .ok_or_unauthorized()?;
    let room = RoomRepository::find_by_id(&state(cx).db, id)
        .await
        .map_err(http_error)?
        .ok_or_not_found()?;
    Json(RoomView {
        id: room.id,
        name: room.name,
        kind: room.kind,
    })
    .into_response(cx)
}

// --- messages ------------------------------------------------------------

#[query_params(error = bad_request)]
struct MessageListQuery {
    before: Option<i64>,
    after: Option<i64>,
    limit: Option<i64>,
}

#[route(GET "/rooms/{room_id}/messages")]
pub async fn list_messages(cx: &Cx) -> Result<Response> {
    let raw = path_param_segment(cx, "room_id").to_owned();
    // Browsers get the paged fragments (`messages#index`); API clients
    // keep JSON.
    if crate::messages::wants_html(cx) {
        let id: i64 = raw.parse().unwrap_or(0);
        return crate::messages::index_html(cx, id).await;
    }
    let room_id: i64 = raw.parse().map_err(|_| bad_request("invalid room id"))?;
    let _user = crate::auth::current_user_or_deny_bot(cx)
        .await?
        .ok_or_unauthorized()?;
    let query = query_params::<MessageListQuery>(cx)?;
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let rows = state(cx)
        .db
        .find_in_room(room_id, query.before, query.after, limit)
        .await
        .map_err(http_error)?;
    Json(rows.into_iter().map(message_view).collect::<Vec<_>>()).into_response(cx)
}

#[derive(Debug, Deserialize)]
struct CreateMessageBody {
    client_message_id: String,
    body: String,
}

#[route(POST "/rooms/{room_id}/messages")]
pub async fn create_message(cx: &Cx, body: Body) -> Result<Response> {
    use topcoat::router::{request, to_bytes};
    let room_param = path_param_segment(cx, "room_id").to_owned();
    // Composer forms post urlencoded (`messages#create`); JSON keeps
    // the API contract.
    let content_type = request::headers(cx)
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    // Multipart uploads ride through `create_html` (128MB cap there);
    // JSON posts stay small.
    let cap = if content_type.contains("application/json") {
        1024 * 1024
    } else {
        128 * 1024 * 1024
    };
    let raw = to_bytes(body, cap).await?;
    if !content_type.contains("application/json") {
        let id: i64 = room_param.parse().unwrap_or(0);
        return crate::messages::create_html(cx, id, Body::from(raw)).await;
    }
    let room_id: i64 = room_param
        .parse()
        .map_err(|_| bad_request("invalid room id"))?;
    let input: CreateMessageBody =
        serde_json::from_slice(&raw).map_err(|_| bad_request("invalid JSON body"))?;
    let user = crate::auth::current_user_or_deny_bot(cx)
        .await?
        .ok_or_unauthorized()?;
    if input.body.trim().is_empty() {
        return Err(bad_request("body must not be blank").into());
    }
    let app = state(cx);
    let row = app
        .db
        .post_message(NewMessage {
            room_id,
            creator_id: user.id,
            client_message_id: input.client_message_id,
            body: input.body,
        })
        .await
        .map_err(http_error)?;
    // API posts publish to the live bus (the room's tail appends).
    crate::live::publish_message_create(&app.db, &app.bus, row.room_id, row.id).await?;
    Json(message_view(row)).into_response(cx)
}

// --- search --------------------------------------------------------------

#[query_params(error = bad_request)]
struct SearchQuery {
    q: Option<String>,
    limit: Option<i64>,
}

#[route(GET "/search")]
pub async fn search(cx: &Cx) -> Result<Json<Vec<MessageView>>> {
    let query = query_params::<SearchQuery>(cx)?;
    let user = crate::auth::current_user_or_deny_bot(cx)
        .await?
        .ok_or_unauthorized()?;
    let terms = query.q.as_deref().unwrap_or("");
    if terms.trim().is_empty() {
        return Ok(Json(Vec::new()));
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let rows = state(cx)
        .db
        .search_reachable(user.id, terms, limit)
        .await
        .map_err(http_error)?;
    Ok(Json(rows.into_iter().map(message_view).collect()))
}
