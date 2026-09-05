//! An Agent Client Protocol (ACP) server for the Lightagent runtime.
//!
//! ACP is the protocol a code editor speaks to an agent it launches: the editor
//! is the client, Lightagent the agent. This crate is the server side over stdio
//! JSON-RPC — it negotiates capabilities, opens sessions, runs a prompt while
//! streaming `session/update`s, asks the client to approve a gated tool via
//! `session/request_permission`, and honours `session/cancel`. An ACP session's
//! prompt is one managed run, so all of that is the [`lightagent_api`]
//! [`RunManager`](lightagent_api::manager::RunManager)'s doing, wrapped in the
//! wire protocol.
//!
//! The wire shapes are aligned to the published ACP JSON schema (v1.21.0,
//! protocol version 1): the method names, the negotiated integer `protocolVersion`,
//! the `initialize` capabilities (`loadSession`, `promptCapabilities`, `agentInfo`),
//! `session/new` (accepting the editor's `cwd`, which becomes the run's confined
//! workspace, and its `mcpServers`), the `session/update` variants
//! (`agent_message_chunk`, `agent_thought_chunk`, `tool_call`/`tool_call_update`
//! with a `kind` and object `rawInput`), the `stopReason` values, and
//! `session/request_permission` with `allow_once`/`reject_once` options. It is
//! exercised by an in-process client in the tests but has not been run against a
//! live editor build; `session/load` and client-provided fs/terminal remain out
//! of scope, and each prompt is an independent run (in-session history is not yet
//! threaded).

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod protocol;
pub mod server;

pub use server::AcpServer;
