//! `AgentProvider` over the OpenAI-compatible gateway.
//!
//! This crate is the only place in Lightagent that speaks HTTP and SSE. It
//! implements [`lightagent_core::AgentProvider`] by streaming
//! `POST /v1/chat/completions` and decoding the response into
//! [`lightagent_core::ProviderEvent`]s, and it lists models over
//! `GET /v1/models`.
//!
//! It depends on `lightagent-core` and never on any `lightweight-*` crate: the
//! wire types and the SSE decoder are reproduced here rather than imported, so
//! the "provider knows nothing about a harness" invariant holds in the
//! dependency graph, not just by convention.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod embed;
pub mod provider;
pub mod sse;
pub mod tls;
pub mod wire;

pub use embed::EmbeddingClient;
pub use provider::{LightweightProvider, ProviderConfig};
pub use tls::ensure_provider;
