//! `UsersController#new/create` over `GET/POST /join/:join_code`:
//! invite-code signup. Signed-in users bounce to `/`; a missing
//! account 500s (`join_code` on nil); a wrong code 404s. Creating
//! auto-logs-in and goes `/`, while a taken email redirects to
//! sign-in with the address prefilled.

use topcamp_db::{
    PgDb,
    repositories::{AccountRepository, UserRepository},
};
use topcamp_domain::error::{DomainError, Error as DomainErrorRoot};
use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        Slot,
        content::multipart::Multipart,
        error::{not_found, see_other},
        path_param_segment,
        response::{AsyncIntoResponse, IntoResponse, Response},
        route,
    },
    view::{BoxView, ViewExt as _, view},
};

use crate::{
    AppState,
    first_run::{attach_avatar, read_form, stage_avatar},
    state::http_error,
    users::{EMAIL_TRANSLATIONS, NAME_TRANSLATIONS, translations_view},
};

/// `head :not_found if Current.account.join_code != params[:join_code]`.
/// No account row is the reference's `NoMethodError`: a 500.
async fn verify_join_code(db: &PgDb, code: &str) -> Result<topcamp_db::repositories::AccountRow> {
    let Some(account) = AccountRepository::first(db).await.map_err(http_error)? else {
        return Err(topcoat::Error::msg("undefined method 'join_code' for nil"));
    };
    if account.join_code != code {
        return Err(not_found().into());
    }
    Ok(account)
}

/// `CGI.escape`: alphanumerics and `_.-` pass, space becomes `+`,
/// everything else `%XX`.
fn cgi_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-') {
            out.push(byte as char);
        } else if byte == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `GET /join/:join_code`: the signup nametag.
#[route(GET "/join/{join_code}")]
pub async fn new(cx: &Cx) -> Result<Response> {
    let db = &app_context::<AppState>(cx).db;
    if crate::auth::current_user(cx).await?.is_some() {
        return see_other("/").into_response(cx);
    }
    let code = path_param_segment(cx, "join_code").to_owned();
    let account = verify_join_code(db, &code).await?;
    let administrator = UserRepository::first_administrator(db)
        .await
        .map_err(http_error)?;
    let help = administrator.map(|admin| (admin.name, admin.email_address.unwrap_or_default()));
    let content = join_view(
        cx,
        &account.name,
        &code,
        &format!("/account/logo?v={}", account.updated_number),
        help,
        crate::csrf::issue(cx),
    );
    crate::pages::document_shell(
        cx,
        "Sign up".to_string(),
        "signup",
        Slot::new(content),
        crate::flash::Flash::default(),
        crate::pages::ShellContext::anonymous(),
        None,
        None,
        None,
        None,
    )
    .boxed()
    .async_into_response(cx)
    .await
}

/// `POST /join/:join_code`: create the user (avatar included),
/// log them in, and go `/`. A taken email sends them to sign-in
/// with the address prefilled instead.
#[route(POST "/join/{join_code}")]
pub async fn create(cx: &Cx, mut multipart: Multipart) -> Result<Response> {
    if crate::auth::current_user(cx).await?.is_some() {
        return see_other("/").into_response(cx);
    }
    let form = read_form(&mut multipart).await?;
    if !crate::csrf::verify(cx, &form.authenticity_token.unwrap_or_default()) {
        return Err(topcoat::router::error::forbidden().into());
    }
    let db = &app_context::<AppState>(cx).db;
    let code = path_param_segment(cx, "join_code").to_owned();
    verify_join_code(db, &code).await?;
    // `user.require(:name)`: a missing name is the reference's 500.
    let Some(name) = form.name else {
        return Err(topcoat::Error::msg("param is missing: name"));
    };
    let staged = match form.avatar {
        Some(ref upload) => Some(stage_avatar(upload).await?),
        None => None,
    };
    let digest = match form.password.filter(|password| !password.is_empty()) {
        Some(password) => Some(bcrypt::hash(password, bcrypt::DEFAULT_COST).map_err(|_| {
            http_error(topcamp_domain::error::Error::Infrastructure(
                topcamp_domain::error::InfrastructureError::new("bcrypt"),
            ))
        })?),
        None => None,
    };
    let email = form.email.clone();
    let created = UserRepository::create(
        db,
        &name,
        &email.clone().unwrap_or_default(),
        digest.as_deref(),
    )
    .await;
    match created {
        Ok(user) => {
            if let Some(staged) = staged.as_ref() {
                attach_avatar(db, user.id, staged)
                    .await
                    .map_err(http_error)?;
            }
            crate::auth::start_session(cx, db, user.id).await?;
            see_other("/").into_response(cx)
        }
        Err(DomainErrorRoot::Domain(DomainError::Conflict)) => {
            let mut location = "/session/new".to_string();
            if let Some(email) = email {
                location.push_str(&format!("?email_address={}", cgi_escape(&email)));
            }
            see_other(location).into_response(cx)
        }
        Err(other) => Err(http_error(other)),
    }
}

