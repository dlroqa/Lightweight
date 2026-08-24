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
pub mod catalog;
pub mod completions;
pub mod control;
pub mod jobs;
pub mod manager;
pub mod metrics;
pub mod routes;
pub mod scheduler;
pub mod state;
pub mod stream;
pub mod system;

pub use auth::AuthPolicy;
pub use catalog::{Catalog, ResidentModel};
pub use state::{GatewayConfig, GatewayState};

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};

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
            axum::routing::delete(control::remove),
        )
        .route("/api/v1/catalog", get(control::pinned))
        .route("/api/v1/jobs", get(control::jobs))
        .route("/api/v1/jobs/{id}", get(control::job))
        .route("/api/v1/jobs/{id}/events", get(control::job_events))
        .route("/api/v1/system", get(system::system))
        .route("/api/v1/gateway", get(control::gateway))
        .with_state(state)
}
