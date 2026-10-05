//! Forwarding one inference request to the deployment that should answer it.
//!
//! The order of a request:
//!
//! 1. **Parse** the body just far enough to read `model`. Every other field is
//!    forwarded as the client sent it — the node, not the router, decides what
//!    `tools`, `max_tokens` or `reasoning_effort` mean.
//! 2. **Resolve** `model` to a route, through the same `default` rules the
//!    gateway applies.
//! 3. **Require**: what the request needs of a deployment — its endpoint,
//!    tools, `tool_choice`, reasoning, and room for its prompt — read once by
//!    [`crate::requirements`]. A request the gateway would refuse is refused
//!    here with the gateway's own 400.
//! 4. **Plan** from the health book: the route's available deployments, then
//!    those that can serve this request, then the route's policy. No network
//!    call is made to decide.
//! 5. **Attempt** each candidate in turn, with `model` rewritten to that node's
//!    local name and the node's own credential. A failure *before the node
//!    answered* — refused connection, timeout, 502/503/504, or a node that no
//!    longer serves the model — moves on to the next candidate.
//! 6. **Commit** on the first answer that is not one of those. From here the
//!    deployment is fixed: the response is returned with `model` rewritten to
//!    the route's name, and a stream is relayed frame by frame. If the node
//!    fails mid-stream the client is told so in-band; no other node is asked
//!    to continue an answer it did not start.
//!
//! Cancellation needs no code of its own. A disconnecting client makes hyper
//! drop the response body; the body owns the upstream stream; dropping that
//! closes the connection to the node; and the node's gateway stops generating
//! when its client goes away. Each link is ownership, not a flag someone must
//! remember to check.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use futures_util::{Stream, StreamExt};
use lightweight_api::error::ErrorEnvelope;
use lightweight_observability::targets;
use serde_json::Value;

use crate::RouterState;
use crate::domain::{CapabilityGap, DeploymentId, Node, RouteName, RoutingFailure};
use crate::error::{json_error, routing_failure, server_error};
use crate::health::{Outcome as Probe, describe_transport};
use crate::load::Lease;
use crate::metrics::{ActiveGuard, Outcome, UNKNOWN_ROUTE};
use crate::requirements;
use crate::select::{Candidate, Selection};
use crate::sse::{FrameRewriter, rewrite_body};

/// The header a request is correlated by, from client to router to node.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The longest client-supplied request id the router will carry on.
const MAX_REQUEST_ID: usize = 128;

/// How much of a refusal body is read before deciding whether to fail over.
/// Refusals are short JSON envelopes; this only bounds a misbehaving peer.
const REFUSAL_LIMIT: usize = 64 * 1024;

/// The two generation endpoints the router forwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    ChatCompletions,
    Completions,
}

impl Endpoint {
    pub const fn path(self) -> &'static str {
        match self {
            Self::ChatCompletions => "/v1/chat/completions",
            Self::Completions => "/v1/completions",
        }
    }

    /// The endpoint's name in a log line.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat",
            Self::Completions => "completion",
        }
    }
}

/// The request id to use: the client's own when it sent a usable one, so a
/// trace started in the client survives the hop, and a fresh one otherwise.
pub fn request_id(headers: &HeaderMap) -> String {
    headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|id| {
            !id.is_empty() && id.len() <= MAX_REQUEST_ID && id.bytes().all(|b| b.is_ascii_graphic())
        })
        .map_or_else(generate_request_id, str::to_owned)
}

fn generate_request_id() -> String {
    let mut bytes = [0_u8; 12];
    if getrandom::fill(&mut bytes).is_err() {
        // Decoration on a log line, not a credential: fall back to the clock
        // rather than failing a request for want of entropy.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        bytes.copy_from_slice(&nanos.to_le_bytes()[..12]);
    }
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("rtr-{hex}")
}

