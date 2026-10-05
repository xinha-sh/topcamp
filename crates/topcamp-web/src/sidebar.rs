//! Users sidebar: the room list, direct pings, and tool links.
//!
//! Upstream `Users::SidebarsController#show` + the `user_sidebar` frame:
//! directs sorted by recency, shared rooms alphabetical, placeholder
//! pings for users without a direct room yet, the new-room button (when
//! the account lets this user create rooms), and the profile/account
//! tools. Signed-in pages server-render the frame inline and the
//! endpoint serves the identical markup. Both lists are Topcoat-native
//! live regions (UI-05r): they rebuild on user/global sidebar events,
//! replacing the Turbo frame + cable stream sources.

use topcamp_db::PgDb;
use topcamp_db::repositories::{
    AccountRepository, AvatarUser, MembershipRepository, SidebarUser, UserRepository,
};
use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        Slot,
        error::see_other,
        response::{AsyncIntoResponse, IntoResponse, Response},
        route,
    },
    runtime::{PrefetchMode, connected, link_attrs},
    view::{BoxView, HoistView, ViewExt as _, emit, live, view},
};

use crate::live::{LiveBus, UserEvent};
use crate::state::{AppState, http_error};

/// A direct room: recency key for client sorting plus the members shown
/// (everyone but self, or self when pinging an empty room).
#[derive(Clone, Debug, PartialEq)]
pub struct DirectEntry {
    pub room_id: i64,
    pub unread: bool,
    pub epoch_ms: i64,
    pub members: Vec<SidebarUser>,
}

/// A shared room: STI suffix for the `list_rooms_<type>_<id>` anchor.
#[derive(Clone, Debug, PartialEq)]
pub struct SharedEntry {
    pub room_id: i64,
    pub suffix: &'static str,
    pub name: String,
    pub unread: bool,
}

/// Everything the frame renders for one signed-in user.
pub struct SidebarData {
    pub me: AvatarUser,
    pub directs: Vec<DirectEntry>,
    pub others: Vec<SharedEntry>,
    pub placeholders: Vec<SidebarUser>,
    pub can_create: bool,
}

/// `dom_id` type segment: `Rooms::Open` → `open`.
pub(crate) fn type_suffix(room_type: &str) -> &'static str {
    match room_type {
        "Rooms::Open" => "open",
        "Rooms::Closed" => "closed",
        "Rooms::Direct" => "direct",
        _ => "open",
    }
}

