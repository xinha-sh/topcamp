//! Typed rooms CRUD: `/rooms/opens|closeds|directs`.
//!
//! Upstream `Rooms::{Opens,Closeds,Directs}Controller`: `index` is
//! `RoomsController`'s (redirect to the newest room); `show` remembers
//! the visit and redirects to the room page; `new`/`edit` render the
//! access forms; `create`/`update`/`destroy` mutate. Open creates grant
//! every active user (upstream's `after_commit`); direct creates reuse
//! the room with the exact member set. Broadcasts land with the live
//! updates (UI-05); cable stream sources stay out of these pages.
//!
//! Divergences, all invisible: forms carry our double-submit CSRF token
//! (upstream relies on `Sec-Fetch-Site`); `contents`/`hidden`/`required`
//! render valued (`contents=""`) because `view!` needs attr values;
//! `set_room` failures redirect home without the alert flash (UI-14).

use topcamp_db::PgDb;
use topcamp_db::repositories::{
    AccountRepository, FormUser, MembershipRepository, RoomRepository, RoomRow, UserRepository,
    UserRow,
};
use topcamp_domain::auth::UserRole;
use topcoat::{
    Result,
    context::{Cx, app_context},
    cookie::{Cookie, Cookies, cookies},
    router::{
        Body, Slot,
        error::{bad_request, forbidden, not_found, see_other},
        path_param_segment, query_params, request,
        response::{AsyncIntoResponse, IntoResponse, Response},
        route, to_bytes,
    },
    view::{BoxView, ViewExt as _, view},
};

use crate::pages::{ShellContext, body_classes, document_shell};
use crate::rooms::cast_integer;
use crate::state::{AppState, http_error};

/// Which typed controller serves the request.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Open,
    Closed,
    Direct,
}

impl Kind {
    pub(crate) fn of(kind: &str) -> Self {
        match kind {
            "Rooms::Closed" => Kind::Closed,
            "Rooms::Direct" => Kind::Direct,
            _ => Kind::Open,
        }
    }

    pub(crate) fn class_name(self) -> &'static str {
        match self {
            Kind::Open => "Rooms::Open",
            Kind::Closed => "Rooms::Closed",
            Kind::Direct => "Rooms::Direct",
        }
    }

    fn collection(self) -> &'static str {
        match self {
            Kind::Open => "/rooms/opens",
            Kind::Closed => "/rooms/closeds",
            Kind::Direct => "/rooms/directs",
        }
    }

    fn new_path(self) -> String {
        format!("{}/new", self.collection())
    }

    fn member_path(self, id: i64) -> String {
        format!("{}/{id}", self.collection())
    }

    pub(crate) fn edit_path(self, id: i64) -> String {
        format!("{}/{id}/edit", self.collection())
    }

    /// `dom_id` segment: `rooms_open`, `rooms_closed`, `rooms_direct`.
    pub(crate) fn param_key(self) -> &'static str {
        match self {
            Kind::Open => "rooms_open",
            Kind::Closed => "rooms_closed",
            Kind::Direct => "rooms_direct",
        }
    }
}

/// Typed `set_room`: membership lookup plus the scope check (directs in
/// or out). Unknown, inaccessible, or out-of-scope rooms redirect home;
/// the alert flash text lands with UI-14.
async fn set_room(db: &PgDb, user_id: i64, param: &str, direct: bool) -> Result<Option<RoomRow>> {
    let id = cast_integer(param);
    let room = match id {
        Some(id) => RoomRepository::find_for_user(db, user_id, id)
            .await
            .map_err(http_error)?,
        None => None,
    };
    Ok(room.filter(|room| (room.kind == "Rooms::Direct") == direct))
}

/// `ensure_permission_to_create_rooms`: 403 unless the caller is an
/// administrator or the account leaves creation open.
async fn ensure_can_create(db: &PgDb, admin: bool) -> Result<()> {
    if admin {
        return Ok(());
    }
    if AccountRepository::room_creation_restricted(db)
        .await
        .map_err(http_error)?
    {
        return Err(forbidden().into());
    }
    Ok(())
}

/// `ensure_can_administer`: 403 unless the caller administers the room
/// (administrator or creator).
fn ensure_can_administer(admin: bool, user_id: i64, room: &RoomRow) -> Result<()> {
    if admin || room.creator_id == user_id {
        Ok(())
    } else {
        Err(forbidden().into())
    }
}

/// `remember_last_room_visited`: `cookies.permanent[:last_room]`
/// (20 years, like upstream).
pub(crate) fn remember_last_room(cx: &Cx, room_id: i64) {
    cookies(cx).add(
        Cookie::build(("last_room", room_id.to_string()))
            .path("/")
            .max_age(time::Duration::days(365 * 20)),
    );
}

/// Back-link target: the remembered room, else the root.
fn back_href(cx: &Cx) -> String {
    cookies(cx)
        .get("last_room")
        .and_then(|cookie| cast_integer(cookie.value()))
        .map(|id| format!("/rooms/{id}"))
        .unwrap_or_else(|| "/".to_string())
}

// --- params ----------------------------------------------------------

/// Room form body: `room[name]`, `user_ids[]`, the Rails `_method`
/// override, and our CSRF token. Parsed from raw pairs because
/// `serde_urlencoded` maps structs by literal key (bracketed nesting
/// needs manual handling). Everything optional so bodyless DELETEs
/// parse (the token check then fails closed).
#[derive(Debug, Default)]
struct RoomForm {
    authenticity_token: Option<String>,
    room_submitted: bool,
    room_name: Option<String>,
    user_ids: Vec<String>,
    method_override: Option<String>,
}