/// A node's answer that made the router try elsewhere, kept in case every
/// candidate refuses: then the last node's own words are better than a generic
/// router error.
struct Refusal {
    status: StatusCode,
    content_type: Option<HeaderValue>,
    retry_after: Option<HeaderValue>,
    body: Bytes,
}

/// What a committed response holds until its body is finished: the router's
/// active-request gauge and the deployment's in-flight slot. Dropped when a
/// stream ends or its client goes away, or once a whole body has been read.
struct InFlight {
    _active: Option<ActiveGuard>,
    _lease: Lease,
}

/// What an attempt that did not commit tells the next step.
enum Attempt {
    Committed(Response),
    /// Try the next candidate. The node's refusal, if it sent one worth
    /// returning.
    Next(Option<Refusal>),
}

/// Forward one generation request.
pub async fn forward(
    state: Arc<RouterState>,
    endpoint: Endpoint,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    let active = state.metrics.enter();
    let request_id = request_id(headers);
    let mut response = route_request(&state, endpoint, headers, body, &request_id, active).await;
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }
    response
}

async fn route_request(
    state: &Arc<RouterState>,
    endpoint: Endpoint,
    headers: &HeaderMap,
    body: &Bytes,
    request_id: &str,
    active: ActiveGuard,
) -> Response {
    let mut request = match parse(body) {
        Ok(request) => request,
        Err(refusal) => {
            state
                .metrics
                .record_request(UNKNOWN_ROUTE, Outcome::ClientError);
            return *refusal;
        }
    };

    let started = Instant::now();
    let requested = request.get("model").and_then(Value::as_str);
    let route = match state.topology.resolve(requested) {
        Ok(route) => route,
        Err(failure) => {
            tracing::info!(
                target: targets::ROUTER,
                request_id,
                requested = requested.unwrap_or(""),
                error = failure_code(&failure),
                "request not routed"
            );
            state
                .metrics
                .record_request(UNKNOWN_ROUTE, Outcome::ClientError);
            return routing_failure(&failure, state.policy.interval);
        }
    };

    // What the request needs, read once, before any deployment is looked at.
    let needs = match requirements::extract(endpoint, body) {
        Ok(needs) => needs,
        Err(refusal) => {
            tracing::info!(
                target: targets::ROUTER,
                request_id,
                route = %route.name,
                endpoint = endpoint.as_str(),
                upstream_status = refusal.status().as_u16(),
                "request refused before routing"
            );
            state
                .metrics
                .record_request(route.name.as_str(), Outcome::ClientError);
            return *refusal;
        }
    };

    // Eligibility, then what this request needs, then the route's policy, then
    // a slot reserved on the first choice - all in the selector. Nothing below
    // this line knows which policy the route uses or what was filtered; it
    // only walks the order it was handed.
    let mut plan = match state.selector.plan_request(
        &state.topology,
        route,
        &state.health.snapshot(),
        &state.health.deployment_snapshot(),
        &needs,
    ) {
        Ok(plan) => plan,
        Err(failure) => {
            if let RoutingFailure::CapabilityMismatch { unfit, unmet, .. } = &failure {
                record_unfit(state, request_id, &route.name, unfit);
                tracing::warn!(
                    target: targets::ROUTER,
                    request_id,
                    route = %route.name,
                    endpoint = endpoint.as_str(),
                    requires_tools = needs.tools,
                    tool_choice = needs.tool_choice.as_str(),
                    requires_reasoning = needs.reasoning,
                    required_context = needs.required_context(),
                    eligible_before = unfit.len(),
                    eligible_after = 0,
                    filtered = filtered_counts(unfit),
                    unmet = unmet.iter().map(|gap| gap.as_str()).collect::<Vec<_>>().join(","),
                    error = failure_code(&failure),
                    "no available deployment can serve this request"
                );
                state
                    .metrics
                    .record_capability_mismatch(route.name.as_str());
                state
                    .metrics
                    .record_request(route.name.as_str(), Outcome::ClientError);
                return routing_failure(&failure, state.policy.interval);
            }
            tracing::warn!(
                target: targets::ROUTER,
                request_id,
                route = %route.name,
                error = failure_code(&failure),
                "no deployment is available"
            );
            state
                .metrics
                .record_request(route.name.as_str(), Outcome::Unavailable);
            return routing_failure(&failure, state.policy.interval);
        }
    };
    record_unfit(state, request_id, &route.name, &plan.unfit);
    let routing_ms = started.elapsed().as_secs_f64() * 1000.0;
    for (deployment, reason) in &plan.skipped {
        tracing::debug!(
            target: targets::ROUTER,
            request_id,
            route = %route.name,
            deployment = %deployment,
            reason = reason.as_str(),
            "deployment skipped"
        );
    }
    let eligible_after = plan.candidates.len();
    let eligible_before = eligible_after + plan.unfit.len();
    let filtered = filtered_counts(&plan.unfit);

    let (cursor, selected_index, active_before, concurrency_limit) = match plan.selection {
        Selection::Priority => (None, None, None, None),
        Selection::RoundRobin {
            cursor,
            selected_index,
        } => (Some(cursor), Some(selected_index), None, None),
        Selection::LeastBusy {
            active_before,
            concurrency_limit,
            ..
        } => (None, None, Some(active_before), concurrency_limit),
    };

    let mut active = Some(active);
    let mut reservation = plan.reservation.take();
    let mut last_refusal = None;
    for (attempt, candidate) in plan.candidates.iter().enumerate() {
        let Some(decision) = plan.decision(attempt) else {
            break;
        };
        let Some(node) = state.topology.node(&candidate.node) else {
            continue;
        };
        // One slot per attempt, held for exactly as long as the attempt: the
        // first choice's was reserved when it was chosen, a failover's is taken
        // here. A failed attempt's lease is dropped at the end of this
        // iteration, before the next deployment's is taken.
        let lease = reservation
            .take()
            .filter(|_| attempt == 0)
            .unwrap_or_else(|| state.selector.load().acquire(&candidate.deployment));
        request.insert(
            "model".into(),
            Value::String(candidate.remote_model.clone()),
        );
        let context = AttemptContext {
            state,
            endpoint,
            headers,
            request_id,
            route: &route.name,
            node,
            candidate,
        };
        match attempt_one(&context, &request).await {
            Attempt::Next(refusal) => {
                drop(lease);
                if refusal.is_some() {
                    last_refusal = refusal;
                }
                if attempt + 1 < plan.candidates.len() {
                    state.metrics.record_failover(route.name.as_str());
                }
            }
            Attempt::Committed(response) => {
                tracing::info!(
                    target: targets::ROUTER,
                    request_id,
                    route = %decision.route,
                    policy = plan.policy.as_str(),
                    node = %decision.node,
                    deployment = %decision.deployment,
                    reason = decision.reason.as_str(),
                    cursor,
                    selected_index,
                    active_before,
                    concurrency_limit,
                    endpoint = endpoint.as_str(),
                    requires_tools = needs.tools,
                    tool_choice = needs.tool_choice.as_str(),
                    requires_reasoning = needs.reasoning,
                    required_context = needs.required_context(),
                    max_tokens = needs.max_tokens,
                    eligible_before,
                    eligible_after,
                    filtered = filtered.as_str(),
                    routing_ms,
                    upstream_status = response.status().as_u16(),
                    failover_count = attempt,
                    "routed"
                );
                state.metrics.record_request(
                    route.name.as_str(),
                    Outcome::of_status(response.status().as_u16()),
                );
                state.metrics.record_decision(
                    route.name.as_str(),
                    plan.policy.as_str(),
                    decision.reason.as_str(),
                );
                let held = InFlight {
                    _active: active.take(),
                    _lease: lease,
                };
                return commit(response, &route.name, held).await;
            }
        }
    }

    let tried = plan.candidates.len();
    match last_refusal {
        Some(refusal) => {
            tracing::warn!(
                target: targets::ROUTER,
                request_id,
                route = %route.name,
                upstream_status = refusal.status.as_u16(),
                failover_count = tried,
                "every deployment refused; returning the last refusal"
            );
            state.metrics.record_request(
                route.name.as_str(),
                Outcome::of_status(refusal.status.as_u16()),
            );
            refusal_response(refusal)
        }
        None => {
            tracing::warn!(
                target: targets::ROUTER,
                request_id,
                route = %route.name,
                failover_count = tried,
                "every deployment failed before answering"
            );
            state
                .metrics
                .record_request(route.name.as_str(), Outcome::Unavailable);
            routing_failure(
                &RoutingFailure::RouteUnavailable {
                    route: route.name.clone(),
                },
                state.policy.interval,
            )
        }
    }
}