/// `name.split(' ')`: ASCII-whitespace runs, empties dropped.
fn name_parts(name: &str) -> impl Iterator<Item = &str> {
    name.split(|c: char| c.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
}

/// `UserSummary#first_name`.
fn first_name(name: &str) -> &str {
    name_parts(name).next().unwrap_or("")
}

/// `String#capitalize` (first up, rest down).
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

/// One member's contribution to a group ping label: the first letter of
/// each of the first three words, capitalized (`David Heinemeier Hansson`
/// → `DHH`).
fn member_code(name: &str) -> String {
    name_parts(name)
        .take(3)
        .map(|part| capitalize(&part.chars().take(1).collect::<String>()))
        .collect()
}

/// `to_sentence(two_words_connector: '+')`: `AB+CD`, else Oxford commas.
fn to_sentence(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [one, two] => format!("{one}+{two}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// `SidebarMembership#member_initials`: group ping label over every
/// member (the avatar stack shows only the first four).
fn member_initials(members: &[SidebarUser]) -> String {
    to_sentence(
        &members
            .iter()
            .map(|member| member_code(&member.name))
            .collect::<Vec<_>>(),
    )
}

/// `Users::SidebarsController#show`: visible memberships partitioned
/// into recency-sorted directs and alpha-sorted shared rooms, plus the
/// oldest users missing a direct room (capped so directs + placeholders
/// never exceed twenty).
pub async fn assemble(db: &PgDb, me: &AvatarUser, admin: bool) -> Result<SidebarData> {
    let direct_class = topcamp_domain::rooms::RoomType::Direct.class_name();
    let mut directs = Vec::new();
    let mut others = Vec::new();
    let mut direct_ids = vec![me.id];
    for membership in MembershipRepository::visible_with_rooms(db, me.id)
        .await
        .map_err(http_error)?
    {
        if membership.room_type == direct_class {
            let members = UserRepository::members_of_room(db, membership.room_id)
                .await
                .map_err(http_error)?;
            direct_ids.extend(members.iter().map(|user| user.id));
            let mut shown: Vec<SidebarUser> = members
                .into_iter()
                .filter(|user| user.id != me.id)
                .collect();
            if shown.is_empty() {
                shown.push(SidebarUser {
                    id: me.id,
                    name: me.name.clone(),
                    updated_number: me.updated_number.clone(),
                });
            }
            directs.push(DirectEntry {
                room_id: membership.room_id,
                unread: membership.unread,
                epoch_ms: membership.room_updated_epoch,
                members: shown,
            });
        } else {
            others.push(SharedEntry {
                room_id: membership.room_id,
                suffix: type_suffix(&membership.room_type),
                name: membership.room_name.clone().unwrap_or_default(),
                unread: membership.unread,
            });
        }
    }
    directs.sort_by_key(|entry| std::cmp::Reverse(entry.epoch_ms));
    direct_ids.sort_unstable();
    direct_ids.dedup();
    let limit = (20 - direct_ids.len() as i64).max(0);
    let placeholders = UserRepository::direct_placeholders(db, &direct_ids, limit)
        .await
        .map_err(http_error)?;
    let restricted = AccountRepository::room_creation_restricted(db)
        .await
        .map_err(http_error)?;
    Ok(SidebarData {
        me: me.clone(),
        directs,
        others,
        placeholders,
        can_create: admin || !restricted,
    })
}

/// Everything a sidebar list region needs to rebuild, owned.
#[derive(Clone)]
pub(crate) struct SidebarCtx {
    pub db: PgDb,
    pub bus: LiveBus,
    pub me: AvatarUser,
    pub admin: bool,
}

fn directs_slots(cx: &Cx, entries: &[DirectEntry]) -> Vec<Slot<'static>> {
    entries
        .iter()
        .map(|entry| Slot::new(direct_room_entry_view(cx, entry)))
        .collect()
}

fn shared_slots(cx: &Cx, entries: &[SharedEntry]) -> Vec<Slot<'static>> {
    entries
        .iter()
        .map(|entry| Slot::new(shared_room_entry_view(cx, entry)))
        .collect()
}

/// Wait for the next event on either sidebar channel (receivers live
/// across calls, so nothing is missed between rebuilds). Any event
/// rebuilds; `Lagged` refetches on the same path; either channel
/// closing ends the region (only the bus dropping closes a channel).
async fn sidebar_wake(
    rx_user: &mut tokio::sync::broadcast::Receiver<UserEvent>,
    rx_global: &mut tokio::sync::broadcast::Receiver<UserEvent>,
) -> bool {
    use tokio::sync::broadcast::error::RecvError;
    tokio::select! {
        event = rx_user.recv() => !matches!(event, Err(RecvError::Closed)),
        event = rx_global.recv() => !matches!(event, Err(RecvError::Closed)),
    }
}

/// The directs list, live: rebuilds on user/global sidebar events
/// (pips, new pings, membership changes), skipping identical renders.
pub(crate) fn directs_live(
    cx: &Cx,
    ctx: SidebarCtx,
    initial: Vec<DirectEntry>,
) -> BoxView<'static> {
    let cx = cx.clone();
    live! { cx =>
        let mut shown = initial;
        let slots = directs_slots(&cx, &shown);
        let token = emit! {
            <div id="direct_rooms" contents="">
                for slot in slots {
                    (slot)
                }
            </div>
        }?;
        if !connected(&cx) {
            return Ok(token);
        }
        let mut rx_user = ctx.bus.user(ctx.me.id).subscribe();
        let mut rx_global = ctx.bus.global().subscribe();
        loop {
            if !sidebar_wake(&mut rx_user, &mut rx_global).await {
                return Ok(token);
            }
            let data = assemble(&ctx.db, &ctx.me, ctx.admin).await?;
            if data.directs != shown {
                shown = data.directs;
                let slots = directs_slots(&cx, &shown);
                let token = emit! {
                    <div id="direct_rooms" contents="">
                        for slot in slots {
                            (slot)
                        }
                    </div>
                }?;
                let _ = token;
            }
        }
    }
    .boxed()
}

