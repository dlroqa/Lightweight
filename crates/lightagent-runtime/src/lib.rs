//! Device-aware runtime/placement selector for Lightagent.
//!
//! Lightagent talks to the model through an OpenAI-compatible gateway, and the
//! gateway also exposes a small **control plane** (`GET /api/v1/gateway`,
//! `GET /api/v1/system`, `GET /api/v1/models`, `POST /api/v1/models/{id}/load`,
//! `POST /api/v1/models/unload`). This crate is a thin, read-first client of
//! that control plane: it reports what device the engine runs on and what it can
//! do, chooses runtime parameters from a policy, and — only when asked — places
//! a chosen model with those parameters.
//!
//! It depends on `lightagent-core` and never on any `lightweight-*` crate: the
//! control-plane wire types are reproduced here rather than imported, so the
//! "harness knows nothing of the engine's internals" invariant holds in the
//! dependency graph, not merely by convention.
//!
//! ## Device kinds and the CPU-only engine
//!
//! The pinned engine runs on the CPU. Its capability report already carries a
//! [`DeviceKind`] with `Cuda`/`Metal`/`Rocm` variants reserved for the backends
//! section 29 of the engine plan will add. This crate mirrors that enum and
//! selects against it, so a `preferred_device = "cuda"` policy already resolves
//! correctly the day the engine begins reporting a CUDA device — no change here.
//! Until then, an explicit non-CPU preference either falls back to the CPU (the
//! default) or is refused with a clear message, and it never edits the engine.
//!
//! ## Placement never mutates state implicitly
//!
//! Reading the gateway, the system probe and the catalog is side-effect free.
//! *Placing* a model swaps what the engine has resident, so it is always an
//! explicit action — a `lightagent runtime place` invocation — and never a
//! silent consequence of a chat turn.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod client;
pub mod placement;
pub mod tls;
pub mod wire;

pub use client::{PlaceOutcome, RuntimeClient, RuntimeEndpoint, RuntimeError};
pub use placement::{DeviceKind, LoadPlan, PlacementError, PlacementPolicy, PlacementResolution};
pub use tls::ensure_provider;
pub use wire::{EngineCapabilities, GatewayInfo, LoadDefaults, ModelStatus, SystemInfo};
