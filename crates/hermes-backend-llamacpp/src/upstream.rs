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
    ChatMessage, FinishReason, GenerationEvent, GenerationRequest, Timings, Usage,
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
    pub async fn count_prompt_tokens(
        &self,
        request: &GenerationRequest,
    ) -> Result<u32, BackendError> {
        let body = json!({
            "messages": messages_to_json(&request.messages),
        });
        let response = self
            .client
            .post(format!(
                "{}/v1/chat/completions/input_tokens",
                self.base_url
            ))
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
        payload
            .get("input_tokens")
            .and_then(Value::as_u64)
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
            .post(format!("{}/v1/chat/completions", self.base_url))
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
        body.insert("messages".into(), messages_to_json(&request.messages));
        body.insert("stream".into(), Value::Bool(true));
        // The usage chunk is not optional for us: the client reads token
        // counts from exactly that chunk, and the metrics layer needs the
        // same numbers.
        body.insert("stream_options".into(), json!({ "include_usage": true }));

        if let Some(max_tokens) = request.max_tokens {
            body.insert("max_tokens".into(), json!(max_tokens));
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
        r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"role":"assistant","content":null}}],"created":1787508555,"id":"chatcmpl-x","model":"/home/agent/models/smollm2.gguf","object":"chat.completion.chunk"}

"#,
        r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":"Hello"}}],"created":1787508555,"id":"chatcmpl-x","model":"/home/agent/models/smollm2.gguf","object":"chat.completion.chunk"}

"#,
        r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":"!"}}],"created":1787508555,"id":"chatcmpl-x","model":"/home/agent/models/smollm2.gguf","object":"chat.completion.chunk"}

"#,
        r#"data: {"choices":[{"finish_reason":"stop","index":0,"delta":{}}],"created":1787508555,"id":"chatcmpl-x","model":"/home/agent/models/smollm2.gguf","object":"chat.completion.chunk"}

"#,
        r#"data: {"choices":[],"created":1787508555,"id":"chatcmpl-x","model":"/home/agent/models/smollm2.gguf","object":"chat.completion.chunk","usage":{"completion_tokens":3,"prompt_tokens":36,"total_tokens":39,"prompt_tokens_details":{"cached_tokens":0}},"timings":{"cache_n":0,"prompt_n":36,"prompt_ms":2040.73,"predicted_n":3,"predicted_ms":171.527}}

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
    fn given_sampling_parameters_are_sent() {
        let client = UpstreamClient::new("http://127.0.0.1:1", "key").expect("client");
        let request = GenerationRequest {
            messages: vec![ChatMessage::user("hi")],
            max_tokens: Some(128),
            sampling: SamplingParams {
                temperature: Some(0.2),
                stop: vec!["\n\n".into()],
                seed: Some(7),
                ..SamplingParams::default()
            },
        };
        let body = client.build_request_body(&request);
        assert_eq!(body["max_tokens"], json!(128));
        assert_eq!(body["temperature"], json!(0.2));
        assert_eq!(body["seed"], json!(7));
        assert_eq!(body["stop"], json!(["\n\n"]));
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
