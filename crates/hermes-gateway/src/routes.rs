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

use std::collections::{BTreeMap, VecDeque};
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
use hermes_api::completions::{CompletionChunkBuilder, CompletionRequest};
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
use crate::completions::{self as completions_run, PendingCompletion};
use crate::metrics::{Endpoint, Outcome};
use crate::scheduler::Band;
use crate::state::GatewayState;
use crate::stream::{self as sse_stream, RequestGuard, StartGeneration};

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

/// `GET /metrics`.
///
/// Prometheus text exposition, because that is what a scrape expects to find
/// at that path and inventing a private format would mean every operator
/// writing an exporter.
///
/// Behind the same key as `/v1/models` when one is configured: request rates,
/// token counts and queue depth describe what this machine is doing, and on an
/// exposed bind that is not public information. It carries no prompt text, no
/// completion text and no filesystem path — see [`crate::metrics`].
pub async fn metrics(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let body = state.metrics_snapshot().await.to_prometheus();
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// `GET /api/v1/metrics`.
///
/// The same snapshot as JSON, for our own UI. Under `/api/v1` rather than
/// `/v1`, because `/v1` is the OpenAI surface and a client walking it must
/// never find one of our own endpoints there.
pub async fn metrics_json(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    axum::Json(state.metrics_snapshot().await).into_response()
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

    let request: ChatCompletionRequest = match parse_body(&body) {
        Ok(request) => request,
        Err(err) => return err.into_response(),
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

    let metrics = Arc::clone(&state);
    let response = match serve_chat(state, request).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    };
    metrics
        .metrics()
        .record_request(Endpoint::ChatCompletions, outcome_of(response.status()));
    response
}

/// `POST /v1/completions`.
///
/// The older endpoint, and a genuinely different one: raw text continued with
/// no chat template. See [`crate::completions`] for why one request here can be
/// several generations.
pub async fn completions(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }

    let request: CompletionRequest = match parse_body(&body) {
        Ok(request) => request,
        Err(err) => return err.into_response(),
    };

    let ignored = request.ignored_keys();
    if !ignored.is_empty() {
        tracing::debug!(
            target: targets::API,
            fields = ignored.join(","),
            "accepted request fields that this gateway does not act on"
        );
    }

    let metrics = Arc::clone(&state);
    let response = match serve_completions(state, request).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    };
    metrics
        .metrics()
        .record_request(Endpoint::Completions, outcome_of(response.status()));
    response
}

/// How a status code counts.
///
/// `Busy` is pulled out of the 5xx range on purpose: it is the only one that
/// says the gateway could not keep up rather than that something broke, and
/// folding it in with genuine failures hides the single number that says the
/// queue needs attention.
fn outcome_of(status: StatusCode) -> Outcome {
    match status {
        StatusCode::SERVICE_UNAVAILABLE => Outcome::Busy,
        status if status.is_success() => Outcome::Ok,
        status if status.is_client_error() => Outcome::ClientError,
        _ => Outcome::ServerError,
    }
}