/// Parse a urlencoded body into bracket-aware fields. Garbage yields
/// an empty form (mutations then fail the token check).
fn parse_room_form(bytes: &[u8]) -> RoomForm {
    let mut form = RoomForm::default();
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(bytes).unwrap_or_default();
    for (key, value) in pairs {
        match key.as_str() {
            "authenticity_token" => form.authenticity_token = Some(value),
            "_method" => form.method_override = Some(value),
            "room[name]" => {
                form.room_submitted = true;
                form.room_name = Some(value);
            }
            nested if nested.starts_with("room[") => {
                form.room_submitted = true;
            }
            "user_ids[]" | "user_ids" => form.user_ids.push(value),
            _ => {}
        }
    }
    form
}

/// Double-submit check for the room forms.
fn csrf_ok(cx: &Cx, input: &RoomForm) -> bool {
    input
        .authenticity_token
        .as_deref()
        .is_some_and(|token| crate::csrf::verify(cx, token))
}

/// `room_name_param`: missing `room` is `ParameterMissing` (400).
fn room_name(form: &RoomForm) -> Result<Option<String>> {
    if !form.room_submitted {
        return Err(bad_request("param is missing or the value is empty: room").into());
    }
    Ok(form.room_name.clone())
}

/// `user_ids_param`: body plus query `user_ids[]`, cast like the
/// column (sidebar placeholder pings submit ids in the query string).
fn submitted_ids(cx: &Cx, form: &RoomForm) -> Vec<i64> {
    let mut ids: Vec<i64> = form
        .user_ids
        .iter()
        .filter_map(|value| cast_integer(value))
        .collect();
    if let Some(query) = request::uri(cx).query() {
        for pair in query.split('&') {
            let value = pair
                .strip_prefix("user_ids%5B%5D=")
                .or_else(|| pair.strip_prefix("user_ids[]="))
                .or_else(|| pair.strip_prefix("user_ids="));
            ids.extend(value.and_then(cast_integer));
        }
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

// --- page assembly ---------------------------------------------------

/// Shell pieces every typed page shares: identity + logo, the sidebar
/// frame, and the back-link nav. `None` is a stale session (the user
/// vanished between the session lookup and the avatar lookup).
struct PageCtx {
    shell: ShellContext,
    sidebar: Slot<'static>,
    nav: Slot<'static>,
    admin: bool,
}

async fn page_ctx(cx: &Cx, db: &PgDb, user: &UserRow) -> Result<Option<PageCtx>> {
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
        return Ok(None);
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
    let nav = back_nav_view(cx, back_href(cx));
    Ok(Some(PageCtx {
        shell,
        sidebar: Slot::new(frame),
        nav: Slot::new(nav),
        admin,
    }))
}

fn shell_page(
    cx: &Cx,
    title: String,
    admin: bool,
    content: BoxView<'static>,
    ctx: PageCtx,
    nav: bool,
) -> impl AsyncIntoResponse + use<> {
    // Form pages carry no `sidebar` body class upstream (only the
    // room page and welcome do), so main justifies to the top.
    document_shell(
        cx,
        title,
        body_classes("", admin),
        Slot::new(content),
        crate::flash::Flash::default(),
        ctx.shell,
        Some(ctx.sidebar),
        nav.then_some(ctx.nav),
        None,
        None,
    )
    .boxed()
}

// --- views -----------------------------------------------------------

/// One access-list row: id, name, `title` tooltip, avatar, and the
/// lowercase filter value.
struct FormUserView {
    id: String,
    name: String,
    title: String,
    avatar: String,
    lower: String,
    path: String,
}

/// `User#title`: name and bio joined by " – ", blanks dropped.
fn form_user_view(user: &FormUser) -> FormUserView {
    let title = [Some(user.name.as_str()), user.bio.as_deref()]
        .into_iter()
        .flatten()
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" – ");
    FormUserView {
        id: user.id.to_string(),
        name: user.name.clone(),
        title,
        avatar: crate::users::avatar_url_for(user.id, &user.updated_number),
        lower: user.name.to_lowercase(),
        path: format!("/users/{}", user.id),
    }
}

/// `link_back_to_last_room_visited`: the nav's back link.
fn back_nav_view(cx: &Cx, href: String) -> BoxView<'static> {
    use crate::assets::*;
    view! {
        cx =>
        <div class="flex-item-justify-start">
            <a class="btn" href=(href)>
                <img aria-hidden="true" src=(img_arrow_left()) width="20" height="20" />
                <span class="for-screen-reader">"Go Back"</span>
            </a>
        </div>
    }
    .boxed()
}

/// `TRANSLATIONS["room_name"]` behind the globe popup.
const ROOM_NAME_TRANSLATIONS: [(&str, &str); 7] = [
    ("🇺🇸", "Name the room"),
    ("🇪🇸", "Nombrar la sala"),
    ("🇫🇷", "Nommez la salle"),
    ("🇮🇳", "कमरे का नाम दें"),
    ("🇩🇪", "Geben Sie dem Raum einen Namen"),
    ("🇧🇷", "Dê um nome a essa sala"),
    ("🇯🇵", "ルームに名前を付ける"),
];

/// `translation_button(ctx, "room_name")`.
fn translation_button_view(cx: &Cx) -> BoxView<'static> {
    use crate::assets::*;
    view! {
        cx =>
        <details class="position-relative">
            <summary class="btn" tabindex="-1">
                <img aria-hidden="true" class="color-icon" src=(img_globe()) width="20" height="20" />
                <span class="for-screen-reader">"Translate"</span>
            </summary>
            <div class="language-list-menu shadow">
                <dl class="language-list">
                    for (flag, text) in ROOM_NAME_TRANSLATIONS {
                        <dt>(flag)</dt>
                        <dd class="margin-none">(text)</dd>
                    }
                </dl>
            </div>
        </details>
    }
    .boxed()
}

