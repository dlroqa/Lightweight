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
//! * the built-ins [`builtins::DateTimeNow`], [`builtins::AgentDelegate`] (which
//!   starts a fresh bounded child run under a worker profile through the *same*
//!   core loop — never a second one), and [`builtins::WebFetch`] /
//!   [`builtins::WebSearch`] (read-only network access under a guard).
//!
//! What a tool needs from the outside is always injected, never built here: the
//! delegate's worker provider arrives as a
//! [`ProviderFactory`](lightagent_core::ProviderFactory), and the web tools' HTTP
//! client and resolved policy arrive as a [`WebContext`] — the caller builds the
//! `reqwest::Client` (having installed the TLS provider) and resolves any secret,
//! so this crate makes requests but owns neither client construction nor the
//! config format.

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
pub mod workspace;

pub use context::{
    Clock, Delegation, SkillContext, ToolCtx, WebContext, WebPolicy, WorkspaceContext,
    WorkspacePolicy,
};
pub use definition::{Tool, ToolDefinition};
pub use executor::BoundedExecutor;
pub use output::clamp;
pub use registry::ToolRegistry;
pub use schema::{SchemaError, validate};
pub use workspace::Workspace;
