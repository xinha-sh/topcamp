//! `AccountsController` (UI-07.1): the settings page, rename /
//! settings updates, people-list role changes + removal, and
//! join-code regeneration. Logo upload/destroy + variants ride
//! UI-07.2; the QR route lands with UI-11, bots + custom styles
//! with UI-13 (their nav links already point there).
//!
//! No `GET /account`: upstream's show is `action_not_found` — the
//! settings page is `GET /account/edit`, sliced 500 users per
//! `?page=`. Without Turbo the next-page container is a plain
//! full-page link instead of a lazy frame.

use std::collections::HashMap;

use topcoat::{
    Result,
    context::{Cx, app_context},
    cookie::{Cookie, Cookies, SameSite, cookies},
    router::{
        Body, Slot,
        error::{bad_request, forbidden, not_found, see_other},
        path_param_segment, query_params, request,
        response::{AsyncIntoResponse, IntoResponse, Response},
        route, to_bytes,
    },
    view::ViewExt as _,
};

use topcamp_db::PgDb;
use topcamp_db::repositories::{
    AccountRepository, AttachmentRepository, FormUser, UserRepository, UserRow,
};
use topcamp_domain::auth::{UserRole, UserStatus};

use crate::pages::{
    ShellContext, THEME_COOKIE, body_classes, document_shell, theme as request_theme,
};
use crate::rooms::cast_integer;
use crate::state::{AppState, http_error};

/// `account[name]` / `account[settings][…]` / `user[role]` submission.
/// `account_present` mirrors `params.require("account")`.
struct AccountForm {
    account_present: bool,
    name: Option<String>,
    custom_styles: Option<String>,
    restrict_room_creation: Option<String>,
    role: Option<String>,
    authenticity_token: Option<String>,
    method_override: Option<String>,
}

