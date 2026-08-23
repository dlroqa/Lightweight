//! The handlers.
//!
//! Request handling follows one order, and each step exists to turn a failure
//! the client cannot diagnose into one it can:
//!
//! 1. **Auth**, which is permissive on loopback and never 401s there.
//! 2. **Parse**, tolerantly — unknown fields are logged, never rejected.
//! 3. **Resolve the model**, tolerating our own context suffix drifting.
//! 4. **Count the prompt** against the loaded context, so an overlong
//!    conversation becomes a parsable 400 instead of an empty stream the
//!    client retries blindly.
//! 5. **Clamp `max_tokens`** rather than rejecting it: the client defaults it
//!    to 65536, which exceeds every context we can load.
//! 6. **Take a slot**, and hold it in a guard that releases on `Drop`.
//! 7. **Stream or aggregate.**

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use hermes_api::chat::{
    ChatCompletionRequest, ChatCompletionResponse, Choice, RequestError, ResponseFunction,
    ResponseMessage, ResponseToolCall, UsageBody,
};
use hermes_api::error::ErrorEnvelope;
use hermes_api::models::ModelList;
use hermes_api::props::PropsBody;
use hermes_api::stream::ChunkBuilder;
use hermes_core::Actionable;
use hermes_inference::BackendError;
use hermes_inference::generation::{FinishReason, GenerationEvent};
use hermes_observability::targets;
use serde_json::json;

use crate::auth::AuthFailure;
use crate::catalog::ResidentModel;
use crate::state::GatewayState;
use crate::stream::{self as sse_stream, RequestGuard};

/// An error on its way to the client.
///
/// The envelope is boxed because this type is the `Err` of the request path's
/// `Result`, and every successful response would otherwise carry the error
/// body's footprint around with it.
struct ApiError {
    status: StatusCode,
    envelope: Box<ErrorEnvelope>,
}

impl ApiError {
    fn new(status: StatusCode, envelope: ErrorEnvelope) -> Self {
        Self {
            status,
            envelope: Box::new(envelope),
        }
    }

    /// Build from any workspace error, taking its own idea of the status.
    fn from_backend(err: &BackendError) -> Self {
        let status =
            StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        Self::new(status, ErrorEnvelope::from_error(err))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::to_vec(&*self.envelope).unwrap_or_else(|_| {
            br#"{"error":{"message":"internal error","type":"server_error","code":"internal"}}"#
                .to_vec()
        });
        (
            self.status,
            [(header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response()
    }
}

/// `GET /health`.
///
/// Shaped like llama.cpp's, because clients probe for exactly that, with what
/// we additionally know appended.
///
/// Never refused, even when a key is configured: this is what a health check
/// calls, and those rarely carry credentials. What it *says* narrows instead —
/// an unauthenticated caller on a bind that requires a key is told whether the
/// gateway is serving, and nothing about what.
pub async fn health(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    let resident = state.catalog.resident().await;
    let mut body = json!({
        "status": if resident.is_some() { "ok" } else { "no model loaded" },
        "backend": state.backend.id().to_string(),
    });

    if is_authorized(&state, &headers)
        && let Some(object) = body.as_object_mut()
    {
        object.insert("engine".into(), json!(state.backend.health().await));
        object.insert(
            "model".into(),
            json!(resident.as_ref().map(|model| model.id.to_string())),
        );
    }
    (StatusCode::OK, axum::Json(body)).into_response()
}

/// `GET /version`.
pub async fn version() -> Response {
    axum::Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "build": concat!("hermes-gateway-", env!("CARGO_PKG_VERSION")),
    }))
    .into_response()
}

/// `GET /props`.
///
/// llama.cpp's endpoint, answered with *our* numbers. A client that probes it
/// must be told the same context `/v1/models` advertises; two endpoints
/// disagreeing about the window is worse than one of them being absent.
pub async fn props(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    let Some(model) = state.catalog.resident().await else {
        return ApiError::from_backend(&BackendError::NoModelLoaded).into_response();
    };
    let body = PropsBody::new(
        model.n_ctx,
        model.model_path.clone(),
        state.config.max_concurrent_requests,
    );
    // Redacted rather than refused: a client resolving a model's context
    // probes this endpoint, and a 401 would degrade that into a guess. The
    // context and the slot count are what it came for; the filesystem path is
    // not.
    let body = if is_authorized(&state, &headers) {
        body
    } else {
        body.redacted()
    };
    axum::Json(body).into_response()
}

/// `GET /v1/models`.
pub async fn models(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    axum::Json(ModelList::new(state.catalog.rows().await)).into_response()
}

