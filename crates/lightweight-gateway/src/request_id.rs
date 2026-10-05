//! `X-Request-Id`: one id for one request, from the client through a router to
//! the node that answers it.
//!
//! The gateway never invents one. A request that arrives with a usable id —
//! from a router, which always sends one, or from a client that set its own —
//! has that exact id written on every log line about it and echoed on the
//! response, so `grep <id>` finds the request in the client's, the router's and
//! the node's logs alike. A request without one is logged as before, under the
//! completion id the gateway has always used; a second, unrelated id would only
//! be one more thing that fails to correlate.
//!
//! The rule for "usable" is shared with the router, so an id one side accepts
//! the other never rewrites: visible ASCII, no spaces, at most
//! [`MAX_REQUEST_ID`] bytes. Anything else is ignored, never truncated into a
//! different id, and never allowed to put a control character in a log line.

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::response::Response;

/// The header the id travels in.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The longest request id carried on.
pub const MAX_REQUEST_ID: usize = 128;

/// The request's id, if it carries a usable one.
pub fn from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|id| usable(id))
        .map(str::to_owned)
}

/// Whether `id` may be carried on as a request id.
pub fn usable(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_REQUEST_ID && id.bytes().all(|b| b.is_ascii_graphic())
}

/// Echo the id on a response, when there is one. Additive: a client that never
/// sent an id sees no new header.
pub fn echo(mut response: Response, id: Option<&str>) -> Response {
    if let Some(value) = id.and_then(|id| HeaderValue::from_str(id).ok()) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_usable_id_is_kept_exactly_and_anything_else_ignored() {
        let mut headers = HeaderMap::new();
        assert_eq!(from_headers(&headers), None);
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("rtr-abc123"));
        assert_eq!(from_headers(&headers).as_deref(), Some("rtr-abc123"));
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("has space"));
        assert_eq!(from_headers(&headers), None);
        let long = "x".repeat(MAX_REQUEST_ID + 1);
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_str(&long).unwrap());
        assert_eq!(from_headers(&headers), None);
    }

    #[test]
    fn the_id_is_echoed_only_when_there_is_one() {
        let response = echo(Response::new(axum::body::Body::empty()), Some("abc"));
        assert_eq!(response.headers()[REQUEST_ID_HEADER], "abc");
        let response = echo(Response::new(axum::body::Body::empty()), None);
        assert!(response.headers().get(REQUEST_ID_HEADER).is_none());
    }
}
