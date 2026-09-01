//! The provider seam: what the loop needs from a model, and nothing about how
//! any one provider speaks to it.
//!
//! A provider takes a [`ProviderRequest`] and yields a [`ProviderStream`] of
//! [`ProviderEvent`]s. The events are deliberately close to the OpenAI
//! streaming shape — a role marker, reasoning and content deltas, index-keyed
//! tool-call deltas, and a terminal finish — because that is the contract the
//! gateway speaks and the one Slice 2's adapter reproduces. The types here name
//! no HTTP, no SSE and no JSON: the adapter owns all of that.

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::invoker::ToolSchema;

/// A message in the conversation sent to the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderMessage {
    pub role: Role,
    pub content: String,
    /// An optional author name (used for tool messages and named participants).
    pub name: Option<String>,
    /// Set on a `tool` message: which call this result answers.
    pub tool_call_id: Option<String>,
    /// Set on an `assistant` message that itself asked for tools, so the
    /// exchange replays faithfully on the next turn.
    pub tool_calls: Vec<ProviderToolCall>,
}

impl ProviderMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::plain(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::plain(Role::Assistant, content)
    }

    /// An assistant message that asked for one or more tools.
    pub fn assistant_tool_calls(content: impl Into<String>, calls: Vec<ProviderToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: calls,
        }
    }

    /// A tool result answering the call with `tool_call_id`.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            name: None,
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: Vec::new(),
        }
    }

    fn plain(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }
}

/// Who authored a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    /// The wire spelling every OpenAI-compatible endpoint expects.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// A completed tool call as it is replayed inside an assistant message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Everything a provider needs to run one turn.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderRequest {
    /// The catalog model id, echoed by the provider.
    pub model: String,
    pub messages: Vec<ProviderMessage>,
    /// Tools declared to the model this turn. Empty means none are offered.
    pub tools: Vec<ToolSchema>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

impl ProviderRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ProviderMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
        }
    }
}

/// Token accounting for a turn, when the provider reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Why a turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishReason {
    /// The model chose to stop — a final answer.
    Stop,
    /// The model asked for one or more tools.
    ToolCalls,
    /// The model hit its token budget.
    Length,
    /// The provider reported an error mid-stream.
    Error,
}

/// One decoded event from a provider stream.
///
/// The order mirrors the wire contract: a [`RoleStarted`](ProviderEvent::RoleStarted)
/// marker, then any number of [`Reasoning`](ProviderEvent::Reasoning),
/// [`Content`](ProviderEvent::Content) and
/// [`ToolCallDelta`](ProviderEvent::ToolCallDelta) events, then exactly one
/// [`Finished`](ProviderEvent::Finished).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderEvent {
    /// The opening role marker of a streamed completion.
    RoleStarted,
    /// A fragment of the model's reasoning trace.
    Reasoning(String),
    /// A fragment of the model's visible answer.
    Content(String),
    /// A fragment of a tool call. `id` and `name` arrive once, on the first
    /// delta of each index; `arguments` are concatenated across deltas.
    ToolCallDelta {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    },
    /// The terminal event of the stream.
    Finished {
        reason: FinishReason,
        usage: Option<Usage>,
    },
}

/// A boxed stream of decoded provider events.
pub type ProviderStream = BoxStream<'static, Result<ProviderEvent, ProviderError>>;

/// Why a provider request or stream failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// The request never reached the model — a connection or transport fault.
    #[error("could not reach the provider: {0}")]
    Transport(String),
    /// The provider refused the request before streaming anything.
    #[error("the provider rejected the request: {0}")]
    Upstream(String),
    /// The stream broke after it began.
    #[error("the provider stream failed: {0}")]
    Stream(String),
    /// A single server-sent event grew past the cap without ending — refused
    /// rather than buffered until the machine runs out of memory.
    #[error("a provider event exceeded {limit} bytes without ending")]
    FrameTooLarge { limit: usize },
    /// The provider ended the stream with an error chunk.
    #[error("the provider reported an error mid-stream: {message}")]
    MidStream { message: String },
}

/// A source of model turns, independent of how it talks to one.
#[async_trait]
pub trait AgentProvider: Send + Sync {
    /// Start one turn and return the decoded event stream.
    ///
    /// Cancellation ends the stream: dropping it closes the underlying
    /// connection, which is what makes a real provider stop generating.
    async fn stream(
        &self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> Result<ProviderStream, ProviderError>;
}
