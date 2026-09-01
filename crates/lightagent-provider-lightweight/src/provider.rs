//! The `AgentProvider` adapter for the OpenAI-compatible gateway.
//!
//! Builds a streamed `POST /v1/chat/completions`, decodes the SSE response with
//! this crate's own [`SseDecoder`], and maps each chunk onto a
//! [`ProviderEvent`]. It knows the wire contract — `stream: true`,
//! `stream_options.include_usage: true`, an optional bearer key, index-keyed
//! tool-call deltas, an empty-`choices` usage chunk, a `[DONE]` terminator and
//! a terminal `error` chunk — and it depends only on `lightagent-core`, never
//! on any `lightweight-*` crate.

use std::collections::VecDeque;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::{self, BoxStream, StreamExt};
use lightagent_core::provider::{
    AgentProvider, FinishReason, ProviderError, ProviderEvent, ProviderMessage, ProviderRequest,
    ProviderStream, Role, Usage,
};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::sse::{SseDecodeError, SseDecoder, SseEvent};
use crate::tls;
use crate::wire::{ChatChunk, ModelsResponse};

/// How to reach the gateway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderConfig {
    /// The OpenAI-compatible base URL, e.g. `http://127.0.0.1:11434`.
    pub base_url: String,
    /// The catalog model id sent on every request.
    pub model: String,
    /// The bearer key, when the gateway requires one. Loopback needs none.
    pub api_key: Option<String>,
}

impl ProviderConfig {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key: None,
        }
    }

    /// Set the bearer key.
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }
}

/// An [`AgentProvider`] backed by the OpenAI-compatible gateway.
#[derive(Clone, Debug)]
pub struct LightweightProvider {
    client: reqwest::Client,
    config: ProviderConfig,
}

impl LightweightProvider {
    /// Build a provider, installing the rustls provider first.
    ///
    /// Without [`tls::ensure_provider`], `reqwest::Client::builder().build()`
    /// panics even for plain HTTP — see [`crate::tls`].
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        tls::ensure_provider();
        let client = reqwest::Client::builder()
            .build()
            .map_err(|err| ProviderError::Transport(err.to_string()))?;
        Ok(Self { client, config })
    }

    /// The base URL, without a trailing slash.
    fn base(&self) -> &str {
        self.config.base_url.trim_end_matches('/')
    }

    /// List the models the gateway serves, newest catalog id first.
    pub async fn models(&self) -> Result<Vec<String>, ProviderError> {
        let mut request = self.client.get(format!("{}/v1/models", self.base()));
        if let Some(key) = &self.config.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|err| ProviderError::Transport(err.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| ProviderError::Transport(err.to_string()))?;
        if !status.is_success() {
            return Err(ProviderError::Upstream(format!(
                "the provider answered {status} to GET /v1/models"
            )));
        }
        let parsed: ModelsResponse =
            serde_json::from_str(&body).map_err(|err| ProviderError::Upstream(err.to_string()))?;
        Ok(parsed.data.into_iter().map(|entry| entry.id).collect())
    }
}

/// Build the OpenAI request body for a turn.
///
/// Free-standing so it is testable without a client: the request-body-shape
/// test calls this directly.
pub(crate) fn build_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(config.model));
    body.insert("messages".into(), messages_to_json(&request.messages));
    // The usage chunk is not optional: the loop reads token counts from it.
    body.insert("stream".into(), Value::Bool(true));
    body.insert("stream_options".into(), json!({ "include_usage": true }));
    if !request.tools.is_empty() {
        body.insert("tools".into(), tools_to_json(&request.tools));
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = request.max_tokens {
        body.insert("max_tokens".into(), json!(max_tokens));
    }
    Value::Object(body)
}