struct AttemptContext<'a> {
    state: &'a Arc<RouterState>,
    endpoint: Endpoint,
    headers: &'a HeaderMap,
    request_id: &'a str,
    route: &'a RouteName,
    node: &'a Node,
    candidate: &'a Candidate,
}

/// Send the request to one deployment and decide whether it committed.
async fn attempt_one(
    context: &AttemptContext<'_>,
    request: &serde_json::Map<String, Value>,
) -> Attempt {
    let AttemptContext {
        state,
        endpoint,
        headers,
        request_id,
        route,
        node,
        candidate,
    } = context;

    let Ok(payload) = serde_json::to_vec(request) else {
        return Attempt::Next(None);
    };
    let mut upstream = state
        .client
        .post(node.endpoint(endpoint.path()))
        .header(header::CONTENT_TYPE, "application/json")
        .header(REQUEST_ID_HEADER, *request_id)
        .body(payload);
    // Forwarded by name, never wholesale: the client's `Authorization` is the
    // router's credential and must not reach a node, and nothing else a client
    // sends is the node's business.
    if let Some(accept) = headers.get(header::ACCEPT) {
        upstream = upstream.header(header::ACCEPT, accept.clone());
    }
    if let Some(value) = node.auth.header_value() {
        upstream = upstream.header(header::AUTHORIZATION, value);
    }

    let response = match upstream.send().await {
        Ok(response) => response,
        Err(err) => {
            let reason = describe_transport(&err);
            // Only what says the node is unreachable counts against its health.
            // A request that failed for a reason of its own says nothing about
            // whether the next one will.
            if err.is_connect() || err.is_timeout() {
                state
                    .health
                    .record(&node.id, Probe::Failure(format!("request: {reason}")));
            }
            tracing::warn!(
                target: targets::ROUTER,
                request_id,
                route = %route,
                node = %node.id,
                deployment = %candidate.deployment,
                error = reason,
                "deployment failed before answering"
            );
            return Attempt::Next(None);
        }
    };

    let status = response.status();
    if !matches!(
        status,
        StatusCode::NOT_FOUND
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    ) {
        return Attempt::Committed(into_response(response, status));
    }

    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let retry_after = response.headers().get(header::RETRY_AFTER).cloned();
    let body = read_bounded(response).await;
    let code = error_code(&body);

    if status == StatusCode::NOT_FOUND {
        if code.as_deref() != Some("model_not_found") {
            // A 404 that is not about the model is the node's answer to this
            // request, and stands.
            return Attempt::Committed(refusal_response(Refusal {
                status,
                content_type,
                retry_after,
                body,
            }));
        }
        // The node swapped models since it was last probed. Nothing ran, so
        // the next deployment may answer; the node's message names its local
        // model and is not shown to the client.
        state.health.forget_model(&node.id);
        tracing::warn!(
            target: targets::ROUTER,
            request_id,
            route = %route,
            node = %node.id,
            deployment = %candidate.deployment,
            upstream_status = status.as_u16(),
            "the node no longer serves this deployment's model"
        );
        return Attempt::Next(None);
    }

    tracing::warn!(
        target: targets::ROUTER,
        request_id,
        route = %route,
        node = %node.id,
        deployment = %candidate.deployment,
        upstream_status = status.as_u16(),
        error = code.as_deref().unwrap_or(""),
        "deployment refused before answering"
    );
    Attempt::Next(Some(Refusal {
        status,
        content_type,
        retry_after,
        body,
    }))
}

