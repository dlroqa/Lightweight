//! The slice of JSON-RPC 2.0 an MCP client needs.
//!
//! Messages are plain [`serde_json::Value`] on the wire; this module only builds
//! the request/notification envelopes and reads a response's `result`/`error`,
//! so the client stays small and the transports stay dumb pipes.

use serde_json::{Value, json};
use thiserror::Error;

/// The JSON-RPC protocol version every message carries.
pub const VERSION: &str = "2.0";

/// Something that went wrong talking to an MCP server.
#[derive(Debug, Error)]
pub enum McpError {
    /// The transport (subprocess or HTTP) failed.
    #[error("transport error: {0}")]
    Transport(String),
    /// The server returned a JSON-RPC error object.
    #[error("server error {code}: {message}")]
    Rpc { code: i64, message: String },
    /// A response could not be understood.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The request outlived its deadline.
    #[error("request timed out")]
    Timeout,
}

/// Build a request envelope for `method` with `params` and `id`.
pub fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": VERSION, "id": id, "method": method, "params": params })
}

/// Build a notification envelope (no id, so no response is expected).
pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": VERSION, "method": method, "params": params })
}

/// Read the `result` from a response envelope, turning an `error` object or a
/// malformed envelope into the matching [`McpError`].
pub fn read_result(mut message: Value) -> Result<Value, McpError> {
    if let Some(error) = message.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let text = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_owned();
        return Err(McpError::Rpc {
            code,
            message: text,
        });
    }
    match message.get_mut("result") {
        Some(result) => Ok(result.take()),
        None => Err(McpError::Protocol("response had no result".to_owned())),
    }
}

/// Whether a message is a response (carries an `id` and a `result`/`error`),
/// and its numeric id when so — used to route a reply to its waiting caller.
pub fn response_id(message: &Value) -> Option<i64> {
    if message.get("result").is_none() && message.get("error").is_none() {
        return None;
    }
    message.get("id").and_then(Value::as_i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_is_well_formed() {
        let value = request(7, "tools/list", json!({}));
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "tools/list");
    }

    #[test]
    fn read_result_extracts_or_reports() {
        let ok = read_result(json!({ "jsonrpc": "2.0", "id": 1, "result": { "x": 1 } }));
        assert_eq!(ok.unwrap()["x"], 1);
        let err = read_result(
            json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32601, "message": "no method" } }),
        );
        assert!(matches!(err, Err(McpError::Rpc { code: -32601, .. })));
    }

    #[test]
    fn response_id_ignores_notifications() {
        assert_eq!(response_id(&json!({ "id": 3, "result": {} })), Some(3));
        assert_eq!(response_id(&json!({ "method": "log", "params": {} })), None);
    }
}