/// The name field (administrators) or bare heading (everyone else).
fn name_field_view(cx: &Cx, name: String, can_administer: bool) -> BoxView<'static> {
    if can_administer {
        let button = Slot::new(translation_button_view(cx));
        view! {
            cx =>
            (button)
            <label class="flex-item-grow txt-large">
                <input name="room[name]" id="room_name" class="input full-width" required="required" autofocus="autofocus" placeholder="Name the room" type="text" value=(name) />
                <span class="for-screen-reader">"Name this room"</span>
            </label>
        }
        .boxed()
    } else {
        view! {
            cx =>
            <h1 class="flex-item-grow txt-x-large">(name)</h1>
        }
        .boxed()
    }
}

/// `user_filter_search_tag`: the filter box above long member lists.
fn filter_search_view(cx: &Cx) -> BoxView<'static> {
    view! {
        cx =>
        <input type="search" id="search" autocorrect="off" autocomplete="off" data-1p-ignore="true" class="input input--transparent full-width" placeholder="Filter…" />
    }
    .boxed()
}

/// One member row shared by both access lists (the trailing control
/// differs, so callers append it).
fn member_row_open(cx: &Cx, user: FormUserView, can_administer: bool) -> BoxView<'static> {
    use crate::assets::*;
    view! {
        cx =>
        <li class="flex align-center gap margin-none" data-value=(user.lower)>
            <figure class="avatar flex-item-no-shrink" style="--avatar-size: 4ch;">
                <a title=(user.title) class="btn avatar" href=(user.path)>
                    <img aria-hidden="true" loading="lazy" src=(user.avatar) width="48" height="48" />
                </a>
            </figure>
            <div class="min-width">
                <div class="overflow-ellipsis fill-shade">
                    <strong>(user.name)</strong>
                </div>
            </div>
            <hr class="separator" aria-hidden="true" />
            if can_administer {
                <img class="colorize--black flex-item-no-shrink" aria-hidden="true" src=(img_check()) width="20" height="20" />
            }
        </li>
    }
    .boxed()
}

/// `rooms/opens/_form`: the Everyone row plus every active user.
fn open_access_list(
    cx: &Cx,
    users: Vec<FormUserView>,
    can_administer: bool,
    type_change: String,
) -> BoxView<'static> {
    use crate::assets::*;
    let rows: Vec<BoxView<'static>> = users
        .into_iter()
        .map(|user| member_row_open(cx, user, can_administer))
        .collect();
    let show_filter = rows.len() > 20;
    let filter = Slot::new(filter_search_view(cx));
    view! {
        cx =>
        <menu class="flex flex-column gap margin-none pad overflow-y constrain-height">
            <li class="flex align-center gap margin-none">
                <figure class="avatar flex-item-no-shrink" style="--avatar-border-radius: 0; --avatar-size: 4ch;">
                    <img aria-hidden="true" class="colorize--black" style="background-color: transparent" src=(img_everyone()) />
                    <span class="for-screen-reader">"Everyone"</span>
                </figure>
                <div class="min-width">
                    <div class="overflow-ellipsis fill-shade">
                        <strong>"Everyone"</strong>
                    </div>
                </div>
                <hr class="separator" aria-hidden="true" />
                if can_administer {
                    <a class="btn--faux flex-inline" tabindex="-1" href=(type_change)>
                        <label for="room_type" class="switch">
                            <input type="checkbox" id="room_type" class="switch__input" checked="checked" />
                            <span class="switch__btn round"></span>
                            <span class="for-screen-reader">"Give only some access to this room"</span>
                        </label>
                    </a>
                }
            </li>
            <hr class="separator full-width" style="--border-style: solid" />
            if show_filter {
                (filter)
            }
            <div contents="">
                for row in rows {
                    (Slot::new(row))
                }
            </div>
        </menu>
    }
    .boxed()
}

/// One closed-room member row: the creator's own row on a new room is a
/// hidden grant (membership is forced); every other row is a switch.
fn member_row_closed(
    cx: &Cx,
    user: FormUserView,
    can_administer: bool,
    selected: bool,
    locked: bool,
) -> BoxView<'static> {
    let label = format!("Give {} access to this room", user.name);
    let grant = Slot::new(closed_grant_view(
        cx,
        user.id.clone(),
        label,
        selected,
        locked,
    ));
    view! {
        cx =>
        <li class="flex align-center gap margin-none" data-value=(user.lower)>
            <figure class="avatar flex-item-no-shrink" style="--avatar-size: 4ch;">
                <a title=(user.title) class="btn avatar" href=(user.path)>
                    <img aria-hidden="true" loading="lazy" src=(user.avatar) width="48" height="48" />
                </a>
            </figure>
            <div class="min-width">
                <div class="overflow-ellipsis fill-shade">
                    <strong>(user.name)</strong>
                </div>
            </div>
            <hr class="separator" aria-hidden="true" />
            if can_administer {
                (grant)
            }
        </li>
    }
    .boxed()
}

/// The closed-room grant control: forced check, checked switch, or
/// unchecked switch. Split out so the checked attribute stays static.
fn closed_grant_view(
    cx: &Cx,
    id: String,
    label: String,
    selected: bool,
    locked: bool,
) -> BoxView<'static> {
    use crate::assets::*;
    if locked {
        view! {
            cx =>
            <input type="hidden" name="user_ids[]" value=(id) />
            <img class="colorize--black flex-item-no-shrink" aria-hidden="true" src=(img_check()) width="20" height="20" />
        }
        .boxed()
    } else if selected {
        view! {
            cx =>
            <label class="switch flex-item-no-shrink">
                <input type="checkbox" name="user_ids[]" value=(id.clone()) class="switch__input" checked="checked" />
                <span class="switch__btn round"></span>
                <span class="for-screen-reader">(label)</span>
            </label>
        }
        .boxed()
    } else {
        view! {
            cx =>
            <label class="switch flex-item-no-shrink">
                <input type="checkbox" name="user_ids[]" value=(id.clone()) class="switch__input" />
                <span class="switch__btn round"></span>
                <span class="for-screen-reader">(label)</span>
            </label>
        }
        .boxed()
    }
}