/// Render messages as the OpenAI chat format.
fn messages_to_json(messages: &[ProviderMessage]) -> Value {
    Value::Array(
        messages
            .iter()
            .map(|message| {
                let mut object = Map::new();
                object.insert("role".into(), json!(message.role.as_str()));
                object.insert("content".into(), json!(message.content));
                if let Some(name) = &message.name {
                    object.insert("name".into(), json!(name));
                }
                if let Some(tool_call_id) = &message.tool_call_id {
                    object.insert("tool_call_id".into(), json!(tool_call_id));
                }
                if message.role == Role::Assistant && !message.tool_calls.is_empty() {
                    object.insert(
                        "tool_calls".into(),
                        Value::Array(
                            message
                                .tool_calls
                                .iter()
                                .map(|call| {
                                    json!({
                                        "id": call.id,
                                        "type": "function",
                                        "function": {
                                            "name": call.name,
                                            "arguments": call.arguments,
                                        },
                                    })
                                })
                                .collect(),
                        ),
                    );
                }
                Value::Object(object)
            })
            .collect(),
    )
}

/// Render tool schemas as the OpenAI `tools` array.
fn tools_to_json(tools: &[lightagent_core::ToolSchema]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|tool| {
                let parameters = if tool.parameters.is_null() {
                    json!({ "type": "object", "properties": {} })
                } else {
                    tool.parameters.clone()
                };
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": parameters,
                    },
                })
            })
            .collect(),
    )
}

