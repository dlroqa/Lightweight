//! The engine's private HTTP client.
//!
//! Everything llama.cpp-shaped stops here. Above this module the workspace
//! speaks [`GenerationEvent`]; below it, the engine's own JSON. That is the
//! seam the whole subprocess design exists to protect: when upstream renames a
//! field, this file's tests fail, rather than a client's conversation.
//!
//! Every fact encoded here was read from the pinned build — `b10590` — either
//! from `tools/server/` at that tag or from a running instance, never from
//! memory or from documentation of a different version:
//!
//! * A streamed chat completion emits a role chunk with `content: null`,
//!   then content deltas, then a chunk carrying `finish_reason`, then — with
//!   `stream_options.include_usage` — a chunk with an **empty** `choices`
//!   array and `usage`, then `data: [DONE]`
//!   (`server-task.cpp:462-520`, confirmed against a live engine).
//! * Tool-call deltas carry `index`, an `id` only on the first delta of a
//!   call, a `name` only when it changes, and `arguments` fragments
//!   (`server-chat.cpp:607-635`).
//! * An error mid-stream arrives as `data: {"error":{…}}` and the stream ends
//!   *without* `[DONE]` (`server-context.cpp:4348`).
//! * The engine sends `:\n\n` comment pings on a slow prefill, which the
//!   decoder discards.
//! * `POST /v1/chat/completions/input_tokens` returns `{"input_tokens":N}`
//!   with the model's own chat template applied — the only honest way to count
//!   a prompt before generating it.
//! * Dropping the connection stops the work: the server's response reader
//!   cancels its tasks when the request goes away
//!   (`server-queue.h:218`, `server-context.cpp:4287`).

use std::collections::VecDeque;
use std::time::Duration;

use futures_util::stream::{self, BoxStream, StreamExt};
use hermes_core::{SseDecoder, SseEvent};
use hermes_inference::generation::{
    ChatMessage, FinishReason, GenerationEvent, GenerationRequest, Prompt, ReasoningControl,
    Timings, ToolChoice, ToolDefinition, Usage,
};
use hermes_inference::{BackendError, GenerationStream};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

/// How long the engine may take to answer a non-streaming request.
///
/// Only used for the small metadata calls — token counting and properties.
/// Generation has no timeout at this layer: prefill on a CPU without AVX can
/// take minutes, and a client that has waited that long must not be cut off by
/// a number chosen on a faster machine.
const METADATA_TIMEOUT: Duration = Duration::from_secs(30);

/// A client for one running engine.
#[derive(Clone, Debug)]
pub struct UpstreamClient {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl UpstreamClient {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self, BackendError> {
        // `reqwest` panics rather than erroring when no rustls provider has
        // been installed, so establish the precondition here as well as in the
        // installer. Both paths can reach this constructor first.
        crate::tls::ensure_provider();
        let client = reqwest::Client::builder()
            // No global timeout: see `METADATA_TIMEOUT`. A generation can
            // legitimately run for many minutes on this hardware.
            .build()
            .map_err(|err| BackendError::GenerationFailed {
                detail: err.to_string(),
            })?;
        Ok(Self {
            client,
            base_url: base_url.into(),
            api_key: api_key.into(),
        })
    }

    /// Count the prompt's tokens with the model's own chat template applied.
    ///
    /// Counted by the engine rather than by us: the template is part of the
    /// prompt, it lives in the GGUF, and rendering it a second time in Rust
    /// would be both a reimplementation and a source of drift.
    ///
    /// **Tool declarations are part of the count.** The template renders every
    /// one of them into the prompt, and measured against a tool-capable
    /// template one small tool moved the count from 9 tokens to 157 — the same
    /// +148 the real generation reported. Omitting `tools` here would leave the
    /// gateway's overflow check short by a whole toolset, which for an agent is
    /// thousands of tokens, and the overflow would then surface from the engine
    /// in wording no client can parse.
    pub async fn count_prompt_tokens(
        &self,
        request: &GenerationRequest,
    ) -> Result<u32, BackendError> {
        let (path, body) = match &request.prompt {
            Prompt::Chat(messages) => {
                let mut body = Map::new();
                body.insert("messages".into(), messages_to_json(messages));
                if !request.tools.is_empty() {
                    body.insert("tools".into(), tools_to_json(&request.tools));
                }
                ("/v1/chat/completions/input_tokens", Value::Object(body))
            }
            // No template to apply, so the question is only how the tokenizer
            // splits the text. `/tokenize` answers exactly that, and counting
            // the array it returns is the count.
            Prompt::Text(text) => ("/tokenize", json!({ "content": text.reveal() })),
        };

        let response = self
            .client
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.api_key)
            .timeout(METADATA_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;

        let status = response.status();
        let payload: Value = response.json().await.map_err(transport_error)?;
        if !status.is_success() {
            return Err(upstream_error(&payload));
        }

        // Two endpoints, two shapes: a count, or the tokens to count.
        let counted = payload
            .get("input_tokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                payload
                    .get("tokens")
                    .and_then(Value::as_array)
                    .map(|tokens| tokens.len() as u64)
            });
        counted
            .map(|tokens| u32::try_from(tokens).unwrap_or(u32::MAX))
            .ok_or_else(|| BackendError::GenerationFailed {
                detail: "the engine did not report a prompt token count".to_owned(),
            })
    }

