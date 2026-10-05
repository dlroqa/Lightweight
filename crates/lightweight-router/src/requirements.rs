//! What one request needs from the deployment that serves it.
//!
//! This answers "what does this request require?" and nothing else. Which
//! deployment receives it stays the selector's job: [`crate::capability`]
//! compares these requirements with each deployment's last-observed
//! capabilities, and only then does a route's policy choose.
//!
//! Requirements are read with the gateway's own request types and the same
//! conversion the gateway runs before it generates
//! ([`ChatCompletionRequest::to_generation_request`],
//! [`CompletionRequest::expand`]). So a request the gateway would refuse is
//! refused here with the gateway's exact code, `param` and sentence, before
//! any deployment is chosen — a malformed request is never routed differently
//! depending on which node it happens to reach.
//!
//! Nothing here reads a prompt for meaning. Every rule is a field the client
//! set.
//!
//! # Context
//!
//! A Lightweight node refuses a request only when its prompt alone fills the
//! window (`prompt_tokens >= n_ctx`, `context_length_exceeded`). The output
//! budget — `max_tokens` or `max_completion_tokens`, or neither — is clamped to
//! what is left, never refused, because real clients send budgets far larger
//! than any window. The router keeps exactly that rule. A deployment can serve
//! a prompt of `p` tokens when `p < context_length`; the requested budget is
//! carried for the log line and does not decide eligibility.
//!
//! The node counts `p` with the model's own tokenizer and chat template. The
//! router has neither, and asking a node would put a network call in the
//! request path. So the router counts a **lower bound**: the bytes of message
//! text, which every template renders, divided by [`BYTES_PER_TOKEN_CEILING`]. It rules a
//! deployment out only when even that bound cannot fit. Near the boundary it
//! lets the request through, and the node — still the authority — answers
//! with its own `context_length_exceeded`. Wrongly refusing a request that a
//! deployment could have served would be worse: that is an error no node
//! would ever have given.

use axum::http::StatusCode;
use axum::response::Response;
use lightweight_api::chat::ChatCompletionRequest;
use lightweight_api::completions::CompletionRequest;
use lightweight_api::error::ErrorEnvelope;
use lightweight_inference::generation::{GenerationRequest, ReasoningControl, ToolChoice};

use crate::error::json_error;
use crate::proxy::Endpoint;

/// The most UTF-8 bytes one token is assumed to cover, on average over a whole
/// prompt.
///
/// Dividing by it gives a count no real tokenizer is expected to go under.
/// Measured on SmolLM2-135M through a real node (`docs/ROUTER.md`): English
/// 3.86 bytes a token, Markdown 3.30, Rust 3.69, indented code 3.76, JSON
/// 1.92, one word repeated 4.87 (the highest seen), base64 1.21, CJK 1.77,
/// digits 0.98. A larger figure here makes the bound lower — safer against a
/// wrong refusal, and slower to rule out a deployment that truly cannot fit.
pub const BYTES_PER_TOKEN_CEILING: usize = 6;

/// How a request uses `tool_choice`, as far as a deployment must honour it.
///
/// The function a request names is not kept: whether a deployment can force a
/// call does not depend on which one, and a function name has no place in a
/// metric or a log field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolChoiceRequirement {
    /// Not sent.
    #[default]
    Unspecified,
    /// `"auto"`: what a node does with tools anyway.
    Auto,
    /// `"none"`.
    None,
    /// `"required"`.
    Required,
    /// `{"type":"function","function":{"name":…}}`.
    Function,
}

impl ToolChoiceRequirement {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Auto => "auto",
            Self::None => "none",
            Self::Required => "required",
            Self::Function => "function",
        }
    }
}

/// What one request needs, derived once, before any deployment is looked at.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestRequirements {
    /// The endpoint the request was sent to. `None` only for a plan that is
    /// not for a request at all, which then requires nothing.
    pub endpoint: Option<Endpoint>,
    /// At least one tool is declared. `"tools": []` declares none.
    pub tools: bool,
    pub tool_choice: ToolChoiceRequirement,
    /// `reasoning_effort` asks for reasoning.
    pub reasoning: bool,
    /// A lower bound on the prompt's tokens; see the module docs. `None` when
    /// the router could not read the request well enough to count it.
    pub prompt_tokens: Option<u32>,
    /// The output budget the client asked for, under either spelling. Logged;
    /// the node clamps it, so it does not decide eligibility.
    pub max_tokens: Option<u32>,
}

impl RequestRequirements {
    /// Nothing at all: every available deployment qualifies.
    pub fn none() -> Self {
        Self::default()
    }

