//! What a backend is asked to generate, and what it reports while doing it.
//!
//! These types are deliberately **not** the OpenAI wire format. The gateway
//! translates inbound requests into them and translates the events back out
//! again, which is what lets the wire contract change — or a second one be
//! added — without an engine noticing, and lets a future Hermes runtime
//! implement generation without knowing OpenAI exists.
//!
//! What is generated comes in two shapes, and [`Prompt`] keeps them apart: a
//! conversation, which the model's chat template renders, and raw text, which
//! must reach the model untouched. `/v1/completions` exists for the second, and
//! collapsing the two would answer a conversation the caller never had.
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

/// A tool the model may call.
///
/// Engine-neutral, and deliberately thin: a name, a sentence, and a schema.
/// Nothing here interprets the schema, because nothing here can — it is written
/// by the client for the model, and the only party that has to understand it is
/// the model's own chat template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDefinition {
    pub name: String,
    /// What the tool does. Optional in the OpenAI schema, and genuinely
    /// optional here: a model can call a well-named tool without one.
    pub description: Option<String>,
    /// JSON Schema for the arguments, carried untouched.
    ///
    /// Not parsed, not validated, not re-serialized into a normal form. It
    /// round-trips into the prompt the template builds, and rewriting it would
    /// change the tokens the model sees for no benefit we could name.
    pub parameters: serde_json::Value,
}

/// Whether, and which, tool the model must call.
///
/// [`ToolChoice::Unspecified`] exists for the same reason
/// [`ReasoningControl::Default`] does: sending nothing lets the engine apply
/// its own default — `auto` when tools are present — and writing our own value
/// over it would override a decision we were never asked to make.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ToolChoice {
    #[default]
    Unspecified,
    /// The model decides whether to call a tool.
    Auto,
    /// The model must not call a tool.
    None,
    /// The model must call one of the declared tools.
    Required,
    /// The model must call this specific function.
    Function(String),
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

/// How much the model should think before answering.
///
/// Engine-neutral on purpose. A model that reasons is not a llama.cpp concept
/// — it is a property of the model — so the *intent* is expressed here and each
/// backend decides how to ask for it. A backend serving a model that cannot
/// reason ignores this entirely, which is the correct behaviour rather than an
/// error: the caller asked for less thinking, and got none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ReasoningControl {
    /// Leave it to the model's own template. Most models reason by default if
    /// they can.
    #[default]
    Default,
    /// Do not reason; answer directly.
    ///
    /// Worth asking for more often than it looks. A reasoning model given a
    /// small token budget can spend the whole of it thinking and return an
    /// answer with no content in it at all, which a client reads as an empty
    /// response.
    Disabled,
    /// A named effort level, passed through to the model's template.
    Effort(String),
}

/// What the model is asked to continue.
///
/// The two variants are not a formatting detail. A chat prompt has the model's
/// own chat template applied to it — roles, turn markers, the tool declarations
/// — and a text prompt must have none of that: `/v1/completions` exists
/// precisely to continue raw text, and wrapping it in a template would return
/// an answer to a conversation the caller never had.
///
/// Keeping the distinction in the type is what stops it being decided by an
/// `Option` somewhere further down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prompt {
    /// A conversation. The engine applies the model's chat template.
    Chat(Vec<ChatMessage>),
    /// Raw text, continued verbatim with no template at all.
    ///
    /// [`Private`] for the same reason a message's content is: it is
    /// user-authored text, and section 26 says it must not reach a log by
    /// accident.
    Text(Private<String>),
}

/// A request to generate a completion.
#[derive(Clone, Debug, PartialEq)]
pub struct GenerationRequest {
    pub prompt: Prompt,
    /// Tools the model may call. Empty means the model is told of none.
    ///
    /// These cost prompt tokens — the template renders every declaration into
    /// the prompt — which is why they must reach the token count as well as
    /// the generation. Measured against a tool-capable template, one small
    /// tool cost 148 tokens; an agent's whole toolset costs thousands.
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: ToolChoice,
    /// Whether several tools may be called in one turn.
    ///
    /// `None` sends nothing and lets the model's template decide, which is the
    /// same discipline every other unset field here follows. Whether it is
    /// honoured at all is a property of that template, not of any gateway.
    pub parallel_tool_calls: Option<bool>,
    /// Maximum tokens to generate.
    ///
    /// Already clamped by the gateway against the remaining context. Hermes
    /// defaults this to 65536, which routinely exceeds the whole window, and
    /// rejecting that would break every request — so the clamp happens above
    /// and the backend receives a number it can honour.
    pub max_tokens: Option<u32>,
    pub sampling: SamplingParams,
    /// Whether the model should reason before answering.
    pub reasoning: ReasoningControl,
    /// Options for the model's own chat template.
    ///
    /// An escape hatch, and deliberately untyped: chat templates are shipped
    /// inside the model and each one invents its own switches. A backend that
    /// applies a template forwards these; one that does not, ignores them.
    /// Nothing above this line has to know which switches exist.
    pub template_options: serde_json::Map<String, serde_json::Value>,
}

impl GenerationRequest {
    /// A chat request. The signature predates [`Prompt`] and is kept, because
    /// it is what every existing caller and test builds.
    pub fn new(messages: Vec<ChatMessage>) -> Self {
        Self::with_prompt(Prompt::Chat(messages))
    }

    /// A raw-text request, for `/v1/completions`.
    pub fn from_text(text: impl Into<String>) -> Self {
        Self::with_prompt(Prompt::Text(Private::new(text.into())))
    }

    pub fn with_prompt(prompt: Prompt) -> Self {
        Self {
            prompt,
            tools: Vec::new(),
            tool_choice: ToolChoice::Unspecified,
            parallel_tool_calls: None,
            max_tokens: None,
            sampling: SamplingParams::default(),
            reasoning: ReasoningControl::Default,
            template_options: serde_json::Map::new(),
        }
    }

    /// The conversation, or nothing when this is a raw-text request.
    ///
    /// Returning an empty slice rather than an `Option` because every caller
    /// wants to iterate turns, and a text prompt genuinely has none.
    pub fn messages(&self) -> &[ChatMessage] {
        match &self.prompt {
            Prompt::Chat(messages) => messages,
            Prompt::Text(_) => &[],
        }
    }

    /// Declare the tools the model may call.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolDefinition>, choice: ToolChoice) -> Self {
        self.tools = tools;
        self.tool_choice = choice;
        self
    }

    /// Ask the model not to reason before answering.
    #[must_use]
    pub fn without_reasoning(mut self) -> Self {
        self.reasoning = ReasoningControl::Disabled;
        self
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
    fn reasoning_defaults_to_the_models_own_behaviour() {
        // Forcing reasoning off by default would silently change what every
        // model produces; forcing it on would make non-reasoning models fail.
        let request = GenerationRequest::new(vec![ChatMessage::user("hi")]);
        assert_eq!(request.reasoning, ReasoningControl::Default);
        assert!(request.template_options.is_empty());
        assert_eq!(
            request.without_reasoning().reasoning,
            ReasoningControl::Disabled
        );
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