fn parse_account_form(raw: &[u8]) -> AccountForm {
    let mut form = AccountForm {
        account_present: false,
        name: None,
        custom_styles: None,
        restrict_room_creation: None,
        role: None,
        authenticity_token: None,
        method_override: None,
    };
    let Ok(pairs) = serde_urlencoded::from_bytes::<Vec<(String, String)>>(raw) else {
        return form;
    };
    for (key, value) in pairs {
        match key.as_str() {
            "account[name]" => {
                form.account_present = true;
                form.name = Some(value);
            }
            "account[custom_styles]" => {
                form.account_present = true;
                form.custom_styles = Some(value);
            }
            "account[settings][restrict_room_creation_to_administrators]" => {
                form.account_present = true;
                form.restrict_room_creation = Some(value);
            }
            "user[role]" => form.role = Some(value),
            "authenticity_token" => form.authenticity_token = Some(value),
            "_method" => form.method_override = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    form
}

fn csrf_ok(cx: &Cx, input: &AccountForm) -> bool {
    input
        .authenticity_token
        .as_deref()
        .is_some_and(|token| crate::csrf::verify(cx, token))
}

fn ensure_admin(admin: bool) -> Result<()> {
    if admin {
        Ok(())
    } else {
        Err(forbidden().into())
    }
}

async fn page_shell(db: &PgDb, user_id: i64, name: &str) -> Result<ShellContext> {
    Ok(ShellContext {
        current_user: Some((user_id, name.to_string())),
        logo_version: AccountRepository::first(db)
            .await
            .map_err(http_error)?
            .map(|account| account.updated_number),
    })
}

/// Back to the last-visited room, else the root.
fn back_href(cx: &Cx) -> String {
    cookies(cx)
        .get("last_room")
        .and_then(|cookie| cookie.value().parse::<i64>().ok())
        .map(|id| format!("/rooms/{id}"))
        .unwrap_or_else(|| "/".to_string())
}

/// Absolute join URL for the invite box (the request's host).
fn join_url(cx: &Cx, join_code: &str) -> String {
    let headers = request::headers(cx);
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http");
    format!("{scheme}://{host}/join/{join_code}")
}

#[query_params(error = bad_request)]
struct EditQuery {
    page: Option<String>,
    confirm: Option<String>,
}

/// One people-list row's display data.
struct PersonView {
    row: UserRow,
    title: String,
    avatar_url: String,
}

async fn people(db: &PgDb, admin: bool) -> Result<(Vec<PersonView>, Vec<PersonView>)> {
    let rows = UserRepository::account_users(db, admin)
        .await
        .map_err(http_error)?;
    let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    let forms = UserRepository::form_users(db, &ids)
        .await
        .map_err(http_error)?;
    let by_id: HashMap<i64, FormUser> = forms.into_iter().map(|user| (user.id, user)).collect();
    let mut administrators = Vec::new();
    let mut members = Vec::new();
    for row in rows {
        let form = by_id.get(&row.id);
        let title =
            crate::room_show::actor_title(&row.name, &form.and_then(|user| user.bio.clone()));
        let avatar_url = crate::users::avatar_url_for(
            row.id,
            form.map(|user| user.updated_number.as_str()).unwrap_or(""),
        );
        let person = PersonView {
            row,
            title,
            avatar_url,
        };
        if person.row.role == UserRole::Administrator.value() {
            administrators.push(person);
        } else {
            members.push(person);
        }
    }
    Ok((administrators, members))
}

/// One 500-row people slice (admins first) + the next page, shared by
/// the edit page and the `GET /account/users` fragment.
async fn people_page(db: &PgDb, admin: bool, page: i64) -> Result<(Vec<PersonView>, Option<i64>)> {
    let (administrators, members) = people(db, admin).await?;
    let total = (administrators.len() + members.len()) as i64;
    let page = page.max(1);
    let start = ((page - 1) * 500).min(total) as usize;
    let end = (start + 500).min(total as usize);
    let mut all = administrators;
    all.extend(members);
    all.drain(end..);
    all.drain(..start.min(all.len()));
    let next_page = if total > page * 500 {
        Some(page + 1)
    } else {
        None
    };
    Ok((all, next_page))
}

/// `translation_button("account_name")`.
fn name_translations(cx: &Cx) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    const ENTRIES: &[(&str, &str)] = &[
        ("🇺🇸", "Name this account"),
        ("🇪🇸", "Nombre de esta cuenta"),
        ("🇫🇷", "Nommez ce compte"),
        ("🇮🇳", "इस खाते का नाम दें"),
        ("🇩🇪", "Benennen Sie dieses Konto"),
        ("🇧🇷", "Dê um nome a essa conta"),
        ("🇯🇵", "アカウントに名前を付ける"),
    ];
    let entries: Vec<(String, String)> = ENTRIES
        .iter()
        .map(|(language, text)| (language.to_string(), text.to_string()))
        .collect();
    topcoat::view::view! {
        cx =>
        <details class="position-relative">
            <summary class="btn" tabindex="-1">
                <img src=(img_globe()) width="20" height="20" aria-hidden="true" class="color-icon" />
                <span class="for-screen-reader">"Translate"</span>
            </summary>
            <div class="language-list-menu shadow">
                <dl class="language-list">
                    for (language, text) in entries {
                        <dt>(language)</dt>
                        <dd class="margin-none">(text)</dd>
                    }
                </dl>
            </div>
        </details>
    }
    .boxed()
}

/// `accounts/users/_user`: avatar, name, and the admin controls.
#[allow(clippy::too_many_arguments)]
fn user_row_view(
    cx: &Cx,
    person: &PersonView,
    me_id: i64,
    admin: bool,
    csrf_token: &str,
    confirm_form: &str,
    list_path: &str,
    page: i64,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let banned = person.row.status == UserStatus::Banned.value();
    let active = person.row.status == UserStatus::Active.value();
    let administrator = person.row.role == UserRole::Administrator.value();
    let is_me = person.row.id == me_id;
    let class = if banned {
        "flex align-center gap margin-none banned"
    } else {
        "flex align-center gap margin-none"
    }
    .to_string();
    let name = person.row.name.clone();
    let title = person.title.clone();
    let avatar_url = person.avatar_url.clone();
    let user_path = format!("/users/{}", person.row.id);
    let account_user_path = format!("/account/users/{}", person.row.id);
    let role_dom_id = format!("user_{}_role", person.row.id);
    let remove_form = format!("remove-member-{}", person.row.id);
    let list_page = format!("{list_path}?page={page}");
    let trigger_href = format!("{list_page}&confirm={remove_form}");
    let remove_dialog = Slot::new(crate::confirm::delete_dialog_view(
        cx,
        &remove_form,
        "Remove",
        "Remove this person?",
        "Are you sure you want to permanently remove this person from the account? This can't be undone.",
        confirm_form == remove_form,
        &list_page,
    ));
    let csrf = csrf_token.to_string();
    let role_label = if administrator {
        "Administrator"
    } else {
        "Member"
    }
    .to_string();
    topcoat::view::view! {
        cx =>
        <li class=(class)>
            <figure class="avatar flex-item-no-shrink" style="--avatar-size: 3.75ch;">
                <a title=(title) class="btn avatar" href=(user_path)><img aria-hidden="true" loading="lazy" src=(avatar_url) width="48" height="48" /></a>
            </figure>
            <div class="min-width">
                <div class="overflow-ellipsis fill-shade"><strong>(name.clone())</strong></div>
            </div>
            <hr class="separator" aria-hidden="true" />
            if admin && active {
                <form action=(account_user_path.clone()) accept-charset="UTF-8" method="post">
                    <input type="hidden" name="_method" value="patch" />
                    <input type="hidden" name="authenticity_token" value=(csrf.clone()) />
                    <label class="btn txt-small flex-item-no-shrink" for=(role_dom_id.clone())>
                        <span class="for-screen-reader">"Role: "(role_label)</span>
                        <img width="20" height="20" aria-hidden="true" src=(img_crown()) />
                        <input type="hidden" name="user[role]" value="member" autocomplete="off" />
                        if administrator {
                            <input type="checkbox" name="user[role]" value="administrator" checked="checked" hidden="hidden" id=(role_dom_id) disabled=(is_me) />
                        } else {
                            <input type="checkbox" name="user[role]" value="administrator" hidden="hidden" id=(role_dom_id) disabled=(is_me) />
                        }
                    </label>
                    <button class="btn txt-small" type="submit">"Save"</button>
                </form>
                if !is_me {
                    <form id=(remove_form.clone()) class="button_to" method="post" action=(account_user_path)>
                        <input type="hidden" name="_method" value="delete" />
                        <input type="hidden" name="authenticity_token" value=(csrf) />
                    </form>
                    <a class="btn txt-small flex-item-no-shrink btn--negative" data-tip=(format!("Delete {name}")) href=(trigger_href)>
                        <img width="20" height="20" aria-hidden="true" src=(img_minus()) />
                        <span class="for-screen-reader">"Delete "(name)</span>
                    </a>
                    (remove_dialog)
                }
            }
            if is_me {
                <a class="btn txt-small flex-item-no-shrink" href="/users/me/profile">
                    <img width="20" height="20" aria-hidden="true" src=(img_pencil()) />
                    <span class="for-screen-reader">"My settings"</span>
                </a>
            }
        </li>
    }
    .boxed()
}

/// `accounts/edit`: settings forms + invite + people list.
#[allow(clippy::too_many_arguments)]
fn edit_view(
    cx: &Cx,
    account_name: &str,
    join_url: &str,
    restrict: bool,
    administrators: Vec<Slot<'static>>,
    members: Vec<Slot<'static>>,
    next_page: Option<i64>,
    admin: bool,
    has_logo: bool,
    logo_url: &str,
    csrf_token: &str,
    theme: &str,
) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let name = account_name.to_string();
    let url = join_url.to_string();
    let csrf = csrf_token.to_string();
    let logo_src = logo_url.to_string();
    let admins = administrators;
    let member_rows = members;
    let show_hr = !admins.is_empty() && !member_rows.is_empty();
    let next = next_page;
    // Hoisted: `(expr)` interpolations must move owned values —
    // building sub-views inline captures `cx` and breaks 'static.
    let translations = Slot::new(name_translations(cx));
    let logo_figure = Slot::new(
        crate::room_show::account_logo_figure(
            cx,
            "/account/logo".to_string(),
            Some("txt-xx-large center"),
        )
        .boxed(),
    );
    let invite = Slot::new(crate::room_show::invite_view(cx, &url, admin, &csrf).boxed());
    let light = theme == "light";
    let dark = theme == "dark";
    let system = !light && !dark;
    topcoat::view::view! {
        cx =>
        <section class="panel txt-align-center flex flex-column gap" style="view-transition-name: account-settings">
            if admin {
                <div class="flex align-center gap">
                    <form class="avatar__form" action="/account" accept-charset="UTF-8" method="post" enctype="multipart/form-data">
                        <input type="hidden" name="_method" value="patch" />
                        <input type="hidden" name="authenticity_token" value=(csrf.clone()) />
                        <figure class="avatar account-logo txt-x-large">
                            <img alt="Account logo" width="300" height="300" src=(logo_src) />
                        </figure>
                        <label class="btn" for="account_logo">
                            <img aria-hidden="true" src=(img_camera()) />
                            "New logo"
                            <input hidden="hidden" accept="image/*" type="file" name="account[logo]" id="account_logo" />
                        </label>
                        <button class="btn txt-small" type="submit">"Upload"</button>
                    </form>
                    if has_logo {
                        <form class="button_to avatar__form delete" method="post" action="/account/logo">
                            <input type="hidden" name="_method" value="delete" />
                            <input type="hidden" name="authenticity_token" value=(csrf.clone()) />
                            <button class="btn center btn--regenerate" type="submit">
                                <img aria-hidden="true" src=(img_refresh()) />
                                <span class="for-screen-reader">"Reset to default"</span>
                            </button>
                        </form>
                    }
                </div>
                <div class="flex align-center gap">
                    (translations)
                    <form action="/account" accept-charset="UTF-8" method="post" class="flex align-center gap flex-item-grow">
                        <input type="hidden" name="_method" value="patch" />
                        <input type="hidden" name="authenticity_token" value=(csrf.clone()) />
                        <label class="flex align-center gap flex-item-grow">
                            <input class="input txt-large" autocomplete="off" placeholder="Name this account" autofocus="autofocus" type="text" name="account[name]" value=(name.clone()) />
                        </label>
                        <button class="btn btn--reversed center" type="submit">
                            <img aria-hidden="true" width="20" height="20" src=(img_check()) />
                            <span class="for-screen-reader">"Save changes"</span>
                        </button>
                    </form>
                </div>
                <div class="margin-block-start pad-block pad-inline-double fill-shade border-radius">
                    <form action="/account" accept-charset="UTF-8" method="post" class="flex align-center gap center">
                        <input type="hidden" name="_method" value="put" />
                        <input type="hidden" name="authenticity_token" value=(csrf.clone()) />
                        <div class="flex-item-grow flex align-center gap txt-align-start">
                            <img class="colorize--black" aria-hidden="true" width="18" height="18" src=(img_crown()) />" Must be admin to create new rooms"
                        </div>
                        <input type="hidden" name="account[settings][restrict_room_creation_to_administrators]" value="false" autocomplete="off" />
                        <label class="switch">
                            if restrict {
                                <input type="checkbox" class="switch__input" checked="checked" name="account[settings][restrict_room_creation_to_administrators]" value="true" />
                            } else {
                                <input type="checkbox" class="switch__input" name="account[settings][restrict_room_creation_to_administrators]" value="true" />
                            }
                            <span class="switch__btn round"></span>
                            <span class="for-screen-reader">"Must be admin to create new rooms"</span>
                        </label>
                        <button class="btn txt-small" type="submit">"Save"</button>
                    </form>
                </div>
            } else {
                (logo_figure)
                <h1 class="flex-item-grow txt-x-large">(name.clone())</h1>
            }
            <div class="margin-block-start pad-block pad-inline-double fill-shade border-radius">
                <form action="/account/theme" accept-charset="UTF-8" method="post" class="flex align-center gap center">
                    <input type="hidden" name="authenticity_token" value=(csrf.clone()) />
                    <fieldset class="flex align-center gap">
                        <legend class="txt-small">"Appearance"</legend>
                        <label class="btn txt-small">
                            if light {
                                <input type="radio" name="theme" value="light" checked="checked" />
                            } else {
                                <input type="radio" name="theme" value="light" />
                            }
                            "Light"
                        </label>
                        <label class="btn txt-small">
                            if dark {
                                <input type="radio" name="theme" value="dark" checked="checked" />
                            } else {
                                <input type="radio" name="theme" value="dark" />
                            }
                            "Dark"
                        </label>
                        <label class="btn txt-small">
                            if system {
                                <input type="radio" name="theme" value="system" checked="checked" />
                            } else {
                                <input type="radio" name="theme" value="system" />
                            }
                            "System"
                        </label>
                    </fieldset>
                    <button class="btn txt-small" type="submit">"Save"</button>
                </form>
            </div>
            <div class="margin-block pad-inline pad-block-start fill-shade border-radius">
                (invite)
                <hr class="margin-block separator full-width" style="--border-style: solid" />
                <menu class="flex flex-column gap margin-none pad">
                    <div id="account_users">
                        for row in admins {
                            (row)
                        }
                        if show_hr {
                            <hr class="separator full-width" style="--border-style: solid" />
                        }
                        for row in member_rows {
                            (row)
                        }
                        if let Some(page) = next {
                            <div class="flex center">
                                <a class="btn" href=(format!("/account/edit?page={page}"))>"Show more people"</a>
                            </div>
                        }
                    </div>
                </menu>
            </div>
        </section>
    }
    .boxed()
}

