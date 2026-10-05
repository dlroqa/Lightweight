//! Placement: which deployments should be loaded, decided ahead of demand.
//!
//! Routing answers "which **ready** deployment serves this request?" and is
//! untouched by anything here. Placement answers "where should this route be
//! prepared?" — on a loop of its own ([`crate::controller`]), never in the
//! request path. A request for a route with nothing ready is refused
//! `route_unavailable` at once, exactly as before; it never waits for a load.
//!
//! A route opts in with a target: `min_ready` deployments it should never fall
//! below, plus `warm_standby` more kept loaded ahead of need, on the nodes the
//! operator listed in `allowed_nodes`. What the controller may do is narrow by
//! design:
//!
//! * **Load an installed model onto an empty node.** A node that is healthy
//!   and serving nothing, whose catalog already holds the route's model. It
//!   never downloads a model, and it never swaps one out: a Lightweight
//!   gateway serves one model at a time, and a node serving anything else —
//!   another route's model included — is left alone.
//! * **Let the node decide.** The node's own admission control judges every
//!   load against its memory. A refusal (`insufficient_memory`) is recorded as
//!   `admission_failed` and backed off; the router never estimates memory.
//! * **Count only what it has seen.** A deployment is ready when the router's
//!   own health probe says the node is serving its model — the same rule the
//!   request path uses — never because a load call returned.
//! * **Unload nothing.** Unloading is out of scope for R7.
//!
//! Nothing here reads latency, TTFT, estimator error or traffic: placement is
//! a count of ready deployments against a configured target.
//!
//! This module is pure state and planning; the network calls live in
//! [`crate::controller`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;

use crate::config::PlacementPolicy;
use crate::domain::{
    DeploymentHealth, DeploymentId, NodeHealth, NodeId, Route, RouteName, Topology,
    UnavailableReason,
};
use crate::health::{NodeStatus, availability};

/// Where one deployment stands, as placement sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeploymentState {
    /// Available by the router's own probe: routable now.
    Ready,
    /// A load this controller requested is in progress.
    Loading,
    /// The node is healthy and serving nothing: a model could be loaded here.
    Empty,
    /// The node is healthy and serving another model. Never swapped.
    Occupied,
    /// The node is disabled, unhealthy or not yet probed.
    Unavailable(UnavailableReason),
}

impl DeploymentState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Loading => "loading",
            Self::Empty => "empty",
            Self::Occupied => "occupied",
            Self::Unavailable(_) => "unavailable",
        }
    }
}

/// Why a placement action failed. Low cardinality: these are the only values
/// the failure counter's `reason` label can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureReason {
    /// The node's catalog has no such model, or its file is missing.
    ModelNotInstalled,
    /// The node refused the control request (authentication, no catalog, a
    /// malformed request).
    LoadRejected,
    /// The node could not be reached for the control request.
    NodeUnhealthy,
    /// The node was already busy with a model operation.
    NodeBusy,
    /// The node's admission control refused the load for memory.
    AdmissionFailed,
    /// The load, or the readiness after it, took longer than allowed.
    LoadTimeout,
    /// The engine failed to start the model, or the job was cancelled.
    ModelFailed,
}

impl FailureReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelNotInstalled => "model_not_installed",
            Self::LoadRejected => "load_rejected",
            Self::NodeUnhealthy => "node_unhealthy",
            Self::NodeBusy => "node_busy",
            Self::AdmissionFailed => "admission_failed",
            Self::LoadTimeout => "load_timeout",
            Self::ModelFailed => "model_failed",
        }
    }

    /// The reason a failed load job's structured `error.code` maps to.
    pub fn of_code(code: &str) -> Self {
        match code {
            "insufficient_memory" | "memory_probe_failed" => Self::AdmissionFailed,
            "model_operation_in_progress" | "drain_timed_out" => Self::NodeBusy,
            "unknown_model" | "model_not_found" | "model_file_not_found" => Self::ModelNotInstalled,
            _ => Self::ModelFailed,
        }
    }
}

/// One load the plan asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub route: RouteName,
    pub deployment: DeploymentId,
    pub node: NodeId,
    /// The model the node is asked to load: the deployment's own name for it.
    pub model: String,
}

/// One route measured against its target.
#[derive(Clone, Debug)]
pub struct Assessment {
    pub route: RouteName,
    pub min_ready: u32,
    pub warm_standby: u32,
    /// Every deployment of the route, in configured order.
    pub deployments: Vec<(DeploymentId, NodeId, DeploymentState, bool)>,
}