/// Read a refusal body, up to [`REFUSAL_LIMIT`].
async fn read_bounded(response: reqwest::Response) -> Bytes {
    let mut collected = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(Ok(chunk)) = stream.next().await {
        let room = REFUSAL_LIMIT.saturating_sub(collected.len());
        collected.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if collected.len() >= REFUSAL_LIMIT {
            break;
        }
    }
    Bytes::from(collected)
}

fn error_code(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value.get("error")?.get("code")?.as_str().map(str::to_owned)
}

fn refusal_response(refusal: Refusal) -> Response {
    let mut response = Response::new(Body::from(refusal.body));
    *response.status_mut() = refusal.status;
    let headers = response.headers_mut();
    if let Some(value) = refusal.content_type {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Some(value) = refusal.retry_after {
        headers.insert(header::RETRY_AFTER, value);
    }
    response
}

/// The committed upstream response, still carrying the node's body unread.
///
/// Response headers are copied by name for the same reason request headers
/// are: what a node says about its own connection is not the router's to
/// repeat.
fn into_response(upstream: reqwest::Response, status: StatusCode) -> Response {
    let mut headers = HeaderMap::new();
    for name in [
        header::CONTENT_TYPE,
        header::CACHE_CONTROL,
        header::RETRY_AFTER,
    ] {
        if let Some(value) = upstream.headers().get(&name) {
            headers.insert(name, value.clone());
        }
    }
    let body = Body::from_stream(upstream.bytes_stream());
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Put the route's name on a committed response.
///
/// A stream is relayed through the frame rewriter and keeps the active-request
/// guard until it ends. A whole body is read first — it has already been
/// generated by the time its head arrives — so a body that cannot be read is a
/// clean 502 rather than a 200 that breaks off, and it is rewritten when it is
/// a success. An error body is the node's own and is forwarded unchanged.
async fn commit(response: Response, route: &RouteName, active: InFlight) -> Response {
    let (mut parts, body) = response.into_parts();
    let is_stream = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));

    if is_stream && parts.status.is_success() {
        let stream = relay(
            body.into_data_stream(),
            FrameRewriter::new(route.as_str()),
            active,
        );
        return Response::from_parts(parts, Body::from_stream(stream));
    }

    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return upstream_unreadable();
    };
    let bytes = if parts.status.is_success() {
        rewrite_body(&bytes, route.as_str()).map_or(bytes, Bytes::from)
    } else {
        bytes
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(bytes))
}