fn nav_view(cx: &Cx, back: &str, admin: bool) -> topcoat::view::BoxView<'static> {
    use crate::assets::*;
    let href = back.to_string();
    topcoat::view::view! {
        cx =>
        <div class="flex-item-justify-start">
            <a class="btn" href=(href)>
                <img aria-hidden="true" width="20" height="20" src=(img_arrow_left()) />
                <span class="for-screen-reader">"Go Back"</span>
            </a>
        </div>
        if admin {
            <div class="flex align-center gap flex-item-justify-end">
                <a class="btn" style="view-transition-name: chat-bots" href="/account/bots">
                    <img aria-hidden="true" width="20" height="20" src=(img_bot()) />
                    <span class="for-screen-reader">"Set up chat bots"</span>
                </a>
                <a class="btn" style="view-transition-name: custom-styles" href="/account/custom_styles/edit">
                    <img aria-hidden="true" width="20" height="20" src=(img_art()) />
                    <span class="for-screen-reader">"Custom styles"</span>
                </a>
            </div>
        }
    }
    .boxed()
}

pub(crate) fn footer_view(cx: &Cx) -> topcoat::view::BoxView<'static> {
    topcoat::view::view! {
        cx =>
        <div class="txt-align-center center margin-block-double txt-subtle">"Topcamp™ version "<span class="version-badge">(env!("CARGO_PKG_VERSION"))</span></div>
    }
    .boxed()
}

