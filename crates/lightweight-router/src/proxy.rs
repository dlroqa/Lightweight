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
//!    those that can serve this request, then — if the request names a session
//!    whose last deployment is still among them — that deployment first, and
//!    otherwise the route's policy. No network call is made to decide.
//! 5. **Attempt** each candidate in turn, with `model` rewritten to that node's
//!    local name and the node's own credential. A failure *before the node
//!    answered* — refused connection, timeout, 502/503/504, or a node that no
//!    longer serves the model — moves on to the next candidate. So does a
//!    `400 context_length_exceeded`, but only to a candidate advertising a
//!    strictly larger context: the router's estimate is a lower bound, and the
//!    node's count is the one that decides.
//! 6. **Commit** on the first answer that is not one of those. From here the
//!    deployment is fixed: the response is returned with `model` rewritten to
//!    the route's name, and a stream is relayed frame by frame. If the node
//!    fails mid-stream the client is told so in-band; no other node is asked
//!    to continue an answer it did not start.
//!
//! A session settles on the deployment that commits a **successful** answer:
//! a first request establishes its affinity there, and a request whose sticky
//! deployment was ruled out or failed before answering moves it there.
//!
//! Every routed request is measured as it goes — planning time, each attempt's
//! time to a response head, time to first token, the whole duration, the
//! node's own prompt count against the router's estimate — and ends as one
//! [`RoutingTrace`]. None of it is read back by any routing decision.
//!
//! Cancellation needs no code of its own. A disconnecting client makes hyper
//! drop the response body; the body owns the upstream stream; dropping that
//! closes the connection to the node; and the node's gateway stops generating
//! when its client goes away. Each link is ownership, not a flag someone must
//! remember to check.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use futures_util::{Stream, StreamExt};
use lightweight_api::error::ErrorEnvelope;
use lightweight_observability::targets;
use serde_json::Value;

use crate::RouterState;
use crate::affinity::{AffinityKey, Established, Reassignment};
use crate::domain::{CapabilityGap, DeploymentId, Node, RouteName, RoutingFailure, RoutingReason};
use crate::error::{json_error, routing_failure, server_error};
use crate::health::{Outcome as Probe, describe_transport};
use crate::load::Lease;
use crate::metrics::{ActiveGuard, Outcome, UNKNOWN_ROUTE};
use crate::requirements;
use crate::select::{Candidate, Selection, Sticky};
use crate::sse::{FrameRewriter, rewrite_body_measuring};
use crate::trace::{
    AttemptTrace, ExcludedTrace, OverflowStep, OverflowTrace, RoutingTrace, SessionTrace,
};

/// The header a request is correlated by, from client to router to node.
pub use lightweight_gateway::request_id::REQUEST_ID_HEADER;

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
    lightweight_gateway::request_id::from_headers(headers).unwrap_or_else(generate_request_id)
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

/// Everything measured about one routed request, recorded exactly once.
///
/// Made as soon as the request has a route, and finished by whichever path
/// ends it: a refusal, a committed body read whole, a stream that ends — or,
/// through `Drop`, a client that went away at any point. That last one is why
/// it is a guard: a cancelled request is exactly the one a recording placed at
/// the end of a happy path would miss.
struct Tracker {
    state: Arc<RouterState>,
    trace: RoutingTrace,
    received: Instant,
    route: RouteName,
    policy: &'static str,
    finished: bool,
}

impl Tracker {
    fn new(
        state: &Arc<RouterState>,
        request_id: &str,
        route: &RouteName,
        policy: &'static str,
        endpoint: Endpoint,
        received: Instant,
    ) -> Self {
        Self {
            state: Arc::clone(state),
            trace: RoutingTrace::new(request_id, route.as_str(), endpoint.as_str(), policy),
            received,
            route: route.clone(),
            policy,
            finished: false,
        }
    }

