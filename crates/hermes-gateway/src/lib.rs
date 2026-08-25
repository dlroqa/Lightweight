//! The HTTP surface.
//!
//! One public endpoint, bound to loopback by default (spec sections 13 and
//! 23). Everything below it — the engine, its port, its private key — is an
//! implementation detail that no client can reach.
//!
//! The gateway owns what the engine must not: the model catalog, the context
//! policy, admission, auth, and the wire contract. In particular it **re-emits
//! its own chunks** rather than forwarding the engine's bytes, so an upstream
//! JSON change breaks a test in [`hermes_api`] instead of breaking a client
//! mid-conversation.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod auth;
pub mod benchmark;
pub mod catalog;
pub mod completions;
pub mod control;
pub mod jobs;
pub mod logs;
pub mod manager;
pub mod metrics;
pub mod routes;
pub mod scheduler;
pub mod state;
pub mod store_api;
pub mod stream;
pub mod system;
pub mod web;

pub use auth::AuthPolicy;
pub use catalog::{Catalog, ResidentModel};
pub use state::{GatewayConfig, GatewayState};

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};

/// The service `axum::serve` is handed, with the peer address attached.
///
/// Separate from [`app`] rather than folded into it because the router is
/// merged into and layered on before it is served - the mock gateway adds its
/// own test routes to it - and because this is the one line that decides
/// whether the scheduler can tell two clients apart. A server built without it
/// still works and still serves: every request then arrives with the same
/// scheduling key, and the queue is ordered exactly as it was before there were
/// clients in it. That degradation is silent, which is why the incantation
/// lives here instead of being retyped at every call site.
pub fn service(
    app: Router,
) -> axum::extract::connect_info::IntoMakeServiceWithConnectInfo<Router, std::net::SocketAddr> {
    app.into_make_service_with_connect_info::<std::net::SocketAddr>()
}

/// Build the router.
///
/// The OpenAI surface lives under `/v1`; `/health`, `/props`, `/version` and
/// `/metrics` are unprefixed because that is where clients and scrapers probe
/// for them. Our own UI control API lives under `/api/v1` and is deliberately
/// never mixed in with the OpenAI routes — a client enumerating `/v1` must find
/// only what OpenAI defines there.
pub fn app(state: Arc<GatewayState>) -> Router {
    Router::new()
        .route("/health", get(routes::health))
        .route("/metrics", get(routes::metrics))
        .route("/api/v1/metrics", get(routes::metrics_json))
        .route("/props", get(routes::props))
        .route("/version", get(routes::version))
        .route("/v1/models", get(routes::models))
        .route("/v1/chat/completions", post(routes::chat_completions))
        .route("/v1/completions", post(routes::completions))
        // Our own control surface. Everything the desktop shell drives lives
        // here, and nothing here is visible to a client walking `/v1`.
        .route("/api/v1/models", get(control::models))
        .route("/api/v1/models/import", post(control::import))
        .route("/api/v1/models/download", post(control::download))
        .route("/api/v1/models/unload", post(control::unload))
        .route("/api/v1/models/{id}/load", post(control::load))
        .route(
            "/api/v1/models/{id}",
            get(control::model_detail).delete(control::remove),
        )
        .route("/api/v1/catalog", get(control::pinned))
        .route(
            "/api/v1/benchmarks",
            get(control::benchmarks).post(control::run_benchmark),
        )
        .route("/api/v1/benchmarks/{id}", get(control::benchmark))
        .route("/api/v1/jobs", get(control::jobs))
        .route("/api/v1/jobs/{id}", get(control::job))
        .route("/api/v1/jobs/{id}/events", get(control::job_events))
        .route("/api/v1/system", get(system::system))
        .route("/api/v1/gateway", get(control::gateway))
        .route("/api/v1/events", get(control::events))
        .route("/api/v1/requests", get(control::requests))
        .route("/api/v1/logs", get(logs::logs))
        .route(
            "/api/v1/conversations",
            get(store_api::list).post(store_api::create),
        )
        .route(
            "/api/v1/conversations/{id}",
            get(store_api::get)
                .put(store_api::save)
                .delete(store_api::delete),
        )
        .route(
            "/api/v1/settings",
            get(store_api::settings).put(store_api::save_settings),
        )
        // Last, so that every route above is matched first: the panel's files
        // can never shadow an endpoint, only fill in what no endpoint claimed.
        .fallback(web::serve)
        // Wrapped around every route rather than written into each handler:
        // there are a dozen of them, and a gauge that a new endpoint can forget
        // to join is a gauge that quietly stops being true.
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            count_in_flight,
        ))
        .with_state(state)
}

/// Count a request as in flight until its response body has been delivered.
///
/// The guard is moved into the body rather than dropped when the handler
/// returns, and that is the whole point. A handler here returns as soon as the
/// response *head* is ready; on a streamed completion the body then runs for
/// as long as the generation does. A gauge that stopped counting at the head
/// would read zero throughout the two minutes this gateway is busiest, which
/// is precisely when someone is looking at it.
async fn count_in_flight(
    axum::extract::State(state): axum::extract::State<Arc<GatewayState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use futures_util::StreamExt;

    // Watching must not change what is watched. The panel polls the status
    // surface every second and holds `/api/v1/events` open for as long as it is
    // running, so counting those would pin this gauge at one on a gateway doing
    // nothing and add one to every reading of it - including the reading being
    // taken by the request doing the asking.
    if is_monitoring(request.uri().path()) {
        return next.run(request).await;
    }

    let guard = state.metrics().enter_request();
    let (parts, body) = next.run(request).await.into_parts();

    // The guard rides along in the stream's state, so it is dropped when the
    // body ends *or* when the client goes away and hyper drops it - the same
    // mechanism `RequestGuard` already relies on for cancellation.
    let counted = futures_util::stream::unfold(
        (body.into_data_stream(), guard),
        |(mut stream, guard)| async move { Some((stream.next().await?, (stream, guard))) },
    );
    axum::response::Response::from_parts(parts, axum::body::Body::from_stream(counted))
}

/// Whether a path is the gateway describing itself rather than doing work.
///
/// Listed explicitly rather than matched by prefix: `/api/v1/models/{id}/load`
/// shares a prefix with `/api/v1/models` and is a multi-second engine restart,
/// which is exactly the kind of work this gauge exists to show.
fn is_monitoring(path: &str) -> bool {
    matches!(
        path,
        "/health"
            | "/version"
            | "/props"
            | "/metrics"
            | "/api/v1/metrics"
            | "/api/v1/system"
            | "/api/v1/gateway"
            | "/api/v1/requests"
            | "/api/v1/logs"
            | "/api/v1/events"
    )
}