    /// The engine's own view of itself.
    ///
    /// Read rather than assumed, so `/props` reports what the engine is really
    /// running instead of what we asked it for.
    pub async fn props(&self) -> Result<Value, BackendError> {
        let response = self
            .client
            .get(format!("{}/props", self.base_url))
            .bearer_auth(&self.api_key)
            .timeout(METADATA_TIMEOUT)
            .send()
            .await
            .map_err(transport_error)?;
        let status = response.status();
        let payload: Value = response.json().await.map_err(transport_error)?;
        if !status.is_success() {
            return Err(upstream_error(&payload));
        }
        Ok(payload)
    }

    /// Start a generation and translate the engine's stream into events.
    ///
    /// Always streamed upstream, even when the caller wants one whole
    /// response. A non-streaming upstream request would hide time-to-first-token
    /// — the number that matters most on a slow CPU — and would give the
    /// gateway nothing to send during a prefill that can last minutes.
    pub async fn generate(
        &self,
        request: GenerationRequest,
        cancel: CancellationToken,
    ) -> Result<GenerationStream, BackendError> {
        let body = self.build_request_body(&request);
        let response = self
            .client
            .post(format!("{}{}", self.base_url, generation_path(&request)))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;

        let status = response.status();
        if !status.is_success() {
            // A refusal before any bytes of the stream: still an ordinary
            // error, so it can become an HTTP status rather than a truncated
            // stream.
            let payload: Value = response.json().await.unwrap_or(Value::Null);
            return Err(upstream_error(&payload));
        }

        Ok(translate(response, cancel))
    }

    /// Build the engine's request body.
    ///
    /// Only fields we were actually given are sent. An unset sampling
    /// parameter is left to the engine, which takes it from the model's own
    /// metadata; writing our own default over it would silently override a
    /// model's recommended settings.
    fn build_request_body(&self, request: &GenerationRequest) -> Value {
        let mut body = Map::new();
        match &request.prompt {
            Prompt::Chat(messages) => {
                body.insert("messages".into(), messages_to_json(messages));
                // Only sent when there are tools to declare. An empty `tools`
                // array is not the same as no tools: the template renders the
                // "you may call these" preamble either way, which costs tokens
                // and changes the model's behaviour for no reason.
                if !request.tools.is_empty() {
                    body.insert("tools".into(), tools_to_json(&request.tools));
                    if let Some(choice) = tool_choice_to_json(&request.tool_choice) {
                        body.insert("tool_choice".into(), choice);
                    }
                    if let Some(parallel) = request.parallel_tool_calls {
                        body.insert("parallel_tool_calls".into(), json!(parallel));
                    }
                }
            }
            // Raw text goes to the engine's own `/v1/completions`, which does
            // not apply a chat template. Sending it as a single user message
            // instead would wrap it in turn markers and answer it as a
            // question, which is precisely what this endpoint exists not to do.
            Prompt::Text(text) => {
                body.insert("prompt".into(), json!(text.reveal()));
            }
        }
        body.insert("stream".into(), Value::Bool(true));
        // The usage chunk is not optional for us: the client reads token
        // counts from exactly that chunk, and the metrics layer needs the
        // same numbers.
        body.insert("stream_options".into(), json!({ "include_usage": true }));

        if let Some(max_tokens) = request.max_tokens {
            body.insert("max_tokens".into(), json!(max_tokens));
        }

        // Reasoning. At the pinned build `reasoning_effort: "none"` sets
        // `enable_thinking = false` before the chat template is applied, and
        // any other value is handed to the template
        // (`tools/server/server-common.cpp:1312-1322`). `Default` sends
        // nothing, so the model behaves as its own template intends.
        match &request.reasoning {
            ReasoningControl::Default => {}
            ReasoningControl::Disabled => {
                body.insert("reasoning_effort".into(), json!("none"));
            }
            ReasoningControl::Effort(effort) => {
                body.insert("reasoning_effort".into(), json!(effort));
            }
        }

        // Template switches, forwarded untouched. The engine merges these into
        // the template's arguments, and `enable_thinking` there is the older,
        // model-specific way to ask for the same thing as above; a client that
        // sends both gets both, and the engine resolves it.
        if !request.template_options.is_empty() {
            body.insert(
                "chat_template_kwargs".into(),
                Value::Object(request.template_options.clone()),
            );
        }

        let sampling = &request.sampling;
        let mut insert = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                body.insert(key.to_owned(), value);
            }
        };
        insert("temperature", sampling.temperature.map(|v| json!(v)));
        insert("top_p", sampling.top_p.map(|v| json!(v)));
        insert("top_k", sampling.top_k.map(|v| json!(v)));
        insert("min_p", sampling.min_p.map(|v| json!(v)));
        insert(
            "presence_penalty",
            sampling.presence_penalty.map(|v| json!(v)),
        );
        insert(
            "frequency_penalty",
            sampling.frequency_penalty.map(|v| json!(v)),
        );
        insert("repeat_penalty", sampling.repeat_penalty.map(|v| json!(v)));
        insert("seed", sampling.seed.map(|v| json!(v)));
        if !sampling.stop.is_empty() {
            body.insert("stop".into(), json!(sampling.stop));
        }

        // Deliberately not set: `cache_prompt`, which defaults to true in this
        // build (`server-task.h:53`) and is the single largest performance
        // lever on a CPU — a repeated conversation prefix is not re-processed.
        // Passing it explicitly would only create a way to turn it off by
        // accident.
        Value::Object(body)
    }
}