    fn finish(&mut self, status: Option<u16>, outcome: &'static str) {
        if self.finished {
            return;
        }
        self.finished = true;
        let elapsed = self.received.elapsed();
        let metrics = &self.state.metrics;
        metrics.observe_request(self.route.as_str(), self.policy, elapsed);
        if let (Some(estimated), Some(actual)) = (
            self.trace.estimated_prompt_tokens,
            self.trace.actual_prompt_tokens,
        ) {
            metrics.observe_estimate(self.route.as_str(), estimated, actual);
        }
        let trace = &mut self.trace;
        trace.duration_ms = millis(elapsed);
        trace.status = status;
        trace.outcome = outcome;
        tracing::info!(
            target: targets::ROUTER,
            request_id = trace.request_id.as_str(),
            route = trace.route.as_str(),
            policy = trace.policy,
            stream = trace.stream,
            session = trace.session.as_ref().map(|s| s.fingerprint.as_str()),
            affinity = trace.session.as_ref().map(|s| s.affinity),
            deployment = trace.final_deployment.as_deref(),
            attempts = trace.attempts.len(),
            status,
            outcome,
            routing_ms = trace.routing_ms,
            ttft_ms = trace.ttft_ms,
            duration_ms = trace.duration_ms,
            estimated_prompt_tokens = trace.estimated_prompt_tokens,
            actual_prompt_tokens = trace.actual_prompt_tokens,
            "request finished"
        );
        self.state.traces.push(trace.clone());
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        self.finish(None, "cancelled");
    }
}

fn millis(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1000.0
}

/// What a committed response holds until its body is finished: the router's
/// active-request gauge, the deployment's in-flight slot, and the request's
/// tracker. Dropped when a stream ends or its client goes away, or once a
/// whole body has been read.
struct InFlight {
    _active: Option<ActiveGuard>,
    _lease: Lease,
    tracker: Tracker,
    /// When the committed attempt was sent, and to which deployment.
    sent: Instant,
    deployment: DeploymentId,
    status: u16,
    upstream_done: bool,
}

impl InFlight {
    /// Note what the relay has seen so far: the first generated output stops
    /// both time-to-first-token clocks, once; a usage chunk gives the node's
    /// prompt count.
    fn saw(&mut self, rewriter: &FrameRewriter) {
        if self.tracker.trace.ttft_ms.is_none() && rewriter.has_generated() {
            let ttft = self.tracker.received.elapsed();
            let metrics = &self.tracker.state.metrics;
            metrics.observe_ttft(self.tracker.route.as_str(), self.tracker.policy, ttft);
            metrics.observe_upstream_ttft(
                self.tracker.route.as_str(),
                self.deployment.as_str(),
                self.sent.elapsed(),
            );
            self.tracker.trace.ttft_ms = Some(millis(ttft));
        }
        if let Some(tokens) = rewriter.prompt_tokens() {
            self.tracker.trace.actual_prompt_tokens = Some(tokens);
        }
    }

    /// The upstream body is over, one way or another.
    fn upstream_finished(&mut self) {
        if self.upstream_done {
            return;
        }
        self.upstream_done = true;
        self.tracker.state.metrics.observe_upstream_duration(
            self.tracker.route.as_str(),
            self.deployment.as_str(),
            self.sent.elapsed(),
        );
    }

    fn end(mut self, outcome: &'static str) {
        self.upstream_finished();
        let status = self.status;
        self.tracker.finish(Some(status), outcome);
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        // Reached without `end` only when the client went away mid-body.
        self.upstream_finished();
        let status = self.status;
        self.tracker.finish(Some(status), "cancelled");
    }
}

/// What an attempt that did not commit tells the next step.
enum Attempt {
    Committed(Response),
    /// Try the next candidate. The node's refusal, if it sent one worth
    /// returning.
    Next(Option<Refusal>),
    /// The node refused the prompt as longer than its context
    /// (`400 context_length_exceeded`) before answering. Only a deployment with
    /// a larger context can do better; the refusal is the answer if none can.
    ContextOverflow(Refusal),
}

/// Artificial pauses at the two edges of the planning window, so a test can
/// prove where `routing_ms` starts and ends. Zero, and never configurable from
/// a file, outside tests: each costs one atomic load per request.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct PhaseDelays {
    /// Slept after the body is parsed and before planning starts.
    pub before_planning_ms: AtomicU64,
    /// Slept at the start of planning, inside the measured window.
    pub during_planning_ms: AtomicU64,
}

