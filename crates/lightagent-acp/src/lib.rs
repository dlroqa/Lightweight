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
//! This targets the ACP wire shapes and is exercised by an in-process client in
//! the tests; it has not been validated against a shipping editor, and each
//! prompt is an independent run (in-session history is not yet threaded).

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod protocol;
pub mod server;

pub use server::AcpServer;