/// The shared-rooms list, live: same wakeups as [`directs_live`].
pub(crate) fn shared_live(cx: &Cx, ctx: SidebarCtx, initial: Vec<SharedEntry>) -> BoxView<'static> {
    let cx = cx.clone();
    live! { cx =>
        let mut shown = initial;
        let slots = shared_slots(&cx, &shown);
        let token = emit! {
            <div id="shared_rooms" contents="">
                for slot in slots {
                    (slot)
                }
            </div>
        }?;
        if !connected(&cx) {
            return Ok(token);
        }
        let mut rx_user = ctx.bus.user(ctx.me.id).subscribe();
        let mut rx_global = ctx.bus.global().subscribe();
        loop {
            if !sidebar_wake(&mut rx_user, &mut rx_global).await {
                return Ok(token);
            }
            let data = assemble(&ctx.db, &ctx.me, ctx.admin).await?;
            if data.others != shown {
                shown = data.others;
                let slots = shared_slots(&cx, &shown);
                let token = emit! {
                    <div id="shared_rooms" contents="">
                        for slot in slots {
                            (slot)
                        }
                    </div>
                }?;
                let _ = token;
            }
        }
    }
    .boxed()
}

/// Pre-rendered strings per direct entry, so the template only
/// interpolates (attribute `if`s stay out of the macro).
struct DirectView {
    class: &'static str,
    link_id: String,
    room_id: String,
    href: String,
    label: String,
    members: Vec<MemberView>,
}

/// One avatar in a direct entry (the group stack takes the first four).
struct MemberView {
    avatar: String,
}

fn direct_view(entry: &DirectEntry) -> DirectView {
    DirectView {
        class: if entry.unread {
            "direct unread"
        } else {
            "direct"
        },
        link_id: format!("list_rooms_direct_{}", entry.room_id),
        room_id: entry.room_id.to_string(),
        href: format!("/rooms/{}", entry.room_id),
        label: if entry.members.len() > 1 {
            member_initials(&entry.members)
        } else {
            entry
                .members
                .first()
                .map(|member| first_name(&member.name).to_string())
                .unwrap_or_default()
        },
        members: entry
            .members
            .iter()
            .map(|member| MemberView {
                avatar: crate::users::avatar_url(member),
            })
            .collect(),
    }
}

struct SharedView {
    link_id: String,
    room_id: String,
    name: String,
    class: &'static str,
    href: String,
}

fn shared_view(entry: &SharedEntry) -> SharedView {
    SharedView {
        link_id: format!("list_rooms_{}_{}", entry.suffix, entry.room_id),
        room_id: entry.room_id.to_string(),
        name: entry.name.clone(),
        class: if entry.unread {
            "align-center gap room btn txt-nowrap unread"
        } else {
            "align-center gap room btn txt-nowrap"
        },
        href: format!("/rooms/{}", entry.room_id),
    }
}

struct PlaceholderView {
    action: String,
    avatar: String,
    label: String,
}

fn placeholder_view(user: &SidebarUser) -> PlaceholderView {
    PlaceholderView {
        action: format!("/rooms/directs?user_ids%5B%5D={}", user.id),
        avatar: crate::users::avatar_url(user),
        label: first_name(&user.name).to_string(),
    }
}

/// One shared-room anchor (`users/sidebars/rooms/_shared`): the
/// sidebar list item, also prepended/replaced over the rooms streams.
/// Client-navigated (`link_attrs`) with prefetch off: rendering a room
/// marks it read (tail presence clears `unread_at`), so a speculative
/// prefetch would unpip rooms the user never opens.
pub(crate) fn shared_room_entry_view(cx: &Cx, entry: &SharedEntry) -> BoxView<'static> {
    let item = shared_view(entry);
    let attrs = link_attrs(cx, item.href.clone(), PrefetchMode::Never);
    view! {
        cx =>
        <a id=(item.link_id.clone()) data-room-id=(item.room_id.clone()) style="--column-gap: 0.5em" class=(item.class) (attrs)>
            <span class="overflow-ellipsis">(item.name.clone())</span>
        </a>
    }
    .boxed()
}