async fn pause(delay: &AtomicU64) {
    let ms = delay.load(Ordering::Relaxed);
    if ms > 0 {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

/// When one attempt was sent and what came back, for the trace.
struct Sent {
    at: Instant,
    /// From sending to the response head, when there was one.
    head: Option<Duration>,
    status: Option<u16>,
}

/// The policy label planning time is recorded under when no route was found.
const NO_POLICY: &str = "none";

/// The code a Lightweight node answers a prompt too long for its context with.
const CONTEXT_OVERFLOW: &str = "context_length_exceeded";

/// Forward one generation request.
pub async fn forward(
    state: Arc<RouterState>,
    endpoint: Endpoint,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    // The router has the whole request from here: every router-side duration
    // starts now.
    let received = Instant::now();
    let active = state.metrics.enter();
    let request_id = request_id(headers);
    let mut response = route_request(
        &state,
        endpoint,
        headers,
        body,
        &request_id,
        active,
        received,
    )
    .await;
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }
    response
}

#[allow(clippy::too_many_lines)]
async fn route_request(
    state: &Arc<RouterState>,
    endpoint: Endpoint,
    headers: &HeaderMap,
    body: &Bytes,
    request_id: &str,
    active: ActiveGuard,
    received: Instant,
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

    pause(&state.phase_delays.before_planning_ms).await;
    // `routing_ms` starts here, after the body is parsed, as it always has:
    // route resolution, requirements, the affinity lookup, eligibility, the
    // capability filter and the policy. TTFT and the request duration start
    // earlier, at `received`; they measure something else.
    let planning_started = Instant::now();
    pause(&state.phase_delays.during_planning_ms).await;

    let requested = request.get("model").and_then(Value::as_str);
    let route = match state.topology.resolve(requested) {
        Ok(route) => route,
        Err(failure) => {
            let routing = planning_started.elapsed();
            state
                .metrics
                .observe_planning(UNKNOWN_ROUTE, NO_POLICY, routing);
            tracing::info!(
                target: targets::ROUTER,
                request_id,
                requested = requested.unwrap_or(""),
                error = failure_code(&failure),
                routing_ms = millis(routing),
                "request not routed"
            );
            state
                .metrics
                .record_request(UNKNOWN_ROUTE, Outcome::ClientError);
            return routing_failure(&failure, state.policy.interval);
        }
    };
    let policy = route.policy.as_str();
    let mut tracker = Tracker::new(state, request_id, &route.name, policy, endpoint, received);
    tracker.trace.stream = request.get("stream") == Some(&Value::Bool(true));
    tracker.trace.deployments = route.deployments.len();

    // What the request needs, read once, before any deployment is looked at.
    let needs = match requirements::extract(endpoint, body) {
        Ok(needs) => needs,
        Err(refusal) => {
            let routing = planning_started.elapsed();
            tracker.trace.routing_ms = millis(routing);
            state
                .metrics
                .observe_planning(route.name.as_str(), policy, routing);
            tracing::info!(
                target: targets::ROUTER,
                request_id,
                route = %route.name,
                endpoint = endpoint.as_str(),
                upstream_status = refusal.status().as_u16(),
                routing_ms = tracker.trace.routing_ms,
                "request refused before routing"
            );
            state
                .metrics
                .record_request(route.name.as_str(), Outcome::ClientError);
            tracker.finish(
                Some(refusal.status().as_u16()),
                Outcome::ClientError.as_str(),
            );
            return *refusal;
        }
    };
    tracker.trace.estimated_prompt_tokens = needs.prompt_tokens;

    // The session, if the client named one and affinity is on, and the
    // deployment it last succeeded on, if that has not expired. Only ever a
    // preference: the selector checks it against health and this request's
    // requirements before it may go first.
    let session = state.affinity.session(&route.name, headers);
    let sticky = session.as_ref().and_then(|key| state.affinity.lookup(key));
    if let Some(key) = &session {
        tracker.trace.session = Some(SessionTrace {
            fingerprint: key.fingerprint(),
            affinity: "none",
            sticky: sticky.as_ref().map(ToString::to_string),
            reassignment: None,
        });
    }

    // Eligibility, then what this request needs, then the session's
    // preference, then the route's policy, then a slot reserved on the first
    // choice - all in the selector. Nothing below this line knows which policy
    // the route uses or what was filtered; it only walks the order it was
    // handed.
    //
    // The observations are read once, so the contexts failover compares below
    // are the ones the plan was made from.
    let observed = state.health.deployment_snapshot();
    let mut plan = match state.selector.plan_with_affinity(
        &state.topology,
        route,
        &state.health.snapshot(),
        &observed,
        &needs,
        sticky.as_ref(),
    ) {
        Ok(plan) => plan,
        Err(failure) => {
            let routing = planning_started.elapsed();
            tracker.trace.routing_ms = millis(routing);
            state
                .metrics
                .observe_planning(route.name.as_str(), policy, routing);
            if let RoutingFailure::CapabilityMismatch { unfit, unmet, .. } = &failure {
                record_unfit(state, request_id, &route.name, unfit);
                tracker.trace.available = unfit.len();
                tracker.trace.unfit = excluded_unfit(unfit);
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
                let response = routing_failure(&failure, state.policy.interval);
                tracker.finish(
                    Some(response.status().as_u16()),
                    Outcome::ClientError.as_str(),
                );
                return response;
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
            let response = routing_failure(&failure, state.policy.interval);
            tracker.finish(
                Some(response.status().as_u16()),
                Outcome::Unavailable.as_str(),
            );
            return response;
        }
    };
    record_unfit(state, request_id, &route.name, &plan.unfit);
    let routing = planning_started.elapsed();
    let routing_ms = millis(routing);
    state
        .metrics
        .observe_planning(route.name.as_str(), policy, routing);
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
    {
        let trace = &mut tracker.trace;
        trace.routing_ms = routing_ms;
        trace.available = eligible_before;
        trace.capable = eligible_after;
        trace.unavailable = plan
            .skipped
            .iter()
            .map(|(deployment, reason)| ExcludedTrace {
                deployment: deployment.to_string(),
                reasons: vec![reason.as_str()],
            })
            .collect();
        trace.unfit = excluded_unfit(&plan.unfit);
        if let Some(first) = plan.decision(0) {
            trace.selected = Some(first.deployment.to_string());
            trace.selection_reason = Some(first.reason.as_str());
        }
    }

    // Affinity as planned: a hit, or a miss (no affinity, or one the filters
    // ruled out). Reassignment is decided only when something commits.
    if session.is_some() {
        let hit = plan.sticky == Some(Sticky::Hit);
        if hit {
            state.metrics.record_affinity_hit(route.name.as_str());
        } else {
            state.metrics.record_affinity_miss(route.name.as_str());
        }
        if let Some(session) = tracker.trace.session.as_mut() {
            session.affinity = if hit { "hit" } else { "miss" };
        }
    }

    let (cursor, selected_index, active_before, concurrency_limit) = match plan.selection {
        Selection::Priority | Selection::SessionAffinity => (None, None, None, None),
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
    // After a `context_length_exceeded`, the largest context known to be too
    // small for this prompt. Only a candidate advertising more is worth trying.
    let mut too_small: Option<u32> = None;
    // Whether the attempt about to be made follows a context overflow.
    let mut after_overflow = false;
    // Why a sticky deployment that was tried first did not answer.
    let mut sticky_failed: Option<Reassignment> = None;
    let mut attempts_made = 0;
    let context_of = |candidate: &Candidate| {
        observed
            .get(&candidate.deployment)
            .map(|seen| seen.context_length)
    };
    let larger_than = |candidate: &Candidate, floor: Option<u32>| match floor {
        None => true,
        Some(floor) => context_of(candidate).is_some_and(|context| context > floor),
    };
    for (attempt, candidate) in plan.candidates.iter().enumerate() {
        if !larger_than(candidate, too_small) {
            tracing::debug!(
                target: targets::ROUTER,
                request_id,
                route = %route.name,
                deployment = %candidate.deployment,
                context = context_of(candidate),
                "not tried: its context is no larger than one this prompt overflowed"
            );
            continue;
        }
        let Some(mut decision) = plan.decision(attempt) else {
            break;
        };
        if after_overflow {
            decision.reason = RoutingReason::ContextOverflowFailover;
        }
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
        attempts_made += 1;
        let rest = &plan.candidates[attempt + 1..];
        let is_sticky = attempt == 0 && plan.sticky == Some(Sticky::Hit);
        let (outcome, sent) = attempt_one(&context, &request).await;
        let mut record = |outcome: &'static str| {
            tracker.trace.attempts.push(AttemptTrace {
                deployment: candidate.deployment.to_string(),
                reason: decision.reason.as_str(),
                outcome,
                upstream_status: sent.status,
                response_ms: sent.head.map(millis),
            });
        };
        match outcome {
            Attempt::Next(refusal) => {
                record("failed");
                drop(lease);
                after_overflow = false;
                if is_sticky {
                    sticky_failed = Some(Reassignment::StickyFailed);
                }
                if refusal.is_some() {
                    last_refusal = refusal;
                }
                if rest.iter().any(|next| larger_than(next, too_small)) {
                    state.metrics.record_failover(route.name.as_str());
                }
            }
            Attempt::ContextOverflow(refusal) => {
                record("context_overflow");
                drop(lease);
                if is_sticky {
                    sticky_failed = Some(Reassignment::StickyContextOverflow);
                }
                let failed_context = context_of(candidate);
                tracker
                    .trace
                    .context_overflow
                    .get_or_insert_with(|| OverflowTrace {
                        estimated_prompt_tokens: needs.prompt_tokens,
                        ..OverflowTrace::default()
                    })
                    .too_small
                    .push(OverflowStep {
                        deployment: candidate.deployment.to_string(),
                        context: failed_context,
                    });
                // A deployment whose context the router never saw cannot be
                // compared with, so nothing is known to be larger than it.
                too_small = Some(
                    failed_context
                        .unwrap_or(u32::MAX)
                        .max(too_small.unwrap_or(0)),
                );
                last_refusal = Some(refusal);
                let next = rest.iter().find(|next| larger_than(next, too_small));
                tracing::warn!(
                    target: targets::ROUTER,
                    request_id,
                    route = %route.name,
                    deployment = %candidate.deployment,
                    context = failed_context,
                    next_deployment = next.map(|next| next.deployment.to_string()),
                    next_context = next.and_then(&context_of),
                    estimated_prompt_tokens = needs.prompt_tokens,
                    "deployment refused the prompt as longer than its context"
                );
                if next.is_none() {
                    // The node's own answer stands: the route is available, and
                    // the router could not have known this before sending.
                    break;
                }
                after_overflow = true;
                state.metrics.record_failover(route.name.as_str());
                state
                    .metrics
                    .record_context_overflow_failover(route.name.as_str());
            }
            Attempt::Committed(response) => {
                record("committed");
                let status = response.status();
                if after_overflow && let Some(overflow) = tracker.trace.context_overflow.as_mut() {
                    overflow.answered_context = context_of(candidate);
                }
                // A session settles where a request succeeded, and only there:
                // a node's refusal, though committed, says nothing about where
                // the next turn should go.
                if status.is_success()
                    && let Some(key) = session.clone()
                {
                    settle_affinity(
                        state,
                        key,
                        plan.sticky,
                        sticky_failed,
                        &candidate.deployment,
                        &mut tracker.trace,
                    );
                }
                tracker.trace.final_deployment = Some(candidate.deployment.to_string());
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
                    session = tracker.trace.session.as_ref().map(|s| s.fingerprint.as_str()),
                    affinity = tracker.trace.session.as_ref().map(|s| s.affinity),
                    reassignment = tracker.trace.session.as_ref().and_then(|s| s.reassignment),
                    routing_ms,
                    upstream_status = status.as_u16(),
                    upstream_response_ms = sent.head.map(millis),
                    failover_count = attempts_made - 1,
                    "routed"
                );
                state
                    .metrics
                    .record_request(route.name.as_str(), Outcome::of_status(status.as_u16()));
                state.metrics.record_decision(
                    route.name.as_str(),
                    plan.policy.as_str(),
                    decision.reason.as_str(),
                );
                let held = InFlight {
                    _active: active.take(),
                    _lease: lease,
                    tracker,
                    sent: sent.at,
                    deployment: candidate.deployment.clone(),
                    status: status.as_u16(),
                    upstream_done: false,
                };
                return commit(response, &route.name, held).await;
            }
        }
    }

    let tried = attempts_made;
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
            let outcome = Outcome::of_status(refusal.status.as_u16());
            state.metrics.record_request(route.name.as_str(), outcome);
            tracker.finish(Some(refusal.status.as_u16()), outcome.as_str());
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
            let response = routing_failure(
                &RoutingFailure::RouteUnavailable {
                    route: route.name.clone(),
                },
                state.policy.interval,
            );
            tracker.finish(
                Some(response.status().as_u16()),
                Outcome::Unavailable.as_str(),
            );
            response
        }
    }
}