/// `rooms/closeds/_form`: the Everyone row (administrators only) plus
/// selected members above unselected users.
fn closed_access_list(
    cx: &Cx,
    selected: Vec<FormUserView>,
    unselected: Vec<FormUserView>,
    can_administer: bool,
    current_user_id: &str,
    editing: bool,
    type_change: String,
) -> BoxView<'static> {
    use crate::assets::*;
    let grant_row = |user: FormUserView, selected: bool| {
        let locked = !editing && user.id == current_user_id;
        member_row_closed(cx, user, can_administer, selected, locked)
    };
    let selected_rows: Vec<BoxView<'static>> = selected
        .into_iter()
        .map(|user| grant_row(user, true))
        .collect();
    let unselected_rows: Vec<BoxView<'static>> = unselected
        .into_iter()
        .map(|user| grant_row(user, false))
        .collect();
    let show_filter = selected_rows.len() + unselected_rows.len() > 20;
    let show_divider = !selected_rows.is_empty() && !unselected_rows.is_empty();
    let filter = Slot::new(filter_search_view(cx));
    view! {
        cx =>
        <menu class="flex flex-column gap margin-none pad overflow-y constrain-height">
            if can_administer {
                <li class="flex align-center gap margin-none">
                    <figure class="avatar flex-item-no-shrink" style="--avatar-border-radius: 0; --avatar-size: 4ch;">
                        <img aria-hidden="true" class="colorize--black" style="background-color: transparent" src=(img_everyone()) />
                        <span class="for-screen-reader">"Everyone"</span>
                    </figure>
                    <div class="min-width">
                        <div class="overflow-ellipsis fill-shade">
                            <strong>"Everyone"</strong>
                        </div>
                    </div>
                    <hr class="separator" aria-hidden="true" />
                    <a class="btn--faux flex-inline" tabindex="-1" href=(type_change)>
                        <label for="room_type" class="switch">
                            <input type="checkbox" id="room_type" class="switch__input" />
                            <span class="switch__btn round"></span>
                            <span class="for-screen-reader">"Give everyone access to this room"</span>
                        </label>
                    </a>
                </li>
                <hr class="separator full-width" style="--border-style: solid" />
            }
            if show_filter {
                (filter)
            }
            <div contents="">
                for row in selected_rows {
                    (Slot::new(row))
                }
                if show_divider {
                    <hr class="separator full-width" style="--border-style: solid" />
                }
                for row in unselected_rows {
                    (Slot::new(row))
                }
            </div>
        </menu>
    }
    .boxed()
}

/// `rooms/layouts/_form` around the access list, plus the delete panel
/// on editable rooms the caller administers.
#[allow(clippy::too_many_arguments)]
fn room_form_page(
    cx: &Cx,
    kind: Kind,
    editing: Option<i64>,
    name: String,
    can_administer: bool,
    csrf_token: String,
    access: BoxView<'static>,
    confirming: bool,
) -> BoxView<'static> {
    use crate::assets::*;
    let action = match editing {
        Some(id) => kind.member_path(id),
        None => kind.collection().to_string(),
    };
    let transition = match editing {
        Some(id) => format!("view-transition-name: edit-room-{id}"),
        None => "view-transition-name: new-room".to_string(),
    };
    let field = Slot::new(name_field_view(cx, name.clone(), can_administer));
    let access = Slot::new(access);
    let delete_label = format!("Delete {name}");
    let delete_action = editing.map(|id| format!("/rooms/{id}"));
    let edit_page = editing.map(|id| kind.edit_path(id)).unwrap_or_default();
    let trigger_href = format!("{edit_page}?confirm=delete-room");
    let room_dialog = Slot::new(crate::confirm::delete_dialog_view(
        cx,
        "delete-room",
        "Delete",
        "Delete this room?",
        "Are you sure you want to delete this room and all messages in it? This can't be undone.",
        confirming,
        &edit_page,
    ));
    view! {
        cx =>
        <section class="panel txt-align-center" style=(transition)>
            <form action=(action) accept-charset="UTF-8" method="post">
                if editing.is_some() {
                    <input type="hidden" name="_method" value="patch" />
                }
                <input type="hidden" name="authenticity_token" value=(csrf_token.clone()) />
                <div class="flex align-center gap">
                    (field)
                </div>
                <hr class="margin-block borderless" />
                <section class="room-access margin-block pad-inline fill-shade border-radius">
                    (access)
                </section>
                if can_administer {
                    <button name="button" type="submit" class="btn btn--reversed txt-large center">
                        <img aria-hidden="true" src=(img_check()) width="20" height="20" />
                        <span class="for-screen-reader">"Save"</span>
                    </button>
                }
            </form>
        </section>
        if editing.is_some() && can_administer {
            <section class="panel txt-align-center">
                <form id="delete-room" class="button_to" method="post" action=(delete_action.unwrap_or_default())>
                    <input type="hidden" name="_method" value="delete" />
                    <input type="hidden" name="authenticity_token" value=(csrf_token) />
                </form>
                <a class="btn btn--negative max-width" aria-label=(delete_label.clone()) data-tip=(delete_label) href=(trigger_href)>
                    <img aria-hidden="true" src=(img_trash()) width="20" height="20" />
                    <span class="overflow-ellipsis">(name)</span>
                </a>
                (room_dialog)
            </section>
        }
    }
    .boxed()
}