impl Assessment {
    pub fn target(&self) -> u32 {
        self.min_ready.saturating_add(self.warm_standby)
    }

    /// Ready deployments, among every deployment the route has — placed by
    /// this controller or not.
    pub fn ready(&self) -> u32 {
        self.count(DeploymentState::Ready)
    }

    pub fn loading(&self) -> u32 {
        self.count(DeploymentState::Loading)
    }

    /// Ready deployments beyond `min_ready`: the warm standbys that exist.
    pub fn standby(&self) -> u32 {
        self.ready().saturating_sub(self.min_ready)
    }

    /// Loads still wanted once those in progress finish.
    pub fn shortfall(&self) -> u32 {
        self.target()
            .saturating_sub(self.ready())
            .saturating_sub(self.loading())
    }

    /// `satisfied`, `below_target` (warm standby short), or `below_min`.
    pub fn status(&self) -> &'static str {
        if self.ready() >= self.target() {
            "satisfied"
        } else if self.ready() >= self.min_ready {
            "below_target"
        } else {
            "below_min"
        }
    }

    fn count(&self, state: DeploymentState) -> u32 {
        u32::try_from(
            self.deployments
                .iter()
                .filter(|(_, _, s, _)| *s == state)
                .count(),
        )
        .unwrap_or(u32::MAX)
    }
}

/// Measure one route against its target. `None` when it has no target.
pub fn assess(
    topology: &Topology,
    route: &Route,
    health: &BTreeMap<NodeId, NodeStatus>,
    loading: &BTreeSet<DeploymentId>,
) -> Option<Assessment> {
    let placement = route.placement.as_ref()?;
    let unknown = NodeStatus::default();
    let deployments = route
        .deployments
        .iter()
        .filter_map(|id| {
            let deployment = topology.deployment(id)?;
            let node = topology.node(&deployment.node)?;
            let status = health.get(&node.id).unwrap_or(&unknown);
            let state = match availability(node, deployment, status) {
                DeploymentHealth::Available => DeploymentState::Ready,
                _ if loading.contains(id) => DeploymentState::Loading,
                DeploymentHealth::Unavailable(UnavailableReason::ModelNotServed) => {
                    if status.health == NodeHealth::Healthy && status.served().is_none() {
                        DeploymentState::Empty
                    } else {
                        DeploymentState::Occupied
                    }
                }
                DeploymentHealth::Unavailable(reason) => DeploymentState::Unavailable(reason),
            };
            let allowed = placement.allowed.contains(id);
            Some((id.clone(), node.id.clone(), state, allowed))
        })
        .collect();
    Some(Assessment {
        route: route.name.clone(),
        min_ready: placement.min_ready,
        warm_standby: placement.warm_standby,
        deployments,
    })
}

/// The loads to start now, for every route with a target.
///
/// Deterministic: routes in configured order, and within a route its allowed
/// deployments in configured order. A deployment is chosen only when its node
/// is healthy and empty, it is not backing off, and no other load is already
/// in progress — or chosen in this same pass — on its node. Never more loads
/// for a route than it is short of its target.
pub fn plan(
    topology: &Topology,
    health: &BTreeMap<NodeId, NodeStatus>,
    loading: &BTreeSet<DeploymentId>,
    backing_off: &dyn Fn(&DeploymentId) -> bool,
) -> Vec<Action> {
    // A node with a load in progress takes no second one.
    let mut claimed: BTreeSet<NodeId> = loading
        .iter()
        .filter_map(|id| topology.deployment(id).map(|d| d.node.clone()))
        .collect();
    let mut actions = Vec::new();
    for route in topology.routes() {
        let Some(assessment) = assess(topology, route, health, loading) else {
            continue;
        };
        let mut wanted = assessment.shortfall();
        for (id, node, state, allowed) in &assessment.deployments {
            if wanted == 0 {
                break;
            }
            if !allowed
                || *state != DeploymentState::Empty
                || backing_off(id)
                || claimed.contains(node)
            {
                continue;
            }
            let Some(deployment) = topology.deployment(id) else {
                continue;
            };
            claimed.insert(node.clone());
            actions.push(Action {
                route: route.name.clone(),
                deployment: id.clone(),
                node: node.clone(),
                model: deployment.remote_model.clone(),
            });
            wanted -= 1;
        }
    }
    actions
}

/// What happened to the last placement action on a deployment.
#[derive(Clone, Debug, Serialize)]
pub struct LastAction {
    pub action: &'static str,
    /// `succeeded`, `failed`, or `already_loaded` (the node turned out to be
    /// serving it; only readiness was confirmed).
    pub result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<FailureReason>,
    /// The node's own error code, when it gave one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Unix seconds.
    pub at: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Default)]
