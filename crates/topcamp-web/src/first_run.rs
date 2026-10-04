//! First-run setup (`GET`+`POST /first_run`).
//!
//! Fresh installs (no account row yet) provision the singleton account,
//! its administrator, and the first room here; [`prevent_repeats`] sends
//! every later visit home. The form posts `multipart/form-data` (avatar
//! upload) with nested `user[...]` params, matching upstream's
//! `first_runs` controller + `FirstRun.create!`.

use std::sync::OnceLock;

use topcamp_db::PgDb;
use topcamp_db::repositories::{
    AccountRepository, AttachmentRepository, MembershipRepository, NewBlob, RoomRepository,
    UserRepository,
};
use topcamp_domain::auth::UserRole;
use topcamp_domain::rooms::RoomType;
use topcamp_storage::{BlobStore, S3BlobStore};
use topcoat::{
    Result,
    context::{Cx, app_context},
    router::{
        Slot,
        content::multipart::Multipart,
        error::see_other,
        response::{AsyncIntoResponse, IntoResponse, Response},
        route,
    },
    view::{BoxView, ViewExt as _, view},
};

use crate::state::{AppState, http_error};

/// Process-wide blob store for avatar uploads, installed by the `serve`
/// binary (mirrors the worker's `WORKER_STORE`: `BlobStore` is not
/// object-safe, so the concrete `S3BlobStore` is shared instead).
static STORE: OnceLock<S3BlobStore> = OnceLock::new();

/// Install the avatar blob store. The second install loses (tests share
/// one process with the dev defaults).
pub fn init_store(store: S3BlobStore) -> std::result::Result<(), S3BlobStore> {
    STORE.set(store)
}

pub(crate) fn store() -> Option<S3BlobStore> {
    STORE.get().cloned()
}

/// Fetch an object for the avatar route: `None` store or fetch failure
/// is a 500 (dependency unavailable), a missing key is `Ok(None)`… but
/// rows never outlive their objects outside crashes, so callers treat
/// `None` as a 500 too.
pub(crate) async fn store_bytes(key: &str) -> Result<Option<Vec<u8>>> {
    use topcamp_domain::error::{Error as DomainError, InfrastructureError};
    let unavailable = || {
        http_error(DomainError::Infrastructure(InfrastructureError::new(
            "storage",
        )))
    };
    let Some(store) = store() else {
        return Err(unavailable());
    };
    store.get(key).await.map_err(|err| {
        tracing::warn!(error = err.to_string(), "avatar fetch failed");
        unavailable()
    })
}

/// `XXXX-XXXX-XXXX` from 12 alphanumerics, matching upstream's
/// `generate_join_code`.
pub(crate) fn generate_join_code() -> String {
    use rand::distr::{Alphanumeric, SampleString};
    let code = Alphanumeric.sample_string(&mut rand::rng(), 12);
    format!("{}-{}-{}", &code[0..4], &code[4..8], &code[8..12])
}

/// `SecureRandom.base36(28)` (`has_secure_token :key, length: 28`).
pub(crate) fn generate_blob_key() -> String {
    use rand::Rng;
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::rng();
    (0..28)
        .map(|_| ALPHABET[rng.random_range(0..36)] as char)
        .collect()
}

/// `OpenSSL::Digest::MD5.base64digest` of the content.
pub(crate) fn checksum(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(md5::compute(data).as_ref())
}

/// `redirect_to root_url if Account.any?`, as a predicate.
async fn prevent_repeats(db: &PgDb) -> Result<bool> {
    Ok(AccountRepository::count(db).await.map_err(http_error)? > 0)
}