const PASSWORD_TRANSLATIONS: &[(&str, &str)] = &[
    ("🇺🇸", "Enter your password"),
    ("🇪🇸", "Introduce tu contraseña"),
    ("🇫🇷", "Saisissez votre mot de passe"),
    ("🇮🇳", "अपना पासवर्ड दर्ज करें"),
    ("🇩🇪", "Geben Sie Ihr Passwort ein"),
    ("🇧🇷", "Insira sua senha"),
    ("🇯🇵", "パスワードを入力してください"),
];

fn translation_button(cx: &Cx, entries: &[(&str, &str)]) -> Slot<'static> {
    Slot::new(translations_view(cx, entries))
}

/// `users/new`: the signup nametag over `POST /join/:code`, with the
/// sign-in nav and the owner's help contact underneath.
#[allow(clippy::too_many_arguments)]
fn join_view(
    cx: &Cx,
    account_name: &str,
    join_code: &str,
    logo_url: &str,
    help: Option<(String, String)>,
    csrf_token: String,
) -> BoxView<'static> {
    use crate::assets::*;
    let action = format!("/join/{join_code}");
    let account_name = account_name.to_string();
    let logo = Slot::new(crate::room_show::account_logo_figure(
        cx,
        logo_url.to_string(),
        None,
    ));
    let name_translations = translation_button(cx, NAME_TRANSLATIONS);
    let email_translations = translation_button(cx, EMAIL_TRANSLATIONS);
    let password_translations = translation_button(cx, PASSWORD_TRANSLATIONS);
    view! {
        cx =>
        <nav>
            <div class="flex-item-justify-end">
                <a class="btn flex-item-justify-end" href="/session/new">
                    <img aria-hidden="true" src=(img_login_keys()) />
                    <span class="for-screen-reader">"Sign in"</span>
                </a>
            </div>
        </nav>
        <form class="center" enctype="multipart/form-data" action=(action) accept-charset="UTF-8" method="post">
            <input type="hidden" name="authenticity_token" value=(csrf_token) autocomplete="off" />
            <section class="nametag u-relative">
                <div class="flex justify-center align-center pad-block">
                    <img class="nametag__lanyard" aria-hidden="true" src=(img_lanyard()) />
                </div>
                <div class="nametag__inner flex flex-column gap">
                    <fieldset class="flex flex-column center-block">
                        <legend class="txt-align-center flex gap">
                            (logo)
                            <strong class="txt-large">(account_name)</strong>
                        </legend>
                        <label class="align-center center avatar__form gap">
                            <div class="btn input--file">
                                <img aria-hidden="true" src=(img_camera()) />
                                <input class="input" accept="image/*" type="file" name="user[avatar]" id="user_avatar" />
                                <span class="for-screen-reader">"Upload avatar"</span>
                            </div>
                            <div class="btn avatar input--file txt-xx-large">
                                <img aria-hidden="true" alt="Add your avatar" src=(img_default_avatar()) />
                                <span class="for-screen-reader">"Avatar"</span>
                            </div>
                        </label>
                    </fieldset>
                    <div class="flex align-center gap">
                        (name_translations)
                        <label class="flex align-center gap flex-item-grow txt-large input input--actor">
                            <input class="input" autocomplete="name" placeholder="Name" autofocus="autofocus" required="required" data-1p-ignore="true" type="text" name="user[name]" id="user_name" />
                            <img aria-hidden="true" class="colorize--black" src=(img_person()) width="24" height="24" />
                        </label>
                    </div>
                    <div class="flex align-center gap">
                        (email_translations)
                        <label class="flex align-center gap flex-item-grow txt-large input input--actor">
                            <input class="input" autocomplete="username" placeholder="Email address" required="required" type="email" name="user[email_address]" id="user_email_address" />
                            <img aria-hidden="true" class="colorize--black" src=(img_email()) width="24" height="24" />
                        </label>
                    </div>
                    <div class="flex align-center gap">
                        (password_translations)
                        <label class="flex align-center gap flex-item-grow txt-large input input--actor">
                            <input class="input" autocomplete="new-password" placeholder="Password" required="required" maxlength="72" size="72" type="password" name="user[password]" id="user_password" />
                            <img aria-hidden="true" class="colorize--black" src=(img_password()) width="24" height="24" />
                        </label>
                    </div>
                    <button name="button" type="submit" class="btn btn--reversed center txt-large">
                        <img aria-hidden="true" src=(img_check()) />
                        <span class="for-screen-reader">"Save"</span>
                    </button>
                </div>
            </section>
        </form>
        if let Some((name, address)) = help {
            <div class="txt-align-center margin-block-double full-width">
                <a class="btn center" title=(format!("Email {name}")) href=(format!("mailto:\"{name}\" <{address}>"))>
                    <img aria-hidden="true" src=(img_lifebuoy()) />
                    <span>(address)</span>
                </a>
                <div class="txt-align-center center margin-block txt-subtle">
                    "Topcamp™ version "
                    <span class="version-badge">(env!("CARGO_PKG_VERSION"))</span>
                </div>
            </div>
        }
    }
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgi_escape_matches_ruby() {
        assert_eq!(cgi_escape("a@b.com"), "a%40b.com");
        assert_eq!(cgi_escape("a b+c"), "a+b%2Bc");
        assert_eq!(cgi_escape("azAZ09_.-"), "azAZ09_.-");
        assert_eq!(cgi_escape(""), "");
    }
}