/// Relay a committed stream, frame by frame.
fn relay<S, E>(
    upstream: S,
    rewriter: FrameRewriter,
    active: InFlight,
) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
{
    let upstream = Box::pin(upstream);
    futures_util::stream::unfold(Some((upstream, rewriter, active)), |state| async move {
        let (mut upstream, mut rewriter, active) = state?;
        loop {
            match upstream.next().await {
                Some(Ok(chunk)) => {
                    let out = rewriter.push(&chunk);
                    if rewriter.is_finished() {
                        return Some((Ok(Bytes::from(out)), None));
                    }
                    if !out.is_empty() {
                        return Some((Ok(Bytes::from(out)), Some((upstream, rewriter, active))));
                    }
                }
                Some(Err(_)) => {
                    tracing::warn!(
                        target: targets::ROUTER,
                        "the upstream stream failed after the response was committed"
                    );
                    let out = rewriter.abort();
                    drop(active);
                    return (!out.is_empty()).then(|| (Ok(Bytes::from(out)), None));
                }
                None => {
                    let out = rewriter.finish();
                    drop(active);
                    return (!out.is_empty()).then(|| (Ok(Bytes::from(out)), None));
                }
            }
        }
    })
}

/// Parse the request body far enough to route it.
///
/// The refusal is boxed, as the gateway boxes its error envelope: it is the
/// rare path, and the common one should not carry its size.
fn parse(body: &[u8]) -> Result<serde_json::Map<String, Value>, Box<Response>> {
    let value: Value = serde_json::from_slice(body).map_err(|err| {
        Box::new(json_error(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::invalid_request(
                format!("the request body is not valid JSON: {err}"),
                "invalid_json",
            ),
        ))
    })?;
    let Value::Object(object) = value else {
        return Err(Box::new(json_error(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::invalid_request(
                "the request body must be a JSON object",
                "invalid_request_body",
            ),
        )));
    };
    if object
        .get("model")
        .is_some_and(|model| !model.is_string() && !model.is_null())
    {
        return Err(Box::new(json_error(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::invalid_request(
                "`model` must be a string naming one of the models GET /v1/models lists",
                "invalid_request_body",
            )
            .with_param("model"),
        )));
    }
    Ok(object)
}