/// Record where a session's request succeeded.
///
/// * A hit that answered keeps the session where it is.
/// * A session with no affinity establishes one — unless a concurrent request
///   for the same session committed first, whose choice then stands.
/// * A sticky deployment that was ruled out, or tried and failed, is replaced
///   by the one that answered, and the reason is counted.
fn settle_affinity(
    state: &RouterState,
    key: AffinityKey,
    planned: Option<Sticky>,
    sticky_failed: Option<Reassignment>,
    answered: &DeploymentId,
    trace: &mut RoutingTrace,
) {
    let reassigned = match (planned, sticky_failed) {
        (Some(Sticky::Hit), Some(reason)) | (Some(Sticky::Broken(reason)), _) => Some(reason),
        _ => None,
    };
    match reassigned {
        Some(reason) => {
            state.affinity.reassign(key.clone(), answered);
            state
                .metrics
                .record_affinity_reassignment(key.route(), reason.as_str());
            if let Some(session) = trace.session.as_mut() {
                session.affinity = "reassigned";
                session.reassignment = Some(reason.as_str());
            }
        }
        None => {
            if let Established::KeptExisting(existing) = state.affinity.establish(key, answered) {
                tracing::debug!(
                    target: targets::ROUTER,
                    request_id = trace.request_id.as_str(),
                    deployment = %answered,
                    kept = %existing,
                    "a concurrent request established this session first"
                );
            }
        }
    }
}

