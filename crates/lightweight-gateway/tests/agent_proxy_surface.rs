//! The agent proxy: the panel's agent screens, forwarded to the agent server so
//! they share the gateway's origin.
//!
//! Checked over real sockets — one gateway, one stand-in agent server — because
//! the property is a routing and forwarding one: a request under
//! `/api/lightagent` must reach the upstream and its answer must come back
//! intact, rather than falling to the panel fallback and returning `index.html`
//! (the `<!doctype …>` the panel choked on). The cross-origin guard is checked
//! the same way, since it lives in the same layer stack.

use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::routing::{get, post};
use lightweight_backend_mock::MockBackend;
use lightweight_gateway::{GatewayConfig, GatewayState};
use serde_json::{Value, json};

/// Install the rustls provider `reqwest` insists on, even for plain HTTP.
fn ensure_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A stand-in agent server: the two shapes the proxy has to carry — a JSON GET
/// and a body-bearing POST that echoes what reached it, so the test can assert
/// the method, path and body all arrived.
async fn start_agent_server() -> String {
    let app = Router::new()
        .route(
            "/api/lightagent/v1/tools",
            get(|| async {
                axum::Json(json!({
                    "tools": [
                        { "name": "web_fetch", "risk": "external", "description": "fetch a URL" }
                    ]
                }))
            }),
        )
        .route(
            "/api/lightagent/v1/runs",
            post(|request: Request| async move {
                let method = request.method().to_string();
                let path = request.uri().path().to_owned();
                let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
                    .await
                    .expect("read body");
                let received: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                axum::Json(json!({ "method": method, "path": path, "received": received }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind agent");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://127.0.0.1:{port}")
}

/// A gateway that forwards `/api/lightagent` to `upstream`, on its own port.
async fn start_gateway(upstream: Option<String>) -> String {
    let state = Arc::new(GatewayState::new(
        Arc::new(MockBackend::default()),
        lightweight_gateway::catalog::shared(None),
        GatewayConfig {
            agent_upstream: upstream,
            ..GatewayConfig::default()
        },
    ));
    let app = lightweight_gateway::app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gateway");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, lightweight_gateway::service(app)).await;
    });
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn a_get_is_forwarded_and_its_json_comes_back_intact() {
    // The bug this closes: served from the gateway, `/api/lightagent/v1/tools`
    // fell to the panel fallback and returned `index.html`, so the panel's
    // `response.json()` failed on the leading `<` of `<!doctype`.
    ensure_provider();
    let upstream = start_agent_server().await;
    let gateway = start_gateway(Some(upstream)).await;

    let response = reqwest::Client::new()
        .get(format!("{gateway}/api/lightagent/v1/tools"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
        "the upstream's content type must be preserved, not replaced with HTML"
    );
    let body: Value = response.json().await.expect("json, not an HTML fallback");
    assert_eq!(body["tools"][0]["name"], "web_fetch");
}

#[tokio::test]
async fn a_same_origin_post_reaches_the_upstream_with_method_path_and_body() {
    ensure_provider();
    let upstream = start_agent_server().await;
    let gateway = start_gateway(Some(upstream)).await;

    // A same-origin request: `Origin` echoes the gateway's own authority, which
    // is what the panel sends and what the write guard admits.
    let authority = gateway.trim_start_matches("http://");
    let response = reqwest::Client::new()
        .post(format!("{gateway}/api/lightagent/v1/runs"))
        .header("origin", &gateway)
        .json(&json!({ "message": "hello" }))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200, "authority under test: {authority}");
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["method"], "POST");
    assert_eq!(body["path"], "/api/lightagent/v1/runs");
    assert_eq!(body["received"]["message"], "hello");
}

#[tokio::test]
async fn a_cross_origin_write_is_refused_before_it_reaches_the_upstream() {
    // The proxied surface is guarded on the same terms as `/api/v1`: the agent
    // server it forwards to is loopback and keyless, so nothing but this stops a
    // page on another origin from starting a run while the user only looks on.
    ensure_provider();
    let upstream = start_agent_server().await;
    let gateway = start_gateway(Some(upstream)).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/api/lightagent/v1/runs"))
        .header("origin", "http://evil.example")
        .json(&json!({ "message": "forged" }))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn without_an_upstream_the_agent_path_is_not_a_proxy_route() {
    // Additivity: a gateway told nothing about an agent server is exactly as it
    // was — the path matches no route and falls to the fallback, which with no
    // panel configured is a plain 404, never a hang or a 502 to some default.
    ensure_provider();
    let gateway = start_gateway(None).await;

    let response = reqwest::Client::new()
        .get(format!("{gateway}/api/lightagent/v1/tools"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_clear_json_502_not_an_html_fallback() {
    // When the agent server is not running, the panel must still get JSON it can
    // parse and a reason to show — the whole point being to never hand it HTML.
    ensure_provider();
    // A port nobody is listening on: bind one, read its number, drop it.
    let dead = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        listener.local_addr().expect("addr").port()
    };
    let gateway = start_gateway(Some(format!("http://127.0.0.1:{dead}"))).await;

    let response = reqwest::Client::new()
        .get(format!("{gateway}/api/lightagent/v1/tools"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 502);
    let body: Value = response.json().await.expect("a json error envelope");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|m| m.contains("agent server")),
        "the body must name what failed: {body}"
    );
}
