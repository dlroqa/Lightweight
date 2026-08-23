//! Core domain types for the Hermes CPU Inference Gateway.
//!
//! This crate is deliberately **pure**: no filesystem, no network, no async
//! runtime. Everything here is a type, a trait, or arithmetic. That keeps it
//! compiling in about a second on a slow machine, and it means the crates that
//! carry the project's hardest logic — GGUF parsing and RAM estimation — can
//! depend on it without dragging in `tokio` or `axum`.
//!
//! Three things live here because every other crate needs to agree on them:
//!
//! * [`error`] — the actionable-error contract. Section 27 of the spec says the
//!   application must never crash on a bad model, a full disk, or an
//!   unsupported CPU; it must show an actionable error instead. That promise is
//!   only keepable if *every* error in the workspace can describe what the user
//!   should do about it, so [`error::Actionable`] makes that a compile-time
//!   obligation rather than a convention.
//! * [`privacy`] — user-authored text is wrapped in [`privacy::Private`], which
//!   cannot be logged by accident.
//! * [`ids`] and [`units`] — the newtypes that stop us mixing up a model id
//!   with a model path, or bytes with tokens.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Production code must never panic (spec section 27). A test, however, reports
// failure *by* panicking, so the deny above would otherwise force every
// assertion helper into needless error plumbing.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod error;
pub mod ggml;
pub mod ids;
pub mod privacy;
pub mod runtime;
pub mod sse;
pub mod units;

pub use error::{Actionable, ErrorKind, ErrorReport, Remedy, RemedyAction, SettingsSection};
pub use ggml::GgmlType;
pub use ids::{ClientKey, InstanceId, JobId, ModelId};
pub use privacy::Private;
pub use runtime::RuntimeParams;
pub use sse::{SseDecodeError, SseDecoder, SseEvent};
pub use units::Bytes;