/// The completion path, once the request has parsed.
async fn serve_completions(
    state: Arc<GatewayState>,
    request: CompletionRequest,
) -> Result<Response, ApiError> {
    let model = state
        .catalog
        .resident()
        .await
        .ok_or_else(|| ApiError::from_backend(&BackendError::NoModelLoaded))?;
    require_matching_model(&model, request.model.as_deref())?;

    let prompts = request.expand().map_err(|err| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            ErrorEnvelope::invalid_request(err.to_string(), err.code()).with_param(err.param()),
        )
    })?;

    // Every prompt is counted and clamped before any of them is generated. A
    // request whose third prompt overflows the window must fail as a 400, not
    // after two completions have already been streamed to the client.
    let mut queue = VecDeque::with_capacity(prompts.len());
    let mut largest_prompt = 0;
    for (index, prompt) in prompts.iter().enumerate() {
        let mut generation = request.to_generation_request(prompt);
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
            .with_param("prompt"));
        }
        largest_prompt = largest_prompt.max(prompt_tokens);
        generation.max_tokens = Some(clamp_max_tokens(
            request.max_tokens,
            model.n_ctx,
            prompt_tokens,
        ));
        queue.push_back(PendingCompletion {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            request: generation,
            echo: request.echoes_the_prompt().then(|| prompt.clone()),
        });
    }

    // One permit for the whole request, held across every generation in it, so
    // a multi-prompt request cannot be interleaved with another client's.
    //
    // The band is taken from the *largest* prompt in the request, and one
    // request here can carry many: a request is as long as its longest part,
    // and classifying by the first prompt alone would let a short opener carry
    // a dozen long continuations into the fast band. The output budget is
    // shared, so it counts once.
    let band = Band::classify(
        largest_prompt,
        request.max_tokens,
        // Live rather than configured: a model swap re-derives these from the
        // context the new model is actually loaded with.
        state.scheduler().band_limits(),
    );
    let cancel = state.job_token();
    let waiting_since = std::time::Instant::now();
    let permit = state.acquire_slot(band).await.ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorEnvelope::invalid_request(
                "the gateway is busy with another request; try again shortly",
                "server_busy",
            ),
        )
    })?;
    // The builder is made first so the guard can be told what it is accounting
    // for. Naming it costs nothing and has no side effects.
    let builder = CompletionChunkBuilder::new(completion_id(), model.id.to_string());
    let mut guard = RequestGuard::new(cancel, Some(permit))
        .reporting_to(Arc::clone(state.metrics()))
        .describing(builder.id(), model.id.to_string());
    guard.waited(waiting_since.elapsed());

    tracing::info!(
        target: targets::INFERENCE,
        id = builder.id(),
        model = %model.id,
        completions = queue.len(),
        stream = request.stream,
        "generating text completions"
    );

    let run = completions_run::Run {
        state: Arc::clone(&state),
        instance: model.instance,
        queue,
        builder,
        guard,
        model_id: model.id.to_string(),
        include_usage: request.wants_usage(),
    };

    if request.stream {
        Ok(streamed_response(completions_run::encode(run)))
    } else {
        match completions_run::aggregate(run).await {
            Ok(response) => Ok((StatusCode::OK, axum::Json(response)).into_response()),
            Err(err) => Err(ApiError::from_backend(&err)),
        }
    }
}

/// Parse a request body, telling malformed JSON apart from the wrong shape.
///
/// They are different mistakes and deserve different sentences. A body that is
/// valid JSON with `tools` as a string is not "not valid JSON", and telling a
/// client it is sends them hunting for a syntax error that is not there.
fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|err| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            ErrorEnvelope::invalid_request(
                format!("the request body is not valid JSON: {err}"),
                "invalid_json",
            ),
        )
    })?;
    serde_json::from_value(value).map_err(|err| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            ErrorEnvelope::invalid_request(
                format!("the request body has a field this endpoint cannot read: {err}"),
                "invalid_request_body",
            ),
        )
    })
}