/// `GET /account/edit`.
async fn edit_account(cx: &Cx) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let admin = user.role == UserRole::Administrator.value();
    let Some(account) = AccountRepository::first(db).await.map_err(http_error)? else {
        return not_found().into_response(cx);
    };
    let query = query_params::<EditQuery>(cx)?;
    let page = query
        .page
        .as_deref()
        .and_then(cast_integer)
        .unwrap_or(1)
        .max(1);
    let restrict = AccountRepository::room_creation_restricted(db)
        .await
        .map_err(http_error)?;
    let (all, next_page) = people_page(db, admin, page).await?;
    let csrf_token = crate::csrf::issue(cx);
    let confirm_form = query.confirm.as_deref().unwrap_or("");
    let admin_rows: Vec<topcoat::router::Slot> = all
        .iter()
        .filter(|person| person.row.role == UserRole::Administrator.value())
        .map(|person| {
            topcoat::router::Slot::new(user_row_view(
                cx,
                person,
                user.id,
                admin,
                &csrf_token,
                confirm_form,
                "/account/edit",
                page,
            ))
        })
        .collect();
    let member_rows: Vec<topcoat::router::Slot> = all
        .iter()
        .filter(|person| person.row.role != UserRole::Administrator.value())
        .map(|person| {
            topcoat::router::Slot::new(user_row_view(
                cx,
                person,
                user.id,
                admin,
                &csrf_token,
                confirm_form,
                "/account/edit",
                page,
            ))
        })
        .collect();
    let has_logo = AccountRepository::logo_attached(db, account.id)
        .await
        .map_err(http_error)?;
    let logo_url = format!("/account/logo?v={}", account.updated_number);
    let content = edit_view(
        cx,
        &account.name,
        &join_url(cx, &account.join_code),
        restrict,
        admin_rows,
        member_rows,
        next_page,
        admin,
        has_logo,
        &logo_url,
        &csrf_token,
        request_theme(cx),
    );
    let shell = page_shell(db, user.id, &user.name).await?;
    let nav = topcoat::router::Slot::new(nav_view(cx, &back_href(cx), admin));
    let footer = topcoat::router::Slot::new(footer_view(cx));
    document_shell(
        cx,
        "Account settings".to_string(),
        body_classes("", admin),
        topcoat::router::Slot::new(content),
        crate::flash::Flash::default(),
        shell,
        None,
        Some(nav),
        None,
        Some(footer),
    )
    .boxed()
    .async_into_response(cx)
    .await
}

/// `accounts#update`: name + settings, plus the logo file when the
/// settings form posts multipart. Both shapes redirect to the page;
/// the ✓ notice rides UI-14's flash.
async fn update_account(cx: &Cx, body: Body) -> Result<Response> {
    let content_type = request::headers(cx)
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let raw = to_bytes(body, 6 * 1024 * 1024).await?;
    let (input, logo_file) = if content_type.starts_with("multipart/") {
        let parsed = parse_multipart_update(&content_type, &raw).await?;
        (parsed.form, parsed.logo)
    } else {
        (parse_account_form(&raw), None)
    };
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    if !input.account_present {
        return Err(bad_request("param is missing or the value is empty: account").into());
    }
    ensure_admin(user.role == UserRole::Administrator.value())?;
    let db = &app_context::<AppState>(cx).db;
    let Some(account) = AccountRepository::first(db).await.map_err(http_error)? else {
        return not_found().into_response(cx);
    };
    // `settings: {}` permits any hash; the page only ever sends the
    // room-creation flag.
    let settings_json = match input.restrict_room_creation.as_deref() {
        Some(value) => format!(
            "{{\"restrict_room_creation_to_administrators\":{}}}",
            serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
        ),
        None => "{}".to_string(),
    };
    AccountRepository::update_name_settings(db, account.id, input.name.as_deref(), &settings_json)
        .await
        .map_err(http_error)?;
    if let Some(file) = logo_file {
        replace_logo(db, account.id, file).await?;
        // The logo `?v=` rides `updated_at`; the settings merge above
        // may have been a no-op rename, so touch it again.
        AccountRepository::update_name_settings(db, account.id, None, "{}")
            .await
            .map_err(http_error)?;
    }
    crate::flash::redirect_with_notice(cx, "/account/edit", "✓")
}

// --- custom styles (UI-17) -------------------------------------------------------------

/// Install stylesheet cap (64 KiB of CSS is plenty; the form posts
/// urlencoded like the other settings forms).
const MAX_CUSTOM_STYLES: usize = 64 * 1024;

/// The `?page=`-less styles form: a textarea prefilled with the live
/// CSS. Interpolation escapes markup, so even `</textarea>` in the
/// CSS round-trips through the form safely.
fn custom_styles_view(cx: &Cx, css: &str, csrf_token: &str) -> topcoat::view::BoxView<'static> {
    use topcoat::view::view;
    let css = css.to_string();
    let csrf_token = csrf_token.to_string();
    view! {
        cx =>
        <section class="txt-align-center">
            <div class="panel">
                <h1 class="txt-large">"Custom styles"</h1>
                <p class="txt-subtle">"Cascading style rules applied to every page of this install."</p>
                <form class="flex flex-column gap" action="/account/custom_styles" accept-charset="UTF-8" method="post">
                    <input type="hidden" name="authenticity_token" value=(csrf_token) />
                    <input type="hidden" name="_method" value="patch" />
                    <label class="flex flex-column gap txt-align-start">
                        <span>"Stylesheet"</span>
                        <textarea class="input txt-small" name="account[custom_styles]" rows="12" maxlength="65536">(css)</textarea>
                    </label>
                    <button class="btn btn--reversed center" type="submit">"Save styles"</button>
                </form>
            </div>
        </section>
    }
    .boxed()
}