/// `POST /v1/chat/completions`.
pub async fn chat_completions(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }

    let request: ChatCompletionRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return ApiError::new(
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::invalid_request(
                    format!("the request body is not valid JSON: {err}"),
                    "invalid_json",
                ),
            )
            .into_response();
        }
    };

    // Logged once per request at debug, never at a level that would fill a
    // log: a client relying on a parameter we ignore is otherwise impossible
    // to discover.
    let ignored = request.ignored_keys();
    if !ignored.is_empty() {
        tracing::debug!(
            target: targets::API,
            fields = ignored.join(","),
            "accepted request fields that this gateway does not act on"
        );
    }

    match serve_chat(state, request).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// The chat completion path, once the request has parsed.
async fn serve_chat(
    state: Arc<GatewayState>,
    request: ChatCompletionRequest,
) -> Result<Response, ApiError> {
    let model = state
        .catalog
        .resident()
        .await
        .ok_or_else(|| ApiError::from_backend(&BackendError::NoModelLoaded))?;

    let requested_model = request.model.clone().unwrap_or_default();
    if !model.matches(&requested_model) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            ErrorEnvelope::invalid_request(
                format!(
                    "the model {requested_model:?} is not loaded; this gateway is serving {:?}",
                    model.id
                ),
                "model_not_found",
            )
            .with_param("model"),
        ));
    }

    let mut generation = request.to_generation_request().map_err(|err| match err {
        RequestError::NoMessages => ApiError::new(
            StatusCode::BAD_REQUEST,
            ErrorEnvelope::invalid_request(
                "messages must be a non-empty array of chat messages",
                "invalid_messages",
            )
            .with_param("messages"),
        ),
    })?;

    // Pre-flight. This is what turns "the conversation outgrew the window"
    // into a 400 the client can parse a number out of and act on, instead of
    // an empty stream it retries verbatim.
    let prompt_tokens = state
        .backend
        .count_prompt_tokens(model.instance, &generation)
        .await
        .map_err(|err| ApiError::from_backend(&err))?;

    if prompt_tokens >= model.n_ctx {
        return Err(ApiError::from_backend(&BackendError::ContextOverflow {
            prompt_tokens,
            n_ctx: model.n_ctx,
        })
        .with_param("messages"));
    }

    generation.max_tokens = Some(clamp_max_tokens(
        request.requested_max_tokens(),
        model.n_ctx,
        prompt_tokens,
    ));

    let cancel = state.job_token();
    let permit = state.acquire_slot().await.ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorEnvelope::invalid_request(
                "the gateway is busy with another request; try again shortly",
                "server_busy",
            ),
        )
    })?;
    let guard = RequestGuard::new(cancel.clone(), Some(permit));

    let events = state
        .backend
        .generate(model.instance, generation, cancel)
        .await
        .map_err(|err| ApiError::from_backend(&err))?;

    let builder = ChunkBuilder::new(completion_id(), model.id.to_string());
    tracing::info!(
        target: targets::INFERENCE,
        id = builder.id(),
        model = %model.id,
        prompt_tokens,
        stream = request.stream,
        "generating"
    );

    if request.stream {
        Ok(streamed_response(sse_stream::encode(
            events,
            builder,
            guard,
            request.wants_usage(),
        )))
    } else {
        Ok(aggregate(events, builder, guard, &model).await)
    }
}

/// The output budget for one request.
///
/// Clamped, never rejected. Hermes defaults `max_tokens` to 65536
/// (`agent/run_agent.py:1673`), which exceeds any context this gateway can
/// load, and refusing it would break every request it makes. One token is
/// reserved so that generation cannot end by filling the window exactly.
fn clamp_max_tokens(requested: Option<u32>, n_ctx: u32, prompt_tokens: u32) -> u32 {
    let available = n_ctx.saturating_sub(prompt_tokens).saturating_sub(1).max(1);
    requested.map_or(available, |requested| requested.clamp(1, available))
}

