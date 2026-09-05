//! Connecting the configured MCP servers and exposing their tools.
//!
//! [`McpHub::connect`] tries each server in turn: a failure to reach one is
//! recorded and skipped, never fatal — one broken server must not take the agent
//! down. Each connected server's tools become [`McpTool`]s sharing that server's
//! client, so the connections live exactly as long as the tools do.

use std::sync::Arc;
use std::time::Duration;

use lightagent_tools::Tool;

use crate::client::{McpClient, McpToolDef};
use crate::connection::{Connection, HttpConnection, StdioConnection};
use crate::jsonrpc::McpError;
use crate::tool::McpTool;

/// How to reach one server, resolved from config by the caller.
pub enum McpTransportSpec {
    /// Spawn a subprocess and speak over its stdio.
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    /// Reach a streamable-HTTP endpoint.
    Http {
        url: String,
        headers: Vec<(String, String)>,
        bearer: Option<String>,
    },
}

/// A named server to connect.
pub struct McpServerSpec {
    pub name: String,
    pub transport: McpTransportSpec,
}

/// The result of connecting the configured servers.
pub struct McpHub {
    /// Every connected server's tools, ready to add to a registry.
    pub tools: Vec<Arc<dyn Tool>>,
    /// One line per connected server, for a startup summary.
    pub connected: Vec<String>,
    /// The servers that could not be reached, with why.
    pub errors: Vec<(String, String)>,
}

impl McpHub {
    /// Connect every spec, collecting tools and recording failures.
    pub async fn connect(
        specs: Vec<McpServerSpec>,
        timeout: Duration,
        http_client: reqwest::Client,
    ) -> Self {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut connected = Vec::new();
        let mut errors = Vec::new();
        for spec in specs {
            match connect_one(&spec, timeout, &http_client).await {
                Ok((client, defs)) => {
                    let client = Arc::new(client);
                    let count = defs.len();
                    for def in defs {
                        tools.push(Arc::new(McpTool::new(&spec.name, Arc::clone(&client), def)));
                    }
                    connected.push(format!("{} ({count} tools)", spec.name));
                }
                Err(error) => errors.push((spec.name.clone(), error.to_string())),
            }
        }
        Self {
            tools,
            connected,
            errors,
        }
    }
}

async fn connect_one(
    spec: &McpServerSpec,
    timeout: Duration,
    http_client: &reqwest::Client,
) -> Result<(McpClient, Vec<McpToolDef>), McpError> {
    let connection: Box<dyn Connection> = match &spec.transport {
        McpTransportSpec::Stdio { command, args, env } => {
            Box::new(StdioConnection::spawn(command, args, env, timeout).await?)
        }
        McpTransportSpec::Http {
            url,
            headers,
            bearer,
        } => Box::new(HttpConnection::new(
            http_client.clone(),
            url.clone(),
            headers.clone(),
            bearer.clone(),
        )),
    };
    let client = McpClient::new(connection);
    client.initialize().await?;
    let defs = client.list_tools().await?;
    Ok((client, defs))
}