/// `custom_styles#edit`: admin-only, like the rest of the account
/// section (the nav button only renders for administrators).
async fn edit_custom_styles(cx: &Cx) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    ensure_admin(user.role == UserRole::Administrator.value())?;
    let db = &app_context::<AppState>(cx).db;
    let Some(_account) = AccountRepository::first(db).await.map_err(http_error)? else {
        return not_found().into_response(cx);
    };
    let css = AccountRepository::custom_styles(db)
        .await
        .map_err(http_error)?
        .unwrap_or_default();
    let csrf_token = crate::csrf::issue(cx);
    let content = custom_styles_view(cx, &css, &csrf_token);
    let shell = page_shell(db, user.id, &user.name).await?;
    let nav = topcoat::router::Slot::new(nav_view(cx, &back_href(cx), true));
    let footer = topcoat::router::Slot::new(footer_view(cx));
    document_shell(
        cx,
        "Custom styles".to_string(),
        body_classes("", true),
        topcoat::router::Slot::new(content),
        crate::flash::Flash::default(),
        shell,
        None,
        Some(nav),
        None,
        Some(footer),
    )
    .boxed()
    .async_into_response(cx)
    .await
}

/// `custom_styles#update`: persist the CSS (empty clears it) and head
/// back to the form; the ✓ notice rides UI-14's flash.
async fn update_custom_styles(cx: &Cx, body: Body) -> Result<Response> {
    let input = parse_account_form(&to_bytes(body, 1024 * 1024).await?);
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    if !input.account_present {
        return Err(bad_request("param is missing or the value is empty: account").into());
    }
    ensure_admin(user.role == UserRole::Administrator.value())?;
    let css = input.custom_styles.unwrap_or_default();
    if css.len() > MAX_CUSTOM_STYLES {
        return Err(bad_request("custom styles are too long").into());
    }
    let db = &app_context::<AppState>(cx).db;
    let Some(account) = AccountRepository::first(db).await.map_err(http_error)? else {
        return not_found().into_response(cx);
    };
    AccountRepository::update_custom_styles(db, account.id, &css)
        .await
        .map_err(http_error)?;
    crate::flash::redirect_with_notice(cx, "/account/custom_styles/edit", "✓")
}

/// `GET /account/custom_styles.css`: the install stylesheet as a
/// plain file (public, like the logo). A separate document keeps a
/// stray `</style>` in the CSS from ever breaking page markup;
/// ETag/304 keeps repeat page loads cheap.
async fn show_custom_styles(cx: &Cx) -> Result<Response> {
    let db = &app_context::<AppState>(cx).db;
    let css = AccountRepository::custom_styles(db)
        .await
        .map_err(http_error)?
        .unwrap_or_default();
    let etag = format!("\"{:x}\"", md5::compute(css.as_bytes()));
    if request::headers(cx)
        .get("if-none-match")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|requested| requested == etag)
    {
        return Ok(Response::builder()
            .status(304)
            .body(Body::empty())
            .expect("304 builds"));
    }
    Ok(Response::builder()
        .status(200)
        .header("content-type", "text/css; charset=utf-8")
        .header(
            "etag",
            etag.parse::<http::HeaderValue>().expect("etag parses"),
        )
        .body(Body::from(css.into_bytes()))
        .expect("css builds"))
}

/// `accounts/users#update`: member ↔ administrator (bots excluded by
/// `set_role`'s allowlist; anything else falls back to member).
async fn update_account_user(cx: &Cx, id_param: &str, body: Body) -> Result<Response> {
    let input = parse_account_form(&to_bytes(body, 1024 * 1024).await?);
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    ensure_admin(user.role == UserRole::Administrator.value())?;
    let Some(id) = cast_integer(id_param) else {
        return not_found().into_response(cx);
    };
    let db = &app_context::<AppState>(cx).db;
    let target = UserRepository::find_by_id(db, id)
        .await
        .map_err(http_error)?;
    let Some(target) = target else {
        return not_found().into_response(cx);
    };
    if target.status != UserStatus::Active.value() {
        return not_found().into_response(cx);
    }
    let role = match input.role.as_deref() {
        Some("administrator") => UserRole::Administrator.value(),
        _ => UserRole::Member.value(),
    };
    UserRepository::set_role(db, id, role)
        .await
        .map_err(http_error)?;
    see_other("/account/edit").into_response(cx)
}

/// `accounts/users#index`: one 500-row people slice as bare `<li>`
/// rows (+ the next-page link when more remain). Any signed-in user
/// may page (upstream gates only update/destroy); admins also see
/// banned rows. The room glue fetches + appends this on "Show more
/// people" (upstream's lazy next-page frame); without JS the edit
/// page's full-page `?page=` link carries later slices.
async fn index_account_users(cx: &Cx) -> Result<Response> {
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let query = query_params::<EditQuery>(cx)?;
    let page = query
        .page
        .as_deref()
        .and_then(cast_integer)
        .unwrap_or(1)
        .max(1);
    let db = &app_context::<AppState>(cx).db;
    let admin = user.role == UserRole::Administrator.value();
    let (all, next_page) = people_page(db, admin, page).await?;
    let csrf_token = crate::csrf::issue(cx);
    let confirm_form = query.confirm.as_deref().unwrap_or("");
    let rows: Vec<Slot> = all
        .iter()
        .map(|person| {
            Slot::new(user_row_view(
                cx,
                person,
                user.id,
                admin,
                &csrf_token,
                confirm_form,
                "/account/users",
                page,
            ))
        })
        .collect();
    topcoat::view::view! {
        cx =>
        <div>
            for row in rows {
                (row)
            }
            if let Some(next) = next_page {
                <div class="flex center">
                    <a class="btn" href=(format!("/account/edit?page={next}"))>"Show more people"</a>
                </div>
            }
        </div>
    }
    .boxed()
    .async_into_response(cx)
    .await
}

/// `GET /account/users`.
#[route(GET "/account/users")]
pub async fn users_index(cx: &Cx) -> Result<Response> {
    index_account_users(cx).await
}

