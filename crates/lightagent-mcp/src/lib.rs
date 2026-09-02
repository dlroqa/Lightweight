//! A minimal Model Context Protocol (MCP) client for the Lightagent runtime.
//!
//! Lightagent is an MCP **client**: it connects to configured MCP **servers** and
//! presents each server's tools to the one agent loop as ordinary
//! [`Tool`](lightagent_tools::Tool)s, so a remote tool is gated, bounded and
//! audited exactly like a built-in one. Two transports are supported — a
//! subprocess over stdio and a streamable-HTTP endpoint — behind a single
//! [`Connection`] trait; the [`McpClient`] adds the protocol (handshake, tool
//! discovery, calls) and [`McpHub`] connects the configured servers and hands
//! back their tools.
//!
//! Only what a tool-using client needs is implemented: server-initiated requests,
//! resources, prompts and sampling are out of scope. What a tool is allowed to do
//! is still the runtime's decision — a server's annotations only *inform* the
//! risk class (see [`client::risk_from_annotations`]); the policy still gates it.
//!
//! No transport logic leaks upward: the HTTP client and any resolved secret are
//! injected by the caller, mirroring the rest of the workspace.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod client;
pub mod connection;
pub mod hub;
pub mod jsonrpc;
pub mod tool;

pub use client::{CallResult, McpClient, McpToolDef};
pub use connection::{Connection, HttpConnection, StdioConnection};
pub use hub::{McpHub, McpServerSpec, McpTransportSpec};
pub use jsonrpc::McpError;
pub use tool::McpTool;
