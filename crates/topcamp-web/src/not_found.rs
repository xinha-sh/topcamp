//! The 404 page: upstream's static `public/404.html`, byte for byte
//! (`not_found.html`, vendored from
//! `basecamp/once-topcamp`), served standalone — outside the
//! document layout, with no session or theme dependency — exactly
//! like Rails serves a static error page.
//!
//! Two entry points share it: the `*{*rest}` catch-all route below
//! (every method, every otherwise-unmatched URL) and handlers that
//! 404 explicitly (drawn-but-unimplemented actions like
//! `rooms#new`), via [`response`]. Explicit-JSON requests get the
//! JSON 404 instead (`{"status":404,"error":"Not Found"}`, like
//! Rails' `head :not_found` with a JSON accept). The
//! [`not_found_normalizer`](crate::layers::not_found_normalizer)
//! layer funnels router 405s here too — Rails has no 405 — while
//! truly empty 404s (avatars, `head :not_found`) stay in their
//! handlers.

use topcoat::{
    Result,
    context::Cx,
    router::{
        Body,
        request::headers,
        response::{IntoResponse, Response},
        route,
    },
};

/// Upstream's `public/404.html`, verbatim.
pub const NOT_FOUND_HTML: &str = include_str!("not_found.html");

/// The JSON 404 body for explicit-JSON requests.
pub const NOT_FOUND_JSON: &str = r#"{"status":404,"error":"Not Found"}"#;

/// True when the request explicitly accepts JSON.
pub fn wants_json(cx: &Cx) -> bool {
    headers(cx)
        .get("accept")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("application/json"))
}

/// The 404 response: the standalone page, or the JSON 404 for
/// explicit-JSON requests.
pub fn response(cx: &Cx) -> Result<Response> {
    let (content_type, body) = if wants_json(cx) {
        ("application/json; charset=UTF-8", NOT_FOUND_JSON)
    } else {
        ("text/html; charset=UTF-8", NOT_FOUND_HTML)
    };
    Response::builder()
        .status(404)
        .header("content-type", content_type)
        .body(Body::from(body))
        .expect("404 builds")
        .into_response(cx)
}

/// Site-wide fallback: more specific routes win, so this only serves
/// URLs nothing else matches. Every method (Rails answers unknown
/// URLs with 404 whatever the verb).
#[route(* "/{*rest}")]
pub async fn catch_all(cx: &Cx) -> Result<Response> {
    response(cx)
}