/// Which of the engine's endpoints serves this request.
///
/// The whole difference between the two OpenAI generation endpoints, in one
/// place: a conversation is templated, raw text is not.
fn generation_path(request: &GenerationRequest) -> &'static str {
    match request.prompt {
        Prompt::Chat(_) => "/v1/chat/completions",
        Prompt::Text(_) => "/v1/completions",
    }
}

/// Render tool declarations as the engine's `tools` array.
///
/// `parameters` defaults to an empty object schema rather than being omitted: a
/// declaration with no schema is a tool that takes no arguments, and leaving
/// the key out makes some templates render nothing callable at all.
fn tools_to_json(tools: &[ToolDefinition]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|tool| {
                let mut function = Map::new();
                function.insert("name".into(), json!(tool.name));
                if let Some(description) = &tool.description {
                    function.insert("description".into(), json!(description));
                }
                function.insert(
                    "parameters".into(),
                    if tool.parameters.is_null() {
                        json!({ "type": "object", "properties": {} })
                    } else {
                        tool.parameters.clone()
                    },
                );
                json!({ "type": "function", "function": Value::Object(function) })
            })
            .collect(),
    )
}

/// Render a tool choice, or `None` to send nothing at all.
///
/// `Unspecified` sends nothing so the engine applies its own default, which is
/// `auto` when tools are present. The named form is an object rather than a
/// string, which is the shape the engine accepts — verified against the pinned
/// build, where an unrecognized string is a 400.
fn tool_choice_to_json(choice: &ToolChoice) -> Option<Value> {
    match choice {
        ToolChoice::Unspecified => None,
        ToolChoice::Auto => Some(json!("auto")),
        ToolChoice::None => Some(json!("none")),
        ToolChoice::Required => Some(json!("required")),
        ToolChoice::Function(name) => Some(json!({
            "type": "function",
            "function": { "name": name },
        })),
    }
}