#[async_trait]
impl AgentProvider for LightweightProvider {
    async fn stream(
        &self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> Result<ProviderStream, ProviderError> {
        let body = build_body(&self.config, &request);
        let mut builder = self
            .client
            .post(format!("{}/v1/chat/completions", self.base()))
            .json(&body);
        if let Some(key) = &self.config.api_key {
            builder = builder.bearer_auth(key);
        }

        let response = builder
            .send()
            .await
            .map_err(|err| ProviderError::Transport(err.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            // A refusal before any stream bytes: an ordinary error, not a
            // truncated stream.
            let detail = response.text().await.unwrap_or_default();
            return Err(ProviderError::Upstream(format!(
                "the provider answered {status}: {}",
                detail.trim()
            )));
        }

        Ok(translate(response.bytes_stream().boxed(), cancel))
    }
}

/// State carried while translating one response stream.
struct Translation {
    body: BoxStream<'static, reqwest::Result<Bytes>>,
    decoder: SseDecoder,
    pending: VecDeque<Result<ProviderEvent, ProviderError>>,
    cancel: CancellationToken,
    role_seen: bool,
    finish_reason: Option<FinishReason>,
    usage: Option<Usage>,
    finished: bool,
    emitted_finish: bool,
}

/// Translate an SSE byte stream into a stream of [`ProviderEvent`]s.
fn translate(
    body: BoxStream<'static, reqwest::Result<Bytes>>,
    cancel: CancellationToken,
) -> ProviderStream {
    let state = Translation {
        body,
        decoder: SseDecoder::new(),
        pending: VecDeque::new(),
        cancel,
        role_seen: false,
        finish_reason: None,
        usage: None,
        finished: false,
        emitted_finish: false,
    };

    stream::unfold(state, |mut state| async move {
        loop {
            if let Some(event) = state.pending.pop_front() {
                return Some((event, state));
            }
            if state.finished {
                return None;
            }

            let chunk = tokio::select! {
                biased;
                () = state.cancel.cancelled() => return None,
                chunk = state.body.next() => chunk,
            };

            match chunk {
                Some(Ok(bytes)) => {
                    if let Err(err) = state.decoder.feed(&bytes) {
                        state.finished = true;
                        state.pending.push_back(Err(map_decode_error(err)));
                        continue;
                    }
                    while let Some(event) = state.decoder.next_event() {
                        absorb(&mut state, &event);
                    }
                }
                Some(Err(err)) => {
                    state.finished = true;
                    state
                        .pending
                        .push_back(Err(ProviderError::Stream(err.to_string())));
                }
                None => {
                    // End of body. Emit a terminal Finished if the stream ended
                    // without a `[DONE]`, so the loop always sees one.
                    state.finished = true;
                    if !state.emitted_finish {
                        let event = finish_event(&mut state);
                        state.pending.push_back(Ok(event));
                    }
                }
            }
        }
    })
    .boxed()
}

/// Fold one decoded SSE event into the translation state.
fn absorb(state: &mut Translation, event: &SseEvent) {
    if event.is_done() {
        state.finished = true;
        if !state.emitted_finish {
            let event = finish_event(state);
            state.pending.push_back(Ok(event));
        }
        return;
    }

    let Ok(chunk) = serde_json::from_str::<ChatChunk>(&event.data) else {
        // Not a chunk we recognize; a stray proxy line must not lose the
        // stream.
        return;
    };

    if let Some(error) = &chunk.error {
        // The terminal error chunk: surface a Finished{Error} and then the
        // error itself, then end.
        let message = if error.message.is_empty() {
            "the provider reported an error".to_owned()
        } else {
            error.message.clone()
        };
        state.finish_reason = Some(FinishReason::Error);
        state.emitted_finish = true;
        state.finished = true;
        state.pending.push_back(Ok(ProviderEvent::Finished {
            reason: FinishReason::Error,
            usage: state.usage,
        }));
        state
            .pending
            .push_back(Err(ProviderError::MidStream { message }));
        return;
    }

    if let Some(usage) = &chunk.usage {
        state.usage = Some(Usage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        });
    }

    for choice in &chunk.choices {
        let delta = &choice.delta;
        if delta.role.is_some() && !state.role_seen {
            state.role_seen = true;
            state.pending.push_back(Ok(ProviderEvent::RoleStarted));
        }
        if let Some(text) = delta.reasoning_text()
            && !text.is_empty()
        {
            state
                .pending
                .push_back(Ok(ProviderEvent::Reasoning(text.to_owned())));
        }
        if let Some(content) = &delta.content
            && !content.is_empty()
        {
            state
                .pending
                .push_back(Ok(ProviderEvent::Content(content.clone())));
        }
        for call in delta.tool_calls.iter().flatten() {
            let function = call.function.as_ref();
            state.pending.push_back(Ok(ProviderEvent::ToolCallDelta {
                index: call.index,
                id: call.id.clone().filter(|id| !id.is_empty()),
                name: function
                    .and_then(|function| function.name.clone())
                    .filter(|name| !name.is_empty()),
                arguments: function.and_then(|function| function.arguments.clone()),
            }));
        }
        if let Some(reason) = &choice.finish_reason {
            state.finish_reason = Some(match reason.as_str() {
                "tool_calls" => FinishReason::ToolCalls,
                "length" => FinishReason::Length,
                "error" => FinishReason::Error,
                _ => FinishReason::Stop,
            });
        }
    }
}

/// Build the terminal Finished event from whatever the stream established.
fn finish_event(state: &mut Translation) -> ProviderEvent {
    state.emitted_finish = true;
    ProviderEvent::Finished {
        // A stream that ended without saying why is treated as a plain stop.
        reason: state.finish_reason.unwrap_or(FinishReason::Stop),
        usage: state.usage,
    }
}

fn map_decode_error(err: SseDecodeError) -> ProviderError {
    match err {
        SseDecodeError::FrameTooLarge { limit } => ProviderError::FrameTooLarge { limit },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightagent_core::ToolSchema;

    #[test]
    fn client_builds_without_panicking() {
        let provider =
            LightweightProvider::new(ProviderConfig::new("http://127.0.0.1:11434", "m@8k"));
        assert!(provider.is_ok());
    }

    #[test]
    fn request_body_shape() {
        let config = ProviderConfig::new("http://127.0.0.1:11434", "lfm2@8k");
        let mut request = ProviderRequest::new(
            "lfm2@8k",
            vec![
                ProviderMessage::system("be brief"),
                ProviderMessage::user("hi"),
            ],
        );
        request.tools = vec![ToolSchema::new(
            "datetime.now",
            "current time",
            json!({"type":"object","properties":{}}),
        )];

        let body = build_body(&config, &request);
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"]["include_usage"], json!(true));
        assert_eq!(body["model"], "lfm2@8k");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hi");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "datetime.now");
    }

    #[test]
    fn a_tool_message_carries_its_call_id() {
        let config = ProviderConfig::new("http://127.0.0.1:11434", "m");
        let request = ProviderRequest::new("m", vec![ProviderMessage::tool_result("call_1", "42")]);
        let body = build_body(&config, &request);
        assert_eq!(body["messages"][0]["role"], "tool");
        assert_eq!(body["messages"][0]["tool_call_id"], "call_1");
    }
}
