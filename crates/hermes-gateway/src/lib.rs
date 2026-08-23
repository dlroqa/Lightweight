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
pub mod routes;
pub mod state;
pub mod stream;

pub use auth::AuthPolicy;
pub use catalog::{Catalog, ResidentModel};
pub use state::{GatewayConfig, GatewayState};

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};

/// Build the router.
///
/// The OpenAI surface lives under `/v1`; `/health`, `/props` and `/version`
/// are unprefixed because that is where clients probe for them. Our own UI
/// control API will live under `/api/v1` and is deliberately never mixed in
/// with the OpenAI routes.
pub fn app(state: Arc<GatewayState>) -> Router {
    Router::new()
        .route("/health", get(routes::health))
        .route("/props", get(routes::props))
        .route("/version", get(routes::version))
        .route("/v1/models", get(routes::models))
        .route("/v1/chat/completions", post(routes::chat_completions))
        .route("/v1/completions", post(routes::completions))
        .with_state(state)
}
