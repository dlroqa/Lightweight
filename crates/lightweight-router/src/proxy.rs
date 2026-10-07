//! Forwarding one inference request to the deployment that should answer it.
//!
//! The order of a request:
//!
//! 1. **Parse** the body just far enough to read `model`. Every other field is
//!    forwarded as the client sent it — the node, not the router, decides what
//!    `tools`, `max_tokens` or `reasoning_effort` mean.
//! 2. **Resolve** `model` to a route, through the same `default` rules the
//!    gateway applies. `Auto`, when it is configured, is resolved by its rules
//!    instead ([`crate::auto_route`]): the request's requirements are read
//!    first and the first matching rule names the route. From here on an
//!    `Auto` request is exactly a request for that route — its affinity, its
//!    policy, its failover and its name on the response.
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
use std::future::Future;
use std::pin::Pin;
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
use crate::auto_route::{AUTO_ROUTE, AutoRoute};
use crate::budget::{self, BudgetTrace, ExhaustedMarker, RequestBudget, Stage};
use crate::classifier::{Classification, ClassificationInput, ClassifierOutcome, RouteClassifier};
use crate::domain::{
    CapabilityGap, DeploymentId, Node, Route, RouteName, RoutingFailure, RoutingReason,
};
use crate::error::{json_error, routing_failure, server_error};
use crate::fallback::{FallbackAttemptTrace, FallbackReason, FallbackTrace};
use crate::health::{Outcome as Probe, describe_transport};
use crate::load::Lease;
use crate::metrics::{ActiveGuard, Outcome, UNKNOWN_ROUTE};
use crate::requirements::{self, RequestRequirements};
use crate::scoring::{Observation, ScoringTrace};
use crate::select::{Candidate, Selection, Sticky};
use crate::sse::{FrameRewriter, rewrite_body_measuring};
use crate::trace::{
    AttemptTrace, ClassifierTrace, ExcludedTrace, OverflowStep, OverflowTrace, RoutingTrace,
    SessionTrace,
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
    /// The router's own classification request: never counted toward route
    /// history.
    nested: bool,
    /// What the request counts as toward route history when its trace
    /// outcome does not say: a capability mismatch, or every deployment
    /// refusing before answering.
    observation: Option<Observation>,
    /// The client request's pre-commit budget (R9.3.2), when one is
    /// configured: the same value at every stage, never re-made.
    budget: Option<RequestBudget>,
    /// The budget refused this route before anything was sent to it, so it
    /// was never attempted and route history observes nothing.
    unobserved: bool,
}

impl Tracker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        state: &Arc<RouterState>,
        request_id: &str,
        route: &RouteName,
        policy: &'static str,
        endpoint: Endpoint,
        received: Instant,
        nested: bool,
        budget: Option<RequestBudget>,
    ) -> Self {
        Self {
            state: Arc::clone(state),
            trace: RoutingTrace::new(request_id, route.as_str(), endpoint.as_str(), policy),
            received,
            route: route.clone(),
            policy,
            finished: false,
            nested,
            observation: None,
            budget,
            unobserved: false,
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
        // Route history (R9.2) learns from the final outcome, here: a stream
        // counts once it has ended, not when its head arrived. (A route an
        // `Auto` request fell back from is observed when it is left.) A route
        // the budget refused was never attempted, and is not observed.
        if !self.unobserved {
            observe_route(
                &self.state,
                &self.route,
                self.observation
                    .unwrap_or_else(|| Observation::of_outcome(outcome)),
                self.nested,
            );
        }
        let trace = &mut self.trace;
        trace.duration_ms = millis(elapsed);
        trace.status = status;
        trace.outcome = outcome;
        // Ended without a commit, for whatever reason of its own: the budget's
        // state at that moment. (A commit or an exhaustion wrote its own.)
        if let Some(budget) = &self.budget
            && trace.request_budget.is_none()
        {
            trace.request_budget = Some(BudgetTrace::ended(budget));
        }
        tracing::info!(
            target: targets::ROUTER,
            request_id = trace.request_id.as_str(),
            route = trace.route.as_str(),
            requested_route = trace.requested_route.as_str(),
            auto_rule = trace.auto_rule.as_deref(),
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
            request_budget_stage = trace.request_budget.as_ref().and_then(|budget| budget.stage),
            "request finished"
        );
        self.state.traces.push(trace.clone());
    }
}