/// `accounts/users#destroy`: deactivate (banned rows stay listed for
/// admins; deactivated ones drop out).
async fn destroy_account_user(cx: &Cx, id_param: &str, body: Body) -> Result<Response> {
    let input = parse_account_form(&to_bytes(body, 1024 * 1024).await?);
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    ensure_admin(user.role == UserRole::Administrator.value())?;
    let Some(id) = cast_integer(id_param) else {
        return not_found().into_response(cx);
    };
    let app = app_context::<AppState>(cx);
    let target = UserRepository::find_by_id(&app.db, id)
        .await
        .map_err(http_error)?;
    // `set_user`: active users only (deactivated/banned 404).
    if target.is_none_or(|target| target.status != UserStatus::Active.value()) {
        return not_found().into_response(cx);
    }
    UserRepository::deactivate(&app.db, id)
        .await
        .map_err(http_error)?;
    crate::cable::disconnect_user(app, id, false);
    see_other("/account/edit").into_response(cx)
}

/// `accounts/join_codes#create`: mint a fresh invite code.
async fn regenerate_join_code(cx: &Cx, body: Body) -> Result<Response> {
    let input = parse_account_form(&to_bytes(body, 1024 * 1024).await?);
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    ensure_admin(user.role == UserRole::Administrator.value())?;
    let db = &app_context::<AppState>(cx).db;
    let Some(account) = AccountRepository::first(db).await.map_err(http_error)? else {
        return not_found().into_response(cx);
    };
    AccountRepository::reset_join_code(db, account.id, &crate::first_run::generate_join_code())
        .await
        .map_err(http_error)?;
    see_other("/account/edit").into_response(cx)
}

// --- logo --------------------------------------------------------------------------