/// Refuse a request naming a model this gateway is not serving.
fn require_matching_model(model: &ResidentModel, requested: Option<&str>) -> Result<(), ApiError> {
    let requested = requested.unwrap_or_default();
    if model.matches(requested) {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::NOT_FOUND,
        ErrorEnvelope::invalid_request(
            format!(
                "the model {requested:?} is not loaded; this gateway is serving {:?}",
                model.id
            ),
            "model_not_found",
        )
        .with_param("model"),
    ))
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

    require_matching_model(&model, request.model.as_deref())?;

    // Every request-level refusal carries its own field, code and sentence, so
    // adding one cannot accidentally produce a 400 that says nothing useful.
    let mut generation = request.to_generation_request().map_err(bad_request)?;

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

    // Which queue this request joins, decided from what has just been
    // measured rather than from anything the client claimed about itself.
    let band = Band::classify(
        prompt_tokens,
        request.requested_max_tokens(),
        // Live rather than configured: a model swap re-derives these from the
        // context the new model is actually loaded with.
        state.scheduler().band_limits(),
    );
    let cancel = state.job_token();
    let builder = ChunkBuilder::new(completion_id(), model.id.to_string());
    tracing::info!(
        target: targets::INFERENCE,
        id = builder.id(),
        model = %model.id,
        prompt_tokens,
        band = band.as_str(),
        stream = request.stream,
        "generating"
    );

    // The uncontended path, unchanged: take the slot, start the generation, and
    // report any failure to start as an HTTP status, because nothing has been
    // written to the client yet.
    if let Some(permit) = state.try_acquire_slot() {
        let guard = RequestGuard::new(cancel.clone(), Some(permit))
            .reporting_to(Arc::clone(state.metrics()))
            .describing(builder.id(), model.id.to_string());
        let events = state
            .backend
            .generate(model.instance, generation, cancel)
            .await
            .map_err(|err| ApiError::from_backend(&err))?;
        return Ok(if request.stream {
            streamed_response(sse_stream::encode(
                events,
                builder,
                guard,
                request.wants_usage(),
            ))
        } else {
            aggregate(events, builder, guard, &model).await
        });
    }

    // Contended. A streamed request is answered *now* and waits inside its own
    // response, so the client can see that it is queued and where; anything
    // else waits here and is told 503 if the wait runs out.
    tracing::info!(
        target: targets::SCHEDULER,
        id = builder.id(),
        band = band.as_str(),
        waiting = state.scheduler().snapshot().waiting,
        "queued behind another request"
    );

    if request.stream {
        let ticket = state.enqueue(band);
        let deadline = tokio::time::Instant::now() + state.config.queue_timeout;
        let guard = RequestGuard::new(cancel.clone(), None)
            .reporting_to(Arc::clone(state.metrics()))
            .describing(builder.id(), model.id.to_string());
        let backend = Arc::clone(&state.backend);
        let instance = model.instance;
        let start: StartGeneration = Box::new(move || {
            Box::pin(async move { backend.generate(instance, generation, cancel).await })
        });
        return Ok(streamed_response(sse_stream::encode_queued(
            ticket,
            start,
            deadline,
            state.config.queue_notice_interval,
            builder,
            guard,
            request.wants_usage(),
        )));
    }

    let waiting_since = std::time::Instant::now();
    let permit = state.acquire_slot(band).await.ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorEnvelope::invalid_request(
                "the gateway is busy with another request; try again shortly",
                "server_busy",
            ),
        )
    })?;
    let mut guard = RequestGuard::new(cancel.clone(), Some(permit))
        .reporting_to(Arc::clone(state.metrics()))
        .describing(builder.id(), model.id.to_string());
    guard.waited(waiting_since.elapsed());

    let events = state
        .backend
        .generate(model.instance, generation, cancel)
        .await
        .map_err(|err| ApiError::from_backend(&err))?;

    Ok(aggregate(events, builder, guard, &model).await)
}

