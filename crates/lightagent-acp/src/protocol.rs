//! The JSON-RPC envelopes and the [`AgentEvent`] → `session/update` mapping ACP
//! needs.
//!
//! ACP is JSON-RPC 2.0 over stdio (newline-delimited here). This module builds
//! request/response/notification envelopes and translates one canonical
//! [`AgentEvent`] into the ACP `session/update` (or a permission request), so the
//! server code stays about transport and lifecycle, not wire shapes.

use lightagent_core::{AgentEvent, StopReason};
use serde_json::{Value, json};

/// The ACP protocol version this server implements.
pub const PROTOCOL_VERSION: u32 = 1;

/// A JSON-RPC success response.
pub fn response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A JSON-RPC error response.
pub fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A JSON-RPC notification (no id).
pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// A JSON-RPC request the server sends to the client (needs a response).
pub fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// Map an [`AgentEvent`] to the `update` object of a `session/update`
/// notification, or `None` when the event carries no client-visible update
/// (approvals and the terminal marker are handled by the server directly).
pub fn update_for(event: &AgentEvent) -> Option<Value> {
    match event {
        AgentEvent::Content { text } => Some(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": text },
        })),
        AgentEvent::Reasoning { text } => Some(json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": text },
        })),
        AgentEvent::ToolCallRequested { call } => Some(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": call.id,
            "title": call.name,
            "status": "pending",
            "rawInput": call.arguments,
        })),
        AgentEvent::ToolCallStarted { id, .. } => Some(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": "in_progress",
        })),
        AgentEvent::ToolCallCompleted { id, outcome } => Some(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": if outcome.is_error { "failed" } else { "completed" },
            "content": [ { "type": "content", "content": { "type": "text", "text": outcome.content } } ],
        })),
        AgentEvent::Error { message } => Some(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": format!("error: {message}") },
        })),
        _ => None,
    }
}

/// The ACP `stopReason` string for a terminal [`StopReason`].
pub fn stop_reason(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::Cancelled => "cancelled",
        StopReason::MaxTurns => "max_turn_requests",
        StopReason::Error => "refusal",
        _ => "end_turn",
    }
}

/// Extract the plain text from an ACP prompt's content-block array.
pub fn prompt_text(prompt: &Value) -> String {
    let Some(blocks) = prompt.as_array() else {
        return String::new();
    };
    let mut out = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = block.get("text").and_then(Value::as_str)
        {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightagent_core::{ToolCall, ToolOutcome};

    #[test]
    fn content_maps_to_a_message_chunk() {
        let update = update_for(&AgentEvent::Content { text: "hi".into() }).unwrap();
        assert_eq!(update["sessionUpdate"], "agent_message_chunk");
        assert_eq!(update["content"]["text"], "hi");
    }

    #[test]
    fn a_failed_tool_reports_failed() {
        let update = update_for(&AgentEvent::ToolCallCompleted {
            id: "t1".into(),
            outcome: ToolOutcome::error("boom"),
        })
        .unwrap();
        assert_eq!(update["status"], "failed");
        assert_eq!(update["toolCallId"], "t1");
    }

    #[test]
    fn tool_request_carries_title_and_input() {
        let call = ToolCall {
            id: "t1".into(),
            name: "fs.read".into(),
            arguments: "{\"path\":\"a\"}".into(),
        };
        let update = update_for(&AgentEvent::ToolCallRequested { call }).unwrap();
        assert_eq!(update["sessionUpdate"], "tool_call");
        assert_eq!(update["title"], "fs.read");
    }

    #[test]
    fn stop_reasons_map() {
        assert_eq!(stop_reason(&StopReason::Cancelled), "cancelled");
        assert_eq!(stop_reason(&StopReason::EndTurn), "end_turn");
        assert_eq!(stop_reason(&StopReason::Error), "refusal");
    }

    #[test]
    fn prompt_text_joins_text_blocks() {
        let prompt = json!([{ "type": "text", "text": "hello" }, { "type": "image" }, { "type": "text", "text": "world" }]);
        assert_eq!(prompt_text(&prompt), "hello\nworld");
    }
}
