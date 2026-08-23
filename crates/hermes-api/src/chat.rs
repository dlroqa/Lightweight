//! `POST /v1/chat/completions` — the request, and the non-streaming response.
//!
//! The one endpoint that must work. Hermes calls it and nothing else for
//! generation (`agent/chat_completion_helpers.py:997,3907`), so every
//! tolerance decision here is load-bearing.

use hermes_core::Private;
use hermes_inference::generation::{
    ChatMessage, GenerationRequest, MessageRole, ReasoningControl, SamplingParams, ToolCall, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Why a request cannot be served.
///
/// Deliberately tiny. Spec section 12 allows exactly two rejections — a
/// request with no usable messages, and a prompt that does not fit — and every
/// other oddity is accepted and worked with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// `messages` was absent, empty, or contained nothing we could send.
    NoMessages,
}

/// `stream_options`, which is how a client asks for the usage chunk.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct StreamOptions {
    #[serde(default)]
    pub include_usage: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A message's content.
///
/// Three shapes reach us in practice: a plain string, `null` (an assistant
/// turn that only called tools), and an array of typed parts. All three are
/// accepted; a request that used the array form would otherwise fail for a
/// reason the user cannot see.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
    /// `null`, or anything else we cannot read as text.
    Other(Value),
}

impl Default for MessageContent {
    fn default() -> Self {
        Self::Other(Value::Null)
    }
}

impl MessageContent {
    /// Flatten to the text a text-only model can be given.
    ///
    /// Non-text parts — an image, an audio clip — are dropped rather than
    /// refused. This gateway is CPU and text only by design; refusing the whole
    /// request because one part is an image would turn a partially usable turn
    /// into a hard failure.
    pub fn to_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|part| part.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Other(Value::String(text)) => text.clone(),
            Self::Other(_) => String::new(),
        }
    }
}

/// One part of a multi-part message.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ContentPart {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A tool call replayed from conversation history.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RequestToolCall {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionCall>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct FunctionCall {
    #[serde(default)]
    pub name: Option<String>,
    /// A JSON document as a string, per the OpenAI schema — not an object.
    #[serde(default)]
    pub arguments: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One inbound message.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RequestMessage {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: MessageContent,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<RequestToolCall>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A field that may be a single string or a list of them, like `stop`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T: Clone> OneOrMany<T> {
    pub fn into_vec(self) -> Vec<T> {
        match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        }
    }
}

/// `POST /v1/chat/completions`.
///
/// Note what is *not* here: `deny_unknown_fields`, and any required field but
/// `messages`. Unknown keys land in [`ChatCompletionRequest::extra`], where
/// they are logged once and ignored.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChatCompletionRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub messages: Vec<RequestMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub stream_options: Option<StreamOptions>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// The newer spelling of `max_tokens`. Accepted as an alias so a client
    /// that moved to it does not silently lose its limit.
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub min_p: Option<f64>,
    #[serde(default)]
    pub presence_penalty: Option<f64>,
    #[serde(default)]
    pub frequency_penalty: Option<f64>,
    #[serde(default)]
    pub repeat_penalty: Option<f64>,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub stop: Option<OneOrMany<String>>,
    /// How hard the model should think before answering.
    ///
    /// Acted on rather than merely tolerated, because it is the only portable
    /// way a client can stop a reasoning model from spending a small token
    /// budget entirely on thinking — which reaches the client as a completion
    /// with no content in it.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Options for the model's own chat template, such as `enable_thinking`.
    ///
    /// Every template invents its own switches, so these are forwarded
    /// untouched rather than interpreted here.
    #[serde(default)]
    pub chat_template_kwargs: Option<Map<String, Value>>,
    /// Everything else the client sent: `think`, `options`, `tools`, and
    /// whatever a future version adds.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ChatCompletionRequest {
    /// Whether the client asked for the terminal usage chunk.
    pub fn wants_usage(&self) -> bool {
        self.stream_options
            .as_ref()
            .is_some_and(|options| options.include_usage)
    }

    /// The requested output limit under either spelling.
    pub fn requested_max_tokens(&self) -> Option<u32> {
        // A client that sends both means the same thing twice; the smaller is
        // the safe reading.
        match (self.max_tokens, self.max_completion_tokens) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        }
    }

    /// The keys we accepted but did not act on.
    ///
    /// Logged once at debug by the gateway. Useful when a client's behaviour
    /// depends on a parameter we are quietly ignoring, and impossible to
    /// discover otherwise.
    pub fn ignored_keys(&self) -> Vec<&str> {
        self.extra.keys().map(String::as_str).collect()
    }

    /// Translate into the engine-neutral request.
    ///
    /// `max_tokens` is **not** applied here: clamping needs the model's
    /// context and the prompt's size, which only the gateway knows.
    pub fn to_generation_request(&self) -> Result<GenerationRequest, RequestError> {
        let messages: Vec<ChatMessage> = self.messages.iter().filter_map(convert_message).collect();
        if messages.is_empty() {
            return Err(RequestError::NoMessages);
        }

        Ok(GenerationRequest {
            messages,
            reasoning: match self.reasoning_effort.as_deref().map(str::trim) {
                // OpenAI's spelling for "do not think", and what the engine
                // recognizes at the pinned build: `reasoning_effort: "none"`
                // sets `enable_thinking = false` before the template is
                // applied (`tools/server/server-common.cpp:1312-1322`).
                Some("none") => ReasoningControl::Disabled,
                Some(effort) if !effort.is_empty() => ReasoningControl::Effort(effort.to_owned()),
                _ => ReasoningControl::Default,
            },
            template_options: self.chat_template_kwargs.clone().unwrap_or_default(),
            max_tokens: self.requested_max_tokens(),
            sampling: SamplingParams {
                temperature: self.temperature,
                top_p: self.top_p,
                top_k: self.top_k,
                min_p: self.min_p,
                presence_penalty: self.presence_penalty,
                frequency_penalty: self.frequency_penalty,
                repeat_penalty: self.repeat_penalty,
                seed: self.seed,
                stop: self
                    .stop
                    .clone()
                    .map(OneOrMany::into_vec)
                    .unwrap_or_default(),
            },
        })
    }
}