/// `rooms/directs/new`: the ping composer (autocomplete fills the
/// hidden `user_ids[]` select; without JS the required select blocks
/// empty submits, exactly like upstream).
fn directs_new_page(cx: &Cx, csrf_token: String) -> BoxView<'static> {
    use crate::assets::*;
    view! {
        cx =>
        <div id="direct_rooms_control" style="display: contents;">
            <div class="directs directs--new flex flex-column gap">
                <form class="flex gap flex-item-grow" action="/rooms/directs" accept-charset="UTF-8" method="post">
                    <input type="hidden" name="authenticity_token" value=(csrf_token) />
                    <a class="btn flex-item-no-shrink" href="/users/me/sidebar">
                        <img aria-hidden="true" src=(img_arrow_left()) />
                        <span class="for-screen-reader">"Cancel changes"</span>
                    </a>
                    <section class="autocomplete__container unpad input input--actor">
                        <div class="autocomplete__input input flex flex-wrap position-relative flex-item-grow">
                            <select name="user_ids[]" multiple="true" hidden="" required=""></select>
                            <input autocomplete="off" autocorrect="off" data-1p-ignore="true" class="autocomplete__input input flex flex-wrap position-relative" type="text" name="rooms_direct[user_ids_input]" id="rooms_direct_user_ids_input" />
                        </div>
                    </section>
                    <button name="button" type="submit" class="btn btn--reversed flex-item-no-shrink">
                        <img aria-hidden="true" src=(img_check()) />
                        <span class="for-screen-reader">"Start Ping"</span>
                    </button>
                </form>
                <span class="txt-small translucent pad-inline-half center">"Type names to ping someone…"</span>
            </div>
        </div>
    }
    .boxed()
}

/// `rooms/directs/edit`: the other members plus the delete-ping form.
fn directs_edit_page(
    cx: &Cx,
    room_id: i64,
    users: Vec<FormUserView>,
    csrf_token: String,
    confirming: bool,
) -> BoxView<'static> {
    use crate::assets::*;
    let action = Kind::Direct.member_path(room_id);
    let edit_page = Kind::Direct.edit_path(room_id);
    let trigger_href = format!("{edit_page}?confirm=delete-ping");
    let ping_dialog = Slot::new(crate::confirm::delete_dialog_view(
        cx,
        "delete-ping",
        "Delete",
        "Delete this ping?",
        "Are you sure you want to delete this ping and all messages in it? This can't be undone.",
        confirming,
        &edit_page,
    ));
    view! {
        cx =>
        <div class="panel txt-align-center">
            <section class="directs--edit margin-block-end">
                for user in users {
                    <div class="member flex flex-column gap fill-shade pad border-radius">
                        <figure class="avatar center" style="--avatar-border-radius: 10ch; --avatar-size: 10ch;">
                            <a title=(user.title.clone()) class="btn avatar" href=(user.path.clone())>
                                <img aria-hidden="true" loading="lazy" src=(user.avatar.clone()) width="48" height="48" />
                            </a>
                        </figure>
                        <strong>(user.name.clone())</strong>
                    </div>
                }
            </section>
            <form id="delete-ping" class="button_to" method="post" action=(action)>
                <input type="hidden" name="_method" value="delete" />
                <input type="hidden" name="authenticity_token" value=(csrf_token) />
            </form>
            <a class="btn btn--negative center" aria-label="Delete Ping" data-tip="Delete Ping" href=(trigger_href)>
                <img aria-hidden="true" src=(img_trash()) />
                "Ping"
            </a>
            (ping_dialog)
        </div>
    }
    .boxed()
}

// --- opens -----------------------------------------------------------

/// `rooms/opens#index`: inherited from `RoomsController`.
#[route(GET "/rooms/opens")]
pub async fn opens_index(cx: &Cx) -> Result<Response> {
    crate::rooms::index_redirect(cx).await
}

/// `rooms/opens#show`: remember the visit, redirect to the room.
#[route(GET "/rooms/opens/{id}")]
pub async fn opens_show(cx: &Cx) -> Result<Response> {
    typed_show(cx).await
}

/// `rooms/opens#new`: the open-room form (`New room` default).
#[route(GET "/rooms/opens/new")]
pub async fn opens_new(cx: &Cx) -> Result<Response> {
    typed_new(cx, Kind::Open).await
}

/// `rooms/opens#create`: create + grant every active user.
#[route(POST "/rooms/opens")]
pub async fn opens_create(cx: &Cx, body: Body) -> Result<Response> {
    let input = parse_room_form(&to_bytes(body, 1024 * 1024).await?);
    typed_create(cx, Kind::Open, input).await
}

/// `rooms/opens#edit`: the open-room form for an existing room.
#[route(GET "/rooms/opens/{id}/edit")]
pub async fn opens_edit(cx: &Cx) -> Result<Response> {
    typed_edit(cx, Kind::Open).await
}

/// `rooms/opens#update` + `#destroy`: one route dispatching on the
/// verb and `_method` (Rack applies the override before routing; here
/// the handler does). Update renames and/or converts to open; destroy
/// is `RoomsController`'s without `set_room`, a 500 for signed-in
/// callers (`undefined method 'destroy' for nil`).
#[route([POST, PATCH, PUT, DELETE] "/rooms/opens/{id}")]
pub async fn opens_modify(cx: &Cx, body: Body) -> Result<Response> {
    let input = parse_room_form(&to_bytes(body, 1024 * 1024).await?);
    typed_modify(cx, Kind::Open, input).await
}

// --- closeds ---------------------------------------------------------

/// `rooms/closeds#index`: inherited from `RoomsController`.
#[route(GET "/rooms/closeds")]
pub async fn closeds_index(cx: &Cx) -> Result<Response> {
    crate::rooms::index_redirect(cx).await
}

/// `rooms/closeds#show`: remember the visit, redirect to the room.
#[route(GET "/rooms/closeds/{id}")]
pub async fn closeds_show(cx: &Cx) -> Result<Response> {
    typed_show(cx).await
}

/// `rooms/closeds#new`: the closed-room form (`New room` default).
#[route(GET "/rooms/closeds/new")]
pub async fn closeds_new(cx: &Cx) -> Result<Response> {
    typed_new(cx, Kind::Closed).await
}

/// `rooms/closeds#create`: create + grant the submitted users.
#[route(POST "/rooms/closeds")]
pub async fn closeds_create(cx: &Cx, body: Body) -> Result<Response> {
    let input = parse_room_form(&to_bytes(body, 1024 * 1024).await?);
    typed_create(cx, Kind::Closed, input).await
}