fn excluded_unfit(unfit: &[(DeploymentId, Vec<CapabilityGap>)]) -> Vec<ExcludedTrace> {
    unfit
        .iter()
        .map(|(deployment, gaps)| ExcludedTrace {
            deployment: deployment.to_string(),
            reasons: gaps.iter().map(|gap| gap.as_str()).collect(),
        })
        .collect()
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
) -> (Attempt, Sent) {
    let AttemptContext {
        state,
        endpoint,
        headers,
        request_id,
        route,
        node,
        candidate,
    } = context;

    let mut sent = Sent {
        at: Instant::now(),
        head: None,
        status: None,
    };
    let Ok(payload) = serde_json::to_vec(request) else {
        return (Attempt::Next(None), sent);
    };
    // The same id on every attempt: failover is one logical request, and a
    // node's log must be findable from the client's id whichever node it was.
    let mut upstream = state
        .client
        .post(node.endpoint(endpoint.path()))
        .header(header::CONTENT_TYPE, "application/json")
        .header(REQUEST_ID_HEADER, *request_id)
        .body(payload);
    // Forwarded by name, never wholesale: the client's `Authorization` is the
    // router's credential and must not reach a node, and nothing else a client
    // sends is the node's business — the session header included: affinity is
    // the router's concern, not the node's.
    if let Some(accept) = headers.get(header::ACCEPT) {
        upstream = upstream.header(header::ACCEPT, accept.clone());
    }
    if let Some(value) = node.auth.header_value() {
        upstream = upstream.header(header::AUTHORIZATION, value);
    }

    sent.at = Instant::now();
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
            return (Attempt::Next(None), sent);
        }
    };
    let head = sent.at.elapsed();
    sent.head = Some(head);
    sent.status = Some(response.status().as_u16());
    state
        .metrics
        .observe_upstream_response(route.as_str(), candidate.deployment.as_str(), head);

    let status = response.status();
    if status == StatusCode::BAD_REQUEST {
        return (bad_request(response).await, sent);
    }
    if !matches!(
        status,
        StatusCode::NOT_FOUND
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    ) {
        return (Attempt::Committed(into_response(response, status)), sent);
    }

    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let retry_after = response.headers().get(header::RETRY_AFTER).cloned();
    let body = read_bounded(response).await;
    let code = error_code(&body);

    if status == StatusCode::NOT_FOUND {
        if code.as_deref() != Some("model_not_found") {
            // A 404 that is not about the model is the node's answer to this
            // request, and stands.
            let refusal = refusal_response(Refusal {
                status,
                content_type,
                retry_after,
                body,
            });
            return (Attempt::Committed(refusal), sent);
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
        return (Attempt::Next(None), sent);
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
    (
        Attempt::Next(Some(Refusal {
            status,
            content_type,
            retry_after,
            body,
        })),
        sent,
    )
}

/// A node's `400`.
///
/// A `400` is the node's answer to this request and stands, with one
/// exception: `context_length_exceeded`, recognised by its structured
/// `error.code` and nothing looser. It means the prompt, counted by the node's
/// own tokenizer, is longer than that deployment's context — which the
/// router's lower-bound estimate could not rule out — and nothing was
/// generated. The caller decides whether a larger deployment is left to try.
///
/// The body is read whole either way. An error body is short, and the commit
/// path reads a non-success body whole too; it is passed on byte for byte.
async fn bad_request(response: reqwest::Response) -> Attempt {
    let status = response.status();
    let mut headers = HeaderMap::new();
    for name in [
        header::CONTENT_TYPE,
        header::CACHE_CONTROL,
        header::RETRY_AFTER,
    ] {
        if let Some(value) = response.headers().get(&name) {
            headers.insert(name, value.clone());
        }
    }
    let Ok(body) = response.bytes().await else {
        return Attempt::Committed(upstream_unreadable());
    };
    if error_code(&body).as_deref() == Some(CONTEXT_OVERFLOW) {
        return Attempt::ContextOverflow(Refusal {
            status,
            content_type: headers.get(header::CONTENT_TYPE).cloned(),
            retry_after: headers.get(header::RETRY_AFTER).cloned(),
            body,
        });
    }
    let mut committed = Response::new(Body::from(body));
    *committed.status_mut() = status;
    *committed.headers_mut() = headers;
    Attempt::Committed(committed)
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
async fn commit(response: Response, route: &RouteName, mut held: InFlight) -> Response {
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
            held,
        );
        return Response::from_parts(parts, Body::from_stream(stream));
    }

    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        held.status = StatusCode::BAD_GATEWAY.as_u16();
        held.end(Outcome::ServerError.as_str());
        return upstream_unreadable();
    };
    let bytes = if parts.status.is_success() {
        let (rewritten, prompt_tokens) = rewrite_body_measuring(&bytes, route.as_str());
        held.tracker.trace.actual_prompt_tokens = prompt_tokens;
        rewritten.map_or(bytes, Bytes::from)
    } else {
        bytes
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    held.end(Outcome::of_status(parts.status.as_u16()).as_str());
    Response::from_parts(parts, Body::from(bytes))
}