/// One direct-room anchor (`users/sidebars/rooms/_direct`): the ping
/// entry, also prepended over each member's rooms stream.
/// Client-navigated with prefetch off, like the shared entries: opening
/// a room marks it read, so prefetching would unpip unvisited rooms.
pub(crate) fn direct_room_entry_view(cx: &Cx, entry: &DirectEntry) -> BoxView<'static> {
    let item = direct_view(entry);
    let attrs = link_attrs(cx, item.href.clone(), PrefetchMode::Never);
    view! {
        cx =>
        <a class=(item.class) id=(item.link_id.clone()) data-room-id=(item.room_id.clone()) (attrs)>
            if item.members.len() > 1 {
                <div class="avatar__group">
                    for member in item.members.iter().take(4) {
                        <span class="avatar">
                            <img aria-hidden="true" src=(member.avatar.clone()) width="20" height="20" />
                        </span>
                    }
                </div>
            } else {
                <span class="avatar">
                    for member in item.members.iter().take(1) {
                        <img aria-hidden="true" src=(member.avatar.clone()) width="48" height="48" />
                    }
                </span>
            }
            <span class="direct__author flex align-center gap max-width min-width border-radius txt-small">
                <span class="txt-nowrap overflow-ellipsis">
                    <span class="for-screen-reader">"Ping with"</span>
                    (item.label.clone())
                </span>
            </span>
        </a>
    }
    .boxed()
}

/// The frame's inner content: directs, shared rooms, toggle, tools.
/// Owned data renders a `'static` view (the shell outlives the handler).
/// Both lists are live regions; the rest is static chrome.
pub(crate) fn sidebar_inner(
    cx: &Cx,
    ctx: SidebarCtx,
    data: SidebarData,
    csrf_token: String,
) -> BoxView<'static> {
    use crate::assets::*;
    let directs = Slot::new(directs_live(cx, ctx.clone(), data.directs));
    let others = Slot::new(shared_live(cx, ctx, data.others));
    let placeholders: Vec<PlaceholderView> =
        data.placeholders.iter().map(placeholder_view).collect();
    let me_avatar = crate::users::avatar_url(&SidebarUser {
        id: data.me.id,
        name: data.me.name.clone(),
        updated_number: data.me.updated_number.clone(),
    });
    let me_transition = format!("view-transition-name: avatar-{}", data.me.id);
    let can_create = data.can_create;
    view! {
        cx =>
        <div class="sidebar__container overflow-y overflow-hide-scrollbar">
            <div id="direct_rooms_control">
                <div class="directs gap overflow-x overflow-hide-scrollbar">
                    <a class="direct direct__new" href="/rooms/directs/new">
                        <span class="avatar avatar--icon">
                            <img aria-hidden="true" class="colorize--black" src=(img_messages_add()) width="20" height="20" />
                        </span>
                        <span class="direct__author flex max-width min-width border-radius pad-inline-half">
                            <span class="for-screen-reader">"New"</span>
                            <span class="txt-small overflow-clip">"Ping"</span>
                        </span>
                    </a>
                    (directs)
                    <div contents="">
                        for placeholder in &placeholders {
                            <form class="button_to" method="post" action=(placeholder.action.clone())>
                                <button class="direct borderless fill-transparent unpad" type="submit">
                                    <span class="avatar">
                                        <img aria-hidden="true" src=(placeholder.avatar.clone()) />
                                    </span>
                                    <span class="direct__author flex align-center gap max-width min-width border-radius txt-small">
                                        <span class="txt-nowrap overflow-ellipsis">
                                            <span class="for-screen-reader">"Start a ping with"</span>
                                            (placeholder.label.clone())
                                        </span>
                                    </span>
                                </button>
                                <input type="hidden" name="authenticity_token" value=(csrf_token.to_string()) />
                            </form>
                        }
                    </div>
                </div>
            </div>
            <div class="rooms position-relative flex flex-column gap">
                (others)
                if can_create {
                    <a class="rooms__new-btn btn room align-center gap txt-reversed" aria-label="New Chat Room" href="/rooms/opens/new">
                        <img aria-hidden="true" style="view-transition-name: new-room" src=(img_add()) width="20" height="20" />
                    </a>
                }
            </div>
            // Explicit close (light-dismiss and Esc also work): the open
            // toggle lives outside the popover in the shell.
            <button class="btn sidebar__close" popovertarget="sidebar" aria-label="Close menu" data-tip="Close menu">
                <img aria-hidden="true" src=(img_remove()) width="20" height="20" />
                <span class="for-screen-reader">"Close menu"</span>
            </button>
        </div>
        <div class="flex align-end sidebar__tools gap justify-end">
            <a class="btn avatar flex-item-no-shrink sidebar__tool" href="/users/me/profile">
                <img aria-hidden="true" style=(me_transition) src=(me_avatar) width="48" height="48" />
                <span class="for-screen-reader">"My Settings"</span>
            </a>
            <a class="btn align-center gap txt-reversed sidebar__tool" href="/account/edit">
                <img aria-hidden="true" style="view-transition-name: account-settings" src=(img_settings()) width="20" height="20" />
                <span class="for-screen-reader">"Account Settings"</span>
            </a>
        </div>
    }
    .boxed()
}