/// An uploaded `account[logo]` file part.
struct LogoFile {
    filename: String,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

struct MultipartUpdate {
    form: AccountForm,
    logo: Option<LogoFile>,
}

/// The settings forms post urlencoded, except the logo form (a file).
/// `Multipart` is extractor-only, so this parses the buffered bytes
/// directly with the same multer topcoat-router uses.
async fn parse_multipart_update(content_type: &str, raw: &[u8]) -> Result<MultipartUpdate> {
    use futures_util::stream::{self};
    let boundary =
        multer::parse_boundary(content_type).map_err(|_| bad_request("malformed multipart"))?;
    let bytes = bytes::Bytes::copy_from_slice(raw);
    let stream = stream::once(async move { Ok::<_, multer::Error>(bytes) });
    let mut multipart = multer::Multipart::new(stream, boundary);
    let mut form = AccountForm {
        account_present: false,
        name: None,
        custom_styles: None,
        restrict_room_creation: None,
        role: None,
        authenticity_token: None,
        method_override: None,
    };
    let mut logo_file = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| bad_request("malformed multipart"))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "account[logo]" {
            let filename = field.file_name().unwrap_or("logo").to_string();
            let content_type = field.content_type().map(|mime| mime.to_string());
            let bytes = field
                .bytes()
                .await
                .map_err(|_| bad_request("malformed multipart"))?;
            // Browsers always send the part (empty when untouched);
            // only a real pick replaces the logo.
            if !bytes.is_empty() {
                if bytes.len() > 5 * 1024 * 1024 {
                    return Err(bad_request("logo too large").into());
                }
                form.account_present = true;
                logo_file = Some(LogoFile {
                    filename,
                    content_type,
                    bytes: bytes.to_vec(),
                });
            }
            continue;
        }
        let value = field
            .text()
            .await
            .map_err(|_| bad_request("malformed multipart"))?;
        match name.as_str() {
            "account[name]" => {
                form.account_present = true;
                form.name = Some(value);
            }
            "account[settings][restrict_room_creation_to_administrators]" => {
                form.account_present = true;
                form.restrict_room_creation = Some(value);
            }
            "authenticity_token" => form.authenticity_token = Some(value),
            "_method" => form.method_override = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    Ok(MultipartUpdate {
        form,
        logo: logo_file,
    })
}

fn storage_unavailable() -> topcoat::Error {
    use topcamp_domain::error::{Error as DomainError, InfrastructureError};
    http_error(DomainError::Infrastructure(InfrastructureError::new(
        "storage",
    )))
}

/// Remove a logo blob's objects: the original plus every recorded
/// variant. Rows go separately via `delete_blob` (rows-first).
async fn purge_logo_objects(
    store: &topcamp_storage::S3BlobStore,
    db: &PgDb,
    blob: &topcamp_db::repositories::BlobRow,
) -> Result<()> {
    use topcamp_storage::BlobStore;
    let digests = AttachmentRepository::variant_digests(db, blob.id)
        .await
        .map_err(http_error)?;
    for digest in &digests {
        store
            .delete(&topcamp_storage::variant_key(&blob.key, digest))
            .await
            .map_err(|_| storage_unavailable())?;
    }
    store
        .delete(&blob.key)
        .await
        .map_err(|_| storage_unavailable())?;
    Ok(())
}

/// `account.update!(logo:)` + both PNG variants, replacing any
/// previous logo (objects + rows). A non-raster file still attaches
/// but yields no variants, so show falls back to stock — matching
/// upstream's `if logo.variable?` gate.
async fn replace_logo(db: &PgDb, account_id: i64, file: LogoFile) -> Result<()> {
    use topcamp_db::repositories::NewBlob;
    use topcamp_storage::BlobStore;
    let Some(store) = crate::first_run::store() else {
        return Err(storage_unavailable());
    };
    if let Some(old) = AttachmentRepository::blob_for_record(db, "Account", account_id, "logo")
        .await
        .map_err(http_error)?
    {
        purge_logo_objects(&store, db, &old).await?;
        AttachmentRepository::delete_blob(db, old.id)
            .await
            .map_err(http_error)?;
    }
    let key = crate::first_run::generate_blob_key();
    store
        .put(&key, file.bytes.clone(), file.content_type.as_deref())
        .await
        .map_err(|_| storage_unavailable())?;
    let blob = AttachmentRepository::insert_blob(
        db,
        NewBlob {
            key: key.clone(),
            filename: file.filename,
            content_type: file.content_type,
            byte_size: file.bytes.len() as i64,
            checksum: Some(crate::first_run::checksum(&file.bytes)),
            service_name: "rustfs".to_string(),
        },
    )
    .await
    .map_err(http_error)?;
    let blob_id = blob.id;
    AttachmentRepository::attach_to_record(db, "Account", account_id, "logo", blob_id)
        .await
        .map_err(http_error)?;
    for (digest, max) in [
        (crate::variants::logo_small_digest(), 192),
        (crate::variants::logo_large_digest(), 512),
    ] {
        if let Some(png) = crate::variants::resize_png(&file.bytes, max) {
            store
                .put(
                    &topcamp_storage::variant_key(&key, &digest),
                    png,
                    Some("image/png"),
                )
                .await
                .map_err(|_| storage_unavailable())?;
            AttachmentRepository::record_variant(db, blob_id, &digest)
                .await
                .map_err(http_error)?;
        }
    }
    Ok(())
}

#[query_params(error = bad_request)]
struct LogoQuery {
    size: Option<String>,
}

fn png_response(bytes: Vec<u8>, etag: &str) -> Result<Response> {
    Ok(Response::builder()
        .status(200)
        .header("content-type", "image/png")
        .header(
            "cache-control",
            "public, max-age=300, stale-while-revalidate=604800",
        )
        .header("etag", etag)
        .body(Body::from(bytes))
        .expect("logo response builds"))
}

fn if_none_match(cx: &Cx) -> Option<String> {
    request::headers(cx)
        .get("if-none-match")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// `accounts/logos#show`: public, `?size=small` for the 192
/// rendition, ETagged on the account (`?v=` rides the same stamp).
/// Missing variant rows self-heal from the original; a missing or
/// non-variable logo serves the stock icon.
async fn show_logo(cx: &Cx) -> Result<Response> {
    let query = query_params::<LogoQuery>(cx)?;
    let small = query.size.as_deref() == Some("small");
    let db = &app_context::<AppState>(cx).db;
    let account = AccountRepository::first(db).await.map_err(http_error)?;
    let etag = account
        .as_ref()
        .map(|account| format!("\"account-{}\"", account.updated_number))
        .unwrap_or_else(|| "\"account-stock\"".to_string());
    if if_none_match(cx).as_deref() == Some(etag.as_str()) {
        return Ok(Response::builder()
            .status(304)
            .body(Body::empty())
            .expect("304 builds"));
    }
    if let Some(account) = account
        && let Some(blob) = AttachmentRepository::blob_for_record(db, "Account", account.id, "logo")
            .await
            .map_err(http_error)?
    {
        let (digest, max) = if small {
            (crate::variants::logo_small_digest(), 192)
        } else {
            (crate::variants::logo_large_digest(), 512)
        };
        let key = topcamp_storage::variant_key(&blob.key, &digest);
        let recorded = AttachmentRepository::find_variant(db, blob.id, &digest)
            .await
            .map_err(http_error)?;
        if recorded.is_none()
            && let Some(store) = crate::first_run::store()
            && let Ok(Some(original)) = topcamp_storage::BlobStore::get(&store, &blob.key).await
            && let Some(png) = crate::variants::resize_png(&original, max)
            && topcamp_storage::BlobStore::put(&store, &key, png, Some("image/png"))
                .await
                .is_ok()
        {
            let _ = AttachmentRepository::record_variant(db, blob.id, &digest).await;
        }
        if let Some(bytes) = crate::first_run::store_bytes(&key).await? {
            return png_response(bytes, &etag);
        }
    }
    png_response(crate::assets::STOCK_ACCOUNT_LOGO.to_vec(), &etag)
}

/// `accounts/logos#destroy`: drop the logo (objects + rows), back to
/// stock. The account touch re-stamps every `?v=` URL.
async fn destroy_logo(cx: &Cx, body: Body) -> Result<Response> {
    let input = parse_account_form(&to_bytes(body, 1024 * 1024).await?);
    let Some(user) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    if !csrf_ok(cx, &input) {
        return Err(forbidden().into());
    }
    ensure_admin(user.role == UserRole::Administrator.value())?;
    let db = &app_context::<AppState>(cx).db;
    let Some(account) = AccountRepository::first(db).await.map_err(http_error)? else {
        return not_found().into_response(cx);
    };
    if let Some(blob) = AttachmentRepository::blob_for_record(db, "Account", account.id, "logo")
        .await
        .map_err(http_error)?
    {
        let Some(store) = crate::first_run::store() else {
            return Err(storage_unavailable());
        };
        purge_logo_objects(&store, db, &blob).await?;
        AttachmentRepository::delete_blob(db, blob.id)
            .await
            .map_err(http_error)?;
        AccountRepository::update_name_settings(db, account.id, None, "{}")
            .await
            .map_err(http_error)?;
    }
    see_other("/account/edit").into_response(cx)
}

// --- routes ------------------------------------------------------------------------

/// `GET /account/edit`.
#[route(GET "/account/edit")]
pub async fn edit(cx: &Cx) -> Result<Response> {
    edit_account(cx).await
}

/// `PATCH /account`.
#[route(PATCH "/account")]
pub async fn update(cx: &Cx, body: Body) -> Result<Response> {
    update_account(cx, body).await
}

/// `PUT /account` (same action).
#[route(PUT "/account")]
pub async fn replace(cx: &Cx, body: Body) -> Result<Response> {
    update_account(cx, body).await
}

/// `PATCH /account/users/:id`.
#[route(PATCH "/account/users/{id}")]
pub async fn update_user(cx: &Cx, body: Body) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    update_account_user(cx, &id, body).await
}

/// `PUT /account/users/:id` (same action).
#[route(PUT "/account/users/{id}")]
pub async fn replace_user(cx: &Cx, body: Body) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    update_account_user(cx, &id, body).await
}

/// `DELETE /account/users/:id`.
#[route(DELETE "/account/users/{id}")]
pub async fn destroy_user(cx: &Cx, body: Body) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    destroy_account_user(cx, &id, body).await
}

/// `POST /account/users/:id`: `_method` patch/put/delete dispatch.
#[route(POST "/account/users/{id}")]
pub async fn modify_user(cx: &Cx, body: Body) -> Result<Response> {
    let id = path_param_segment(cx, "id").to_owned();
    let raw = to_bytes(body, 1024 * 1024).await?;
    let method = parse_account_form(&raw).method_override;
    match method.as_deref() {
        Some("patch") | Some("put") => update_account_user(cx, &id, Body::from(raw)).await,
        Some("delete") => destroy_account_user(cx, &id, Body::from(raw)).await,
        _ => not_found().into_response(cx),
    }
}

