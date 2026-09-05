//! The MCP semantics over a [`Connection`]: handshake, tool discovery, calls.

#[cfg(test)]
use async_trait::async_trait;
use lightagent_core::RiskClass;
use serde_json::{Value, json};

use crate::connection::{Connection, PROTOCOL_VERSION};
use crate::jsonrpc::McpError;

/// A tool as an MCP server declares it.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDef {
    /// The server-side tool name (used verbatim in `tools/call`).
    pub name: String,
    /// A one-line description declared to the model.
    pub description: String,
    /// The JSON Schema for the tool's arguments.
    pub input_schema: Value,
    /// The risk class derived from the tool's annotations.
    pub risk: RiskClass,
}

/// The text and error flag of a `tools/call` result.
#[derive(Debug, Clone, PartialEq)]
pub struct CallResult {
    pub text: String,
    pub is_error: bool,
}

/// An MCP session: a [`Connection`] plus the protocol's methods.
pub struct McpClient {
    connection: Box<dyn Connection>,
}

impl McpClient {
    /// Wrap a connection.
    pub fn new(connection: Box<dyn Connection>) -> Self {
        Self { connection }
    }

    /// Complete the `initialize` handshake and confirm the server is ready.
    pub async fn initialize(&self) -> Result<(), McpError> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "lightagent", "version": env!("CARGO_PKG_VERSION") },
        });
        self.connection.request("initialize", params).await?;
        self.connection
            .notify("notifications/initialized", json!({}))
            .await
    }

    /// List every tool the server offers, following pagination.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.connection.request("tools/list", params).await?;
            if let Some(array) = result.get("tools").and_then(Value::as_array) {
                for item in array {
                    if let Some(tool) = parse_tool(item) {
                        tools.push(tool);
                    }
                }
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    /// Call a tool by its server-side `name`, returning its text and error flag.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallResult, McpError> {
        let params = json!({ "name": name, "arguments": arguments });
        let result = self.connection.request("tools/call", params).await?;
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = render_content(result.get("content"));
        Ok(CallResult { text, is_error })
    }
}

/// Parse one entry of a `tools/list` result into a [`McpToolDef`].
fn parse_tool(item: &Value) -> Option<McpToolDef> {
    let name = item.get("name").and_then(Value::as_str)?.to_owned();
    let description = item
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let input_schema = item
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object" }));
    let risk = risk_from_annotations(item.get("annotations"));
    Some(McpToolDef {
        name,
        description,
        input_schema,
        risk,
    })
}

/// Map MCP tool annotations to a risk class: a read-only tool reaches no further
/// than the network for data ([`RiskClass::External`]); a destructive one changes
/// state ([`RiskClass::Mutating`]); anything unannotated is treated as opaque
/// external code ([`RiskClass::Executable`]) so the default policy asks first.
pub fn risk_from_annotations(annotations: Option<&Value>) -> RiskClass {
    let hint = |key: &str| {
        annotations
            .and_then(|value| value.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    if hint("readOnlyHint") {
        RiskClass::External
    } else if hint("destructiveHint") {
        RiskClass::Mutating
    } else {
        RiskClass::Executable
    }
}

/// Flatten a `content` array to text, keeping the text parts and naming others.
fn render_content(content: Option<&Value>) -> String {
    let Some(array) = content.and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for part in array {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
            Some(other) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[{other} content]"));
            }
            None => {}
        }
    }
    out
}

/// A no-op connection used to unit-test the pure mapping helpers.
#[cfg(test)]
struct NullConnection;

#[cfg(test)]
#[async_trait]
impl Connection for NullConnection {
    async fn request(&self, _method: &str, _params: Value) -> Result<Value, McpError> {
        Ok(json!({}))
    }
    async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotations_map_to_risk() {
        assert_eq!(
            risk_from_annotations(Some(&json!({ "readOnlyHint": true }))),
            RiskClass::External
        );
        assert_eq!(
            risk_from_annotations(Some(&json!({ "destructiveHint": true }))),
            RiskClass::Mutating
        );
        assert_eq!(risk_from_annotations(None), RiskClass::Executable);
        assert_eq!(
            risk_from_annotations(Some(&json!({ "readOnlyHint": false }))),
            RiskClass::Executable
        );
    }

    #[test]
    fn parse_tool_defaults_a_missing_schema() {
        let tool = parse_tool(&json!({ "name": "ping" })).unwrap();
        assert_eq!(tool.name, "ping");
        assert_eq!(tool.input_schema, json!({ "type": "object" }));
        assert_eq!(tool.risk, RiskClass::Executable);
    }

    #[test]
    fn content_is_flattened_to_text() {
        let content = json!([
            { "type": "text", "text": "hello" },
            { "type": "image", "data": "..." },
            { "type": "text", "text": "world" }
        ]);
        assert_eq!(
            render_content(Some(&content)),
            "hello\n[image content]\nworld"
        );
    }

    #[tokio::test]
    async fn client_over_null_connection_lists_nothing() {
        let client = McpClient::new(Box::new(NullConnection));
        assert!(client.list_tools().await.unwrap().is_empty());
    }
}