/// Log and count the deployments a request's requirements ruled out.
fn record_unfit(
    state: &RouterState,
    request_id: &str,
    route: &RouteName,
    unfit: &[(DeploymentId, Vec<CapabilityGap>)],
) {
    for (deployment, gaps) in unfit {
        tracing::debug!(
            target: targets::ROUTER,
            request_id,
            route = %route,
            deployment = %deployment,
            reasons = gaps.iter().map(|gap| gap.as_str()).collect::<Vec<_>>().join(","),
            "deployment cannot serve this request"
        );
        for gap in gaps {
            state
                .metrics
                .record_capability_filtered(route.as_str(), gap.as_str());
        }
    }
}

/// `tools_unsupported=1,context_too_small=2`: how many deployments each
/// requirement ruled out, for one log field.
fn filtered_counts(unfit: &[(DeploymentId, Vec<CapabilityGap>)]) -> String {
    let mut counts = std::collections::BTreeMap::<CapabilityGap, usize>::new();
    for gap in unfit.iter().flat_map(|(_, gaps)| gaps) {
        *counts.entry(*gap).or_default() += 1;
    }
    counts
        .iter()
        .map(|(gap, count)| format!("{}={count}", gap.as_str()))
        .collect::<Vec<_>>()
        .join(",")
}

const fn failure_code(failure: &RoutingFailure) -> &'static str {
    match failure {
        RoutingFailure::UnknownRoute { .. } => "model_not_found",
        RoutingFailure::NoDefaultRoute => "no_default_route",
        RoutingFailure::RouteUnavailable { .. } => "route_unavailable",
        RoutingFailure::CapabilityMismatch { .. } => "route_capability_mismatch",
    }
}

/// A committed upstream body that could not be read to its end.
fn upstream_unreadable() -> Response {
    json_error(
        StatusCode::BAD_GATEWAY,
        &server_error(
            "the upstream node's response could not be read",
            "upstream_failed",
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_request_id_is_kept_and_a_bad_one_replaced() {
        let mut headers = HeaderMap::new();
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("abc-123"));
        assert_eq!(request_id(&headers), "abc-123");

        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("has space"));
        assert!(request_id(&headers).starts_with("rtr-"));

        let long = "x".repeat(MAX_REQUEST_ID + 1);
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_str(&long).unwrap());
        assert!(request_id(&headers).starts_with("rtr-"));

        assert_ne!(request_id(&HeaderMap::new()), request_id(&HeaderMap::new()));
    }

    #[test]
    fn a_body_is_parsed_only_as_far_as_routing_needs() {
        let parsed = parse(br#"{"model":"Coder","messages":[],"anything":{"kept":true}}"#).unwrap();
        assert_eq!(parsed["anything"]["kept"], true);
        assert!(
            parse(br#"{"messages":[]}"#).is_ok(),
            "an omitted model routes to the default"
        );
        assert!(parse(b"[1,2]").is_err());
        assert!(parse(b"{nope").is_err());
        assert!(parse(br#"{"model":42}"#).is_err());
    }
}
