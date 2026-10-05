//! A federated model router for Lightweight.
//!
//! One OpenAI-compatible endpoint in front of any number of Lightweight
//! gateways. A client — Lightagent, an SDK, `curl` — discovers and sends stable
//! logical names (`Coder`, `Fast`); the router decides which node answers and
//! what that node calls the model; the node decides how the model runs.
//!
//! ```text
//! client ──model="Coder"──▶ router ──model="QwenCoder"──▶ node A (or B)
//!        ◀─model="Coder"───        ◀─model="QwenCoder"──
//! ```
//!
//! What this crate owns: routes, the deployment registry, node health, the
//! priority policy, forwarding, stream relaying, pre-response failover, and its
//! own logs and metrics. What it deliberately does not: GGUF, memory
//! estimates, admission, scheduling, engine lifecycle, or loading anything.
//! Those stay on the node, and nothing here can reach them — the router only
//! speaks the node's public `/v1` surface.
//!
//! Selection is deterministic in this version: priority order, filtered by
//! health. See `docs/ROUTER.md` for the roadmap beyond it.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod config;
pub mod domain;
pub mod health;
pub mod select;

pub use config::{RouterConfig, load, validate};
pub use domain::Topology;