/// Wrap an SSE frame stream in a response with the headers streaming needs.
fn streamed_response(
    frames: impl futures_util::Stream<Item = Result<String, std::convert::Infallible>> + Send + 'static,
) -> Response {
    let body = Body::from_stream(frames.map(|frame| frame.map(Bytes::from)));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        // Without these, a chunk can sit in a buffer somewhere until the next
        // one pushes it out - which on a slow CPU means tokens arriving in
        // bursts, or a keep-alive that never reaches the client it exists for.
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .header("x-accel-buffering", "no")
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Collect a whole generation into one response.
///
/// The non-streaming path is the streaming one with an accumulator on the end,
/// which is the right way round: a stream can always be collected, while a
/// completed response cannot be given back its timing.
async fn aggregate(
    mut events: hermes_inference::GenerationStream,
    builder: ChunkBuilder,
    guard: RequestGuard,
    model: &ResidentModel,
) -> Response {
    // Held for the whole aggregation: the slot is released, and the work
    // cancelled, when this returns by any path.
    let _guard = guard;

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: BTreeMap<u32, PartialToolCall> = BTreeMap::new();
    let mut usage = UsageBody::default();
    let mut finish_reason = FinishReason::Length;
    let mut timings = None;

    while let Some(event) = events.next().await {
        match event {
            Ok(GenerationEvent::ContentDelta { text }) => content.push_str(&text),
            Ok(GenerationEvent::ReasoningDelta { text }) => reasoning.push_str(&text),
            Ok(GenerationEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            }) => {
                let call = tool_calls.entry(index).or_default();
                // Assigned, not appended - the same discipline the streamed
                // form follows, so both produce the same call.
                if let Some(id) = id {
                    call.id = id;
                }
                if let Some(name) = name {
                    call.name = name;
                }
                if let Some(arguments) = arguments {
                    call.arguments.push_str(&arguments);
                }
            }
            Ok(GenerationEvent::Timings(measured)) => {
                timings = serde_json::to_value(measured).ok();
            }
            Ok(GenerationEvent::Finished {
                finish_reason: reason,
                usage: measured,
            }) => {
                finish_reason = reason;
                usage = UsageBody::from(measured);
            }
            Ok(GenerationEvent::Started { .. }) => {}
            Err(err) => {
                // Nothing has been sent yet, so this can still be an honest
                // HTTP status rather than a half-written body.
                return ApiError::from_backend(&err).into_response();
            }
        }
    }

    if content.is_empty() && tool_calls.is_empty() {
        tracing::warn!(
            target: targets::INFERENCE,
            id = builder.id(),
            "the model produced no content and no tool calls"
        );
    }

    let mut response = ChatCompletionResponse::new(
        builder.id().to_owned(),
        model.id.to_string(),
        Choice {
            index: 0,
            message: ResponseMessage {
                role: "assistant".to_owned(),
                content,
                reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
                tool_calls: tool_calls
                    .into_values()
                    .map(|call| ResponseToolCall {
                        id: call.id,
                        r#type: "function".to_owned(),
                        function: ResponseFunction {
                            name: call.name,
                            arguments: call.arguments,
                        },
                    })
                    .collect(),
            },
            finish_reason: finish_reason.as_str().to_owned(),
        },
        usage,
    );
    response.timings = timings;

    (StatusCode::OK, axum::Json(response)).into_response()
}

/// A tool call being assembled from deltas.
#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Whether this request carries credentials the policy accepts.
///
/// Distinct from [`authorize`], which produces the refusal: some endpoints
/// answer either way and only vary in how much they say.
fn is_authorized(state: &GatewayState, headers: &HeaderMap) -> bool {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    state.config.auth.check(presented).is_ok()
}

/// Check the request's credentials, if any are required.
///
/// Returns the refusal to send, or `None` when the request may proceed.
fn authorize(state: &GatewayState, headers: &HeaderMap) -> Option<Response> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    state.config.auth.check(presented).err().map(|failure| {
        let code = match failure {
            AuthFailure::Missing => "missing_api_key",
            AuthFailure::Invalid => "invalid_api_key",
        };
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            ErrorEnvelope {
                error: hermes_api::error::ErrorBody {
                    message: failure.message().to_owned(),
                    r#type: "authentication_error".to_owned(),
                    param: None,
                    code: code.to_owned(),
                    hermes: None,
                },
            },
        )
        .into_response()
    })
}

impl ApiError {
    fn with_param(mut self, param: &str) -> Self {
        self.envelope = Box::new((*self.envelope).with_param(param));
        self
    }
}

/// A completion id in OpenAI's shape.
///
/// Random rather than sequential: it appears on every chunk and in the client's
/// logs, and two runs of the gateway producing the same ids would make those
/// logs ambiguous.
fn completion_id() -> String {
    let mut bytes = [0_u8; 12];
    if getrandom::fill(&mut bytes).is_err() {
        // Entropy is not available. An id is decoration on a response, so fall
        // back to the clock rather than failing a request over it.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or_default();
        bytes[..4].copy_from_slice(&nanos.to_le_bytes());
    }
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("chatcmpl-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oversized_max_tokens_is_clamped_rather_than_refused() {
        // The client's default is 65536. Refusing it would break every request
        // it makes; clamping gives it the largest answer that fits.
        assert_eq!(clamp_max_tokens(Some(65_536), 8192, 1000), 7191);
        assert_eq!(clamp_max_tokens(None, 8192, 1000), 7191);
        assert_eq!(clamp_max_tokens(Some(100), 8192, 1000), 100);
    }

    #[test]
    fn the_clamp_never_reaches_zero() {
        // A budget of zero produces an empty completion, which is exactly the
        // empty stream the client retries blindly.
        assert_eq!(clamp_max_tokens(Some(0), 4096, 10), 1);
        assert_eq!(clamp_max_tokens(Some(500), 4096, 4095), 1);
        assert_eq!(clamp_max_tokens(None, 4096, 5000), 1);
    }

    #[test]
    fn completion_ids_look_like_openai_ids_and_do_not_repeat() {
        let id = completion_id();
        assert!(id.starts_with("chatcmpl-"), "{id}");
        assert_ne!(id, completion_id());
    }
}