struct Record {
    loading_since: Option<Instant>,
    last: Option<LastAction>,
    failures: u32,
    retry_at: Option<Instant>,
}

/// Placement state for one deployment, as the admin view shows it.
#[derive(Clone, Debug, Default)]
pub struct RecordView {
    pub loading_for: Option<Duration>,
    pub last: Option<LastAction>,
    pub failures: u32,
    pub retry_in: Option<Duration>,
}

/// The controller's memory: loads in progress, the last result, and backoff.
/// In memory only; a restart starts every deployment with a clean slate.
#[derive(Debug)]
pub struct PlacementBook {
    policy: PlacementPolicy,
    records: Mutex<BTreeMap<DeploymentId, Record>>,
    /// Set by `POST /api/router/v1/placement/reconcile` to run a pass now.
    pub wake: tokio::sync::Notify,
    last_pass: Mutex<Option<SystemTime>>,
}

impl PlacementBook {
    pub fn new(policy: PlacementPolicy) -> Self {
        Self {
            policy,
            records: Mutex::new(BTreeMap::new()),
            wake: tokio::sync::Notify::new(),
            last_pass: Mutex::new(None),
        }
    }

    pub fn policy(&self) -> &PlacementPolicy {
        &self.policy
    }

    /// Deployments with a load in progress.
    pub fn loading(&self) -> BTreeSet<DeploymentId> {
        self.lock()
            .iter()
            .filter(|(_, record)| record.loading_since.is_some())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Whether a deployment is still waiting out a failure.
    pub fn backing_off(&self, id: &DeploymentId, now: Instant) -> bool {
        self.lock()
            .get(id)
            .and_then(|record| record.retry_at)
            .is_some_and(|retry| now < retry)
    }

    /// Mark a load started. `false` if one already is: never two at once.
    pub fn begin(&self, id: &DeploymentId, now: Instant) -> bool {
        let mut records = self.lock();
        let record = records.entry(id.clone()).or_default();
        if record.loading_since.is_some() {
            return false;
        }
        record.loading_since = Some(now);
        true
    }

    /// Record a load that ended in readiness.
    pub fn succeeded(&self, id: &DeploymentId, last: LastAction) {
        let mut records = self.lock();
        let record = records.entry(id.clone()).or_default();
        record.loading_since = None;
        record.failures = 0;
        record.retry_at = None;
        record.last = Some(last);
    }

    /// Record a failure and schedule the next attempt: the backoff, doubled
    /// for each consecutive failure, never more than the maximum.
    pub fn failed(&self, id: &DeploymentId, last: LastAction, now: Instant) -> Duration {
        let mut records = self.lock();
        let record = records.entry(id.clone()).or_default();
        record.loading_since = None;
        record.failures = record.failures.saturating_add(1);
        let delay = self.backoff_after(record.failures);
        record.retry_at = Some(now + delay);
        record.last = Some(last);
        delay
    }

    /// The wait after the `failures`-th consecutive failure.
    pub fn backoff_after(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(20);
        self.policy
            .backoff
            .saturating_mul(1_u32 << doublings)
            .min(self.policy.backoff_max)
    }

    /// Forget a load that was abandoned (the router is stopping).
    pub fn abandon(&self, id: &DeploymentId) {
        if let Some(record) = self.lock().get_mut(id) {
            record.loading_since = None;
        }
    }

    pub fn view(&self, id: &DeploymentId, now: Instant) -> RecordView {
        self.lock()
            .get(id)
            .map(|record| RecordView {
                loading_for: record
                    .loading_since
                    .map(|since| now.saturating_duration_since(since)),
                last: record.last.clone(),
                failures: record.failures,
                retry_in: record
                    .retry_at
                    .filter(|retry| *retry > now)
                    .map(|retry| retry.saturating_duration_since(now)),
            })
            .unwrap_or_default()
    }

    pub fn passed(&self, at: SystemTime) {
        *self
            .last_pass
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(at);
    }

    pub fn last_pass(&self) -> Option<SystemTime> {
        *self
            .last_pass
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<DeploymentId, Record>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Whether any route asks for placement: the controller runs only then.
pub fn configured(topology: &Topology) -> bool {
    topology
        .routes()
        .iter()
        .any(|route| route.placement.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RouterFile, validate};
    use crate::domain::CapabilitySet;
    use crate::health::{Observation, Outcome, ServedModel, apply};
    use serde_json::{Value, json};

    fn topology(routes: Value) -> Topology {
        let file: RouterFile = serde_json::from_value(json!({
            "nodes": [
                {"id": "a", "url": "http://192.0.2.10:11434"},
                {"id": "b", "url": "http://192.0.2.11:11434"},
                {"id": "c", "url": "http://192.0.2.12:11434"}
            ],
            "routes": routes
        }))
        .unwrap();
        validate(file, &|_| None).expect("valid").topology
    }

    fn coder(min_ready: u32, warm: u32, allowed: &[&str]) -> Topology {
        topology(json!([{"name": "Coder", "deployments": [
            {"node": "a", "model": "QwenCoder"},
            {"node": "b", "model": "QwenCoder"},
            {"node": "c", "model": "QwenCoder"}
        ], "placement": {"min_ready": min_ready, "warm_standby": warm, "allowed_nodes": allowed}}]))
    }

    fn status(serving: Option<&str>) -> NodeStatus {
        let mut status = NodeStatus::default();
        apply(
            &mut status,
            Outcome::Success(Observation {
                served: serving.map(|id| ServedModel {
                    id: id.into(),
                    context_length: 4096,
                }),
                features: CapabilitySet::none(),
                max_concurrent_requests: 1,
                version: "0.5.0".into(),
            }),
            1,
            SystemTime::now(),
        );
        status
    }

    fn down() -> NodeStatus {
        let mut status = NodeStatus::default();
        apply(
            &mut status,
            Outcome::Failure("gone".into()),
            1,
            SystemTime::now(),
        );
        status
    }

    fn health(a: NodeStatus, b: NodeStatus, c: NodeStatus) -> BTreeMap<NodeId, NodeStatus> {
        BTreeMap::from([
            (NodeId::parse("a").unwrap(), a),
            (NodeId::parse("b").unwrap(), b),
            (NodeId::parse("c").unwrap(), c),
        ])
    }

    fn id(node: &str) -> DeploymentId {
        DeploymentId::of(&NodeId::parse(node).unwrap(), "QwenCoder")
    }

    fn never(_: &DeploymentId) -> bool {
        false
    }

    fn nodes(actions: &[Action]) -> Vec<&str> {
        actions.iter().map(|a| a.node.as_str()).collect()
    }

    #[test]
    fn ready_and_standby_are_counted_against_the_target() {
        let t = coder(1, 1, &["a", "b", "c"]);
        let h = health(
            status(Some("QwenCoder")),
            status(None),
            status(Some("Other")),
        );
        let assessment = assess(&t, &t.routes()[0], &h, &BTreeSet::new()).unwrap();
        assert_eq!(assessment.target(), 2);
        assert_eq!(assessment.ready(), 1);
        assert_eq!(assessment.standby(), 0);
        assert_eq!(assessment.shortfall(), 1);
        assert_eq!(assessment.status(), "below_target");
        let states: Vec<&str> = assessment
            .deployments
            .iter()
            .map(|(_, _, s, _)| s.as_str())
            .collect();
        assert_eq!(states, ["ready", "empty", "occupied"]);

        let both = health(
            status(Some("QwenCoder")),
            status(Some("qwencoder")),
            status(None),
        );
        let assessment = assess(&t, &t.routes()[0], &both, &BTreeSet::new()).unwrap();
        assert_eq!((assessment.ready(), assessment.standby()), (2, 1));
        assert_eq!(assessment.status(), "satisfied");
    }

    #[test]
    fn an_empty_allowed_node_is_loaded_and_nothing_else_is_touched() {
        let t = coder(1, 1, &["a", "b", "c"]);
        // b serves another model: never swapped. c is empty: loaded.
        let h = health(
            status(Some("QwenCoder")),
            status(Some("Other")),
            status(None),
        );
        let actions = plan(&t, &h, &BTreeSet::new(), &never);
        assert_eq!(nodes(&actions), ["c"]);
        assert_eq!(actions[0].model, "QwenCoder");
    }

    #[test]
    fn an_already_satisfied_route_plans_nothing() {
        let t = coder(1, 1, &["a", "b", "c"]);
        let h = health(
            status(Some("QwenCoder")),
            status(Some("QwenCoder")),
            status(None),
        );
        assert!(plan(&t, &h, &BTreeSet::new(), &never).is_empty());
    }

    #[test]
    fn a_load_in_progress_is_never_repeated_and_counts_toward_the_target() {
        let t = coder(1, 1, &["a", "b", "c"]);
        let h = health(status(Some("QwenCoder")), status(None), status(None));
        let loading = BTreeSet::from([id("b")]);
        assert!(
            plan(&t, &h, &loading, &never).is_empty(),
            "b's load fills the one standby wanted"
        );
        let t = coder(1, 2, &["a", "b", "c"]);
        assert_eq!(nodes(&plan(&t, &h, &loading, &never)), ["c"]);
    }

    #[test]
    fn unhealthy_disallowed_and_backing_off_nodes_get_no_action() {
        let t = coder(1, 2, &["a", "b"]);
        let h = health(status(None), down(), status(None));
        // a is empty and allowed; b is down; c is empty but not allowed.
        assert_eq!(nodes(&plan(&t, &h, &BTreeSet::new(), &never)), ["a"]);
        let backing = |d: &DeploymentId| *d == id("a");
        assert!(plan(&t, &h, &BTreeSet::new(), &backing).is_empty());
        // Not yet probed is not healthy enough either.
        let unknown = BTreeMap::new();
        assert!(plan(&t, &unknown, &BTreeSet::new(), &never).is_empty());
    }

    #[test]
    fn two_routes_never_claim_one_empty_node_in_the_same_pass() {
        let t = topology(json!([
            {"name": "Coder", "deployments": [{"node": "a", "model": "QwenCoder"}],
             "placement": {"allowed_nodes": ["a"]}},
            {"name": "Fast", "deployments": [{"node": "a", "model": "Fast"}],
             "placement": {"allowed_nodes": ["a"]}}
        ]));
        let h = health(status(None), status(None), status(None));
        let actions = plan(&t, &h, &BTreeSet::new(), &never);
        assert_eq!(actions.len(), 1, "a single-model node takes one load");
        assert_eq!(
            actions[0].route.as_str(),
            "Coder",
            "configured order decides"
        );
    }

    #[test]
    fn a_route_without_placement_is_never_acted_on() {
        let t = topology(json!([{"name": "Coder", "deployments": [
            {"node": "a", "model": "QwenCoder"}]}]));
        let h = health(status(None), status(None), status(None));
        assert!(plan(&t, &h, &BTreeSet::new(), &never).is_empty());
        assert!(!configured(&t));
    }

    #[test]
    fn backoff_doubles_and_is_bounded() {
        let book = PlacementBook::new(PlacementPolicy {
            backoff: Duration::from_secs(10),
            backoff_max: Duration::from_secs(60),
            ..PlacementPolicy::default()
        });
        let delays: Vec<u64> = (1..=6).map(|n| book.backoff_after(n).as_secs()).collect();
        assert_eq!(delays, [10, 20, 40, 60, 60, 60]);
        assert_eq!(book.backoff_after(u32::MAX).as_secs(), 60);
    }

    #[test]
    fn the_book_refuses_a_second_load_and_backs_off_after_failure() {
        let book = PlacementBook::new(PlacementPolicy::default());
        let now = Instant::now();
        assert!(book.begin(&id("a"), now));
        assert!(!book.begin(&id("a"), now), "never two loads at once");
        assert_eq!(book.loading(), BTreeSet::from([id("a")]));
        let last = LastAction {
            action: "load",
            result: "failed",
            reason: Some(FailureReason::AdmissionFailed),
            code: Some("insufficient_memory".into()),
            at: 0,
            duration_ms: 5,
        };
        let delay = book.failed(&id("a"), last.clone(), now);
        assert_eq!(delay, book.policy().backoff);
        assert!(book.loading().is_empty());
        assert!(book.backing_off(&id("a"), now));
        assert!(!book.backing_off(&id("a"), now + delay));
        let view = book.view(&id("a"), now);
        assert_eq!(view.failures, 1);
        assert_eq!(
            view.last.unwrap().reason,
            Some(FailureReason::AdmissionFailed)
        );

        assert!(book.begin(&id("a"), now + delay));
        book.succeeded(
            &id("a"),
            LastAction {
                result: "succeeded",
                reason: None,
                code: None,
                ..last
            },
        );
        assert!(!book.backing_off(&id("a"), now + delay));
        assert_eq!(book.view(&id("a"), now).failures, 0);
    }

    #[test]
    fn node_error_codes_map_to_failure_reasons() {
        assert_eq!(
            FailureReason::of_code("insufficient_memory"),
            FailureReason::AdmissionFailed
        );
        assert_eq!(
            FailureReason::of_code("model_operation_in_progress"),
            FailureReason::NodeBusy
        );
        assert_eq!(
            FailureReason::of_code("model_file_not_found"),
            FailureReason::ModelNotInstalled
        );
        assert_eq!(
            FailureReason::of_code("engine_crashed"),
            FailureReason::ModelFailed
        );
    }
}
