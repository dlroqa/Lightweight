//! Provider-neutral agent runtime.
//!
//! `lightagent-core` is the loop and its vocabulary, and nothing about how a
//! model is reached over the wire. It defines a run's identity ([`RunId`]), the
//! events a run emits ([`AgentEvent`]), the provider seam ([`AgentProvider`])
//! with a scripted [`MockProvider`] for tests, the tool-invocation seam
//! ([`ToolInvoker`]), the bounded state machine ([`AgentLoop`]), the profile
//! ("bot") model and store, the typed configuration, and the isolated on-disk
//! layout.
//!
//! It carries no HTTP, no SSE and no reqwest: a provider adapter (see
//! `lightagent-provider-lightweight`) owns all of that and depends on this
//! crate, never the other way around, so the loop can never grow a dependency
//! on any one transport.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod config;
pub mod event;
pub mod ids;
pub mod invoker;
pub mod limits;
pub mod loop_;
pub mod mock;
pub mod paths;
pub mod permissions;
pub mod profile;
pub mod provider;
pub mod tool_stream;

pub use config::{
    AgentConfig, ApprovalPolicy, Config, ConfigError, ConfigStore, InferenceConfig, SecretRef,
    SecurityConfig, WebConfig,
};
pub use event::{AgentEvent, StopReason};
pub use ids::RunId;
pub use invoker::{NullInvoker, ToolCall, ToolInvoker, ToolOutcome, ToolSchema};
pub use limits::RunLimits;
pub use loop_::{AgentError, AgentLoop, RunConfig, RunOutcome, Suspended};
pub use mock::MockProvider;
pub use paths::{LightagentPaths, PathsError};
pub use permissions::{
    ApprovalDecision, ApprovalId, ApprovalNeed, ApprovalRecord, ApprovalRequest, ApprovalStore,
    ApprovalStoreError, PolicyEngine, RiskClass, Scope,
};
pub use profile::{
    AgentProfile, ModelRouting, ProfileError, ProfileHandle, ProfileId, ProfileStore, ProviderKind,
};
pub use provider::{
    AgentProvider, FinishReason, ProviderError, ProviderEvent, ProviderFactory, ProviderMessage,
    ProviderRequest, ProviderStream, ProviderToolCall, Role, Usage,
};
pub use tool_stream::ToolCallAccumulator;