/// `POST /account`: `_method` patch/put dispatch (the settings forms
/// post here — urlencoded, except the multipart logo form); anything
/// else 404s like upstream's missing actions.
#[route(POST "/account")]
pub async fn modify(cx: &Cx, body: Body) -> Result<Response> {
    let content_type = request::headers(cx)
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let raw = to_bytes(body, 6 * 1024 * 1024).await?;
    let method = if content_type.starts_with("multipart/") {
        parse_multipart_update(&content_type, &raw)
            .await?
            .form
            .method_override
    } else {
        parse_account_form(&raw).method_override
    };
    match method.as_deref() {
        Some("patch") | Some("put") => update_account(cx, Body::from(raw)).await,
        _ => not_found().into_response(cx),
    }
}

/// `POST /account/join_code`.
#[route(POST "/account/join_code")]
pub async fn create_join_code(cx: &Cx, body: Body) -> Result<Response> {
    regenerate_join_code(cx, body).await
}

/// `GET /account/custom_styles/edit`.
#[route(GET "/account/custom_styles/edit")]
pub async fn custom_styles_edit(cx: &Cx) -> Result<Response> {
    edit_custom_styles(cx).await
}

/// `PATCH /account/custom_styles`.
#[route(PATCH "/account/custom_styles")]
pub async fn custom_styles_update(cx: &Cx, body: Body) -> Result<Response> {
    update_custom_styles(cx, body).await
}

/// `PUT /account/custom_styles` (same action).
#[route(PUT "/account/custom_styles")]
pub async fn custom_styles_replace(cx: &Cx, body: Body) -> Result<Response> {
    update_custom_styles(cx, body).await
}

/// `POST /account/custom_styles`: `_method` patch/put dispatch,
/// anything else 404s like the other settings forms.
#[route(POST "/account/custom_styles")]
pub async fn modify_custom_styles(cx: &Cx, body: Body) -> Result<Response> {
    let raw = to_bytes(body, 1024 * 1024).await?;
    let method = parse_account_form(&raw).method_override;
    match method.as_deref() {
        Some("patch") | Some("put") => update_custom_styles(cx, Body::from(raw)).await,
        _ => not_found().into_response(cx),
    }
}

/// `GET /account/custom_styles.css`.
#[route(GET "/account/custom_styles.css")]
pub async fn custom_styles_css(cx: &Cx) -> Result<Response> {
    show_custom_styles(cx).await
}

/// `POST /account/theme`: persist the appearance picker
/// (`light` / `dark` / `system`) in the `topcamp_theme` cookie and
/// head back to the settings page. `system` clears the cookie so the
/// stylesheet follows the OS scheme again. Login is not required —
/// the preference is per browser, not per user.
async fn update_theme(cx: &Cx, body: Body) -> Result<Response> {
    let raw = to_bytes(body, 1024 * 1024).await?;
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(&raw).unwrap_or_default();
    let mut theme = None;
    let mut authenticity_token = None;
    for (key, value) in pairs {
        match key.as_str() {
            "theme" => theme = Some(value),
            "authenticity_token" => authenticity_token = Some(value),
            _ => {}
        }
    }
    if !authenticity_token
        .as_deref()
        .is_some_and(|token| crate::csrf::verify(cx, token))
    {
        return Err(forbidden().into());
    }
    match theme.as_deref() {
        Some("light") | Some("dark") => {
            cookies(cx).add(
                Cookie::build((THEME_COOKIE, theme.unwrap()))
                    .path("/")
                    .max_age(time::Duration::days(365))
                    .same_site(SameSite::Lax)
                    .http_only(true),
            );
        }
        Some("system") => {
            cookies(cx).remove(Cookie::new(THEME_COOKIE, ""));
        }
        _ => return Err(bad_request("param is missing or the value is empty: theme").into()),
    }
    see_other("/account/edit").into_response(cx)
}

/// `POST /account/theme`.
#[route(POST "/account/theme")]
pub async fn set_theme(cx: &Cx, body: Body) -> Result<Response> {
    update_theme(cx, body).await
}

/// `GET /account/logo` (public; `?size=small` for the 192 rendition).
#[route(GET "/account/logo")]
pub async fn logo_show(cx: &Cx) -> Result<Response> {
    show_logo(cx).await
}

/// `DELETE /account/logo`.
#[route(DELETE "/account/logo")]
pub async fn destroy(cx: &Cx, body: Body) -> Result<Response> {
    destroy_logo(cx, body).await
}

/// `POST /account/logo`: the reset form carries `_method=delete`.
#[route(POST "/account/logo")]
pub async fn modify_logo(cx: &Cx, body: Body) -> Result<Response> {
    let raw = to_bytes(body, 1024 * 1024).await?;
    let method = parse_account_form(&raw).method_override;
    match method.as_deref() {
        Some("delete") => destroy_logo(cx, Body::from(raw)).await,
        _ => not_found().into_response(cx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_parses_account_keys() {
        let form = parse_account_form(
            b"account%5Bname%5D=HQ&account%5Bsettings%5D%5Brestrict_room_creation_to_administrators%5D=true&authenticity_token=t",
        );
        assert!(form.account_present);
        assert_eq!(form.name.as_deref(), Some("HQ"));
        assert_eq!(form.restrict_room_creation.as_deref(), Some("true"));
        assert_eq!(form.authenticity_token.as_deref(), Some("t"));
    }

    #[test]
    fn form_parses_role_and_override() {
        let form = parse_account_form(b"user%5Brole%5D=administrator&_method=PATCH");
        assert!(!form.account_present);
        assert_eq!(form.role.as_deref(), Some("administrator"));
        assert_eq!(form.method_override.as_deref(), Some("patch"));
    }

    #[test]
    fn form_parses_custom_styles() {
        let form = parse_account_form(
            b"account%5Bcustom_styles%5D=.x%7Bcolor%3Ared%7D&authenticity_token=t",
        );
        assert!(form.account_present);
        assert_eq!(form.custom_styles.as_deref(), Some(".x{color:red}"));
        assert_eq!(form.authenticity_token.as_deref(), Some("t"));
    }
}