/// `rooms/closeds#edit`: the closed-room form for an existing room.
#[route(GET "/rooms/closeds/{id}/edit")]
pub async fn closeds_edit(cx: &Cx) -> Result<Response> {
    typed_edit(cx, Kind::Closed).await
}

/// `rooms/closeds#update` + `#destroy`: update renames, converts to
/// closed, and revises the member list; destroy is a signed-in 500.
#[route([POST, PATCH, PUT, DELETE] "/rooms/closeds/{id}")]
pub async fn closeds_modify(cx: &Cx, body: Body) -> Result<Response> {
    let input = parse_room_form(&to_bytes(body, 1024 * 1024).await?);
    typed_modify(cx, Kind::Closed, input).await
}

// --- directs ---------------------------------------------------------

/// `rooms/directs#index`: inherited from `RoomsController`.
#[route(GET "/rooms/directs")]
pub async fn directs_index(cx: &Cx) -> Result<Response> {
    crate::rooms::index_redirect(cx).await
}

/// `rooms/directs#show`: an integer id redirects to the room page
/// (no membership or existence check); anything else 404s.
#[route(GET "/rooms/directs/{id}")]
pub async fn directs_show(cx: &Cx) -> Result<Response> {
    let Some(_) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let param = path_param_segment(cx, "id").to_owned();
    match cast_integer(&param) {
        Some(id) => see_other(format!("/rooms/{id}")).into_response(cx),
        None => Err(not_found().into()),
    }
}

/// `rooms/directs#new`: the ping composer (no creation gate: any
/// member can ping).
#[route(GET "/rooms/directs/new")]
pub async fn directs_new(cx: &Cx) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let Some(ctx) = page_ctx(cx, db, &user).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let admin = ctx.admin;
    let content = directs_new_page(cx, crate::csrf::issue(cx));
    shell_page(cx, "Topcamp".to_string(), admin, content, ctx, false)
        .async_into_response(cx)
        .await
}

/// `rooms/directs#create`: find or create the direct room for the
/// submitted users plus the caller.
#[route(POST "/rooms/directs")]
pub async fn directs_create(cx: &Cx, body: Body) -> Result<Response> {
    let input = parse_room_form(&to_bytes(body, 1024 * 1024).await?);
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    let db = &app_context::<AppState>(cx).db;
    let mut ids = submitted_ids(cx, &input);
    ids.push(user.id);
    ids.sort_unstable();
    ids.dedup();
    let users = UserRepository::where_ids(db, &ids)
        .await
        .map_err(http_error)?;
    let room_id = match RoomRepository::find_direct_for(db, &users)
        .await
        .map_err(http_error)?
    {
        Some(id) => id,
        None => {
            let room = RoomRepository::create(db, user.id, None, Kind::Direct.class_name())
                .await
                .map_err(http_error)?;
            MembershipRepository::grant_to(db, room.id, "everything", &users)
                .await
                .map_err(http_error)?;
            room.id
        }
    };
    // Each member's live sidebar rebuilds (UI-05r).
    let app = app_context::<AppState>(cx);
    crate::live::publish_member_rooms(&app.db, &app.bus, room_id).await?;
    see_other(format!("/rooms/{room_id}")).into_response(cx)
}

/// `rooms/directs#edit`: the other members (or self in a solo ping).
#[route(GET "/rooms/directs/{id}/edit")]
pub async fn directs_edit(cx: &Cx) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let param = path_param_segment(cx, "id").to_owned();
    let Some(room) = set_room(db, user.id, &param, true).await? else {
        return see_other("/").into_response(cx);
    };
    let members = UserRepository::members_of_room(db, room.id)
        .await
        .map_err(http_error)?;
    let shown: Vec<_> = if members.len() > 1 {
        members.into_iter().filter(|m| m.id != user.id).collect()
    } else {
        members
    };
    let active = UserRepository::active_ordered(db)
        .await
        .map_err(http_error)?;
    let views: Vec<FormUserView> = shown
        .iter()
        .filter_map(|member| {
            active
                .iter()
                .find(|form| form.id == member.id)
                .map(form_user_view)
        })
        .collect();
    let display = display_name(&views, &user.name);
    let Some(ctx) = page_ctx(cx, db, &user).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let admin = ctx.admin;
    let query = query_params::<crate::confirm::ConfirmQuery>(cx)?;
    let confirming = crate::confirm::confirming(query.confirm.as_deref(), "delete-ping");
    let content = directs_edit_page(cx, room.id, views, crate::csrf::issue(cx), confirming);
    shell_page(
        cx,
        format!("Edit settings for {display}"),
        admin,
        content,
        ctx,
        true,
    )
    .async_into_response(cx)
    .await
}

/// `rooms/directs#update` + `#destroy`: updates were never drawn
/// (`ActionNotFound`, even signed out); destroys delete the ping for
/// any member.
#[route([POST, PATCH, PUT, DELETE] "/rooms/directs/{id}")]
pub async fn directs_modify(cx: &Cx, body: Body) -> Result<Response> {
    let input = parse_room_form(&to_bytes(body, 1024 * 1024).await?);
    typed_modify(cx, Kind::Direct, input).await
}

// --- shared action bodies --------------------------------------------

/// Typed `show` for opens/closeds: membership-scoped, directs
/// excluded; failures redirect home.
async fn typed_show(cx: &Cx) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let param = path_param_segment(cx, "id").to_owned();
    let Some(room) = set_room(db, user.id, &param, false).await? else {
        return see_other("/").into_response(cx);
    };
    remember_last_room(cx, room.id);
    see_other(format!("/rooms/{}", room.id)).into_response(cx)
}

