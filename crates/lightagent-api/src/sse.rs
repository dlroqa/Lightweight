//! Mapping the canonical [`AgentEvent`] stream to named SSE events.
//!
//! The wire names follow the master plan's vocabulary (`run.started`,
//! `model.delta`, `tool.requested`, `approval.required`, `run.completed`, …), so
//! a browser or a script reads the run by the `event:` name and the JSON `data:`
//! without knowing the Rust enum.

use axum::response::sse::Event;
use lightagent_core::{AgentEvent, StopReason};
use serde_json::json;

/// The SSE event name for an [`AgentEvent`].
pub fn name(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::RunStarted { .. } => "run.started",
        AgentEvent::Reasoning { .. } | AgentEvent::Content { .. } => "model.delta",
        AgentEvent::ToolCallRequested { .. } => "tool.requested",
        AgentEvent::ToolCallStarted { .. } => "tool.started",
        AgentEvent::ToolCallCompleted { outcome, .. } => {
            if outcome.is_error {
                "tool.failed"
            } else {
                "tool.output"
            }
        }
        AgentEvent::AwaitingApproval { .. } => "approval.required",
        AgentEvent::TurnCompleted { .. } => "turn.completed",
        AgentEvent::RunCompleted { reason } => match reason {
            StopReason::Cancelled => "run.cancelled",
            StopReason::Error => "run.failed",
            _ => "run.completed",
        },
        AgentEvent::Error { .. } => "error",
        _ => "event",
    }
}

/// The JSON `data:` payload for an [`AgentEvent`].
pub fn data(event: &AgentEvent) -> serde_json::Value {
    match event {
        AgentEvent::RunStarted { run, parent } => json!({
            "run": run.as_str(),
            "parent": parent.as_ref().map(|id| id.as_str()),
        }),
        AgentEvent::Reasoning { text } => json!({ "reasoning": text }),
        AgentEvent::Content { text } => json!({ "content": text }),
        AgentEvent::ToolCallRequested { call } => json!({
            "id": call.id,
            "name": call.name,
            "arguments": call.arguments,
        }),
        AgentEvent::ToolCallStarted { id, name } => json!({ "id": id, "name": name }),
        AgentEvent::ToolCallCompleted { id, outcome } => json!({
            "id": id,
            "is_error": outcome.is_error,
            "content": outcome.content,
        }),
        AgentEvent::AwaitingApproval { id, name } => json!({ "id": id, "name": name }),
        AgentEvent::TurnCompleted { usage: Some(usage) } => json!({
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "total_tokens": usage.total_tokens,
        }),
        AgentEvent::TurnCompleted { usage: None } => json!({}),
        AgentEvent::RunCompleted { reason } => json!({ "reason": format!("{reason:?}") }),
        AgentEvent::Error { message } => json!({ "message": message }),
        _ => json!({}),
    }
}

/// Build the axum SSE [`Event`] for an [`AgentEvent`].
pub fn to_sse(event: &AgentEvent) -> Event {
    Event::default()
        .event(name(event))
        .data(data(event).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightagent_core::ids::RunId;

    #[test]
    fn names_follow_the_canonical_vocabulary() {
        assert_eq!(
            name(&AgentEvent::RunStarted {
                run: RunId::new(),
                parent: None
            }),
            "run.started"
        );
        assert_eq!(
            name(&AgentEvent::Content { text: "hi".into() }),
            "model.delta"
        );
        assert_eq!(
            name(&AgentEvent::RunCompleted {
                reason: StopReason::Cancelled
            }),
            "run.cancelled"
        );
        assert_eq!(
            name(&AgentEvent::RunCompleted {
                reason: StopReason::Error
            }),
            "run.failed"
        );
        assert_eq!(
            name(&AgentEvent::RunCompleted {
                reason: StopReason::EndTurn
            }),
            "run.completed"
        );
    }

    #[test]
    fn content_data_carries_the_text() {
        let value = data(&AgentEvent::Content {
            text: "hello".into(),
        });
        assert_eq!(value["content"], "hello");
    }
}