/// The `user_sidebar` frame: layout-embedded pages carry the `src`
/// the lazy load would fetch; the endpoint response is the frame alone.
/// A plain `div` now (the live list regions replaced the Turbo frame +
/// cable stream sources); the `src` stays as an inert marker.
pub(crate) async fn sidebar_frame(
    cx: &Cx,
    ctx: SidebarCtx,
    csrf_token: String,
    embedded: bool,
) -> Result<BoxView<'static>> {
    let data = assemble(&ctx.db, &ctx.me, ctx.admin).await?;
    let inner = Slot::new(sidebar_inner(cx, ctx, data, csrf_token));
    if embedded {
        Ok(view! {
            cx =>
            <div id="user_sidebar" src="/users/me/sidebar">
                (inner)
            </div>
        }
        .boxed())
    } else {
        Ok(view! {
            cx =>
            <div id="user_sidebar">
                (inner)
            </div>
        }
        .boxed())
    }
}

/// `Users::SidebarsController#show`: the frame document (csrf head +
/// frame body, matching upstream's frame layout).
#[route(GET "/users/me/sidebar")]
pub async fn sidebar_show(cx: &Cx) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let Some(me) = UserRepository::find_avatar_user(db, user.id)
        .await
        .map_err(http_error)?
    else {
        return see_other("/session/new").into_response(cx);
    };
    let admin = user.role == topcamp_domain::auth::UserRole::Administrator.value();
    let app = app_context::<AppState>(cx);
    let ctx = SidebarCtx {
        db: app.db.clone(),
        bus: app.bus.clone(),
        me: me.clone(),
        admin,
    };
    let csrf_token = crate::csrf::issue(cx);
    let frame = Slot::new(sidebar_frame(cx, ctx, csrf_token.clone(), false).await?);
    HoistView::new(view! {
        cx =>
        <html>
            <head>
                <meta name="csrf-param" content="authenticity_token" />
                <meta name="csrf-token" content=(csrf_token) />
            </head>
            <body>
                (frame)
            </body>
        </html>
    })
    .boxed()
    .async_into_response(cx)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: i64, name: &str) -> SidebarUser {
        SidebarUser {
            id,
            name: name.to_string(),
            updated_number: "20260926130020".to_string(),
        }
    }

    #[test]
    fn first_name_is_the_first_word() {
        assert_eq!(first_name("Kevin Rood"), "Kevin");
        assert_eq!(first_name("  Anna"), "Anna");
        assert_eq!(first_name(""), "");
    }

    #[test]
    fn group_labels_join_member_codes() {
        let solo = vec![member(1, "Kevin Rood")];
        assert_eq!(member_initials(&solo), "KR");
        let pair = vec![member(1, "Kevin Rood"), member(2, "Jason Fried")];
        assert_eq!(member_initials(&pair), "KR+JF");
        let trio = vec![
            member(1, "Anna Doe"),
            member(2, "Bo Zhang"),
            member(3, "Cy Lee"),
        ];
        assert_eq!(member_initials(&trio), "AD, BZ, and CL");
        assert_eq!(member_initials(&[]), "");
    }

    #[test]
    fn member_codes_take_three_words_first_letters() {
        assert_eq!(member_code("David Heinemeier Hansson"), "DHH");
        assert_eq!(member_code("david heinemeier hansson extra"), "DHH");
        assert_eq!(member_code("Bender"), "B");
    }

    #[test]
    fn type_suffixes_match_dom_ids() {
        assert_eq!(type_suffix("Rooms::Open"), "open");
        assert_eq!(type_suffix("Rooms::Closed"), "closed");
        assert_eq!(type_suffix("Rooms::Direct"), "direct");
    }
}