/// Turn a request-level refusal into the 400 that names the field at fault.
fn bad_request(err: RequestError) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        ErrorEnvelope::invalid_request(err.to_string(), err.code()).with_param(err.param()),
    )
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
    // Held for the whole aggregation: the slot is released, the work
    // cancelled, and what it cost recorded, when this returns by any path.
    let mut guard = guard;

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: BTreeMap<u32, PartialToolCall> = BTreeMap::new();
    let mut usage = UsageBody::default();
    let mut finish_reason = FinishReason::Length;
    let mut timings = None;

    while let Some(event) = events.next().await {
        match event {
            Ok(GenerationEvent::ContentDelta { text }) => {
                guard.first_token();
                content.push_str(&text);
            }
            Ok(GenerationEvent::ReasoningDelta { text }) => {
                guard.first_token();
                reasoning.push_str(&text);
            }
            Ok(GenerationEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            }) => {
                guard.first_token();
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
                let record = guard.record_mut();
                record.prefill = Some(std::time::Duration::from_secs_f64(
                    measured.prompt_ms / 1000.0,
                ));
                record.decode = Some(std::time::Duration::from_secs_f64(
                    measured.predicted_ms / 1000.0,
                ));
                record.cached_tokens = measured.cached_n;
                timings = serde_json::to_value(measured).ok();
            }
            Ok(GenerationEvent::Finished {
                finish_reason: reason,
                usage: measured,
            }) => {
                finish_reason = reason;
                let record = guard.record_mut();
                record.finish_reason = Some(reason);
                record.prompt_tokens = measured.prompt_tokens;
                record.completion_tokens = measured.completion_tokens;
                if measured.cached_tokens > 0 {
                    record.cached_tokens = measured.cached_tokens;
                }
                usage = UsageBody::from(measured);
            }
            Ok(GenerationEvent::Started { .. }) => {}
            Err(err) => {
                // Nothing has been sent yet, so this can still be an honest
                // HTTP status rather than a half-written body.
                guard.record_mut().finish_reason = Some(FinishReason::Error);
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
pub(crate) fn authorize(state: &GatewayState, headers: &HeaderMap) -> Option<Response> {
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

    /// Every failure spec section 27 enumerates, with the status it must carry.
    ///
    /// The table is written out rather than derived so that a variant whose
    /// class changes fails here, in a diff a reviewer reads, instead of
    /// silently turning a 400 a client can act on into a 500 it cannot.
    fn section_27_taxonomy() -> Vec<(BackendError, StatusCode, &'static str)> {
        vec![
            // Runtime acquisition: our problem, not the caller's, except when
            // the machine is out of room.
            (
                BackendError::UnsupportedPlatform {
                    os: "plan9",
                    arch: "x86_64",
                },
                StatusCode::INTERNAL_SERVER_ERROR,
                "unsupported_platform",
            ),
            (
                BackendError::RuntimeDownloadFailed {
                    reason: "offline".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
                "runtime_download_failed",
            ),
            (
                BackendError::RuntimeCorrupt {
                    expected: "a".into(),
                    actual: "b".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
                "runtime_corrupt",
            ),
            (
                BackendError::RuntimeMissing { path: "/x".into() },
                StatusCode::INTERNAL_SERVER_ERROR,
                "runtime_missing",
            ),
            (
                BackendError::LowDisk {
                    needed: 100,
                    available: 1,
                },
                StatusCode::INSUFFICIENT_STORAGE,
                "low_disk",
            ),
            // Model admission: things the caller chose, so 400 or 404.
            (
                BackendError::ModelNotFound {
                    path: "/x.gguf".into(),
                },
                StatusCode::NOT_FOUND,
                "model_not_found",
            ),
            (
                BackendError::UnsupportedArchitecture {
                    found: "xyz".into(),
                    supported: vec!["llama".into()],
                },
                StatusCode::BAD_REQUEST,
                "unsupported_architecture",
            ),
            (
                BackendError::InvalidContextLength {
                    requested: 99_999,
                    max: 8192,
                },
                StatusCode::BAD_REQUEST,
                "invalid_context_length",
            ),
            (
                BackendError::ContextOverflow {
                    prompt_tokens: 41_022,
                    n_ctx: 32_768,
                },
                StatusCode::BAD_REQUEST,
                "context_length_exceeded",
            ),
            (
                BackendError::UnsupportedKvCacheType {
                    requested: "q6_K".into(),
                    supported: vec!["f16".into()],
                },
                StatusCode::BAD_REQUEST,
                "unsupported_kv_cache_type",
            ),
            (
                BackendError::InsufficientMemory {
                    model: "m".into(),
                    required: "8 GiB".into(),
                    available: "2 GiB".into(),
                },
                StatusCode::INSUFFICIENT_STORAGE,
                "insufficient_memory",
            ),
            // Process lifecycle: the engine is not able to serve right now.
            (
                BackendError::StartTimeout { seconds: 300 },
                StatusCode::SERVICE_UNAVAILABLE,
                "engine_start_timeout",
            ),
            (
                BackendError::EngineCrashed {
                    detail: "signal 11".into(),
                    exit_code: None,
                    signal: Some(11),
                    tail: vec![],
                },
                StatusCode::SERVICE_UNAVAILABLE,
                "engine_crashed",
            ),
            (
                BackendError::EngineOom { tail: vec![] },
                StatusCode::INSUFFICIENT_STORAGE,
                "engine_out_of_memory",
            ),
            (
                BackendError::UnsupportedCpuInstruction {
                    detected: "SSE4.2".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
                "unsupported_cpu_instruction",
            ),
            (
                BackendError::EngineUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "engine_unavailable",
            ),
            (
                BackendError::GenerationFailed {
                    detail: "slot lost".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
                "generation_failed",
            ),
            (
                BackendError::NoModelLoaded,
                StatusCode::SERVICE_UNAVAILABLE,
                "no_model_loaded",
            ),
            (
                BackendError::Cancelled,
                StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_REQUEST),
                "cancelled",
            ),
            (
                BackendError::io("reading", std::io::Error::other("boom")),
                StatusCode::INTERNAL_SERVER_ERROR,
                "io_error",
            ),
        ]
    }

    #[test]
    fn every_section_27_failure_becomes_the_right_status_and_body() {
        // The gateway's half of the promise. `hermes_api` proves the body is
        // well formed; this proves the status a client branches on before it
        // ever reads the body, and that the two agree.
        for (err, expected_status, expected_code) in section_27_taxonomy() {
            let api_error = ApiError::from_backend(&err);
            assert_eq!(
                api_error.status, expected_status,
                "{expected_code} carried the wrong status"
            );

            let json = serde_json::to_value(&*api_error.envelope).expect("serialize");
            assert_eq!(json["error"]["code"], expected_code);
            assert!(
                json["error"]["message"]
                    .as_str()
                    .is_some_and(|message| !message.is_empty()),
                "{expected_code} has an empty message"
            );
            assert!(
                json["error"]["type"]
                    .as_str()
                    .is_some_and(|kind| kind.ends_with("error")),
                "{expected_code} has a non-OpenAI type: {json}"
            );
            // The status the error itself claims and the one the response
            // carries must never diverge - a client reading one and logging the
            // other would be told two different things about one failure.
            assert_eq!(u16::from(api_error.status), err.http_status());
        }
    }

    #[test]
    fn the_taxonomy_covers_every_variant_the_backend_can_produce() {
        // A guard on the table above: a new `BackendError` variant that nobody
        // adds here would otherwise be untested, and would reach a client with
        // whatever status its class happened to default to.
        let covered: std::collections::BTreeSet<&str> = section_27_taxonomy()
            .iter()
            .map(|(err, _, _)| err.code())
            .collect();
        assert_eq!(
            covered.len(),
            section_27_taxonomy().len(),
            "two entries share a code, so one of them is not being checked"
        );
        // Every code the api crate's own error-path gate exercises must appear
        // here too; the two lists are the same taxonomy seen from either side.
        for code in [
            "unsupported_platform",
            "runtime_download_failed",
            "runtime_corrupt",
            "runtime_missing",
            "low_disk",
            "model_not_found",
            "unsupported_architecture",
            "invalid_context_length",
            "context_length_exceeded",
            "unsupported_kv_cache_type",
            "insufficient_memory",
            "engine_start_timeout",
            "engine_crashed",
            "engine_out_of_memory",
            "unsupported_cpu_instruction",
            "engine_unavailable",
            "generation_failed",
            "no_model_loaded",
            "cancelled",
            "io_error",
        ] {
            assert!(covered.contains(code), "{code} is not in the status table");
        }
    }

    #[test]
    fn a_request_level_refusal_names_its_field_and_code() {
        // Each variant of `RequestError` must arrive as a 400 that says which
        // field to look at; a 400 with no `param` sends the client guessing.
        for err in [
            RequestError::NoMessages,
            RequestError::ToolWithoutName { index: 2 },
            RequestError::UnknownToolChoice {
                value: "banana".into(),
            },
            RequestError::ToolChoiceNotDeclared {
                name: "missing".into(),
            },
            RequestError::ToolChoiceWithoutTools,
        ] {
            let api_error = bad_request(err.clone());
            assert_eq!(api_error.status, StatusCode::BAD_REQUEST);
            let json = serde_json::to_value(&*api_error.envelope).expect("serialize");
            assert_eq!(json["error"]["param"], err.param());
            assert_eq!(json["error"]["code"], err.code());
            assert_eq!(json["error"]["type"], "invalid_request_error");
            assert!(
                json["error"]["message"]
                    .as_str()
                    .is_some_and(|message| !message.is_empty())
            );
        }
    }

    #[test]
    fn completion_ids_look_like_openai_ids_and_do_not_repeat() {
        let id = completion_id();
        assert!(id.starts_with("chatcmpl-"), "{id}");
        assert_ne!(id, completion_id());
    }
}
