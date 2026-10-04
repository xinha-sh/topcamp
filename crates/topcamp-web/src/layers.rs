//! Cross-cutting HTTP layers (§29).
//!
//! Focused and reusable; no business logic. [`request_scope`] assigns every
//! request a correlation id, visible to handlers via [`try_request_id`] and
//! echoed back as `x-request-id` (also on error responses, via the
//! `response_headers` slot).

use std::sync::atomic::{AtomicU64, Ordering};

use topcoat::{
    Result,
    context::{Cx, try_request_context},
    router::{
        Body, HeaderName, HeaderValue, LayerFn, LayerFuture, Next, layer,
        response::{Response, response_headers},
    },
};

/// Per-request correlation id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestId(pub u64);

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Read the request id installed by [`request_scope`], if present.
pub fn try_request_id(cx: &Cx) -> Option<u64> {
    try_request_context::<RequestId>(cx).map(|id| id.0)
}

#[layer("/")]
pub async fn request_scope(cx: &Cx, body: Body, next: Next<'_>) -> Result<Response> {
    use tracing::Instrument as _;

    let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let child = cx.with(RequestId(id));
    response_headers(&child).append(
        HeaderName::from_static("x-request-id"),
        HeaderValue::from_str(&id.to_string()).expect("u64 is a valid header value"),
    );
    // Structured request log (§39): ids only — never query strings, bodies,
    // or tokens.
    let span = tracing::info_span!("request", request_id = id);
    let response = next.run(&child, body).instrument(span).await?;
    tracing::info!(
        request_id = id,
        status = response.status().as_u16(),
        "handled"
    );
    Ok(response)
}

/// `allow_browser`: old browsers get the incompatible-browser
/// page (200) instead of the app. Infrastructure paths bypass the
/// gate (the page itself needs its stylesheets); requests without a
/// user agent or with an unversioned one pass through, like Rails.
#[layer("/")]
pub async fn browser_gate(cx: &Cx, body: Body, next: Next<'_>) -> Result<Response> {
    if !crate::transfer::gate_skipped(crate::transfer::request_path(cx))
        && let Some(blocked) = crate::transfer::gate_response(cx).await?
    {
        return Ok(blocked);
    }
    next.run(cx, body).await
}

/// Rails has no 405: a path drawn for other verbs answers 404 with
/// the static page, like an unknown URL. The router reports the
/// method mismatch as a `MethodNotAllowedError`, which becomes the
/// shared 404 response (JSON-aware); every other error passes
/// through untouched.
///
/// Pathless (`None` path), so it wraps misses too — `#[layer]`
/// layers never see 404/405 terminals.
pub fn not_found_normalizer() -> LayerFn {
    LayerFn::new(None::<&str>, normalize_not_found)
}

fn normalize_not_found<'a>(cx: &'a Cx, body: Body, next: Next<'a>) -> LayerFuture<'a> {
    use topcoat::router::error::MethodNotAllowedError;
    Box::pin(async move {
        match next.run(cx, body).await {
            Ok(response) => Ok(response),
            Err(error) => match error.downcast_cloned::<MethodNotAllowedError>() {
                Ok(_) => crate::not_found::response(cx),
                Err(error) => Err(error),
            },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_roundtrips_through_context() {
        let cx = Cx::default();
        assert_eq!(try_request_id(&cx), None);
        let child = cx.with(RequestId(7));
        assert_eq!(try_request_id(&child), Some(7));
    }

    #[test]
    fn request_ids_increase() {
        let a = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let b = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        assert!(b > a);
    }
}
