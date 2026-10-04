//! Rails-style flash: a notice or alert that survives one redirect.
//!
//! Upstream keeps the flash in the session (`FlashHash` with
//! discard-on-read); here it rides a short-lived `flash` cookie
//! (`n`/`a` form-encoded, like the double-submit CSRF cookie — no
//! server state, works before login). Writers use
//! [`redirect_with_notice`] / [`redirect_with_alert`]; every
//! [`document_shell`](crate::pages::document_shell) render consumes
//! the cookie exactly once. Direct renders pass `flash.now`
//! equivalents ([`Flash::notice`] / [`Flash::alert`]), which win over
//! carried values for their keys, like the shared flash hash.

use topcoat::{
    Result,
    context::Cx,
    cookie::{Cookie, Cookies, SameSite, cookies},
    router::{
        error::see_other,
        response::{IntoResponse, Response},
    },
};

/// The flash cookie name.
pub const FLASH_COOKIE: &str = "flash";

/// A render's flash: carried cookie values plus `flash.now` values.
#[derive(Clone, Debug, Default)]
pub struct Flash {
    /// `flash[:notice]` (the ✓ style).
    pub notice: Option<String>,
    /// `flash[:alert]` (the negative style).
    pub alert: Option<String>,
}

impl Flash {
    /// `flash.now[:notice] = ...` for a direct render.
    pub fn notice(text: impl Into<String>) -> Self {
        Self {
            notice: Some(text.into()),
            alert: None,
        }
    }

    /// `flash.now[:alert] = ...` for a direct render.
    pub fn alert(text: impl Into<String>) -> Self {
        Self {
            notice: None,
            alert: Some(text.into()),
        }
    }

    /// Nothing to show.
    pub fn is_empty(&self) -> bool {
        self.notice.is_none() && self.alert.is_none()
    }

    /// `flash.now` wins per key; carried values fill the rest.
    fn merge(&mut self, carried: Flash) {
        if self.notice.is_none() {
            self.notice = carried.notice;
        }
        if self.alert.is_none() {
            self.alert = carried.alert;
        }
    }
}

/// Encode the cookie value (`n`/`a` form fields; ASCII-safe).
fn encode(notice: Option<&str>, alert: Option<&str>) -> String {
    let mut pairs = Vec::new();
    if let Some(notice) = notice {
        pairs.push(("n", notice));
    }
    if let Some(alert) = alert {
        pairs.push(("a", alert));
    }
    serde_urlencoded::to_string(pairs).unwrap_or_default()
}

/// Decode the cookie value; garbage reads as empty (fails closed).
fn decode(value: &str) -> Flash {
    let mut flash = Flash::default();
    let Ok(pairs) = serde_urlencoded::from_str::<Vec<(String, String)>>(value) else {
        return flash;
    };
    for (key, text) in pairs {
        if text.is_empty() {
            continue;
        }
        match key.as_str() {
            "n" => flash.notice = Some(text),
            "a" => flash.alert = Some(text),
            _ => {}
        }
    }
    flash
}

/// Queue the flash cookie for the redirect response.
fn store(cx: &Cx, notice: Option<&str>, alert: Option<&str>) {
    cookies(cx).add(
        Cookie::build((FLASH_COOKIE, encode(notice, alert)))
            .path("/")
            .http_only(true)
            .same_site(SameSite::Lax),
    );
}

/// `redirect_to url, notice:` — the notice shows on the next page.
pub fn redirect_with_notice(
    cx: &Cx,
    location: impl Into<String>,
    notice: impl Into<String>,
) -> Result<Response> {
    store(cx, Some(notice.into().as_str()), None);
    see_other(location.into()).into_response(cx)
}

/// `redirect_to url, alert:` — the alert shows on the next page.
pub fn redirect_with_alert(
    cx: &Cx,
    location: impl Into<String>,
    alert: impl Into<String>,
) -> Result<Response> {
    store(cx, None, Some(alert.into().as_str()));
    see_other(location.into()).into_response(cx)
}

/// Consume the carried flash (the cookie clears whether or not the
/// render shows it, like `FlashHash`'s discard list), merged under
/// `flash.now` values. Pre-cookie contexts (layers) only see `now`.
pub fn take(cx: &Cx, mut now: Flash) -> Flash {
    use topcoat::{context::try_request_context, cookie::CookieJarCell};
    if try_request_context::<CookieJarCell>(cx).is_none() {
        return now;
    }
    let jar = cookies(cx);
    let carried = jar
        .get(FLASH_COOKIE)
        .map(|cookie| decode(cookie.value()))
        .unwrap_or_default();
    if jar.get(FLASH_COOKIE).is_some() {
        jar.remove(Cookie::new(FLASH_COOKIE, ""));
    }
    now.merge(carried);
    now
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flash_round_trips_unicode() {
        let encoded = encode(Some("✓ done"), Some("oops & <br>"));
        let flash = decode(&encoded);
        assert_eq!(flash.notice.as_deref(), Some("✓ done"));
        assert_eq!(flash.alert.as_deref(), Some("oops & <br>"));
        assert!(encoded.is_ascii());
    }

    #[test]
    fn garbage_decodes_empty() {
        assert!(decode("%%%").is_empty());
        assert!(decode("").is_empty());
        assert!(decode("x=1").is_empty());
    }

    #[test]
    fn now_wins_per_key() {
        let mut now = Flash::notice("now");
        now.merge(Flash {
            notice: Some("carried".to_string()),
            alert: Some("carried".to_string()),
        });
        assert_eq!(now.notice.as_deref(), Some("now"));
        assert_eq!(now.alert.as_deref(), Some("carried"));
    }
}