    /// Whether a deployment must honour `tool_choice` itself.
    ///
    /// * `required` and a named function force a call: only a node that
    ///   implements `tool_choice` can do that.
    /// * `none` beside declared tools forbids a call the model could otherwise
    ///   make, so it too needs a node that honours it. Without tools it asks
    ///   for nothing that is not already so.
    /// * `auto`, or nothing, is what a node does with tools by default.
    pub const fn needs_tool_choice(&self) -> bool {
        match self.tool_choice {
            ToolChoiceRequirement::Required | ToolChoiceRequirement::Function => true,
            ToolChoiceRequirement::None => self.tools,
            ToolChoiceRequirement::Unspecified | ToolChoiceRequirement::Auto => false,
        }
    }

    /// The smallest context that can serve this prompt: the prompt, plus the
    /// one token a node must have left to generate anything.
    pub fn required_context(&self) -> Option<u32> {
        self.prompt_tokens.map(|tokens| tokens.saturating_add(1))
    }
}

/// Derive what `body`, sent to `endpoint`, requires — or the gateway's own 400
/// for a request it would refuse.
///
/// A body that does not even deserialize into the endpoint's request type is
/// not refused here: what the gateway answers to that is its own business, so
/// it is sent on with only its endpoint required, exactly as before R5.
pub fn extract(endpoint: Endpoint, body: &[u8]) -> Result<RequestRequirements, Box<Response>> {
    let only_endpoint = RequestRequirements {
        endpoint: Some(endpoint),
        ..RequestRequirements::none()
    };
    match endpoint {
        Endpoint::ChatCompletions => {
            let Ok(request) = serde_json::from_slice::<ChatCompletionRequest>(body) else {
                return Ok(only_endpoint);
            };
            let generation = request.to_generation_request().map_err(|err| {
                invalid(
                    ErrorEnvelope::invalid_request(err.to_string(), err.code())
                        .with_param(err.param()),
                )
            })?;
            Ok(chat_requirements(
                &generation,
                request.requested_max_tokens(),
            ))
        }
        Endpoint::Completions => {
            let Ok(request) = serde_json::from_slice::<CompletionRequest>(body) else {
                return Ok(only_endpoint);
            };
            let prompts = request.expand().map_err(|err| {
                invalid(
                    ErrorEnvelope::invalid_request(err.to_string(), err.code())
                        .with_param(err.param()),
                )
            })?;
            // The gateway counts and checks every prompt on its own, so the
            // largest one is what has to fit.
            let largest = prompts.iter().map(String::len).max().unwrap_or(0);
            Ok(RequestRequirements {
                prompt_tokens: Some(tokens_at_least(largest)),
                max_tokens: request.max_tokens,
                ..only_endpoint
            })
        }
    }
}

/// The requirements of a chat request, read from the engine-neutral request
/// the gateway would build from it.
fn chat_requirements(
    generation: &GenerationRequest,
    max_tokens: Option<u32>,
) -> RequestRequirements {
    let tool_choice = match generation.tool_choice {
        ToolChoice::Unspecified => ToolChoiceRequirement::Unspecified,
        ToolChoice::Auto => ToolChoiceRequirement::Auto,
        ToolChoice::None => ToolChoiceRequirement::None,
        ToolChoice::Required => ToolChoiceRequirement::Required,
        ToolChoice::Function(_) => ToolChoiceRequirement::Function,
    };
    RequestRequirements {
        endpoint: Some(Endpoint::ChatCompletions),
        tools: !generation.tools.is_empty(),
        tool_choice,
        // `"none"` turns thinking off, which any model can do; only an effort
        // asks for reasoning. `chat_template_kwargs` is the template's own
        // business and is not read.
        reasoning: matches!(generation.reasoning, ReasoningControl::Effort(_)),
        prompt_tokens: Some(tokens_at_least(chat_bytes(generation))),
        max_tokens,
    }
}

/// The bytes of message text the chat template is given.
///
/// Only the messages' own text. Every chat template renders it, so it is in
/// the prompt whatever the model. What a template does with the rest varies
/// with the model, and this is a lower bound, so the rest is left out:
///
/// * tool declarations: SmolLM2's template drops them entirely — eight tools,
///   1 778 bytes of schema, measured at 31 prompt tokens in all — while a
///   tool-aware template renders every one;
/// * tool calls replayed in history, and author names, for the same reason;
/// * the template's own markup, which only adds tokens.
fn chat_bytes(generation: &GenerationRequest) -> usize {
    match &generation.prompt {
        lightweight_inference::generation::Prompt::Chat(messages) => messages
            .iter()
            .map(|message| message.content.reveal().len())
            .sum(),
        lightweight_inference::generation::Prompt::Text(text) => text.reveal().len(),
    }
}

