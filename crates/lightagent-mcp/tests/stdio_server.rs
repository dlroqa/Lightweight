//! A live end-to-end test against a real MCP server: a small Python script that
//! speaks the protocol over stdio. Skipped when `python3` is unavailable.

use std::io::Write as _;
use std::time::Duration;

use lightagent_mcp::client::McpClient;
use lightagent_mcp::connection::StdioConnection;
use lightagent_mcp::tool::McpTool;
use lightagent_tools::{Tool, ToolCtx};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const SERVER: &str = r#"
import sys, json

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"pytest","version":"0"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[
            {"name":"echo","description":"Echo text back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]},"annotations":{"readOnlyHint":True}}
        ]}})
    elif method == "tools/call":
        args = msg["params"].get("arguments", {})
        send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"echo: " + args.get("text","")}],"isError":False}})
    else:
        send({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"no method"}})
"#;

fn python_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn connects_lists_and_calls_a_real_stdio_server() {
    if !python_available() {
        eprintln!("python3 unavailable; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("lightagent-mcp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("server.py");
    std::fs::File::create(&script)
        .unwrap()
        .write_all(SERVER.as_bytes())
        .unwrap();

    let connection = StdioConnection::spawn(
        "python3",
        &[script.to_string_lossy().into_owned()],
        &[],
        Duration::from_secs(5),
    )
    .await
    .expect("spawn server");
    let client = Arc::new(McpClient::new(Box::new(connection)));
    client.initialize().await.expect("initialize");

    let defs = client.list_tools().await.expect("list tools");
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "echo");
    assert_eq!(defs[0].risk, lightagent_core::RiskClass::External);

    let tool = McpTool::new("pytest", Arc::clone(&client), defs[0].clone());
    assert_eq!(tool.definition().name, "mcp.pytest.echo");

    let ctx = ToolCtx::new(CancellationToken::new());
    let out = tool.call(&json!({ "text": "hi" }), &ctx).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "echo: hi");

    std::fs::remove_dir_all(&dir).ok();
}
