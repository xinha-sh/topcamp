//! Double-submit CSRF tokens for HTML forms (§10).
//!
//! Rails' `authenticity_token` preserved in Topcoat-native form: the server
//! sets a `csrf_token` cookie and renders the same value into each form's
//! hidden field; POST handlers accept only when the two match (constant
//! time). No server state, works before login, and complements Topcoat's
//! origin policy (which stays default-on as defense in depth).
//!
//! JSON routes (`/rooms/*`, `/search`) take no token: they rely on the
//! session cookie plus the origin policy, the standard API posture.

use rand::RngCore;
use subtle::ConstantTimeEq;
use topcoat::{
    context::Cx,
    cookie::{Cookie, Cookies, SameSite, cookies},
};

/// Cookie + form field name (field keeps the Rails name for familiarity).
pub const CSRF_COOKIE: &str = "csrf_token";
const HEX: &[u8; 16] = b"0123456789abcdef";

/// 32 random bytes, hex-encoded (64 chars).
pub fn generate() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn well_formed(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Constant-time match on well-formed tokens; anything else fails closed.
pub fn tokens_match(cookie: &str, submitted: &str) -> bool {
    well_formed(cookie)
        && well_formed(submitted)
        && cookie.as_bytes().ct_eq(submitted.as_bytes()).into()
}

/// The request's token, reusing the cookie's when valid, else minting and
/// setting a fresh one. Call from any handler/page that renders a form.
///
/// Layers run before the cookie middleware, so no jar exists there:
/// fall back to an unpersisted token. The only layer-rendered page
/// (the browser block) carries no forms, so nothing ever submits it.
pub fn issue(cx: &Cx) -> String {
    use topcoat::{context::try_request_context, cookie::CookieJarCell};
    if try_request_context::<CookieJarCell>(cx).is_none() {
        return generate();
    }
    let jar = cookies(cx);
    if let Some(cookie) = jar.get(CSRF_COOKIE)
        && well_formed(cookie.value())
    {
        return cookie.value().to_string();
    }
    let token = generate();
    jar.add(
        Cookie::build((CSRF_COOKIE, token.clone()))
            .path("/")
            .http_only(true)
            .same_site(SameSite::Lax),
    );
    token
}

/// Accept only when the submitted field matches the request's cookie.
pub fn verify(cx: &Cx, submitted: &str) -> bool {
    cookies(cx)
        .get(CSRF_COOKIE)
        .is_some_and(|cookie| tokens_match(cookie.value(), submitted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_64_hex_chars() {
        for _ in 0..16 {
            let token = generate();
            assert!(well_formed(&token), "token {token}");
        }
        assert_ne!(generate(), generate());
    }

    #[test]
    fn matching_tokens_verify() {
        let token = generate();
        assert!(tokens_match(&token, &token));
    }

    #[test]
    fn mismatched_tokens_rejected() {
        assert!(!tokens_match(&generate(), &generate()));
    }

    #[test]
    fn malformed_tokens_fail_closed() {
        let good = generate();
        for bad in ["", "xyz", &"0".repeat(63), &"0".repeat(65), &"g".repeat(64)] {
            assert!(!tokens_match(&good, bad), "submitted {bad:?}");
            assert!(!tokens_match(bad, &good), "cookie {bad:?}");
        }
    }
}