impl Tracker {
    /// Point the request at the next route of its fallback plan: what the
    /// trace says about one route — its deployments, plan, session, selection
    /// — starts over; what it says about the request — its id, timings so
    /// far, classification, scoring, every deployment attempt — is kept.
    fn retarget(&mut self, route: &Route) {
        self.route = route.name.clone();
        self.policy = route.policy.as_str();
        self.observation = None;
        self.unobserved = false;
        let trace = &mut self.trace;
        trace.route = route.name.to_string();
        trace.policy = self.policy;
        trace.session = None;
        trace.deployments = route.deployments.len();
        trace.available = 0;
        trace.capable = 0;
        trace.unavailable.clear();
        trace.unfit.clear();
        trace.selected = None;
        trace.selection_reason = None;
        trace.final_deployment = None;
        trace.context_overflow = None;
        trace.routing_ms = 0.0;
    }
}

impl Tracker {
    /// The budget refused `route`'s first attempt: count the request under
    /// `label` — `Auto`, or the route the client named — never under a route
    /// nothing was sent to, and observe no route at all.
    fn unattempted(&mut self, label: RouteName) {
        self.trace.route = label.to_string();
        self.route = label;
        self.unobserved = true;
    }
}

/// Record one route's observation in route history (R9.2: observational only)
/// — never for the router's own classification requests.
fn observe_route(state: &RouterState, route: &RouteName, observation: Observation, nested: bool) {
    if !nested
        && state
            .route_history
            .observe(route, observation, Instant::now())
    {
        state
            .metrics
            .record_route_history(route.as_str(), observation.as_str());
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
    /// Slept after each deployment attempt that did not commit, before the
    /// next step's budget check: lets a test place "the attempt failed just
    /// as the budget ran out" exactly.
    pub after_attempt_ms: AtomicU64,
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
    forward_as(state, endpoint, headers, body, false, None).await
}

/// Forward a request the router itself makes — a classification — through
/// the same pipeline as a client's.
///
/// `nested` is what makes classification non-recursive: a nested request that
/// reaches a classifying `Auto` rule takes the rule's fallback rather than
/// classifying again. Boxed because it is called from inside the pipeline it
/// runs.
///
/// It inherits the client request's pre-commit budget, `budget`, and never
/// starts one of its own: one client request has one deadline.
pub(crate) fn forward_nested(
    state: Arc<RouterState>,
    endpoint: Endpoint,
    headers: HeaderMap,
    body: Bytes,
    budget: Option<RequestBudget>,
) -> Pin<Box<dyn Future<Output = Response> + Send>> {
    Box::pin(async move { forward_as(state, endpoint, &headers, &body, true, budget).await })
}

async fn forward_as(
    state: Arc<RouterState>,
    endpoint: Endpoint,
    headers: &HeaderMap,
    body: &Bytes,
    nested: bool,
    inherited: Option<RequestBudget>,
) -> Response {
    // The router has the whole request from here: every router-side duration
    // starts now.
    let received = Instant::now();
    // R9.3.2: the client request's one pre-commit deadline, from the same
    // moment — after the body was read, before anything else. Made here and
    // nowhere else; a nested classification request runs on its parent's.
    let budget = if nested {
        inherited
    } else {
        state.request_budget.map(RequestBudget::start)
    };
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
        nested,
        budget,
    )
    .await;
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }
    response
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn route_request(
    state: &Arc<RouterState>,
    endpoint: Endpoint,
    headers: &HeaderMap,
    body: &Bytes,
    request_id: &str,
    active: ActiveGuard,
    received: Instant,
    nested: bool,
    budget: Option<RequestBudget>,
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
    let mut planning_started = Instant::now();
    pause(&state.phase_delays.during_planning_ms).await;

    let requested = request.get("model").and_then(Value::as_str);
    // `Auto` needs the request's requirements to choose a route, so for it
    // they are read here, once, and reused below. Everything else resolves by
    // name, as it always has.
    let mut early_needs = None;
    // The rule that chose the route — `Some(None)` for the fallback — when
    // the client asked for `Auto`.
    let mut auto_rule: Option<Option<String>> = None;
    let mut classification = None;
    let mut scoring = None;
    let resolution = match state.auto.as_ref().filter(|auto| auto.claims(requested)) {
        Some(auto) => {
            match resolve_auto(
                state,
                auto,
                endpoint,
                body,
                request_id,
                planning_started,
                nested,
                budget,
            )
            .await
            {
                Ok(resolved) => {
                    // The classifier's time is its own, measured on its own:
                    // `routing_ms` stays the router's planning time, as in R6.
                    if let Some(classification) = &resolved.classification {
                        planning_started += classification.duration;
                    }
                    early_needs = Some(resolved.needs);
                    auto_rule = Some(resolved.rule);
                    classification = resolved.classification;
                    scoring = resolved.scoring;
                    resolved.route
                }
                Err(AutoStop::Refused(refusal)) => return *refusal,
                Err(AutoStop::BudgetExhausted {
                    classification,
                    rule,
                }) => {
                    // No route was attempted, and none is invented: the
                    // request is counted and traced under `Auto`.
                    let mut tracker = Tracker::new(
                        state,
                        request_id,
                        &auto_route_name(),
                        NO_POLICY,
                        endpoint,
                        received,
                        nested,
                        budget,
                    );
                    tracker.unobserved = true;
                    tracker.trace.requested_route = AUTO_ROUTE.to_owned();
                    tracker.trace.auto_rule = rule;
                    tracker.trace.stream = request.get("stream") == Some(&Value::Bool(true));
                    if let Some(classifier) = auto.classifier.as_ref() {
                        tracker.trace.classifier =
                            Some(classifier_trace(*classification, classifier));
                    }
                    return exhaust(state, tracker, Stage::Classifier, None);
                }
            }
        }
        None => state.topology.resolve(requested),
    };
    let route = match resolution {
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
    let mut tracker = Tracker::new(
        state,
        request_id,
        &route.name,
        policy,
        endpoint,
        received,
        nested,
        budget,
    );
    // R9.3.1: the routes an `Auto` request may move to if its initial route
    // cannot execute — read once, here, from the initial route's own list,
    // and frozen for the request. A fallback route's own list is never read.
    // Only for a client that asked for `Auto`: a named route is that route or
    // its error. Never for the router's own classification requests.
    let fallback_plan: Vec<RouteName> = match (&auto_rule, state.auto.as_ref()) {
        (Some(_), Some(auto)) if !nested => auto.cross_route_fallback.chain(&route.name).to_vec(),
        _ => Vec::new(),
    };
    let is_auto = auto_rule.is_some();
    if let Some(rule) = auto_rule {
        tracker.trace.requested_route = AUTO_ROUTE.to_owned();
        tracker.trace.auto_fallback = rule.is_none();
        tracker.trace.auto_rule = rule;
    }
    if let (Some(classification), Some(classifier)) = (
        classification,
        state
            .auto
            .as_ref()
            .and_then(|auto| auto.classifier.as_ref()),
    ) {
        tracker.trace.classifier = Some(classifier_trace(classification, classifier));
    }
    tracker.trace.scoring = scoring;
    tracker.trace.stream = request.get("stream") == Some(&Value::Bool(true));
    tracker.trace.deployments = route.deployments.len();

    // What the request needs, read once, before any deployment is looked at.
    let extracted = match early_needs {
        Some(needs) => Ok(needs),
        None => requirements::extract(endpoint, body),
    };
    let needs = match extracted {
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

    // R9.3.2: the start check before the initial route's first attempt. If
    // the budget is already spent, nothing is sent and no route is counted
    // as attempted: the request is `Auto`'s or the named route's.
    if budget::expired(budget) {
        let label = if is_auto {
            auto_route_name()
        } else {
            route.name.clone()
        };
        tracker.unattempted(label);
        return exhaust(state, tracker, Stage::RoutePlanning, Some(&route.name));
    }

    // One route attempt at a time: the route's own pipeline, start to finish,
    // same-route failover included. Only an uncommitted failure for one of
    // the three fallback reasons may move the request to the next route of
    // the frozen plan; anything committed, and anything else, ends it here.
    let initial = route;
    let mut route = route;
    let mut active = Some(active);
    let mut remaining = fallback_plan.iter();
    let mut hops: Vec<FallbackAttemptTrace> = Vec::new();
    loop {
        let on_fallback = !hops.is_empty();
        let failure = match attempt_route(
            state,
            endpoint,
            headers,
            &mut request,
            request_id,
            route,
            &needs,
            tracker,
            active,
            planning_started,
            budget,
        )
        .await
        {
            RouteEnd::Committed(response) => return response,
            RouteEnd::Failed(failure) => *failure,
            RouteEnd::BudgetExhausted(stop) => {
                let BudgetStop {
                    mut tracker,
                    before_first_attempt,
                } = *stop;
                let stage = if on_fallback {
                    // The fallback route was entered: its transition counted.
                    // It is the terminal route; the chain did not run out.
                    if let Some(block) = tracker.trace.cross_route_fallback.as_mut()
                        && let Some(last) = block.attempts.last_mut()
                    {
                        last.outcome = "failed";
                        last.reason = Some(budget::EXHAUSTED);
                    }
                    if before_first_attempt {
                        tracker.unobserved = true;
                    }
                    Stage::CrossRouteFallback
                } else if before_first_attempt {
                    let label = if is_auto {
                        auto_route_name()
                    } else {
                        route.name.clone()
                    };
                    tracker.unattempted(label);
                    Stage::RoutePlanning
                } else {
                    Stage::SameRouteAttempt
                };
                let next = (before_first_attempt && !on_fallback).then_some(&route.name);
                return exhaust(state, tracker, stage, next);
            }
        };
        let next = failure
            .reason
            .and_then(|_| remaining.next())
            .and_then(|name| state.topology.route(name));
        let (Some(reason), Some(next)) = (failure.reason, next) else {
            return conclude_chain(state, request_id, initial, hops, failure);
        };

        // R9.3.2: the start check before a fallback route, before its
        // transition is counted. The failed route's own failure completed and
        // keeps what it earned; the next route is never attempted.
        if budget::expired(budget) {
            let RouteFailure {
                outcome,
                observation,
                route: failed,
                mut tracker,
                ..
            } = failure;
            tracker.observation =
                Some(observation.unwrap_or_else(|| Observation::of_outcome(outcome.as_str())));
            hops.push(FallbackAttemptTrace {
                route: failed.to_string(),
                outcome: "failed",
                reason: Some(reason.as_str()),
            });
            tracker.trace.cross_route_fallback = Some(FallbackTrace {
                initial_route: initial.name.to_string(),
                final_route: failed.to_string(),
                exhausted: false,
                attempts: hops,
            });
            return exhaust(state, tracker, Stage::CrossRouteFallback, Some(&next.name));
        }

        // The failed route's own observation, as a final request's would be
        // recorded; the request itself is counted once, at the end.
        observe_route(
            state,
            &failure.route,
            failure
                .observation
                .unwrap_or_else(|| Observation::of_outcome(failure.outcome.as_str())),
            nested,
        );
        state.metrics.record_cross_route_fallback(
            failure.route.as_str(),
            next.name.as_str(),
            reason.as_str(),
        );
        tracing::info!(
            target: targets::ROUTER,
            request_id,
            initial_route = %initial.name,
            from_route = %failure.route,
            to_route = %next.name,
            reason = reason.as_str(),
            attempt = hops.len() + 2,
            "cross-route fallback"
        );
        hops.push(FallbackAttemptTrace {
            route: failure.route.to_string(),
            outcome: "failed",
            reason: Some(reason.as_str()),
        });
        tracker = failure.tracker;
        active = failure.active;
        tracker.retarget(next);
        // Provisional: correct if this route commits, replaced if it fails.
        let mut attempts = hops.clone();
        attempts.push(FallbackAttemptTrace {
            route: next.name.to_string(),
            outcome: "committed",
            reason: None,
        });
        tracker.trace.cross_route_fallback = Some(FallbackTrace {
            initial_route: initial.name.to_string(),
            final_route: next.name.to_string(),
            exhausted: false,
            attempts,
        });
        route = next;
        planning_started = Instant::now();
    }
}

/// Answer an `Auto` request whose last route attempt committed nothing, with
/// that route's own error. If the request had moved to fallback routes, the
/// trace and metrics say how the chain ended.
fn conclude_chain(
    state: &Arc<RouterState>,
    request_id: &str,
    initial: &Route,
    mut hops: Vec<FallbackAttemptTrace>,
    mut failure: RouteFailure,
) -> Response {
    if !hops.is_empty() {
        // Ended for a fallback reason with the plan used up: exhausted. Ended
        // any other way (a context overflow): the chain stopped there.
        let exhausted = failure.reason.is_some();
        let reason = failure.reason.map(FallbackReason::as_str).or_else(|| {
            failure
                .tracker
                .trace
                .context_overflow
                .is_some()
                .then_some("context_length_exceeded")
        });
        hops.push(FallbackAttemptTrace {
            route: failure.route.to_string(),
            outcome: "failed",
            reason,
        });
        if let (true, Some(last)) = (exhausted, failure.reason) {
            state
                .metrics
                .record_cross_route_exhausted(initial.name.as_str(), last.as_str());
        }
        tracing::info!(
            target: targets::ROUTER,
            request_id,
            initial_route = %initial.name,
            final_route = %failure.route,
            exhausted,
            route_attempts = hops.len(),
            "cross-route fallback ended without an answer"
        );
        failure.tracker.trace.cross_route_fallback = Some(FallbackTrace {
            initial_route: initial.name.to_string(),
            final_route: failure.route.to_string(),
            exhausted,
            attempts: hops,
        });
    }
    failure.conclude(state)
}

/// How one logical-route attempt ended.
enum RouteEnd {
    /// A deployment answered and the response was committed — whatever its
    /// status. Nothing after this can change the route.
    Committed(Response),
    /// Nothing was committed: the route's own error, not yet sent. Boxed, as
    /// refusals are: it is the rare path.
    Failed(Box<RouteFailure>),
    /// The pre-commit budget (R9.3.2) stopped the route: it cut an attempt in
    /// flight, or refused to start the next one. Nothing was committed.
    BudgetExhausted(Box<BudgetStop>),
}

/// A route attempt the pre-commit budget stopped.
struct BudgetStop {
    tracker: Tracker,
    /// The budget refused the route's first deployment attempt: nothing was
    /// sent to this route at all.
    before_first_attempt: bool,
}

/// `Auto`'s fixed name, as a route label. Never a name a client typed.
fn auto_route_name() -> RouteName {
    RouteName::label(AUTO_ROUTE)
}

/// End a request the pre-commit budget stopped (R9.3.2): `504
/// request_budget_exhausted`, counted once in `router_requests_total` under
/// the tracker's route — the terminal attempted route, or `Auto` / the named
/// route when none was attempted — and once by `stage`.
///
/// Called only where the budget is the causal reason the request cannot go
/// on: an attempt it cut, or a start it refused. A request that already ended
/// with an outcome of its own never comes here, whatever the clock says.
fn exhaust(
    state: &RouterState,
    mut tracker: Tracker,
    stage: Stage,
    next_unattempted: Option<&RouteName>,
) -> Response {
    let Some(budget) = tracker.budget else {
        // Unreachable: only a configured budget stops a request.
        tracker.finish(None, Outcome::ServerError.as_str());
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &server_error("the request ended unexpectedly", "internal_error"),
        );
    };
    // A nested classification request reports to its parent, which counts
    // the client request once; it records no budget metric of its own.
    if !tracker.nested {
        state.metrics.record_budget_exhausted(stage);
    }
    state
        .metrics
        .record_request(tracker.route.as_str(), Outcome::RequestBudgetExhausted);
    tracker.trace.request_budget = Some(BudgetTrace::exhausted(
        &budget,
        stage,
        next_unattempted.map(ToString::to_string),
    ));
    tracing::warn!(
        target: targets::ROUTER,
        request_id = tracker.trace.request_id.as_str(),
        route = tracker.route.as_str(),
        requested_route = tracker.trace.requested_route.as_str(),
        stage = stage.as_str(),
        next_unattempted_route = next_unattempted.map(RouteName::as_str),
        budget_ms = u64::try_from(budget.configured().as_millis()).unwrap_or(u64::MAX),
        elapsed_ms = millis(budget.elapsed()),
        nested = tracker.nested,
        "pre-commit request budget exhausted"
    );
    tracker.finish(
        Some(StatusCode::GATEWAY_TIMEOUT.as_u16()),
        Outcome::RequestBudgetExhausted.as_str(),
    );
    budget_exhausted_response(budget.configured())
}

/// The `504` a request the budget ended is answered with. It names the
/// configured budget only — never a node, model, route or prompt — and
/// carries no `Retry-After`: nothing says a retry would be faster.
fn budget_exhausted_response(configured: Duration) -> Response {
    let mut response = json_error(
        StatusCode::GATEWAY_TIMEOUT,
        &server_error(
            format!(
                "The router's pre-commit request budget of {} ms ran out before a response started.",
                configured.as_millis()
            ),
            budget::EXHAUSTED,
        ),
    );
    response.extensions_mut().insert(ExhaustedMarker);
    response
}

/// A classification, as its trace shows it.
fn classifier_trace(
    classification: Classification,
    classifier: &RouteClassifier,
) -> ClassifierTrace {
    ClassifierTrace {
        provider: classification.provider.as_str(),
        route: classifier.provider.route().map(ToString::to_string),
        model: classification
            .verdict
            .as_ref()
            .and_then(|verdict| verdict.model.clone())
            .or_else(|| classifier.provider.model().map(str::to_owned)),
        outcome: classification.outcome.as_str(),
        chosen_route: classification
            .verdict
            .as_ref()
            .map(|verdict| verdict.route.to_string()),
        confidence: classification.verdict.as_ref().map(|v| v.confidence),
        duration_ms: millis(classification.duration),
        request_id: classification.request_id,
        input_truncated: classification.input_truncated,
    }
}

/// A route attempt that committed nothing, with everything needed either to
/// answer the client with the route's own error or to try another route.
struct RouteFailure {
    /// The route's own error, exactly as the client would get it.
    response: Response,
    /// How `router_requests_total` and the trace count it, if it is final.
    outcome: Outcome,
    /// What route history observes for it, when the outcome does not say.
    observation: Option<Observation>,
    /// The cross-route fallback reason this is, if it is one of the three.
    reason: Option<FallbackReason>,
    /// The route that failed.
    route: RouteName,
    tracker: Tracker,
    active: Option<ActiveGuard>,
}

impl RouteFailure {
    /// Answer the client with the route's own error, counting the request
    /// once, under this route.
    fn conclude(self, state: &RouterState) -> Response {
        let Self {
            response,
            outcome,
            observation,
            route,
            mut tracker,
            ..
        } = self;
        state.metrics.record_request(route.as_str(), outcome);
        tracker.observation = observation;
        tracker.finish(Some(response.status().as_u16()), outcome.as_str());
        response
    }
}

/// One attempt at one logical route: the session's affinity for this route,
/// the plan (health, R5 capability filtering, policy), and every deployment
/// in it with the existing pre-response failover. A committed answer is
/// relayed from here; anything else is handed back uncommitted.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn attempt_route(
    state: &Arc<RouterState>,
    endpoint: Endpoint,
    headers: &HeaderMap,
    request: &mut serde_json::Map<String, Value>,
    request_id: &str,
    route: &Route,
    needs: &RequestRequirements,
    mut tracker: Tracker,
    mut active: Option<ActiveGuard>,
    planning_started: Instant,
    budget: Option<RequestBudget>,
) -> RouteEnd {
    let policy = route.policy.as_str();
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
        needs,
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
                return RouteEnd::Failed(Box::new(RouteFailure {
                    response: routing_failure(&failure, state.policy.interval),
                    outcome: Outcome::ClientError,
                    // Fit, not route quality: observed, never scored.
                    observation: Some(Observation::CapabilityMismatch),
                    reason: Some(FallbackReason::RouteCapabilityMismatch),
                    route: route.name.clone(),
                    tracker,
                    active,
                }));
            }
            tracing::warn!(
                target: targets::ROUTER,
                request_id,
                route = %route.name,
                error = failure_code(&failure),
                "no deployment is available"
            );
            return RouteEnd::Failed(Box::new(RouteFailure {
                response: routing_failure(&failure, state.policy.interval),
                outcome: Outcome::Unavailable,
                observation: None,
                reason: matches!(failure, RoutingFailure::RouteUnavailable { .. })
                    .then_some(FallbackReason::RouteUnavailable),
                route: route.name.clone(),
                tracker,
                active,
            }));
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
        // R9.3.2: no attempt starts once the budget is spent — no lease, no
        // connection, nothing sent, no failover counted.
        if budget::expired(budget) {
            return RouteEnd::BudgetExhausted(Box::new(BudgetStop {
                tracker,
                before_first_attempt: attempts_made == 0,
            }));
        }
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
        // Connect, response head and any pre-decision error-body read, capped
        // by the budget: the connect timeout becomes min(its own, what is
        // left), and the unbounded head wait — a node's queue included — what
        // is left. A cut attempt is dropped, which closes its connection; it
        // is not the node's failure, so its health is untouched.
        let Ok((outcome, sent)) = budget::bound(budget, attempt_one(&context, request)).await
        else {
            tracker.trace.attempts.push(AttemptTrace {
                route: route.name.to_string(),
                deployment: candidate.deployment.to_string(),
                reason: decision.reason.as_str(),
                outcome: budget::EXHAUSTED,
                upstream_status: None,
                response_ms: None,
            });
            drop(lease);
            return RouteEnd::BudgetExhausted(Box::new(BudgetStop {
                tracker,
                before_first_attempt: false,
            }));
        };
        if !matches!(outcome, Attempt::Committed(_)) {
            pause(&state.phase_delays.after_attempt_ms).await;
        }
        let mut record = |outcome: &'static str| {
            tracker.trace.attempts.push(AttemptTrace {
                route: route.name.to_string(),
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
                    // The next deployment's start check, before its failover
                    // is counted: an attempt that never starts is not one.
                    if budget::expired(budget) {
                        return RouteEnd::BudgetExhausted(Box::new(BudgetStop {
                            tracker,
                            before_first_attempt: false,
                        }));
                    }
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
                if budget::expired(budget) {
                    return RouteEnd::BudgetExhausted(Box::new(BudgetStop {
                        tracker,
                        before_first_attempt: false,
                    }));
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
                    requested_route = tracker.trace.requested_route.as_str(),
                    auto_rule = tracker.trace.auto_rule.as_deref(),
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
                // The budget's state at commit, its last word: from here on it
                // is neither held nor polled.
                if let Some(budget) = &budget {
                    tracker.trace.request_budget = Some(BudgetTrace::committed(budget));
                    if !tracker.nested {
                        state.metrics.observe_budget_remaining(budget.remaining());
                    }
                }
                let held = InFlight {
                    _active: active.take(),
                    _lease: lease,
                    tracker,
                    sent: sent.at,
                    deployment: candidate.deployment.clone(),
                    status: status.as_u16(),
                    upstream_done: false,
                };
                return RouteEnd::Committed(commit(response, &route.name, held).await);
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
            // Every deployment turned the request away before answering with a
            // 502, 503 or 504 (none ran it): to route history that is the route
            // having nothing ready, like `route_unavailable`, not a wrong route.
            let unready = matches!(
                refusal.status,
                StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT
            );
            // `route_exhausted`: the plan was walked to its end and the route
            // ended on such a refusal. A context overflow on the way makes it
            // context-dependent, which R9.3.1 leaves alone.
            let exhausted = unready && tracker.trace.context_overflow.is_none();
            RouteEnd::Failed(Box::new(RouteFailure {
                outcome: Outcome::of_status(refusal.status.as_u16()),
                observation: unready.then_some(Observation::Unavailable),
                reason: exhausted.then_some(FallbackReason::RouteExhausted),
                response: refusal_response(refusal),
                route: route.name.clone(),
                tracker,
                active,
            }))
        }
        None => {
            tracing::warn!(
                target: targets::ROUTER,
                request_id,
                route = %route.name,
                failover_count = tried,
                "every deployment failed before answering"
            );
            RouteEnd::Failed(Box::new(RouteFailure {
                response: routing_failure(
                    &RoutingFailure::RouteUnavailable {
                        route: route.name.clone(),
                    },
                    state.policy.interval,
                ),
                outcome: Outcome::Unavailable,
                observation: None,
                reason: Some(FallbackReason::RouteUnavailable),
                route: route.name.clone(),
                tracker,
                active,
            }))
        }
    }
}

