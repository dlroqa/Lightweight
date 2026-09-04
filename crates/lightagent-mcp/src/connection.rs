//! The two MCP transports behind one [`Connection`] trait.
//!
//! [`StdioConnection`] spawns the server as a subprocess and speaks
//! newline-delimited JSON-RPC over its stdin/stdout, correlating each reply to
//! its request by id through a background reader task. [`HttpConnection`] speaks
//! the streamable-HTTP transport: it POSTs a request and reads the reply from
//! either a JSON body or a `text/event-stream` body, carrying the server's
//! `Mcp-Session-Id` on later calls. Both hand the client a `result` or an
//! [`McpError`]; neither knows any MCP method.

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex as AsyncMutex, oneshot};

use crate::jsonrpc::{self, McpError};

/// A live link to one MCP server.
#[async_trait]
pub trait Connection: Send + Sync {
    /// Send a request and await its correlated result.
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError>;
    /// Send a fire-and-forget notification.
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError>;
}

/// The MCP protocol version this client speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

// --- stdio -----------------------------------------------------------------

struct StdioInner {
    pending: StdMutex<HashMap<i64, oneshot::Sender<Value>>>,
    stdin: AsyncMutex<ChildStdin>,
    next_id: AtomicI64,
    timeout: Duration,
}

/// A server run as a subprocess, spoken to over stdin/stdout.
pub struct StdioConnection {
    inner: std::sync::Arc<StdioInner>,
    // Held so the child lives as long as the connection; killed on drop.
    _child: AsyncMutex<Child>,
}

impl StdioConnection {
    /// Spawn `command` and complete no handshake — the caller drives that.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        timeout: Duration,
    ) -> Result<Self, McpError> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .envs(env.iter().map(|(k, v)| (k.clone(), v.clone())))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|error| {
            McpError::Transport(format!("could not start {command:?}: {error}"))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("child has no stdin".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("child has no stdout".to_owned()))?;

        let inner = std::sync::Arc::new(StdioInner {
            pending: StdMutex::new(HashMap::new()),
            stdin: AsyncMutex::new(stdin),
            next_id: AtomicI64::new(1),
            timeout,
        });

        let reader_inner = std::sync::Arc::clone(&inner);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue; // a line that is not JSON is not ours to route
                };
                if let Some(id) = jsonrpc::response_id(&message) {
                    let sender = reader_inner
                        .pending
                        .lock()
                        .ok()
                        .and_then(|mut pending| pending.remove(&id));
                    if let Some(sender) = sender {
                        let _ = sender.send(message);
                    }
                }
                // Server-initiated requests and notifications are ignored: this
                // client exposes tools and needs no server-driven callbacks.
            }
        });

        Ok(Self {
            inner,
            _child: AsyncMutex::new(child),
        })
    }
}

#[async_trait]
impl Connection for StdioConnection {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        if let Ok(mut pending) = self.inner.pending.lock() {
            pending.insert(id, sender);
        }
        let line = format!("{}\n", jsonrpc::request(id, method, params));
        {
            let mut stdin = self.inner.stdin.lock().await;
            stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|error| McpError::Transport(error.to_string()))?;
            stdin
                .flush()
                .await
                .map_err(|error| McpError::Transport(error.to_string()))?;
        }
        match tokio::time::timeout(self.inner.timeout, receiver).await {
            Ok(Ok(message)) => jsonrpc::read_result(message),
            Ok(Err(_)) => Err(McpError::Transport("connection closed".to_owned())),
            Err(_) => {
                if let Ok(mut pending) = self.inner.pending.lock() {
                    pending.remove(&id);
                }
                Err(McpError::Timeout)
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let line = format!("{}\n", jsonrpc::notification(method, params));
        let mut stdin = self.inner.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| McpError::Transport(error.to_string()))?;
        stdin
            .flush()
            .await
            .map_err(|error| McpError::Transport(error.to_string()))
    }
}

// --- streamable HTTP -------------------------------------------------------

/// A server reached over the streamable-HTTP transport.
pub struct HttpConnection {
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    bearer: Option<String>,
    session: StdMutex<Option<String>>,
    next_id: AtomicI64,
}

impl HttpConnection {
    /// Build a connection over an already-constructed client (redirects and TLS
    /// are the caller's to set), targeting `url`.
    pub fn new(
        client: reqwest::Client,
        url: String,
        headers: Vec<(String, String)>,
        bearer: Option<String>,
    ) -> Self {
        Self {
            client,
            url,
            headers,
            bearer,
            session: StdMutex::new(None),
            next_id: AtomicI64::new(1),
        }
    }

