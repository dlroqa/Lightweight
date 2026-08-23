//! The inference backend abstraction.
//!
//! This crate contains no engine. That is the point: it is the seam spec
//! sections 28 and 37 ask for, so the llama.cpp backend can later be swapped
//! for a proprietary Hermes runtime without the gateway, the scheduler or the
//! UI noticing.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod backend;
pub mod error;

pub use backend::{
    BackendCapabilities, BackendHealth, BackendId, DeviceKind, InferenceBackend, LoadProgress,
    LoadRequest, LoadedModel, ResourceSnapshot,
};
pub use error::BackendError;