/// Convert one inbound message, or drop it.
///
/// A message is dropped only when it carries nothing at all — no text and no
/// tool calls. Keeping an empty turn would add template tokens for no content,
/// which on a small context is real waste.
fn convert_message(message: &RequestMessage) -> Option<ChatMessage> {
    let text = message.content.to_text();
    let tool_calls: Vec<ToolCall> = message
        .tool_calls
        .iter()
        .filter_map(|call| {
            let function = call.function.as_ref()?;
            Some(ToolCall {
                id: call.id.clone().unwrap_or_default(),
                name: function.name.clone()?,
                arguments: function.arguments.clone().unwrap_or_default(),
            })
        })
        .collect();

    if text.is_empty() && tool_calls.is_empty() {
        return None;
    }

    Some(ChatMessage {
        role: parse_role(message.role.as_deref()),
        content: Private::new(text),
        name: message.name.clone(),
        tool_call_id: message.tool_call_id.clone(),
        tool_calls,
    })
}

/// Map a role string onto ours.
///
/// `developer` is OpenAI's newer name for a system message, and `function` is
/// the older name for a tool result; both are still sent by real clients. An
/// unrecognized role becomes `user`, which is the reading that loses the least:
/// the text still reaches the model instead of the turn vanishing.
fn parse_role(role: Option<&str>) -> MessageRole {
    match role.unwrap_or("user") {
        "system" | "developer" => MessageRole::System,
        "assistant" => MessageRole::Assistant,
        "tool" | "function" => MessageRole::Tool,
        _ => MessageRole::User,
    }
}

/// The `usage` object.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct UsageBody {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct PromptTokensDetails {
    /// Prompt tokens reused from the engine's prefix cache.
    pub cached_tokens: u32,
}

impl From<Usage> for UsageBody {
    fn from(usage: Usage) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            prompt_tokens_details: Some(PromptTokensDetails {
                cached_tokens: usage.cached_tokens,
            }),
        }
    }
}

/// The assistant's message in a non-streamed response.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ResponseMessage {
    pub role: String,
    /// Always present, even when empty.
    ///
    /// A client that finds neither content nor tool calls treats the response
    /// as empty and retries; `null` here would be one more way to look empty.
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ResponseToolCall>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResponseToolCall {
    pub id: String,
    pub r#type: String,
    pub function: ResponseFunction,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResponseFunction {
    pub name: String,
    pub arguments: String,
}

/// One choice of a non-streamed response.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Choice {
    pub index: u32,
    pub message: ResponseMessage,
    pub finish_reason: String,
}