    fn builder(&self, body: &Value) -> reqwest::RequestBuilder {
        let mut builder = self
            .client
            .post(&self.url)
            .header(ACCEPT, "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(body);
        for (name, value) in &self.headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                builder = builder.header(name, value);
            }
        }
        if let Some(bearer) = &self.bearer {
            builder = builder.bearer_auth(bearer);
        }
        if let Some(session) = self.session.lock().ok().and_then(|s| s.clone()) {
            builder = builder.header("Mcp-Session-Id", session);
        }
        builder
    }

    fn capture_session(&self, response: &reqwest::Response) {
        if let Some(value) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            && let Ok(mut session) = self.session.lock()
        {
            *session = Some(value.to_owned());
        }
    }
}

#[async_trait]
impl Connection for HttpConnection {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = jsonrpc::request(id, method, params);
        let response = self
            .builder(&body)
            .send()
            .await
            .map_err(|error| McpError::Transport(error.to_string()))?;
        self.capture_session(&response);
        if !response.status().is_success() {
            return Err(McpError::Transport(format!(
                "HTTP {}",
                response.status().as_u16()
            )));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        // Bounded, so a remote server (which may be reached over the network and
        // is not fully trusted) cannot exhaust memory with an unbounded body.
        let text = read_bounded_text(response, MAX_HTTP_BODY_BYTES).await?;
        let message = if content_type.contains("text/event-stream") {
            sse_message_for(&text, id)
                .ok_or_else(|| McpError::Protocol("no response in the event stream".to_owned()))?
        } else {
            serde_json::from_str::<Value>(&text)
                .map_err(|error| McpError::Protocol(format!("response was not JSON: {error}")))?
        };
        jsonrpc::read_result(message)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let body = jsonrpc::notification(method, params);
        let response = self
            .builder(&body)
            .send()
            .await
            .map_err(|error| McpError::Transport(error.to_string()))?;
        self.capture_session(&response);
        Ok(())
    }
}

/// The most bytes an HTTP MCP reply may be before it is refused.
///
/// A JSON-RPC message is small; this is a generous ceiling that only a runaway
/// or hostile server reaches. Chosen to match the 8 MiB frame cap the provider
/// adapter's SSE decoder uses.
const MAX_HTTP_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Read a response body as text, refusing one larger than `cap`.
///
/// `reqwest::Response::text` would buffer the whole body however large it is;
/// this streams it and stops with an error once `cap` is exceeded, so a server
/// cannot exhaust memory with an unbounded reply.
async fn read_bounded_text(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<String, McpError> {
    let mut buffer = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| McpError::Transport(error.to_string()))?
    {
        if buffer.len() + chunk.len() > cap {
            return Err(McpError::Protocol(format!(
                "the server's reply exceeded {cap} bytes"
            )));
        }
        buffer.extend_from_slice(&chunk);
    }
    String::from_utf8(buffer)
        .map_err(|error| McpError::Protocol(format!("the reply was not UTF-8: {error}")))
}

/// Find the JSON-RPC response with `id` among an SSE body's `data:` events.
fn sse_message_for(body: &str, id: i64) -> Option<Value> {
    let mut fallback = None;
    for block in body.split("\n\n") {
        let mut data = String::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest.trim_start());
            }
        }
        if data.is_empty() {
            continue;
        }
        if let Ok(message) = serde_json::from_str::<Value>(&data) {
            if jsonrpc::response_id(&message) == Some(id) {
                return Some(message);
            }
            if fallback.is_none() && jsonrpc::response_id(&message).is_some() {
                fallback = Some(message);
            }
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn sse_body_yields_the_matching_response() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"ok\":true}}\n\n";
        let message = sse_message_for(body, 5).unwrap();
        assert_eq!(message["result"]["ok"], json!(true));
        assert!(
            sse_message_for(body, 99).is_some(),
            "falls back to a response"
        );
        assert!(sse_message_for("data: not json\n\n", 5).is_none());
    }

    /// A one-shot loopback server that answers with a body of `body_len` bytes.
    async fn serve_body(body_len: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let body = "a".repeat(body_len);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn a_body_within_the_cap_is_read() {
        lightagent_provider_lightweight::ensure_provider();
        let url = serve_body(500).await;
        let client = reqwest::Client::builder().build().unwrap();
        let response = client.get(&url).send().await.unwrap();
        let text = read_bounded_text(response, 8192).await.unwrap();
        assert_eq!(text.len(), 500);
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_refused_not_buffered() {
        lightagent_provider_lightweight::ensure_provider();
        let url = serve_body(4096).await;
        let client = reqwest::Client::builder().build().unwrap();
        let response = client.get(&url).send().await.unwrap();
        let error = read_bounded_text(response, 1024).await.unwrap_err();
        match error {
            McpError::Protocol(message) => assert!(message.contains("exceeded")),
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }
}