/// Render our messages as the engine's chat format.
///
/// `reveal()` appears here, once. This is the boundary where user text has to
/// become bytes on a socket, and keeping it to a single call site is what
/// makes the privacy rule auditable with `rg 'reveal\(\)'`.
fn messages_to_json(messages: &[ChatMessage]) -> Value {
    Value::Array(
        messages
            .iter()
            .map(|message| {
                let mut object = Map::new();
                object.insert("role".into(), json!(message.role.as_str()));
                object.insert("content".into(), json!(message.content.reveal()));
                if let Some(name) = &message.name {
                    object.insert("name".into(), json!(name));
                }
                if let Some(tool_call_id) = &message.tool_call_id {
                    object.insert("tool_call_id".into(), json!(tool_call_id));
                }
                if !message.tool_calls.is_empty() {
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

/// A transport failure talking to our own child process.
fn transport_error(err: reqwest::Error) -> BackendError {
    if err.is_connect() {
        // The engine was there a moment ago and is not answering now, which
        // means it died. Reported as unavailable so the caller re-checks
        // health rather than treating it as a bad request.
        return BackendError::EngineUnavailable;
    }
    BackendError::GenerationFailed {
        detail: err.to_string(),
    }
}

/// Turn the engine's error body into ours.
///
/// The engine reports an overlong prompt as `exceed_context_size_error` with
/// `n_prompt_tokens` and `n_ctx` (`server-common.cpp:51`). That is precisely
/// the case the client can act on, so it is preserved rather than flattened
/// into a generic failure.
fn upstream_error(payload: &Value) -> BackendError {
    let error = payload.get("error").unwrap_or(payload);
    let kind = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("the engine rejected the request");

    if kind == "exceed_context_size_error" {
        let prompt_tokens = error
            .get("n_prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let n_ctx = error
            .get("n_ctx")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        if n_ctx > 0 {
            return BackendError::ContextOverflow {
                prompt_tokens: u32::try_from(prompt_tokens).unwrap_or(u32::MAX),
                n_ctx: u32::try_from(n_ctx).unwrap_or(u32::MAX),
            };
        }
    }

    BackendError::GenerationFailed {
        detail: message.to_owned(),
    }
}

/// State carried while translating one upstream stream.
struct Translation {
    body: BoxStream<'static, reqwest::Result<bytes::Bytes>>,
    decoder: SseDecoder,
    pending: VecDeque<Result<GenerationEvent, BackendError>>,
    cancel: CancellationToken,
    started: bool,
    finish_reason: Option<FinishReason>,
    usage: Option<Usage>,
    finished: bool,
    /// Set once the terminal event has been produced, so a stream that ends
    /// without one can be told apart from a normal end.
    reported_finish: bool,
}

/// Translate an upstream response into a stream of generation events.
fn translate(response: reqwest::Response, cancel: CancellationToken) -> GenerationStream {
    let state = Translation {
        body: response.bytes_stream().boxed(),
        decoder: SseDecoder::new(),
        pending: VecDeque::new(),
        cancel,
        started: false,
        finish_reason: None,
        usage: None,
        finished: false,
        reported_finish: false,
    };

    stream::unfold(state, |mut state| async move {
        loop {
            if let Some(event) = state.pending.pop_front() {
                return Some((event, state));
            }
            if state.finished {
                return None;
            }

            // Cancellation ends the stream without an event. There is nobody
            // left to tell: the client that would have read it is the reason
            // we are cancelling. Returning here drops the response body, which
            // closes the connection, which is what makes the engine stop
            // decoding rather than finishing a completion nobody wants.
            let chunk = tokio::select! {
                biased;
                () = state.cancel.cancelled() => return None,
                chunk = state.body.next() => chunk,
            };

            match chunk {
                Some(Ok(bytes)) => {
                    if let Err(err) = state.decoder.feed(&bytes) {
                        state.finished = true;
                        state.pending.push_back(Err(BackendError::GenerationFailed {
                            detail: err.to_string(),
                        }));
                        continue;
                    }
                    while let Some(event) = state.decoder.next_event() {
                        absorb(&mut state, &event);
                    }
                }
                Some(Err(err)) => {
                    state.finished = true;
                    state.pending.push_back(Err(transport_error(err)));
                }
                None => {
                    // End of body. A stream that stopped before saying why is
                    // a truncated one; the caller is told rather than being
                    // left to infer it from an empty result.
                    state.finished = true;
                    if !state.reported_finish {
                        let event = finish_event(&mut state);
                        state.pending.push_back(event);
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
        if !state.reported_finish {
            let event = finish_event(state);
            state.pending.push_back(event);
        }
        return;
    }

    let Ok(chunk) = serde_json::from_str::<Value>(&event.data) else {
        // Not JSON. Skipped rather than fatal: a proxy inserting a stray line
        // must not lose a conversation that is otherwise fine.
        return;
    };

    if let Some(error) = chunk.get("error") {
        state.finished = true;
        state.reported_finish = true;
        state
            .pending
            .push_back(Err(upstream_error(&json!({ "error": error.clone() }))));
        return;
    }

    if !state.started {
        state.started = true;
        state.pending.push_back(Ok(GenerationEvent::Started {
            prompt_tokens: None,
        }));
    }

    if let Some(usage) = chunk.get("usage").and_then(parse_usage) {
        state.usage = Some(usage);
    }
    if let Some(timings) = chunk.get("timings").and_then(parse_timings) {
        state
            .pending
            .push_back(Ok(GenerationEvent::Timings(timings)));
    }

    let Some(choice) = chunk
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        // The usage chunk has an empty `choices` array. Nothing else to read
        // from it, and the finish event waits for `[DONE]`.
        return;
    };

    if let Some(delta) = choice.get("delta") {
        absorb_delta(state, delta);
    }

    // A text completion has no `delta`: its content arrives as `text` on the
    // choice itself. Reading both here is what lets one translator serve both
    // of the engine's generation endpoints, so cancellation, error handling and
    // the finish/usage bookkeeping are shared rather than written twice.
    if let Some(text) = choice.get("text").and_then(Value::as_str)
        && !text.is_empty()
    {
        state.pending.push_back(Ok(GenerationEvent::ContentDelta {
            text: text.to_owned(),
        }));
    }

    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        state.finish_reason = Some(match reason {
            "length" => FinishReason::Length,
            "tool_calls" => FinishReason::ToolCalls,
            _ => FinishReason::Stop,
        });
    }
}

/// Fold one `delta` object into events.
fn absorb_delta(state: &mut Translation, delta: &Value) {
    // `content` is explicitly `null` on the engine's opening role chunk, so a
    // naive read would emit an empty content delta before anything exists.
    if let Some(text) = delta.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        state.pending.push_back(Ok(GenerationEvent::ContentDelta {
            text: text.to_owned(),
        }));
    }

    // Hermes reads either spelling; the engine sends the first.
    for key in ["reasoning_content", "reasoning"] {
        if let Some(text) = delta.get(key).and_then(Value::as_str)
            && !text.is_empty()
        {
            state.pending.push_back(Ok(GenerationEvent::ReasoningDelta {
                text: text.to_owned(),
            }));
            break;
        }
    }

    for call in delta
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let index = call
            .get("index")
            .and_then(Value::as_u64)
            .map(|index| u32::try_from(index).unwrap_or(u32::MAX))
            .unwrap_or_default();
        let function = call.get("function");
        state.pending.push_back(Ok(GenerationEvent::ToolCallDelta {
            index,
            id: call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
            name: function
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
            arguments: function
                .and_then(|function| function.get("arguments"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        }));
    }
}

/// Build the terminal event from whatever the stream established.
fn finish_event(state: &mut Translation) -> Result<GenerationEvent, BackendError> {
    state.reported_finish = true;
    Ok(GenerationEvent::Finished {
        // A stream that ended without saying why is treated as having hit the
        // token budget rather than as a clean stop: "stop" would tell the
        // client the model chose to end, which we do not know.
        finish_reason: state.finish_reason.unwrap_or(FinishReason::Length),
        usage: state.usage.unwrap_or_default(),
    })
}

fn parse_usage(usage: &Value) -> Option<Usage> {
    let field = |name: &str| {
        usage
            .get(name)
            .and_then(Value::as_u64)
            .map(|value| u32::try_from(value).unwrap_or(u32::MAX))
            .unwrap_or_default()
    };
    Some(Usage {
        prompt_tokens: field("prompt_tokens"),
        completion_tokens: field("completion_tokens"),
        total_tokens: field("total_tokens"),
        cached_tokens: usage
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .map(|value| u32::try_from(value).unwrap_or(u32::MAX))
            .unwrap_or_default(),
    })
}

fn parse_timings(timings: &Value) -> Option<Timings> {
    let count = |name: &str| {
        timings
            .get(name)
            .and_then(Value::as_u64)
            .map(|value| u32::try_from(value).unwrap_or(u32::MAX))
            .unwrap_or_default()
    };
    let millis = |name: &str| {
        timings
            .get(name)
            .and_then(Value::as_f64)
            .unwrap_or_default()
    };
    Some(Timings {
        prompt_n: count("prompt_n"),
        prompt_ms: millis("prompt_ms"),
        predicted_n: count("predicted_n"),
        predicted_ms: millis("predicted_ms"),
        cached_n: count("cache_n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_inference::generation::{MessageRole, SamplingParams};

    /// Drive the translator over canned SSE bytes, exactly as they arrive from
    /// the engine.
    fn absorb_all(frames: &[&str]) -> Vec<Result<GenerationEvent, BackendError>> {
        let mut state = Translation {
            body: stream::empty().boxed(),
            decoder: SseDecoder::new(),
            pending: VecDeque::new(),
            cancel: CancellationToken::new(),
            started: false,
            finish_reason: None,
            usage: None,
            finished: false,
            reported_finish: false,
        };
        for frame in frames {
            state.decoder.feed(frame.as_bytes()).expect("decode");
            while let Some(event) = state.decoder.next_event() {
                absorb(&mut state, &event);
            }
        }
        state.pending.drain(..).collect()
    }

    /// A transcript captured from the pinned engine (b10590) running
    /// SmolLM2-135M-Instruct, trimmed to two content tokens.
    const REAL_TRANSCRIPT: &[&str] = &[
        r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"role":"assistant","content":null}}],"created":1787508555,"id":"chatcmpl-x","model":"/models/smollm2.gguf","object":"chat.completion.chunk"}

"#,
        r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":"Hello"}}],"created":1787508555,"id":"chatcmpl-x","model":"/models/smollm2.gguf","object":"chat.completion.chunk"}

"#,
        r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":"!"}}],"created":1787508555,"id":"chatcmpl-x","model":"/models/smollm2.gguf","object":"chat.completion.chunk"}

"#,
        r#"data: {"choices":[{"finish_reason":"stop","index":0,"delta":{}}],"created":1787508555,"id":"chatcmpl-x","model":"/models/smollm2.gguf","object":"chat.completion.chunk"}

"#,
        r#"data: {"choices":[],"created":1787508555,"id":"chatcmpl-x","model":"/models/smollm2.gguf","object":"chat.completion.chunk","usage":{"completion_tokens":3,"prompt_tokens":36,"total_tokens":39,"prompt_tokens_details":{"cached_tokens":0}},"timings":{"cache_n":0,"prompt_n":36,"prompt_ms":2040.73,"predicted_n":3,"predicted_ms":171.527}}

"#,
        "data: [DONE]\n\n",
    ];

    #[test]
    fn a_real_engine_transcript_translates_to_our_events() {
        let events: Vec<_> = absorb_all(REAL_TRANSCRIPT)
            .into_iter()
            .map(|event| event.expect("no error"))
            .collect();

        assert_eq!(
            events[0],
            GenerationEvent::Started {
                prompt_tokens: None
            }
        );
        assert_eq!(
            events[1],
            GenerationEvent::ContentDelta {
                text: "Hello".into()
            }
        );
        assert_eq!(
            events[2],
            GenerationEvent::ContentDelta { text: "!".into() }
        );
        // The engine's opening role chunk carries `content: null`; emitting an
        // empty delta for it would put a spurious chunk on the wire.
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, GenerationEvent::ContentDelta { .. }))
                .count(),
            2
        );
        assert!(matches!(events[3], GenerationEvent::Timings(_)));
        assert_eq!(
            events[4],
            GenerationEvent::Finished {
                finish_reason: FinishReason::Stop,
                usage: Usage {
                    prompt_tokens: 36,
                    completion_tokens: 3,
                    total_tokens: 39,
                    cached_tokens: 0,
                },
            }
        );
    }

    #[test]
    fn a_stream_split_mid_frame_still_translates() {
        // Chunk boundaries land anywhere on a real socket.
        let whole: String = REAL_TRANSCRIPT.concat();
        let (head, tail) = whole.split_at(whole.len() / 3);
        let events = absorb_all(&[head, tail]);
        assert!(events.iter().any(|event| matches!(
            event,
            Ok(GenerationEvent::ContentDelta { text }) if text == "Hello"
        )));
        assert!(matches!(
            events.last(),
            Some(Ok(GenerationEvent::Finished { .. }))
        ));
    }

    #[test]
    fn a_comment_ping_produces_no_event() {
        // The engine sends `:\n\n` during a slow prefill. A client that saw it
        // as a chunk would report an empty completion.
        let events = absorb_all(&[":\n\n", ": ping\n\n"]);
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn an_overlong_prompt_keeps_its_numbers() {
        // Captured from the engine: the one error a client can actually act
        // on, so it must not be flattened into a generic failure.
        let events = absorb_all(&[
            r#"data: {"error":{"code":400,"message":"request (4031 tokens) exceeds the available context size (2048 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":4031,"n_ctx":2048}}

"#,
        ]);
        match &events[0] {
            Err(BackendError::ContextOverflow {
                prompt_tokens,
                n_ctx,
            }) => {
                assert_eq!(*prompt_tokens, 4031);
                assert_eq!(*n_ctx, 2048);
            }
            other => panic!("expected a context overflow, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_deltas_keep_the_index_id_name_and_argument_fragments() {
        // The accumulation the client performs depends on exactly this shape:
        // id and name once, arguments concatenated.
        let events = absorb_all(&[
            r#"data: {"choices":[{"index":0,"finish_reason":null,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":""}}]}}]}

"#,
            r#"data: {"choices":[{"index":0,"finish_reason":null,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}

"#,
            r#"data: {"choices":[{"index":0,"finish_reason":"tool_calls","delta":{}}]}

"#,
            "data: [DONE]\n\n",
        ]);
        let events: Vec<_> = events.into_iter().map(|e| e.expect("ok")).collect();
        assert_eq!(
            events[1],
            GenerationEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("read_file".into()),
                // An empty first fragment is preserved: the client concatenates,
                // so an empty string is harmless, and dropping the field would
                // lose the distinction between "no arguments yet" and "none".
                arguments: Some(String::new()),
            }
        );
        assert_eq!(
            events[2],
            GenerationEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments: Some("{\"path\":".into()),
            }
        );
        assert_eq!(
            events[3],
            GenerationEvent::Finished {
                finish_reason: FinishReason::ToolCalls,
                usage: Usage::default(),
            }
        );
    }

    #[test]
    fn a_truncated_stream_still_ends_with_a_finish_event() {
        // Without this the caller sees content and then nothing, which is
        // indistinguishable from a stream still in progress.
        let mut state = Translation {
            body: stream::empty().boxed(),
            decoder: SseDecoder::new(),
            pending: VecDeque::new(),
            cancel: CancellationToken::new(),
            started: true,
            finish_reason: None,
            usage: None,
            finished: false,
            reported_finish: false,
        };
        let event = finish_event(&mut state).expect("event");
        assert_eq!(
            event,
            GenerationEvent::Finished {
                // Not "stop": we do not know the model chose to end.
                finish_reason: FinishReason::Length,
                usage: Usage::default(),
            }
        );
    }

    #[test]
    fn unset_sampling_parameters_are_not_sent() {
        // Writing our own defaults over the model's recommended settings is a
        // quality regression nobody can explain afterwards.
        let client = UpstreamClient::new("http://127.0.0.1:1", "key").expect("client");
        let request = GenerationRequest::new(vec![ChatMessage::user("hi")]);
        let body = client.build_request_body(&request);

        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"]["include_usage"], json!(true));
        for absent in [
            "temperature",
            "top_p",
            "top_k",
            "seed",
            "stop",
            "max_tokens",
            "cache_prompt",
        ] {
            assert!(body.get(absent).is_none(), "{absent} should not be sent");
        }
    }

    #[test]
    fn declared_tools_are_sent_to_the_engine() {
        // Verified against the pinned build: the engine accepts `tools` and
        // `tool_choice` on /v1/chat/completions, and a tool-capable template
        // renders them into the prompt.
        let client = UpstreamClient::new("http://127.0.0.1:1", "key").expect("client");
        let request = GenerationRequest::new(vec![ChatMessage::user("weather?")]).with_tools(
            vec![ToolDefinition {
                name: "get_weather".into(),
                description: Some("Get the weather".into()),
                parameters: json!({"type":"object","properties":{"city":{"type":"string"}}}),
            }],
            ToolChoice::Auto,
        );
        let body = client.build_request_body(&request);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(
            body["tools"][0]["function"]["description"],
            "Get the weather"
        );
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["city"]["type"],
            "string"
        );
        assert_eq!(body["tool_choice"], json!("auto"));
    }

    #[test]
    fn no_tools_means_no_tools_key_at_all() {
        // An empty `tools` array is not the same as no tools: a template
        // renders the "you may call these" preamble either way, which costs
        // prompt tokens and changes what the model does.
        let client = UpstreamClient::new("http://127.0.0.1:1", "key").expect("client");
        let body =
            client.build_request_body(&GenerationRequest::new(vec![ChatMessage::user("hi")]));
        assert!(body.get("tools").is_none(), "{body}");
        assert!(body.get("tool_choice").is_none(), "{body}");
    }

    #[test]
    fn an_unspecified_tool_choice_sends_nothing() {
        // Same discipline as reasoning and sampling: sending nothing leaves the
        // engine's own default in place, which is `auto` when tools exist.
        let client = UpstreamClient::new("http://127.0.0.1:1", "key").expect("client");
        let request = GenerationRequest::new(vec![ChatMessage::user("hi")]).with_tools(
            vec![ToolDefinition {
                name: "f".into(),
                description: None,
                parameters: Value::Null,
            }],
            ToolChoice::Unspecified,
        );
        let body = client.build_request_body(&request);
        assert!(body.get("tools").is_some());
        assert!(body.get("tool_choice").is_none(), "{body}");
        // A tool with no schema still has to be callable, so the absent schema
        // becomes an empty object rather than being left out.
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn a_named_tool_choice_is_sent_as_the_object_form() {
        // The pinned build answers 400 to an unrecognized `tool_choice` string
        // and 200 to the object form, so a function choice must be an object.
        let client = UpstreamClient::new("http://127.0.0.1:1", "key").expect("client");
        let request = GenerationRequest::new(vec![ChatMessage::user("hi")]).with_tools(
            vec![ToolDefinition {
                name: "f".into(),
                description: None,
                parameters: Value::Null,
            }],
            ToolChoice::Function("f".into()),
        );
        let body = client.build_request_body(&request);
        assert_eq!(body["tool_choice"]["type"], "function");
        assert_eq!(body["tool_choice"]["function"]["name"], "f");
    }

    #[test]
    fn a_text_prompt_goes_to_the_completions_endpoint_untemplated() {
        // The whole point of /v1/completions. Sending this as a chat message
        // would wrap it in turn markers and answer it as a question instead of
        // continuing it.
        let client = UpstreamClient::new("http://127.0.0.1:1", "key").expect("client");
        let request = GenerationRequest::from_text("The capital of France is");
        assert_eq!(generation_path(&request), "/v1/completions");
        let body = client.build_request_body(&request);
        assert_eq!(body["prompt"], "The capital of France is");
        assert!(body.get("messages").is_none(), "{body}");
        // Still streamed upstream, and still asking for usage: the gateway
        // needs the same token counts whichever endpoint served the request.
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"]["include_usage"], json!(true));
    }

    #[test]
    fn a_chat_prompt_goes_to_the_chat_endpoint() {
        let request = GenerationRequest::new(vec![ChatMessage::user("hi")]);
        assert_eq!(generation_path(&request), "/v1/chat/completions");
    }

    #[test]
    fn a_text_completions_chunk_is_read_from_its_text_field() {
        // A text completion has no `delta`: the content is `text` on the choice
        // itself, and usage rides on the final chunk rather than a separate one.
        // Captured from the pinned build.
        let events = absorb_all(&[
            r#"data: {"choices":[{"text":" Paris","index":0,"logprobs":null,"finish_reason":null}],"object":"text_completion"}

"#,
            r#"data: {"choices":[{"text":".","index":0,"logprobs":null,"finish_reason":"length"}],"object":"text_completion","usage":{"completion_tokens":2,"prompt_tokens":5,"total_tokens":7,"prompt_tokens_details":{"cached_tokens":0}}}

"#,
            "data: [DONE]\n\n",
        ]);
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                Ok(GenerationEvent::ContentDelta { text }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, " Paris.");
        let finished = events.iter().find_map(|event| match event {
            Ok(GenerationEvent::Finished {
                finish_reason,
                usage,
            }) => Some((*finish_reason, *usage)),
            _ => None,
        });
        let (reason, usage) = finished.expect("a terminal event");
        assert_eq!(reason, FinishReason::Length);
        assert_eq!(usage.prompt_tokens, 5);
        assert_eq!(usage.completion_tokens, 2);
    }

    #[test]
    fn given_sampling_parameters_are_sent() {
        let client = UpstreamClient::new("http://127.0.0.1:1", "key").expect("client");
        let request = GenerationRequest {
            max_tokens: Some(128),
            sampling: SamplingParams {
                temperature: Some(0.2),
                stop: vec!["\n\n".into()],
                seed: Some(7),
                ..SamplingParams::default()
            },
            ..GenerationRequest::new(vec![ChatMessage::user("hi")])
        };
        let body = client.build_request_body(&request);
        assert_eq!(body["max_tokens"], json!(128));
        assert_eq!(body["temperature"], json!(0.2));
        assert_eq!(body["seed"], json!(7));
        assert_eq!(body["stop"], json!(["\n\n"]));
    }

    #[test]
    fn reasoning_is_only_mentioned_when_the_caller_asked() {
        let client = UpstreamClient::new("http://127.0.0.1:1", "key").expect("client");

        // Untouched by default: the model's own template decides.
        let default =
            client.build_request_body(&GenerationRequest::new(vec![ChatMessage::user("hi")]));
        assert!(default.get("reasoning_effort").is_none());
        assert!(default.get("chat_template_kwargs").is_none());

        let quiet = client.build_request_body(
            &GenerationRequest::new(vec![ChatMessage::user("hi")]).without_reasoning(),
        );
        assert_eq!(quiet["reasoning_effort"], json!("none"));

        let mut effort = GenerationRequest::new(vec![ChatMessage::user("hi")]);
        effort.reasoning = ReasoningControl::Effort("high".into());
        assert_eq!(
            client.build_request_body(&effort)["reasoning_effort"],
            json!("high")
        );
    }

    #[test]
    fn template_options_are_forwarded_verbatim() {
        let client = UpstreamClient::new("http://127.0.0.1:1", "key").expect("client");
        let mut request = GenerationRequest::new(vec![ChatMessage::user("hi")]);
        request
            .template_options
            .insert("enable_thinking".into(), json!(false));
        let body = client.build_request_body(&request);
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"],
            json!(false)
        );
    }

    #[test]
    fn messages_carry_roles_names_and_tool_results() {
        let messages = vec![
            ChatMessage::system("be brief"),
            ChatMessage {
                tool_call_id: Some("call_1".into()),
                ..ChatMessage::new(MessageRole::Tool, "42")
            },
        ];
        let json = messages_to_json(&messages);
        assert_eq!(json[0]["role"], "system");
        assert_eq!(json[0]["content"], "be brief");
        assert_eq!(json[1]["role"], "tool");
        assert_eq!(json[1]["tool_call_id"], "call_1");
    }
}