/// Why `Auto` resolved no route.
enum AutoStop {
    /// The gateway would refuse the request: the refusal, already counted.
    Refused(Box<Response>),
    /// The pre-commit budget ran out before or during classification (R9.3.2).
    /// The request ends here: no classifier fallback, no scoring, no route.
    BudgetExhausted {
        classification: Box<Classification>,
        rule: Option<String>,
    },
}

/// What `Auto` resolved one request to.
struct AutoResolution<'a> {
    /// The route the rules named. Always configured — validation saw to it —
    /// but carried as a resolution so the ordinary refusal stands if not.
    route: Result<&'a Route, RoutingFailure>,
    /// Read to decide, and reused for capability filtering.
    needs: RequestRequirements,
    /// The rule that matched; `None` for the fallback.
    rule: Option<String>,
    /// The classification, when the rule asked for one.
    classification: Option<Classification>,
    /// How adaptive scoring resolved the classification, when it is on.
    scoring: Option<ScoringTrace>,
}

/// Resolve an `Auto` request: read what it requires, and let the first
/// matching rule, or the fallback, name the route.
///
/// Only a route is chosen here. Which of its deployments answers is decided
/// afterwards exactly as for a request that named the route. A request the
/// gateway would refuse is refused before any route is chosen, and counted
/// under `Auto` — a fixed name, never one a client typed.
///
/// When the matching rule asks to classify, the classifier route is asked
/// which candidate should answer — unless this request is itself a
/// classification, which takes the rule's fallback instead. A classification
/// that fails or is unsure resolves to the classifier's fallback.
///
/// With adaptive scoring on (R9.2), an accepted classification is then scored:
/// its verdict route against the fallback, by classifier signal, prior and
/// route history. That is the only thing scoring can change, and it changes
/// only which logical route; a rejected or failed classification resolves
/// exactly as above.
#[allow(clippy::too_many_arguments)]
async fn resolve_auto<'a>(
    state: &'a Arc<RouterState>,
    auto: &'a AutoRoute,
    endpoint: Endpoint,
    body: &[u8],
    request_id: &str,
    planning_started: Instant,
    nested: bool,
    budget: Option<RequestBudget>,
) -> Result<AutoResolution<'a>, AutoStop> {
    let needs = match requirements::extract(endpoint, body) {
        Ok(needs) => needs,
        Err(refusal) => {
            let routing = planning_started.elapsed();
            state
                .metrics
                .observe_planning(AUTO_ROUTE, NO_POLICY, routing);
            tracing::info!(
                target: targets::ROUTER,
                request_id,
                requested_route = AUTO_ROUTE,
                endpoint = endpoint.as_str(),
                upstream_status = refusal.status().as_u16(),
                routing_ms = millis(routing),
                "request refused before routing"
            );
            state
                .metrics
                .record_request(AUTO_ROUTE, Outcome::ClientError);
            return Err(AutoStop::Refused(refusal));
        }
    };
    let decision = auto.decide(&needs);
    let classification = match auto.classifier.as_ref().filter(|_| decision.classify) {
        Some(classifier) if nested => Some(Classification::nested(classifier.provider.kind())),
        Some(classifier) => {
            let input = ClassificationInput::read(
                endpoint,
                body,
                &needs,
                classifier.limits().max_input_chars,
            );
            Some(crate::classifier::classify(state, classifier, &input, request_id, budget).await)
        }
        None => None,
    };
    // R9.3.2: the request deadline ran out before or during classification.
    // No subsystem may create more work: not R9.1's fallback route, not
    // scoring, not planning. Counted as a classification outcome of its own,
    // never `timeout`, and never as a provider failure.
    if let Some(classification) = classification.as_ref().filter(|classification| {
        classification.outcome == ClassifierOutcome::RequestBudgetExhausted
    }) {
        tracing::info!(
            target: targets::ROUTER,
            request_id,
            auto_rule = decision.rule_label(),
            classifier_provider = classification.provider.as_str(),
            classifier_outcome = classification.outcome.as_str(),
            classifier_duration_ms = millis(classification.duration),
            "auto route classification ended by the request budget"
        );
        state.metrics.record_classification(
            classification.provider.as_str(),
            classification.outcome.as_str(),
            None,
            classification.duration,
        );
        return Err(AutoStop::BudgetExhausted {
            classification: Box::new(classification.clone()),
            rule: decision.rule.map(str::to_owned),
        });
    }
    // R9.2: only a classification, only while scoring is on, only a route.
    let scored = match (
        &classification,
        &auto.classifier,
        auto.scoring.as_ref().filter(|scoring| scoring.enabled),
    ) {
        (Some(classification), Some(classifier), Some(scoring)) => {
            let decision = crate::scoring::decide(
                scoring,
                &state.route_history,
                classification,
                classifier,
                Instant::now(),
            );
            state.metrics.record_scoring(
                decision.trace.reason,
                decision.route.as_str(),
                decision.trace.overrode,
            );
            Some(decision)
        }
        _ => None,
    };
    if let (Some(classification), Some(classifier)) = (&classification, &auto.classifier) {
        let chosen = classification.verdict.as_ref();
        tracing::info!(
            target: targets::ROUTER,
            request_id,
            auto_rule = decision.rule_label(),
            classifier_provider = classification.provider.as_str(),
            classifier_route = classifier.provider.route().map(RouteName::as_str),
            classifier_model = classifier.provider.model(),
            classifier_outcome = classification.outcome.as_str(),
            classifier_chosen = chosen.map(|verdict| verdict.route.as_str()),
            classifier_confidence = chosen.map(|verdict| verdict.confidence),
            classifier_duration_ms = millis(classification.duration),
            input_truncated = classification.input_truncated,
            classified_route = %classification.route(classifier),
            scoring_reason = scored.as_ref().map(|scored| scored.trace.reason.as_str()),
            scoring_overrode = scored.as_ref().map(|scored| scored.trace.overrode),
            resolved_route = %scored.as_ref().map_or_else(
                || classification.route(classifier),
                |scored| &scored.route,
            ),
            "auto route classified"
        );
        state.classifier_status.record(classification.outcome);
        state.metrics.record_classification(
            classification.provider.as_str(),
            classification.outcome.as_str(),
            (classification.outcome == crate::classifier::ClassifierOutcome::Chosen)
                .then(|| classification.route(classifier).as_str()),
            classification.duration,
        );
    }
    let resolved = match (&scored, &classification, &auto.classifier) {
        (Some(scored), _, _) => &scored.route,
        (None, Some(classification), Some(classifier)) => classification.route(classifier),
        _ => decision.route,
    };
    tracing::info!(
        target: targets::ROUTER,
        request_id,
        requested_route = AUTO_ROUTE,
        auto_rule = decision.rule_label(),
        resolved_route = %resolved,
        endpoint = endpoint.as_str(),
        requires_tools = needs.tools,
        tool_choice = needs.tool_choice.as_str(),
        requires_reasoning = needs.reasoning,
        estimated_prompt_tokens = needs.prompt_tokens,
        "auto route resolved"
    );
    state.metrics.record_auto_decision(
        decision.rule_label(),
        resolved.as_str(),
        decision.is_fallback(),
    );
    let route = state
        .topology
        .route(resolved)
        .ok_or_else(|| RoutingFailure::UnknownRoute {
            requested: resolved.to_string(),
        });
    Ok(AutoResolution {
        route,
        needs,
        rule: decision.rule.map(str::to_owned),
        classification,
        scoring: scored.map(|scored| scored.trace),
    })
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