/// Typed `new`: the form with every active user listed.
async fn typed_new(cx: &Cx, kind: Kind) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let admin = user.role == UserRole::Administrator.value();
    ensure_can_create(db, admin).await?;
    let users = UserRepository::active_ordered(db)
        .await
        .map_err(http_error)?;
    let views: Vec<FormUserView> = users.iter().map(form_user_view).collect();
    let (selected, unselected) = match kind {
        Kind::Open => (Vec::new(), views),
        Kind::Closed => partition_users(views, &[]),
        Kind::Direct => unreachable!("directs have their own new page"),
    };
    let access = match kind {
        Kind::Open => open_access_list(cx, unselected, true, Kind::Closed.new_path()),
        Kind::Closed => closed_access_list(
            cx,
            selected,
            unselected,
            true,
            &user.id.to_string(),
            false,
            Kind::Open.new_path(),
        ),
        Kind::Direct => unreachable!("directs have their own new page"),
    };
    let Some(ctx) = page_ctx(cx, db, &user).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let content = room_form_page(
        cx,
        kind,
        None,
        "New room".to_string(),
        true,
        crate::csrf::issue(cx),
        access,
        false,
    );
    shell_page(cx, "New chat room".to_string(), admin, content, ctx, true)
        .async_into_response(cx)
        .await
}

/// Typed `create`: the row plus memberships, then the room page.
async fn typed_create(cx: &Cx, kind: Kind, input: RoomForm) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    let db = &app_context::<AppState>(cx).db;
    let admin = user.role == UserRole::Administrator.value();
    ensure_can_create(db, admin).await?;
    let name = room_name(&input)?;
    let room = RoomRepository::create(db, user.id, name.as_deref(), kind.class_name())
        .await
        .map_err(http_error)?;
    match kind {
        Kind::Open => {
            let users = UserRepository::active_ordered(db)
                .await
                .map_err(http_error)?;
            let ids: Vec<i64> = users.iter().map(|form| form.id).collect();
            MembershipRepository::grant_to(db, room.id, "mentions", &ids)
                .await
                .map_err(http_error)?;
        }
        Kind::Closed => {
            let grantees = UserRepository::where_ids(db, &submitted_ids(cx, &input))
                .await
                .map_err(http_error)?;
            MembershipRepository::grant_to(db, room.id, "mentions", &grantees)
                .await
                .map_err(http_error)?;
        }
        Kind::Direct => unreachable!("directs have their own create"),
    }
    // Live sidebars rebuild (UI-05r).
    let app = app_context::<AppState>(cx);
    match kind {
        Kind::Open => crate::live::publish_open_rooms(&app.bus),
        Kind::Closed => {
            crate::live::publish_member_rooms(&app.db, &app.bus, room.id).await?;
        }
        Kind::Direct => unreachable!("directs have their own create"),
    }
    see_other(format!("/rooms/{}", room.id)).into_response(cx)
}

/// Typed `edit`: the form for an existing room (the posted type wins,
/// so an open room edited as closed converts on save).
async fn typed_edit(cx: &Cx, kind: Kind) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let param = path_param_segment(cx, "id").to_owned();
    let Some(room) = set_room(db, user.id, &param, false).await? else {
        return see_other("/").into_response(cx);
    };
    let admin = user.role == UserRole::Administrator.value();
    let can_administer = admin || room.creator_id == user.id;
    let users = UserRepository::active_ordered(db)
        .await
        .map_err(http_error)?;
    let views: Vec<FormUserView> = users.iter().map(form_user_view).collect();
    let name = room.name.clone().unwrap_or_default();
    let (access, title) = match kind {
        Kind::Open => (
            open_access_list(cx, views, can_administer, Kind::Closed.edit_path(room.id)),
            format!("Edit settings for {name}"),
        ),
        Kind::Closed => {
            let member_ids = MembershipRepository::member_user_ids(db, room.id)
                .await
                .map_err(http_error)?;
            let (selected, unselected) = partition_users(views, &member_ids);
            (
                closed_access_list(
                    cx,
                    selected,
                    unselected,
                    can_administer,
                    &user.id.to_string(),
                    true,
                    Kind::Open.edit_path(room.id),
                ),
                format!("Edit settings for {name}"),
            )
        }
        Kind::Direct => unreachable!("directs have their own edit page"),
    };
    let Some(ctx) = page_ctx(cx, db, &user).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let query = query_params::<crate::confirm::ConfirmQuery>(cx)?;
    let confirming = crate::confirm::confirming(query.confirm.as_deref(), "delete-room");
    let content = room_form_page(
        cx,
        kind,
        Some(room.id),
        name,
        can_administer,
        crate::csrf::issue(cx),
        access,
        confirming,
    );
    shell_page(cx, title, admin, content, ctx, true)
        .async_into_response(cx)
        .await
}

/// Member-path dispatcher: routing-level 404s first (unknown POST
/// overrides, directs updates), then auth, then the verb/override.
async fn typed_modify(cx: &Cx, kind: Kind, input: RoomForm) -> Result<Response> {
    use http::Method;
    let method = request::method(cx);
    let over = input.method_override.as_deref();
    if method == Method::POST && !matches!(over, Some("patch" | "delete")) {
        return Err(not_found().into());
    }
    if kind == Kind::Direct
        && (method == Method::PATCH || method == Method::PUT || over == Some("patch"))
    {
        return Err(not_found().into());
    }
    if method == Method::DELETE || over == Some("delete") {
        return typed_destroy(cx, kind, &input).await;
    }
    typed_update(cx, kind, input).await
}

/// Typed `destroy`: opens/closeds 500 past auth; directs delete the
/// ping for any member (token-checked).
async fn typed_destroy(cx: &Cx, kind: Kind, input: &RoomForm) -> Result<Response> {
    if kind != Kind::Direct {
        return destroy_without_room(cx).await;
    }
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, input) {
        return Err(forbidden().into());
    }
    let db = &app_context::<AppState>(cx).db;
    let param = path_param_segment(cx, "id").to_owned();
    let Some(room) = set_room(db, user.id, &param, true).await? else {
        return see_other("/").into_response(cx);
    };
    // Publish BEFORE the destroy (memberships still addressable).
    let app = app_context::<AppState>(cx);
    crate::live::publish_member_rooms(&app.db, &app.bus, room.id).await?;
    destroy_room(db, room.id).await?;
    see_other("/").into_response(cx)
}