/// Relay a committed stream, frame by frame.
///
/// The request's measurements ride along: the first frame carrying generated
/// output stops the time-to-first-token clocks, a usage chunk gives the node's
/// prompt count, and the stream's end — complete, broken off by the node, or
/// abandoned by the client — finishes the trace.
fn relay<S, E>(
    upstream: S,
    rewriter: FrameRewriter,
    held: InFlight,
) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
{
    let upstream = Box::pin(upstream);
    futures_util::stream::unfold(Some((upstream, rewriter, held)), |state| async move {
        let (mut upstream, mut rewriter, mut held) = state?;
        loop {
            match upstream.next().await {
                Some(Ok(chunk)) => {
                    let out = rewriter.push(&chunk);
                    held.saw(&rewriter);
                    if rewriter.is_finished() {
                        held.end("interrupted");
                        return Some((Ok(Bytes::from(out)), None));
                    }
                    if !out.is_empty() {
                        return Some((Ok(Bytes::from(out)), Some((upstream, rewriter, held))));
                    }
                }
                Some(Err(_)) => {
                    tracing::warn!(
                        target: targets::ROUTER,
                        request_id = held.tracker.trace.request_id.as_str(),
                        deployment = %held.deployment,
                        "the upstream stream failed after the response was committed"
                    );
                    let out = rewriter.abort();
                    held.end("interrupted");
                    return (!out.is_empty()).then(|| (Ok(Bytes::from(out)), None));
                }
                None => {
                    let out = rewriter.finish();
                    held.saw(&rewriter);
                    let outcome = if rewriter.completed() {
                        Outcome::Ok.as_str()
                    } else {
                        "interrupted"
                    };
                    held.end(outcome);
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
    use lightweight_gateway::request_id::MAX_REQUEST_ID;

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
