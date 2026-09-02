//! Tool invocation seam.
//!
//! Slice 1 has no real tools — the loop drives a final-answer turn and, when a
//! model asks for a tool, hands it to a [`NullInvoker`] that declines cleanly.
//! The trait and its data types are the shape Slice 3 fills in, defined here so
//! the loop can be written against the seam rather than around it.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::permissions::{ApprovalDecision, ApprovalNeed};

/// One tool call the model asked for, reconstructed from the stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    /// The id the model assigned, echoed back on the matching tool result.
    pub id: String,
    /// The namespaced tool name.
    pub name: String,
    /// The raw JSON argument string, concatenated from the stream. Not parsed
    /// here: parsing and schema validation are the invoker's concern.
    pub arguments: String,
}

/// The result of running (or declining) a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutcome {
    /// The content appended to the conversation as the tool's result.
    pub content: String,
    /// Whether the tool failed. A failure is still a result the model sees —
    /// it is told what went wrong — rather than an error that ends the run.
    pub is_error: bool,
}

impl ToolOutcome {
    /// A successful result.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    /// A failed result, reported to the model rather than raised.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// A tool as declared to the model: name, description and a JSON Schema for its
/// arguments. This is the OpenAI `function` shape, minus the wrapper.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// A JSON Schema object. An empty object means "no arguments".
    pub parameters: Value,
}

impl ToolSchema {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// Executes the tool calls a model asks for.
///
/// Implementors declare which tools exist ([`schemas`](ToolInvoker::schemas))
/// and run one call at a time ([`invoke`](ToolInvoker::invoke)). Invocation is
/// cancellable: a run that is being torn down passes a cancelled token, and a
/// long-running tool is expected to stop.
#[async_trait]
pub trait ToolInvoker: Send + Sync {
    /// The tools available this run, declared to the model in the request.
    fn schemas(&self) -> Vec<ToolSchema>;

    /// Decide whether `call` may run now, needs a human decision, or is denied.
    ///
    /// The loop consults this before invoking, so a call that requires approval
    /// pauses the run rather than running unasked. The default auto-approves
    /// everything, which is what the [`NullInvoker`] and any tool-free run want;
    /// a real invoker (Slice 4's `BoundedExecutor`) delegates to a policy
    /// engine.
    fn approval_for(&self, _call: &ToolCall) -> ApprovalNeed {
        ApprovalNeed::AutoApprove
    }

    /// Run one tool call to a result.
    async fn invoke(&self, call: &ToolCall, cancel: CancellationToken) -> ToolOutcome;

    /// Persist a remembered grant for `call`.
    ///
    /// Called by the loop when a human grants a call and asks to remember it, so
    /// a matching call does not ask again until the grant expires. The default
    /// is a no-op — nothing is remembered — which suits an invoker with no
    /// policy of its own.
    fn remember(&self, _decision: &ApprovalDecision, _call: &ToolCall) {}
}

/// An invoker that declares no tools and declines every call.
///
/// The Slice 1 default: the loop is complete for the final-answer path, and a
/// model that nonetheless asks for a tool gets a controlled "not available"
/// result rather than a panic or a silent drop.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullInvoker;

#[async_trait]
impl ToolInvoker for NullInvoker {
    fn schemas(&self) -> Vec<ToolSchema> {
        Vec::new()
    }

    async fn invoke(&self, call: &ToolCall, _cancel: CancellationToken) -> ToolOutcome {
        ToolOutcome::error(format!(
            "the tool '{}' is not available in this run",
            call.name
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_invoker_declares_nothing_and_declines() {
        let invoker = NullInvoker;
        assert!(invoker.schemas().is_empty());
        let call = ToolCall {
            id: "call_1".into(),
            name: "datetime.now".into(),
            arguments: "{}".into(),
        };
        let outcome = invoker.invoke(&call, CancellationToken::new()).await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("datetime.now"));
    }
}
