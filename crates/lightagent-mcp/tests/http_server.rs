//! A live end-to-end test of the streamable-HTTP transport against a raw
//! loopback HTTP server that answers JSON-RPC over POST.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use lightagent_mcp::client::McpClient;
use lightagent_mcp::connection::HttpConnection;
use serde_json::{Value, json};

fn reply_for(body: &str) -> Option<String> {
    let msg: Value = serde_json::from_str(body).ok()?;
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => Some(
            json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}}}})
                .to_string(),
        ),
        "notifications/initialized" => None,
        "tools/list" => Some(
            json!({"jsonrpc":"2.0","id":id,"result":{"tools":[
                {"name":"ping","description":"Ping","inputSchema":{"type":"object"}}
            ]}})
            .to_string(),
        ),
        "tools/call" => Some(
            json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}})
                .to_string(),
        ),
        _ => Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"no"}}).to_string()),
    }
}

fn spawn_http_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 2048];
            // Read until we have headers and the whole declared body.
            let (mut header_end, mut content_len) = (None, 0usize);
            loop {
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    header_end = Some(pos + 4);
                    let headers = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                    for line in headers.lines() {
                        if let Some(v) = line.strip_prefix("content-length:") {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
                if let Some(end) = header_end
                    && buf.len() >= end + content_len
                {
                    break;
                }
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
            let body = header_end
                .map(|end| String::from_utf8_lossy(&buf[end..end + content_len]).into_owned())
                .unwrap_or_default();
            let response = match reply_for(&body) {
                Some(json) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    json.len(),
                    json
                ),
                None => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_owned(),
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[tokio::test]
async fn connects_lists_and_calls_over_http() {
    lightagent_provider_lightweight::ensure_provider();
    let port = spawn_http_server();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let connection = HttpConnection::new(
        client,
        format!("http://127.0.0.1:{port}/mcp"),
        Vec::new(),
        None,
    );
    let mcp = Arc::new(McpClient::new(Box::new(connection)));
    mcp.initialize().await.expect("initialize");
    let defs = mcp.list_tools().await.expect("list");
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "ping");
    let result = mcp.call_tool("ping", json!({})).await.expect("call");
    assert!(!result.is_error);
    assert_eq!(result.text, "pong");
}
