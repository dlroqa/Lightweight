//! What a backend is asked to generate, and what it reports while doing it.
//!
//! These types are deliberately **not** the OpenAI wire format. The gateway
//! translates inbound requests into them and translates the events back out
//! again, which is what lets the wire contract change — or a second one be
//! added — without an engine noticing, and lets a future Hermes runtime
//! implement generation without knowing OpenAI exists.
//!
//! The event stream's shape is dictated by the client that has to consume it.
//! Hermes accumulates tool calls keyed by `index`, **assigning** the name and
//! **concatenating** the arguments, and treats a second `id` at an already-seen
//! index as a brand new call (verified in `agent/chat_completion_helpers.py`
//! lines 4179-4245). [`GenerationEvent::ToolCallDelta`] is shaped to make that
//! accumulation come out right: `id` and `name` are `Option`, sent once, and
//! `arguments` are fragments meant to be concatenated in order.

use hermes_core::Private;
use serde::{Deserialize, Serialize};

/// Who authored a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    /// The result of a tool call, replayed into the conversation.
    Tool,
}

impl MessageRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// A complete tool call, as it appears in conversation history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// A JSON document as a string. Kept unparsed on purpose: it is the
    /// model's output, it round-trips back to the client verbatim, and
    /// re-serializing it would silently reorder keys the client may be
    /// comparing.
    pub arguments: String,
}

/// One turn of a conversation.
///
/// `content` is [`Private`] because it is user-authored text and section 26
/// says it must not reach a log by accident. Reaching the engine requires
/// `reveal()`, which is a single greppable call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: Private<String>,
    /// The optional author name OpenAI allows on a message.
    pub name: Option<String>,
    /// Which call a `tool` message answers.
    pub tool_call_id: Option<String>,
    /// Tool calls an assistant turn made, when history is replayed.
    pub tool_calls: Vec<ToolCall>,
}

impl ChatMessage {
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Private::new(content.into()),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new(MessageRole::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(MessageRole::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(MessageRole::Assistant, content)
    }
}

/// How the model should sample.
///
/// Every field is optional and every `None` means "leave it to the engine's
/// own default". That is not laziness: pinning a value we were not asked for
/// would override a model's recommended settings from its own metadata, and
/// the difference shows up as output quality nobody can explain.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SamplingParams {
    // `f64` rather than `f32` throughout: these arrive as JSON numbers and
    // leave as JSON numbers, and narrowing in between turns a client's 0.2
    // into 0.20000000298023224 on the engine's command line.
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub min_p: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub seed: Option<u64>,
    /// Strings that stop generation when produced.
    pub stop: Vec<String>,
}

/// A request to generate a completion.
#[derive(Clone, Debug, PartialEq)]
pub struct GenerationRequest {
    pub messages: Vec<ChatMessage>,
    /// Maximum tokens to generate.
    ///
    /// Already clamped by the gateway against the remaining context. Hermes
    /// defaults this to 65536, which routinely exceeds the whole window, and
    /// rejecting that would break every request — so the clamp happens above
    /// and the backend receives a number it can honour.
    pub max_tokens: Option<u32>,
    pub sampling: SamplingParams,
}

impl GenerationRequest {
    pub fn new(messages: Vec<ChatMessage>) -> Self {
        Self {
            messages,
            max_tokens: None,
            sampling: SamplingParams::default(),
        }
    }
}

/// Why generation stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// A stop token or stop string.
    Stop,
    /// The token budget ran out.
    Length,
    /// The model called tools and is waiting for their results.
    ToolCalls,
    /// Generation failed after output had already been sent.
    ///
    /// Present because the alternative is worse: once headers are on the wire
    /// an error cannot become an HTTP status, and simply dropping the
    /// connection reads to the client as a truncated stream. A terminal chunk
    /// saying `error` is a clean end that the client can act on.
    Error,
}

impl FinishReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::Error => "error",
        }
    }
}

/// Token counts for one completion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// Prompt tokens served from the engine's prefix cache.
    ///
    /// The single most important number on a CPU: prefill dominates an agentic
    /// turn, and a cache hit removes it entirely. Reported so a regression in
    /// prefix reuse is visible rather than merely slow.
    pub cached_tokens: u32,
}

impl Usage {
    pub fn new(prompt_tokens: u32, completion_tokens: u32) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens.saturating_add(completion_tokens),
            cached_tokens: 0,
        }
    }
}

/// What the engine measured while generating.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    pub prompt_n: u32,
    pub prompt_ms: f64,
    pub predicted_n: u32,
    pub predicted_ms: f64,
    pub cached_n: u32,
}

/// Something that happened while generating.
#[derive(Clone, Debug, PartialEq)]
pub enum GenerationEvent {
    /// The engine has accepted the prompt and begun producing tokens.
    ///
    /// Marks the end of prefill, which on a CPU without AVX can be minutes.
    /// The gateway stops sending keep-alives when it arrives.
    Started {
        prompt_tokens: Option<u32>,
    },
    ContentDelta {
        text: String,
    },
    /// Chain-of-thought, kept separate from content.
    ///
    /// Hermes reads either `delta.reasoning_content` or `delta.reasoning`;
    /// merging it into content instead would put the model's private reasoning
    /// into the visible answer.
    ReasoningDelta {
        text: String,
    },
    ToolCallDelta {
        index: u32,
        /// Sent on the first delta of a call and never again.
        ///
        /// A different id at an index the client has already seen forces it to
        /// start a *new* call, so repeating or changing it corrupts the
        /// accumulation.
        id: Option<String>,
        /// Assigned, never concatenated.
        name: Option<String>,
        /// Concatenated in order.
        arguments: Option<String>,
    },
    Timings(Timings),
    Finished {
        finish_reason: FinishReason,
        usage: Usage,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_content_is_redacted_in_debug_output() {
        // `tracing`'s `?field` capture uses Debug, and a message is the most
        // likely thing in this crate to end up in a log line.
        let message = ChatMessage::user("my private prompt");
        let rendered = format!("{message:?}");
        assert!(
            !rendered.contains("my private prompt"),
            "prompt text leaked into Debug output: {rendered}"
        );
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    #[test]
    fn usage_totals_are_consistent() {
        let usage = Usage::new(36, 3);
        assert_eq!(usage.total_tokens, 39);
    }

    #[test]
    fn finish_reasons_use_the_wire_spellings() {
        // These strings go straight onto the wire, where a client matches them
        // exactly.
        assert_eq!(FinishReason::Stop.as_str(), "stop");
        assert_eq!(FinishReason::Length.as_str(), "length");
        assert_eq!(FinishReason::ToolCalls.as_str(), "tool_calls");
        assert_eq!(FinishReason::Error.as_str(), "error");
    }

    #[test]
    fn sampling_defaults_to_deferring_to_the_engine() {
        // Every unset field must stay unset, so a model's own recommended
        // sampling settings are not silently overridden.
        let sampling = SamplingParams::default();
        assert!(sampling.temperature.is_none());
        assert!(sampling.top_p.is_none());
        assert!(sampling.stop.is_empty());
    }
}