/// A token count no tokenizer is expected to go under, for `bytes` of text.
fn tokens_at_least(bytes: usize) -> u32 {
    u32::try_from(bytes.div_ceil(BYTES_PER_TOKEN_CEILING)).unwrap_or(u32::MAX)
}

fn invalid(envelope: ErrorEnvelope) -> Box<Response> {
    Box::new(json_error(StatusCode::BAD_REQUEST, &envelope))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn chat(body: Value) -> RequestRequirements {
        extract(Endpoint::ChatCompletions, body.to_string().as_bytes()).expect("valid")
    }

    fn refused(endpoint: Endpoint, body: Value) -> (u16, Value) {
        let response = extract(endpoint, body.to_string().as_bytes()).expect_err("refused");
        let status = response.status().as_u16();
        let bytes = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(axum::body::to_bytes((*response).into_body(), usize::MAX))
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn tool(name: &str) -> Value {
        json!({"type": "function", "function": {"name": name, "parameters": {"type": "object"}}})
    }

    fn hello() -> Value {
        json!([{"role": "user", "content": "hello"}])
    }

    #[test]
    fn an_ordinary_chat_requires_chat_and_a_little_context_and_nothing_else() {
        let needs = chat(json!({"model": "Coder", "messages": hello()}));
        assert_eq!(needs.endpoint, Some(Endpoint::ChatCompletions));
        assert!(!needs.tools);
        assert_eq!(needs.tool_choice, ToolChoiceRequirement::Unspecified);
        assert!(!needs.needs_tool_choice());
        assert!(!needs.reasoning);
        // "hello" is five bytes: at least one token.
        assert_eq!(needs.prompt_tokens, Some(1));
        assert_eq!(needs.required_context(), Some(2));
        assert_eq!(needs.max_tokens, None);
    }

    #[test]
    fn tools_absent_or_empty_require_nothing_and_present_require_tools() {
        assert!(!chat(json!({"messages": hello()})).tools);
        // The gateway reads `[]` as no tools declared, and sends the engine
        // none; so does the router.
        assert!(!chat(json!({"messages": hello(), "tools": []})).tools);
        assert!(chat(json!({"messages": hello(), "tools": [tool("search")]})).tools);
    }

    #[test]
    fn each_tool_choice_asks_for_what_it_forces() {
        let with = |choice: Value| {
            chat(json!({"messages": hello(), "tools": [tool("search")], "tool_choice": choice}))
        };
        let auto = with(json!("auto"));
        assert_eq!(auto.tool_choice, ToolChoiceRequirement::Auto);
        assert!(auto.tools && !auto.needs_tool_choice());

        let none = with(json!("none"));
        assert_eq!(none.tool_choice, ToolChoiceRequirement::None);
        assert!(
            none.needs_tool_choice(),
            "it forbids a call the model could make"
        );

        let required = with(json!("required"));
        assert_eq!(required.tool_choice, ToolChoiceRequirement::Required);
        assert!(required.needs_tool_choice());

        let function = with(json!({"type": "function", "function": {"name": "search"}}));
        assert_eq!(function.tool_choice, ToolChoiceRequirement::Function);
        assert!(function.needs_tool_choice());

        // `none` with nothing declared asks for nothing that is not already so.
        let bare_none = chat(json!({"messages": hello(), "tool_choice": "none"}));
        assert!(!bare_none.tools && !bare_none.needs_tool_choice());
    }

    #[test]
    fn a_malformed_tool_request_is_the_gateways_own_400() {
        let (status, body) = refused(
            Endpoint::ChatCompletions,
            json!({"messages": hello(), "tool_choice": "required"}),
        );
        assert_eq!(status, 400);
        assert_eq!(body["error"]["code"], "invalid_tool_choice");
        assert_eq!(body["error"]["param"], "tool_choice");

        let (_, body) = refused(
            Endpoint::ChatCompletions,
            json!({"messages": hello(), "tools": [tool("search")],
                   "tool_choice": {"type": "function", "function": {"name": "other"}}}),
        );
        assert_eq!(body["error"]["code"], "invalid_tool_choice");
        assert!(body["error"]["message"].as_str().unwrap().contains("other"));

        let (_, body) = refused(
            Endpoint::ChatCompletions,
            json!({"messages": hello(), "tool_choice": "sometimes"}),
        );
        assert_eq!(body["error"]["code"], "invalid_tool_choice");

        let (_, body) = refused(
            Endpoint::ChatCompletions,
            json!({"messages": hello(), "tools": [{"type": "function", "function": {}}]}),
        );
        assert_eq!(body["error"]["code"], "invalid_tools");

        let (_, body) = refused(Endpoint::ChatCompletions, json!({"messages": []}));
        assert_eq!(body["error"]["code"], "invalid_messages");
    }

    #[test]
    fn only_a_reasoning_effort_asks_for_reasoning() {
        assert!(!chat(json!({"messages": hello()})).reasoning);
        assert!(chat(json!({"messages": hello(), "reasoning_effort": "high"})).reasoning);
        assert!(
            !chat(json!({"messages": hello(), "reasoning_effort": "none"})).reasoning,
            "turning thinking off is something any model can do"
        );
        assert!(!chat(json!({"messages": hello(), "reasoning_effort": " "})).reasoning);
        assert!(
            !chat(json!({"messages": hello(), "chat_template_kwargs": {"enable_thinking": true}}))
                .reasoning,
            "a template's own switch is not interpreted"
        );
    }

    #[test]
    fn the_context_bound_counts_the_message_text_every_template_renders() {
        let short = chat(json!({"messages": hello()})).prompt_tokens.unwrap();
        let long_text = "word ".repeat(6_000);
        let long = chat(json!({"messages": [{"role": "user", "content": long_text}]}))
            .prompt_tokens
            .unwrap();
        assert!(long > short);
        // 30 000 bytes: at least 5 000 tokens, never more than the bytes.
        assert_eq!(long, 5_000);

        // Every message's text counts, system turns included.
        let history = chat(json!({"messages": [
            {"role": "system", "content": "x".repeat(600)},
            {"role": "assistant", "content": "y".repeat(600)},
            {"role": "user", "content": "hello"}
        ]}))
        .prompt_tokens
        .unwrap();
        assert_eq!(history, 201, "1 205 bytes");

        // Tool declarations and replayed calls do not: some templates drop
        // them, and a lower bound cannot count what may not be there.
        let with_tools =
            chat(json!({"messages": hello(), "tools": [tool("search"), tool("fetch")]}))
                .prompt_tokens
                .unwrap();
        assert_eq!(with_tools, short);
        let replayed = chat(json!({"messages": [
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "search", "arguments": "y".repeat(600)}}
            ]},
            {"role": "user", "content": "hello"}
        ]}))
        .prompt_tokens
        .unwrap();
        assert_eq!(replayed, short);
    }

    #[test]
    fn the_output_budget_is_carried_but_never_required() {
        let explicit = chat(json!({"messages": hello(), "max_tokens": 65_536}));
        assert_eq!(explicit.max_tokens, Some(65_536));
        // Clamped by the node, so a huge budget does not make the request need
        // a huge window.
        assert_eq!(explicit.required_context(), Some(2));

        let newer =
            chat(json!({"messages": hello(), "max_completion_tokens": 300, "max_tokens": 500}));
        assert_eq!(
            newer.max_tokens,
            Some(300),
            "the gateway's own precedence: the smaller"
        );

        let omitted = chat(json!({"messages": hello()}));
        assert_eq!(omitted.max_tokens, None);
        assert_eq!(omitted.required_context(), Some(2));
    }

    #[test]
    fn a_completion_requires_completions_and_its_largest_prompt() {
        let needs = extract(
            Endpoint::Completions,
            json!({"prompt": ["short", "x".repeat(600)], "max_tokens": 16})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(needs.endpoint, Some(Endpoint::Completions));
        assert_eq!(needs.prompt_tokens, Some(100));
        assert_eq!(needs.max_tokens, Some(16));
        assert!(!needs.tools && !needs.reasoning && !needs.needs_tool_choice());

        let (status, body) = refused(Endpoint::Completions, json!({"prompt": [1, 2, 3]}));
        assert_eq!(status, 400);
        assert_eq!(body["error"]["code"], "invalid_prompt");
        let (_, body) = refused(Endpoint::Completions, json!({"prompt": "x", "suffix": "y"}));
        assert_eq!(body["error"]["code"], "unsupported_parameter");
        assert_eq!(body["error"]["param"], "suffix");
    }

    #[test]
    fn a_body_the_types_cannot_read_is_left_to_the_node() {
        // `messages` as a string is not a chat request the router can read;
        // the node answers it, as it did before R5.
        let needs = extract(
            Endpoint::ChatCompletions,
            json!({"messages": "hi"}).to_string().as_bytes(),
        )
        .unwrap();
        assert_eq!(needs.endpoint, Some(Endpoint::ChatCompletions));
        assert_eq!(needs.prompt_tokens, None);
        assert!(!needs.tools);
    }
}
