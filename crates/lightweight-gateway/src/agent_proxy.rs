//! Reverse-proxying the panel's agent screens to the Lightagent API.
//!
//! The panel is one bundle over two backends: the inference control API under
//! `/api/v1`, which this gateway answers itself, and the agent API under
//! `/api/lightagent/v1` (start a run, stream its events, list tools, respond to
//! an approval), which is a separate server — `lightagent serve`, on its own
//! port. In development the Vite dev server proxies each prefix to its own
//! origin; served from the gateway there was no second proxy, so every agent
//! call fell to the panel fallback and came back as `index.html`. A client
//! expecting JSON then failed on the leading `<` of `<!doctype …>`.
//!
//! This closes that gap the same way [`crate::web`] closes the CORS one: by
//! putting the second surface on the gateway's own origin. When
//! [`GatewayConfig::agent_upstream`] is set, `/api/lightagent` and everything
//! under it is forwarded verbatim to that origin — the agent server serves the
//! same paths this gateway receives, so nothing is rewritten — and the response,
//! including a long-lived `text/event-stream`, is streamed straight back.
//!
//! [`GatewayConfig::agent_upstream`]: crate::state::GatewayConfig::agent_upstream

use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::state::GatewayState;

/// The largest agent request body this gateway will buffer before forwarding.
///
/// The agent API's requests are small JSON — a run's opening message, an
/// approval decision — so buffering them is free and lets the forwarded body be
/// plain bytes rather than a stream (a stream would have to be `Sync` to cross
/// into `reqwest`, which an incoming body is not). The streaming that matters is
/// the *response*: `runs/{id}/events` is an SSE tail held open for a whole run,
/// and that is forwarded chunk by chunk below, not buffered.
const MAX_REQUEST_BODY: usize = 4 * 1024 * 1024;

/// Forward one request to the configured agent server and stream its answer
/// back.
///
/// Registered only when [`GatewayConfig::agent_upstream`] is set, so the missing
/// upstream is a defensive `404` rather than a real path.
///
/// [`GatewayConfig::agent_upstream`]: crate::state::GatewayConfig::agent_upstream
pub async fn proxy(State(state): State<Arc<GatewayState>>, request: Request) -> Response {
    let Some(upstream) = state.config.agent_upstream.as_deref() else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let (parts, body) = request.into_parts();

    // The path and query verbatim: the agent server answers the same
    // `/api/lightagent/v1/...` paths this gateway received, so the tail is
    // forwarded unchanged rather than stripped and re-prefixed.
    let tail = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path(), |pq| pq.as_str());
    let target = format!("{}{tail}", upstream.trim_end_matches('/'));

    let body_bytes = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
                error_body("the request body was too large to forward to the agent server"),
            )
                .into_response();
        }
    };

    let response = client()
        .request(parts.method, &target)
        .headers(forwardable(&parts.headers))
        .body(body_bytes)
        .send()
        .await;

    let response = match response {
        Ok(response) => response,
        Err(error) => {
            // A clear JSON body, so the panel shows the reason rather than
            // choking on an HTML fallback — the exact failure this proxy ends.
            return (
                StatusCode::BAD_GATEWAY,
                [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
                error_body(&format!(
                    "could not reach the agent server at {upstream}: {error}"
                )),
            )
                .into_response();
        }
    };

    let mut builder = Response::builder().status(response.status());
    for (name, value) in response.headers() {
        if !is_hop_by_hop(name) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// The request's headers, minus the ones that describe only this hop.
fn forwardable(headers: &HeaderMap) -> HeaderMap {
    let mut forwarded = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if !is_hop_by_hop(name) {
            // `append` rather than `insert`: a header may legitimately repeat,
            // and the upstream should see it exactly as the client sent it.
            forwarded.append(name.clone(), value.clone());
        }
    }
    forwarded
}

/// Whether a header describes one connection hop and must not be relayed to the
/// next one.
///
/// `host` is included: `reqwest` sets the upstream's own `Host`, and forwarding
/// the gateway's would name the wrong server.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
    )
}

/// A JSON error envelope matching the shape the agent API itself returns, so the
/// panel's client parses a gateway-side failure the same way it parses one from
/// the agent server.
fn error_body(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

/// One process-wide HTTP client for the proxy, built after the crypto provider
/// is in place.
///
/// A shared client rather than one per request: it holds the connection pool to
/// the agent server, which a fresh client would rebuild every call. `reqwest`
/// **panics** rather than errors when no rustls provider is installed — even for
/// the plain-HTTP loopback this only ever talks to — so [`ensure_provider`] runs
/// first, and the fallback keeps the lint against `unwrap` honest for a build
/// that cannot in practice fail here.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        ensure_provider();
        reqwest::Client::builder()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Install the `ring` rustls provider once per process.
///
/// The same precondition every HTTP client in this workspace shares; kept local
/// rather than shared because it is two lines and a dependency is not.
fn ensure_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        // Fails only when a provider is already installed, which satisfies the
        // requirement just as well.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
