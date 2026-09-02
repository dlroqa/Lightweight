//! Tools for the Lightagent runtime.
//!
//! This crate fills the [`ToolInvoker`](lightagent_core::ToolInvoker) seam that
//! `lightagent-core` defines but deliberately leaves empty. It carries:
//!
//! * [`ToolDefinition`] and the [`Tool`] trait — what a tool is;
//! * [`ToolRegistry`] — the named set available to a run, and how it is scoped;
//! * [`schema`] — a small, dependency-free JSON-Schema validator that gates a
//!   model's arguments before a tool ever runs;
//! * [`BoundedExecutor`] — the concrete invoker the loop drives, enforcing the
//!   policy, a per-call timeout, cancellation and an output ceiling;
//! * the built-ins [`builtins::DateTimeNow`] and [`builtins::AgentDelegate`],
//!   the latter starting a fresh bounded child run under a worker profile
//!   through the *same* core loop — never a second one.
//!
//! It depends on `lightagent-core` and nothing that reaches a network: a tool
//! that needs the wire (the delegate's worker provider) receives it through a
//! [`ProviderFactory`](lightagent_core::ProviderFactory) injected by the caller,
//! so this crate never grows a transport dependency.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod builtins;
pub mod context;
pub mod definition;
pub mod executor;
pub mod output;
pub mod registry;
pub mod schema;

pub use context::{Clock, Delegation, ToolCtx};
pub use definition::{Tool, ToolDefinition};
pub use executor::BoundedExecutor;
pub use output::clamp;
pub use registry::ToolRegistry;
pub use schema::{SchemaError, validate};