/// Typed `update`: rename, convert the type, and (closed) revise the
/// member list. Only changed columns write.
async fn typed_update(cx: &Cx, kind: Kind, input: RoomForm) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    let db = &app_context::<AppState>(cx).db;
    let param = path_param_segment(cx, "id").to_owned();
    let Some(room) = set_room(db, user.id, &param, false).await? else {
        return see_other("/").into_response(cx);
    };
    let admin = user.role == UserRole::Administrator.value();
    ensure_can_administer(admin, user.id, &room)?;
    let name = room_name(&input)?;
    let name_changed = name.as_ref() != room.name.as_ref();
    let kind_changed = room.kind != kind.class_name();
    if name_changed || kind_changed {
        RoomRepository::update(
            db,
            room.id,
            name_changed.then(|| name.clone().unwrap_or_default()),
            kind_changed.then_some(kind.class_name()),
        )
        .await
        .map_err(http_error)?;
    }
    if kind == Kind::Closed {
        let wanted = submitted_ids(cx, &input);
        let granted = UserRepository::where_ids(db, &wanted)
            .await
            .map_err(http_error)?;
        let current = MembershipRepository::member_user_ids(db, room.id)
            .await
            .map_err(http_error)?;
        let revoked: Vec<i64> = current
            .into_iter()
            .filter(|id| !wanted.contains(id))
            .collect();
        MembershipRepository::grant_to(db, room.id, "mentions", &granted)
            .await
            .map_err(http_error)?;
        MembershipRepository::revoke_from(db, room.id, &revoked)
            .await
            .map_err(http_error)?;
    }
    // Live sidebars rebuild (UI-05r).
    let app = app_context::<AppState>(cx);
    match kind {
        Kind::Open => crate::live::publish_open_rooms(&app.bus),
        Kind::Closed => {
            crate::live::publish_member_rooms(&app.db, &app.bus, room.id).await?;
        }
        Kind::Direct => unreachable!("directs have no update"),
    }
    see_other(format!("/rooms/{}", room.id)).into_response(cx)
}

/// `destroy_without_room`: signed-in callers 500 (the action derefs a
/// room `set_room` never loaded).
async fn destroy_without_room(cx: &Cx) -> Result<Response> {
    use topcamp_domain::error::{Error as DomainError, InfrastructureError};
    let Some(_) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    Err(http_error(DomainError::Infrastructure(
        InfrastructureError::new("rooms"),
    )))
}

/// `destroy_room`: memberships, messages, and the row go, in order.
async fn destroy_room(db: &PgDb, room_id: i64) -> Result<()> {
    MembershipRepository::delete_for_room(db, room_id)
        .await
        .map_err(http_error)?;
    for message_id in topcamp_db::repositories::MessageRepository::ids_for_room(db, room_id)
        .await
        .map_err(http_error)?
    {
        topcamp_db::repositories::MessageRepository::destroy(db, message_id)
            .await
            .map_err(http_error)?;
    }
    RoomRepository::destroy(db, room_id)
        .await
        .map_err(http_error)?;
    Ok(())
}

/// Partition form users into selected members and the rest.
fn partition_users(
    users: Vec<FormUserView>,
    member_ids: &[i64],
) -> (Vec<FormUserView>, Vec<FormUserView>) {
    let (selected, unselected): (Vec<_>, Vec<_>) = users.into_iter().partition(|user| {
        user.id
            .parse::<i64>()
            .is_ok_and(|id| member_ids.contains(&id))
    });
    (selected, unselected)
}

/// `room_display_name` for a direct room: the other members' names as
/// a sentence, else the caller's own.
fn display_name(users: &[FormUserView], fallback: &str) -> String {
    if users.is_empty() {
        return fallback.to_string();
    }
    let names: Vec<&str> = users.iter().map(|user| user.name.as_str()).collect();
    match names.as_slice() {
        [one] => one.to_string(),
        [one, two] => format!("{one} and {two}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
        [] => fallback.to_string(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn form_user(name: &str, bio: Option<&str>) -> FormUser {
        FormUser {
            id: 7,
            name: name.to_string(),
            bio: bio.map(str::to_string),
            updated_number: "20260926130020".to_string(),
        }
    }

    #[test]
    fn kind_paths_match_upstream_routes() {
        assert_eq!(Kind::Open.new_path(), "/rooms/opens/new");
        assert_eq!(Kind::Closed.member_path(3), "/rooms/closeds/3");
        assert_eq!(Kind::Direct.edit_path(9), "/rooms/directs/9/edit");
        assert_eq!(Kind::Open.class_name(), "Rooms::Open");
        assert_eq!(Kind::Closed.class_name(), "Rooms::Closed");
        assert_eq!(Kind::Direct.class_name(), "Rooms::Direct");
    }

    #[test]
    fn titles_join_name_and_bio() {
        assert_eq!(form_user_view(&form_user("David", None)).title, "David");
        assert_eq!(
            form_user_view(&form_user("David", Some("Founder"))).title,
            "David – Founder"
        );
        assert_eq!(
            form_user_view(&form_user("David", Some("  "))).title,
            "David"
        );
    }

    #[test]
    fn direct_display_names_sentence_join() {
        let view = |name: &str| form_user_view(&form_user(name, None));
        assert_eq!(display_name(&[], "Me"), "Me");
        assert_eq!(display_name(&[view("Ann")], "Me"), "Ann");
        assert_eq!(display_name(&[view("Ann"), view("Bo")], "Me"), "Ann and Bo");
        assert_eq!(
            display_name(&[view("Ann"), view("Bo"), view("Cy")], "Me"),
            "Ann, Bo, and Cy"
        );
    }
}