/// A complete, non-streamed chat completion.
///
/// `choices` is a hard requirement for the client: an absent or empty array is
/// rejected outright (`transports/chat_completions.py:1010`), so it is never
/// optional here.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    /// Our catalog id, never the engine's model path.
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: UsageBody,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<Value>,
}

impl ChatCompletionResponse {
    pub fn new(
        id: impl Into<String>,
        model: impl Into<String>,
        choice: Choice,
        usage: UsageBody,
    ) -> Self {
        Self {
            id: id.into(),
            object: "chat.completion".to_owned(),
            created: crate::unix_now(),
            model: model.into(),
            choices: vec![choice],
            usage,
            timings: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> ChatCompletionRequest {
        serde_json::from_str(json).expect("a tolerant parse")
    }

    #[test]
    fn a_reasoning_model_can_be_told_not_to_think() {
        // The failure this prevents, seen against a real model: a reasoning
        // model given 24 tokens spends all 24 inside its reasoning and returns
        // a completion whose content is empty — which a client treats as an
        // empty response and retries.
        let request =
            parse(r#"{"messages":[{"role":"user","content":"hi"}],"reasoning_effort":"none"}"#);
        assert_eq!(
            request.to_generation_request().expect("ok").reasoning,
            ReasoningControl::Disabled
        );

        let effort =
            parse(r#"{"messages":[{"role":"user","content":"hi"}],"reasoning_effort":"high"}"#);
        assert_eq!(
            effort.to_generation_request().expect("ok").reasoning,
            ReasoningControl::Effort("high".into())
        );

        // Absent means "whatever this model does by default": forcing it
        // either way would change what every model produces.
        let quiet = parse(r#"{"messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(
            quiet.to_generation_request().expect("ok").reasoning,
            ReasoningControl::Default
        );
    }

    #[test]
    fn chat_template_options_reach_the_backend_untouched() {
        // Each template invents its own switches — `enable_thinking` is
        // Qwen's — so these are forwarded rather than interpreted.
        let request = parse(
            r#"{"messages":[{"role":"user","content":"hi"}],
                "chat_template_kwargs":{"enable_thinking":false,"custom":"value"}}"#,
        );
        let generation = request.to_generation_request().expect("ok");
        assert_eq!(generation.template_options["enable_thinking"], false);
        assert_eq!(generation.template_options["custom"], "value");
    }

    #[test]
    fn a_request_full_of_unknown_fields_still_parses() {
        // Hermes sends `reasoning_effort` at the top level and arbitrary keys
        // under `extra_body`. A 400 here would break every request from a
        // client version we have not seen.
        let request = parse(
            r#"{
                "model": "m@8k",
                "messages": [{"role": "user", "content": "hi"}],
                "reasoning_effort": "high",
                "think": true,
                "options": {"num_ctx": 8192},
                "some_future_key": {"nested": [1, 2, 3]}
            }"#,
        );
        assert_eq!(request.messages.len(), 1);
        let ignored = request.ignored_keys();
        for key in ["think", "options", "some_future_key"] {
            assert!(
                ignored.contains(&key),
                "{key} was not captured: {ignored:?}"
            );
        }
        // `reasoning_effort` used to land in the catch-all too. It is now a
        // typed field the gateway acts on, which is why it is no longer among
        // the ignored keys - a strengthening, not a regression.
        assert!(!ignored.contains(&"reasoning_effort"));
        assert_eq!(request.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn a_request_with_no_messages_is_the_one_thing_refused() {
        let request = parse(r#"{"model": "m"}"#);
        assert_eq!(
            request.to_generation_request().expect_err("must refuse"),
            RequestError::NoMessages
        );
    }

    #[test]
    fn content_parts_are_flattened_to_text() {
        // The array form is what clients send once they support images. The
        // text must survive; the image part is dropped rather than refused.
        let request = parse(
            r#"{"messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                {"type": "text", "text": "briefly"}
            ]}]}"#,
        );
        let generation = request.to_generation_request().expect("converted");
        assert_eq!(
            generation.messages[0].content.reveal(),
            "describe this\nbriefly"
        );
    }

    #[test]
    fn a_null_content_assistant_turn_survives_if_it_called_tools() {
        // The shape a client replays after a tool call: no text, only the call
        // it made. Dropping it would lose the model's own turn from history.
        let request = parse(
            r#"{"messages": [
                {"role": "user", "content": "read a.txt"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "hello"}
            ]}"#,
        );
        let generation = request.to_generation_request().expect("converted");
        assert_eq!(generation.messages.len(), 3);
        assert_eq!(generation.messages[1].role, MessageRole::Assistant);
        assert_eq!(generation.messages[1].tool_calls[0].name, "read_file");
        assert_eq!(generation.messages[2].role, MessageRole::Tool);
        assert_eq!(
            generation.messages[2].tool_call_id.as_deref(),
            Some("call_1")
        );
    }

    #[test]
    fn developer_and_function_roles_are_understood() {
        // Both are real spellings from real clients; mapping them to `user`
        // would put a system prompt in the user's voice.
        assert_eq!(parse_role(Some("developer")), MessageRole::System);
        assert_eq!(parse_role(Some("function")), MessageRole::Tool);
        // Anything unrecognized still reaches the model rather than vanishing.
        assert_eq!(parse_role(Some("wizard")), MessageRole::User);
        assert_eq!(parse_role(None), MessageRole::User);
    }

    #[test]
    fn stop_accepts_a_string_or_a_list() {
        let single = parse(r#"{"messages":[{"role":"user","content":"x"}],"stop":"END"}"#);
        assert_eq!(
            single.to_generation_request().expect("ok").sampling.stop,
            vec!["END".to_owned()]
        );
        let many = parse(r#"{"messages":[{"role":"user","content":"x"}],"stop":["A","B"]}"#);
        assert_eq!(
            many.to_generation_request().expect("ok").sampling.stop,
            vec!["A".to_owned(), "B".to_owned()]
        );
    }

    #[test]
    fn both_spellings_of_the_output_limit_are_read() {
        let old = parse(r#"{"messages":[],"max_tokens":100}"#);
        assert_eq!(old.requested_max_tokens(), Some(100));
        let new = parse(r#"{"messages":[],"max_completion_tokens":50}"#);
        assert_eq!(new.requested_max_tokens(), Some(50));
        // Sending both means the same thing twice; take the safer number.
        let both = parse(r#"{"messages":[],"max_tokens":100,"max_completion_tokens":50}"#);
        assert_eq!(both.requested_max_tokens(), Some(50));
    }

    #[test]
    fn the_usage_chunk_is_only_promised_when_asked_for() {
        let with = parse(
            r#"{"messages":[{"role":"user","content":"x"}],"stream":true,
                "stream_options":{"include_usage":true}}"#,
        );
        assert!(with.stream);
        assert!(with.wants_usage());
        let without = parse(r#"{"messages":[{"role":"user","content":"x"}],"stream":true}"#);
        assert!(!without.wants_usage());
    }

    #[test]
    fn sampling_parameters_pass_through_unchanged() {
        // A client's 0.2 must reach the engine as 0.2, not as a float that
        // widened on the way.
        let request = parse(
            r#"{"messages":[{"role":"user","content":"x"}],
                "temperature":0.2,"top_p":0.95,"seed":7,"presence_penalty":1.5}"#,
        );
        let sampling = request.to_generation_request().expect("ok").sampling;
        assert_eq!(sampling.temperature, Some(0.2));
        assert_eq!(sampling.top_p, Some(0.95));
        assert_eq!(sampling.seed, Some(7));
        assert_eq!(sampling.presence_penalty, Some(1.5));
    }

    #[test]
    fn a_response_always_serializes_a_content_string() {
        // A client that finds neither content nor tool calls treats the whole
        // response as empty and retries blindly.
        let response = ChatCompletionResponse::new(
            "chatcmpl-1",
            "m@8k",
            Choice {
                index: 0,
                message: ResponseMessage {
                    role: "assistant".into(),
                    content: String::new(),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                },
                finish_reason: "stop".into(),
            },
            UsageBody::default(),
        );
        let json = serde_json::to_value(&response).expect("serialize");
        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["choices"][0]["message"]["content"], "");
        assert!(json["choices"][0]["message"].get("tool_calls").is_none());
        assert_eq!(json["model"], "m@8k");
    }

    #[test]
    fn usage_reports_cached_prompt_tokens() {
        // The number that shows whether prefix reuse is working, which is the
        // difference between a fast turn and a two-minute one on a CPU.
        let body = UsageBody::from(Usage {
            prompt_tokens: 100,
            completion_tokens: 5,
            total_tokens: 105,
            cached_tokens: 96,
        });
        let json = serde_json::to_value(body).expect("serialize");
        assert_eq!(json["prompt_tokens_details"]["cached_tokens"], 96);
    }
}
