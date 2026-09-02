//! A remote MCP tool presented to the runtime as a local [`Tool`].
//!
//! One [`McpTool`] wraps a shared [`McpClient`] and a server-side tool name. It
//! carries the tool's real input schema (validated locally before a call, like
//! any tool) and the risk class derived from the server's annotations, and its
//! `call` is a `tools/call` round-trip whose result becomes the tool outcome.

use std::sync::Arc;

use async_trait::async_trait;
use lightagent_core::{Scope, ToolOutcome};
use lightagent_tools::{Tool, ToolCtx, ToolDefinition};
use serde_json::Value;

use crate::client::{McpClient, McpToolDef};

/// A single remote tool, callable through its server's client.
pub struct McpTool {
    client: Arc<McpClient>,
    remote_name: String,
    definition: ToolDefinition,
}

impl McpTool {
    /// Build the local tool for `def` on `server`, sharing `client`.
    pub fn new(server: &str, client: Arc<McpClient>, def: McpToolDef) -> Self {
        let definition = ToolDefinition::new(
            tool_name(server, &def.name),
            def.description,
            def.input_schema,
            def.risk,
            vec![Scope::new(format!("mcp:{}", sanitize(server)))],
        );
        Self {
            client,
            remote_name: def.name,
            definition,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, args: &Value, _ctx: &ToolCtx) -> ToolOutcome {
        match self.client.call_tool(&self.remote_name, args.clone()).await {
            Ok(result) => ToolOutcome {
                content: result.text,
                is_error: result.is_error,
            },
            Err(error) => ToolOutcome::error(format!(
                "mcp call to {:?} failed: {error}",
                self.remote_name
            )),
        }
    }
}

/// The local name for a server's tool: `mcp.<server>.<tool>`, each segment
/// reduced to a safe charset so a server or tool name can never inject a dot or
/// other character that would confuse the registry or the model.
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("mcp.{}.{}", sanitize(server), sanitize(tool))
}

fn sanitize(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_namespaced_and_sanitized() {
        assert_eq!(tool_name("git", "status"), "mcp.git.status");
        assert_eq!(tool_name("my server", "do.thing"), "mcp.my_server.do_thing");
    }
}