/// Setup form. Fresh installs only; configured installs go home.
#[route(GET "/first_run")]
pub async fn show(cx: &Cx) -> Result<Response> {
    let db = &app_context::<AppState>(cx).db;
    if prevent_repeats(db).await? {
        return see_other("/").into_response(cx);
    }
    let content = first_run_view(cx, crate::csrf::issue(cx));
    crate::pages::document_shell(
        cx,
        "Set up Topcamp".to_string(),
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

/// Parsed nametag submission (first-run setup and join share the
/// fields): nested `user[...]` text params plus the optional avatar
/// file. A text (non-file) `user[avatar]` part counts as no avatar:
/// browsers only ever send a file part or nothing.
pub(crate) struct UserForm {
    pub(crate) authenticity_token: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) email: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) avatar: Option<AvatarUpload>,
}

pub(crate) struct AvatarUpload {
    filename: String,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

pub(crate) async fn read_form(multipart: &mut Multipart) -> Result<UserForm> {
    let mut form = UserForm {
        authenticity_token: None,
        name: None,
        email: None,
        password: None,
        avatar: None,
    };
    while let Some(field) = multipart.next_field().await? {
        let name = field.name().map(str::to_owned);
        let file_name = field.file_name().map(str::to_owned);
        let content_type = field.content_type().map(str::to_owned);
        match name.as_deref() {
            Some("user[avatar]") => {
                let bytes = field.bytes().await?;
                let is_file = file_name.as_deref().is_some_and(|n| !n.is_empty());
                if is_file && !bytes.is_empty() {
                    form.avatar = Some(AvatarUpload {
                        filename: file_name.unwrap_or_default(),
                        content_type,
                        bytes: bytes.to_vec(),
                    });
                }
            }
            Some("authenticity_token") => form.authenticity_token = Some(field.text().await?),
            Some("user[name]") => form.name = Some(field.text().await?),
            Some("user[email_address]") => form.email = Some(field.text().await?),
            Some("user[password]") => form.password = Some(field.text().await?),
            _ => {
                field.bytes().await?;
            }
        }
    }
    Ok(form)
}

/// Avatar bytes staged to the object store ahead of the rows that
/// reference them (upstream `Upload::stage`).
pub(crate) struct StagedAvatar {
    key: String,
    filename: String,
    content_type: Option<String>,
    byte_size: i64,
    checksum: String,
}

pub(crate) async fn stage_avatar(upload: &AvatarUpload) -> Result<StagedAvatar> {
    use topcamp_domain::error::{Error as DomainError, InfrastructureError};
    let unavailable = || {
        http_error(DomainError::Infrastructure(InfrastructureError::new(
            "storage",
        )))
    };
    let Some(store) = store() else {
        return Err(unavailable());
    };
    let key = generate_blob_key();
    if let Err(err) = store
        .put(&key, upload.bytes.clone(), upload.content_type.as_deref())
        .await
    {
        tracing::warn!(error = err.to_string(), "first-run avatar staging failed");
        return Err(unavailable());
    }
    Ok(StagedAvatar {
        key,
        filename: upload.filename.clone(),
        content_type: upload.content_type.clone(),
        byte_size: upload.bytes.len() as i64,
        checksum: checksum(&upload.bytes),
    })
}

/// Provision the install: account, administrator, first room + grant,
/// avatar rows. Sequential (repos own their connections); a mid-run
/// failure compensates our account row so the install stays fresh, and
/// a rival winner's account turns the failure into [`Race::Lost`]
/// (upstream's `rescue RecordNotUnique`).
enum Race {
    Lost,
    Failed,
}

async fn provision(
    db: &PgDb,
    name: &str,
    email: &str,
    digest: &str,
    staged: Option<&StagedAvatar>,
) -> std::result::Result<i64, Race> {
    let account_id = match AccountRepository::create(db, "Topcamp", &generate_join_code()).await {
        Ok(account) => account.id,
        Err(_) => return settle(db, None).await,
    };
    let outcome = provision_rows(db, name, email, digest, staged).await;
    match outcome {
        Ok(user_id) => Ok(user_id),
        Err(()) => settle(db, Some(account_id)).await,
    }
}

async fn provision_rows(
    db: &PgDb,
    name: &str,
    email: &str,
    digest: &str,
    staged: Option<&StagedAvatar>,
) -> std::result::Result<i64, ()> {
    let user = UserRepository::create(db, name, email, Some(digest))
        .await
        .map_err(|_| ())?;
    UserRepository::set_role(db, user.id, UserRole::Administrator.value())
        .await
        .map_err(|_| ())?;
    let room = RoomRepository::create(db, user.id, Some("All Talk"), RoomType::Open.class_name())
        .await
        .map_err(|_| ())?;
    MembershipRepository::create(db, room.id, user.id)
        .await
        .map_err(|_| ())?;
    if let Some(staged) = staged {
        attach_avatar(db, user.id, staged).await.map_err(|_| ())?;
    }
    Ok(user.id)
}

/// After a provisioning failure: compensate our account row, then a
/// surviving account means a rival won the race (redirect home);
/// otherwise the install is still fresh and the failure is a 500.
async fn settle(db: &PgDb, ours: Option<i64>) -> std::result::Result<i64, Race> {
    if let Some(id) = ours {
        let _ = AccountRepository::destroy(db, id).await;
    }
    match AccountRepository::count(db).await {
        Ok(n) if n > 0 => Err(Race::Lost),
        _ => Err(Race::Failed),
    }
}

/// Blob + attachment rows for a staged avatar, then `analyze_later`
/// (thumbnail + metadata via the worker relay).
pub(crate) async fn attach_avatar(
    db: &PgDb,
    user_id: i64,
    staged: &StagedAvatar,
) -> topcamp_db::repositories::RepoResult<()> {
    let blob = AttachmentRepository::insert_blob(
        db,
        NewBlob {
            key: staged.key.clone(),
            filename: staged.filename.clone(),
            content_type: staged.content_type.clone(),
            byte_size: staged.byte_size,
            checksum: Some(staged.checksum.clone()),
            service_name: "rustfs".to_string(),
        },
    )
    .await?;
    AttachmentRepository::attach_to_record(db, "User", user_id, "avatar", blob.id).await?;
    let mut tx = db.pool().begin().await.map_err(topcamp_db::DbError::Sqlx)?;
    topcamp_db::outbox::publish(
        &mut tx,
        "process_attachment",
        &format!("{{\"blob_id\":{}}}", blob.id),
    )
    .await
    .map_err(topcamp_db::DbError::Sqlx)?;
    tx.commit().await.map_err(topcamp_db::DbError::Sqlx)?;
    Ok(())
}

/// Provision the install from the setup form, sign the administrator
/// in, and go home. A missing name is a 500 (upstream raises NOT NULL);
/// blank values insert as-is, matching upstream's blind create.
#[route(POST "/first_run")]
pub async fn create(cx: &Cx, mut multipart: Multipart) -> Result<Response> {
    use topcamp_domain::error::{Error as DomainError, InfrastructureError};
    let db = &app_context::<AppState>(cx).db;
    if prevent_repeats(db).await? {
        return see_other("/").into_response(cx);
    }
    let form = read_form(&mut multipart).await?;
    if !crate::csrf::verify(cx, form.authenticity_token.as_deref().unwrap_or("")) {
        return Err(topcoat::router::error::forbidden().into());
    }
    let Some(name) = form.name else {
        return Err(http_error(DomainError::Infrastructure(
            InfrastructureError::new("first_run"),
        )));
    };
    let staged = match form.avatar {
        Some(ref upload) => Some(stage_avatar(upload).await?),
        None => None,
    };
    let digest =
        bcrypt::hash(form.password.unwrap_or_default(), bcrypt::DEFAULT_COST).map_err(|_| {
            http_error(DomainError::Infrastructure(InfrastructureError::new(
                "bcrypt",
            )))
        })?;
    match provision(
        db,
        &name,
        &form.email.unwrap_or_default(),
        &digest,
        staged.as_ref(),
    )
    .await
    {
        Ok(user_id) => {
            crate::auth::start_session(cx, db, user_id).await?;
            see_other("/").into_response(cx)
        }
        Err(Race::Lost) => see_other("/").into_response(cx),
        Err(Race::Failed) => Err(http_error(DomainError::Infrastructure(
            InfrastructureError::new("first_run"),
        ))),
    }
}

/// Setup nametag, matching upstream's `first_runs/show` (avatar picker,
/// translate widgets, credential rows). `data-controller` attributes
/// stay out until a Topcoat behavior backs them.
fn first_run_view(cx: &Cx, csrf_token: String) -> BoxView<'static> {
    use crate::assets::*;
    view! {
        cx =>
        <form class="center max-width" enctype="multipart/form-data" action="/first_run" accept-charset="UTF-8" method="post">
            <input type="hidden" name="authenticity_token" value=(csrf_token) />
            <section class="nametag u-relative">
                <div class="flex justify-center align-center pad-block">
                    <img class="nametag__lanyard" aria-hidden="true" src=(img_lanyard()) />
                </div>
                <div class="nametag__inner flex flex-column gap">
                    <fieldset class="flex flex-column center-block">
                        <legend class="txt-large txt-align-center"><strong>"Set up Topcamp"</strong></legend>
                        <label class="align-center center avatar__form gap">
                            <div class="btn input--file">
                                <img aria-hidden="true" src=(img_camera()) />
                                <input class="input" accept="image/*" type="file" name="user[avatar]" id="user_avatar" />
                                <span class="for-screen-reader">"Add your avatar"</span>
                            </div>
                            <div class="btn avatar input--file txt-xx-large">
                                <img aria-hidden="true" alt="Add your avatar" src=(img_default_avatar()) />
                                <span class="for-screen-reader">"Avatar"</span>
                            </div>
                        </label>
                    </fieldset>
                    <div class="flex align-center gap">
                        <details class="position-relative">
                            <summary class="btn" tabindex="-1">
                                <img aria-hidden="true" class="color-icon" src=(img_globe()) width="20" height="20" />
                                <span class="for-screen-reader">"Translate"</span>
                            </summary>
                            <div class="language-list-menu shadow">
                                <dl class="language-list">
                                    <dt>"🇺🇸"</dt><dd class="margin-none">"Enter your name"</dd>
                                    <dt>"🇪🇸"</dt><dd class="margin-none">"Introduce tu nombre"</dd>
                                    <dt>"🇫🇷"</dt><dd class="margin-none">"Entrez votre nom"</dd>
                                    <dt>"🇮🇳"</dt><dd class="margin-none">"अपना नाम दर्ज करें"</dd>
                                    <dt>"🇩🇪"</dt><dd class="margin-none">"Geben Sie Ihren Namen ein"</dd>
                                    <dt>"🇧🇷"</dt><dd class="margin-none">"Insira seu nome"</dd>
                                    <dt>"🇯🇵"</dt><dd class="margin-none">"お名前を入力してください"</dd>
                                </dl>
                            </div>
                        </details>
                        <label class="flex align-center gap flex-item-grow txt-large input input--actor">
                            <input class="input" autocomplete="name" placeholder="Name" autofocus="autofocus" required="required" type="text" name="user[name]" id="user_name" />
                            <img aria-hidden="true" class="colorize--black" src=(img_person()) width="24" height="24" />
                        </label>
                    </div>
                    <div class="flex align-center gap">
                        <details class="position-relative">
                            <summary class="btn" tabindex="-1">
                                <img aria-hidden="true" class="color-icon" src=(img_globe()) width="20" height="20" />
                                <span class="for-screen-reader">"Translate"</span>
                            </summary>
                            <div class="language-list-menu shadow">
                                <dl class="language-list">
                                    <dt>"🇺🇸"</dt><dd class="margin-none">"Enter your email address"</dd>
                                    <dt>"🇪🇸"</dt><dd class="margin-none">"Introduce tu correo electrónico"</dd>
                                    <dt>"🇫🇷"</dt><dd class="margin-none">"Entrez votre adresse courriel"</dd>
                                    <dt>"🇮🇳"</dt><dd class="margin-none">"अपना ईमेल पता दर्ज करें"</dd>
                                    <dt>"🇩🇪"</dt><dd class="margin-none">"Geben Sie Ihre E-Mail-Adresse ein"</dd>
                                    <dt>"🇧🇷"</dt><dd class="margin-none">"Insira seu endereço de email"</dd>
                                    <dt>"🇯🇵"</dt><dd class="margin-none">"メールアドレスを入力してください"</dd>
                                </dl>
                            </div>
                        </details>
                        <label class="flex align-center gap flex-item-grow txt-large input input--actor">
                            <input class="input" autocomplete="username" placeholder="Email address" required="required" type="email" name="user[email_address]" id="user_email_address" />
                            <img aria-hidden="true" class="colorize--black" src=(img_email()) width="24" height="24" />
                        </label>
                    </div>
                    <div class="flex align-center gap">
                        <details class="position-relative">
                            <summary class="btn" tabindex="-1">
                                <img aria-hidden="true" class="color-icon" src=(img_globe()) width="20" height="20" />
                                <span class="for-screen-reader">"Translate"</span>
                            </summary>
                            <div class="language-list-menu shadow">
                                <dl class="language-list">
                                    <dt>"🇺🇸"</dt><dd class="margin-none">"Enter your password"</dd>
                                    <dt>"🇪🇸"</dt><dd class="margin-none">"Introduce tu contraseña"</dd>
                                    <dt>"🇫🇷"</dt><dd class="margin-none">"Saisissez votre mot de passe"</dd>
                                    <dt>"🇮🇳"</dt><dd class="margin-none">"अपना पासवर्ड दर्ज करें"</dd>
                                    <dt>"🇩🇪"</dt><dd class="margin-none">"Geben Sie Ihr Passwort ein"</dd>
                                    <dt>"🇧🇷"</dt><dd class="margin-none">"Insira sua senha"</dd>
                                    <dt>"🇯🇵"</dt><dd class="margin-none">"パスワードを入力してください"</dd>
                                </dl>
                            </div>
                        </details>
                        <label class="flex align-center gap flex-item-grow txt-large input input--actor">
                            <input class="input" autocomplete="new-password" placeholder="Password" required="required" maxlength="72" size="72" type="password" name="user[password]" id="user_password" />
                            <img aria-hidden="true" class="colorize--black" src=(img_password()) width="24" height="24" />
                        </label>
                    </div>
                    <button name="button" type="submit" class="btn btn--reversed center txt-large">
                        <img aria-hidden="true" src=(img_arrow_right()) />
                        <span class="for-screen-reader">"Save"</span>
                    </button>
                </div>
            </section>
        </form>
    }
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_code_groups_alphanumerics() {
        let code = generate_join_code();
        assert_eq!(code.len(), 14, "code: {code}");
        assert_eq!(&code[4..5], "-");
        assert_eq!(&code[9..10], "-");
        assert!(
            code.chars()
                .filter(|c| *c != '-')
                .all(|c| c.is_ascii_alphanumeric()),
            "code: {code}"
        );
        assert_ne!(generate_join_code(), generate_join_code());
    }

    #[test]
    fn blob_key_is_base36_28() {
        let key = generate_blob_key();
        assert_eq!(key.len(), 28, "key: {key}");
        assert!(
            key.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "key: {key}"
        );
        assert_ne!(generate_blob_key(), generate_blob_key());
    }

    #[test]
    fn checksum_is_base64_md5() {
        // md5("") = d41d8cd98f00b204e9800998ecf8427e.
        assert_eq!(checksum(b""), "1B2M2Y8AsgTpgAmY7PhCfg==");
        assert_eq!(checksum(b"avatar"), checksum(b"avatar"));
        assert_ne!(checksum(b"a"), checksum(b"b"));
    }
}
