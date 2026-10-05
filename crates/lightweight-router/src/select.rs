//! Choosing where a request goes.
//!
//! Always in the same order: **eligibility first, policy second.**
//!
//! 1. [`eligible`] removes every deployment that cannot take traffic now —
//!    disabled, unknown, unhealthy, or not serving its model — using the one
//!    availability rule in [`crate::health`]. It is shared by every policy and
//!    by the route summaries clients read, so no policy can reach a
//!    deployment the others would refuse.
//! 2. [`crate::capability::filter`] removes every deployment left that cannot
//!    serve *this* request — its endpoint, tools, `tool_choice`, reasoning, or
//!    a prompt too long for its context. A plan made outside a request
//!    requires nothing and removes nothing.
//! 3. The route's policy orders what is left. The first deployment is the
//!    initial choice; the rest, in order, are where pre-commit failover goes.
//!    * [`order_priority`]: configured order.
//!    * [`order_round_robin`]: the configured order rotated by a per-route
//!      cursor, one step per request.
//!    * [`order_least_busy`]: lowest `active / limit` first, ties to configured
//!      order.
//!
//! The ordering functions are pure — a cursor or a set of loads in, a plan out —
//! so every rule is testable without a network. The state they need (cursors,
//! in-flight counts) lives in [`Selector`] and [`crate::load::LoadBook`], and the
//! proxy only walks the plan it is handed: it holds no policy of its own.
//!
//! The policies never see a requirement: they are handed a candidate list that
//! is already right for the request, and order it exactly as they did before
//! capabilities existed. Nothing here reads latency, history or a prompt's
//! meaning.
//!
//! Session affinity sits between steps 2 and 3, and only as a preference over
//! what steps 1 and 2 left: when a request's session last succeeded on a
//! deployment that is still in the candidate set, that deployment goes first
//! and the policy does not take a turn — round-robin draws no cursor value,
//! least-busy compares no loads for the first choice. The rest of the plan,
//! where failover goes, is the policy's own order of what remains. A sticky
//! deployment that is not in the candidate set is ignored, and the policy
//! chooses exactly as it would for a request with no session.

use std::cmp::Ordering as Order;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::affinity::Reassignment;
use crate::domain::{CapabilityGap, CapabilitySet};
use crate::domain::{
    DeploymentId, NodeId, Route, RouteName, RoutePolicy, RoutingDecision, RoutingFailure,
    RoutingReason, Topology, UnavailableReason,
};
use crate::health::{DeploymentObservation, NodeStatus, availability};
use crate::load::{Lease, LoadBook};
use crate::requirements::RequestRequirements;

/// One deployment the plan may try.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub deployment: DeploymentId,
    pub node: NodeId,
    /// What to send upstream as `model`.
    pub remote_model: String,
    /// Where the deployment sits in the route's configured order, from zero.
    pub position: usize,
}

/// The deployments of one route that can take traffic now, in configured
/// order.
#[derive(Clone, Debug)]
pub struct Eligible {
    pub route: RouteName,
    pub candidates: Vec<Candidate>,
    /// Deployments left out, and why, for the log line.
    pub skipped: Vec<(DeploymentId, UnavailableReason)>,
    /// Deployments that could take traffic but not this request, and every
    /// requirement each one fails. Filled by [`crate::capability::filter`].
    pub unfit: Vec<(DeploymentId, Vec<CapabilityGap>)>,
    /// How many deployments the route has in total.
    pub route_size: usize,
}

/// How a plan's first choice was made, for the log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    Priority,
    RoundRobin {
        /// The route's cursor value this request drew.
        cursor: u64,
        /// The chosen deployment's index in the eligible ring.
        selected_index: usize,
    },
    LeastBusy {
        /// The chosen deployment's in-flight count before this request.
        active_before: u64,
        /// Its concurrency limit, when known and positive.
        concurrency_limit: Option<u32>,
        /// Whether another eligible deployment had exactly the same load.
        tied: bool,
    },
    /// The session's sticky deployment was still a candidate and went first;
    /// the policy took no turn.
    SessionAffinity,
}

/// What became of a request's sticky deployment when its plan was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sticky {
    /// Still a candidate: it is `candidates[0]`.
    Hit,
    /// Not a candidate, for this reason; the policy chose instead.
    Broken(Reassignment),
}

/// The ordered deployments one request may be sent to.
#[derive(Debug)]
pub struct Plan {
    pub route: RouteName,
    pub policy: RoutePolicy,
    pub candidates: Vec<Candidate>,
    /// Deployments left out, and why, for the log line.
    pub skipped: Vec<(DeploymentId, UnavailableReason)>,
    /// Available deployments this request's requirements ruled out. Never
    /// tried, not even as a failover.
    pub unfit: Vec<(DeploymentId, Vec<CapabilityGap>)>,
    pub selection: Selection,
    /// An in-flight slot already taken on `candidates[0]`, when the plan came
    /// from a [`Selector`]. Least-busy has to take it while it still holds the
    /// lock it chose under; the other policies take it the same way so the
    /// proxy has one path.
    pub reservation: Option<Lease>,
    /// Set when the request named a sticky deployment.
    pub sticky: Option<Sticky>,
    route_size: usize,
}

impl Plan {
    fn from(eligible: Eligible, policy: RoutePolicy, selection: Selection) -> Self {
        Self {
            route: eligible.route,
            policy,
            candidates: eligible.candidates,
            skipped: eligible.skipped,
            unfit: eligible.unfit,
            selection,
            reservation: None,
            sticky: None,
            route_size: eligible.route_size,
        }
    }

    /// Describe sending to `candidates[attempt]`, given that every earlier
    /// candidate in this plan was tried and failed.
    pub fn decision(&self, attempt: usize) -> Option<RoutingDecision> {
        let candidate = self.candidates.get(attempt)?;
        if attempt == 0 && self.sticky == Some(Sticky::Hit) {
            return Some(RoutingDecision {
                route: self.route.clone(),
                deployment: candidate.deployment.clone(),
                node: candidate.node.clone(),
                reason: RoutingReason::SessionAffinity,
            });
        }
        let reason = match self.policy {
            RoutePolicy::Priority => {
                if self.route_size == 1 {
                    RoutingReason::ExplicitSingleDeployment
                } else if attempt > 0 {
                    RoutingReason::PrimaryFailedFallback
                } else if candidate.position == 0 {
                    RoutingReason::PrimaryHealthy
                } else {
                    RoutingReason::PrimaryUnavailableFallback
                }
            }
            RoutePolicy::RoundRobin if attempt > 0 => RoutingReason::RoundRobinFailover,
            RoutePolicy::RoundRobin => RoutingReason::RoundRobin,
            RoutePolicy::LeastBusy if attempt > 0 => RoutingReason::LeastBusyFailover,
            RoutePolicy::LeastBusy => match self.selection {
                Selection::LeastBusy { tied: true, .. } => RoutingReason::LeastBusyTiebreak,
                _ => RoutingReason::LeastBusy,
            },
        };
        Some(RoutingDecision {
            route: self.route.clone(),
            deployment: candidate.deployment.clone(),
            node: candidate.node.clone(),
            reason,
        })
    }
}

/// The deployments of `route` that can take traffic now.
///
/// Refuses with [`RoutingFailure::RouteUnavailable`] when there are none — the
/// route exists, so this is never `model_not_found`.
pub fn eligible(
    topology: &Topology,
    route: &Route,
    health: &BTreeMap<NodeId, NodeStatus>,
) -> Result<Eligible, RoutingFailure> {
    let unknown = NodeStatus::default();
    let mut candidates = Vec::new();
    let mut skipped = Vec::new();

    for (position, id) in route.deployments.iter().enumerate() {
        // The topology guarantees both lookups; a miss would be a bug in
        // validation, and the safe response to it is "not eligible".
        let Some(deployment) = topology.deployment(id) else {
            continue;
        };
        let Some(node) = topology.node(&deployment.node) else {
            continue;
        };
        let status = health.get(&node.id).unwrap_or(&unknown);
        match availability(node, deployment, status) {
            crate::domain::DeploymentHealth::Available => candidates.push(Candidate {
                deployment: deployment.id.clone(),
                node: node.id.clone(),
                remote_model: deployment.remote_model.clone(),
                position,
            }),
            crate::domain::DeploymentHealth::Unavailable(reason) => {
                skipped.push((deployment.id.clone(), reason));
            }
        }
    }

    if candidates.is_empty() {
        return Err(RoutingFailure::RouteUnavailable {
            route: route.name.clone(),
        });
    }
    Ok(Eligible {
        route: route.name.clone(),
        candidates,
        skipped,
        unfit: Vec::new(),
        route_size: route.deployments.len(),
    })
}

/// The stateless plan: the eligible set in configured order — exactly what a
/// priority route does, and the eligibility every other policy starts from.
pub fn plan(
    topology: &Topology,
    route: &Route,
    health: &BTreeMap<NodeId, NodeStatus>,
) -> Result<Plan, RoutingFailure> {
    eligible(topology, route, health).map(order_priority)
}

/// Priority: configured order, first eligible first.
pub fn order_priority(eligible: Eligible) -> Plan {
    Plan::from(eligible, RoutePolicy::Priority, Selection::Priority)
}

/// Round-robin: the eligible ring, starting at `cursor % len`.
///
/// The ring is the eligible deployments in configured order, so a deployment
/// that is unhealthy is simply not in it — the rotation never lands on it to
/// keep a count — and one that recovers rejoins it on the next request. The
/// rest of the ring after the chosen one is where failover goes.
pub fn order_round_robin(mut eligible: Eligible, cursor: u64) -> Plan {
    let len = eligible.candidates.len().max(1);
    let start = usize::try_from(cursor % len as u64).unwrap_or(0);
    eligible.candidates.rotate_left(start);
    Plan::from(
        eligible,
        RoutePolicy::RoundRobin,
        Selection::RoundRobin {
            cursor,
            selected_index: start,
        },
    )
}

/// What least-busy knows about one candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Load {
    /// Requests this router has in flight on the deployment.
    pub active: u64,
    /// The node's advertised slot count, if the router has observed one.
    pub limit: Option<u32>,
}

impl Load {
    /// The limit, if it can be divided by: known and positive.
    fn capacity(self) -> Option<u64> {
        self.limit.filter(|limit| *limit > 0).map(u64::from)
    }
}

/// Compare two loads, ignoring configured order.
///
/// * Known capacity before unknown or zero capacity: a deployment that has not
///   said how much it can take is never assumed to have room.
/// * Between two known capacities, the lower `active / limit`, compared
///   exactly as `a.active × b.limit` against `b.active × a.limit` — no
///   division, no floating point, and no overflow in `u128`.
/// * Between two unknowns, the fewer in flight.
fn compare_load(a: Load, b: Load) -> Order {
    match (a.capacity(), b.capacity()) {
        (Some(limit_a), Some(limit_b)) => (u128::from(a.active) * u128::from(limit_b))
            .cmp(&(u128::from(b.active) * u128::from(limit_a))),
        (Some(_), None) => Order::Less,
        (None, Some(_)) => Order::Greater,
        (None, None) => a.active.cmp(&b.active),
    }
}

/// Least-busy: lowest normalized load first, ties to configured order.
///
/// `loads` is aligned with `eligible.candidates`. The whole order is fixed
/// here, so failover walks the loads as they were when the request was
/// planned rather than re-reading them mid-request.
pub fn order_least_busy(eligible: Eligible, loads: &[Load]) -> Plan {
    let unknown = Load {
        active: 0,
        limit: None,
    };
    let mut ranked: Vec<(Candidate, Load)> = eligible
        .candidates
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, candidate)| (candidate, loads.get(index).copied().unwrap_or(unknown)))
        .collect();
    ranked.sort_by(|(a, load_a), (b, load_b)| {
        compare_load(*load_a, *load_b).then(a.position.cmp(&b.position))
    });

    let (active_before, concurrency_limit) = ranked.first().map_or((0, None), |(_, load)| {
        (
            load.active,
            load.capacity().and_then(|c| u32::try_from(c).ok()),
        )
    });
    let tied = match (ranked.first(), ranked.get(1)) {
        (Some((_, first)), Some((_, second))) => compare_load(*first, *second) == Order::Equal,
        _ => false,
    };
    let candidates = ranked.into_iter().map(|(candidate, _)| candidate).collect();
    Plan::from(
        Eligible {
            candidates,
            ..eligible
        },
        RoutePolicy::LeastBusy,
        Selection::LeastBusy {
            active_before,
            concurrency_limit,
            tied,
        },
    )
}

/// Per-route state for one route.
#[derive(Debug, Default)]
struct RouteState {
    /// Round-robin's position. Advanced once per planned request with a single
    /// atomic `fetch_add`, so two concurrent requests always draw different
    /// values without a lock.
    cursor: AtomicU64,
    /// Held by least-busy while it reads loads, chooses, and reserves the
    /// chosen slot, so two concurrent requests cannot both see the same idle
    /// deployment and both take it. Per route: other routes never wait on it.
    least_busy: Mutex<()>,
}

/// Plans requests with the state the policies need.
#[derive(Debug)]
pub struct Selector {
    /// Aligned with `Topology::routes()`.
    routes: Vec<RouteState>,
    load: Arc<LoadBook>,
}

impl Selector {
    pub fn new(topology: &Topology, load: Arc<LoadBook>) -> Self {
        Self {
            routes: topology
                .routes()
                .iter()
                .map(|_| RouteState::default())
                .collect(),
            load,
        }
    }

    pub fn load(&self) -> &Arc<LoadBook> {
        &self.load
    }

    /// Round-robin's cursor for a route: how many requests it has planned.
    pub fn cursor(&self, topology: &Topology, route: &RouteName) -> Option<u64> {
        let index = topology.routes().iter().position(|r| &r.name == route)?;
        Some(self.routes.get(index)?.cursor.load(Ordering::Relaxed))
    }

    /// Plan with nothing required of the deployments beyond availability.
    pub fn plan(
        &self,
        topology: &Topology,
        route: &Route,
        health: &BTreeMap<NodeId, NodeStatus>,
        observed: &BTreeMap<DeploymentId, DeploymentObservation>,
    ) -> Result<Plan, RoutingFailure> {
        self.plan_request(
            topology,
            route,
            health,
            observed,
            &RequestRequirements::none(),
        )
    }

    /// Plan one request: eligibility, then what this request needs, then the
    /// route's policy, then a slot reserved on the first choice.
    ///
    /// The policy below is handed only deployments that passed both filters,
    /// and does not know there were any.
    pub fn plan_request(
        &self,
        topology: &Topology,
        route: &Route,
        health: &BTreeMap<NodeId, NodeStatus>,
        observed: &BTreeMap<DeploymentId, DeploymentObservation>,
        needs: &RequestRequirements,
    ) -> Result<Plan, RoutingFailure> {
        self.plan_with_affinity(topology, route, health, observed, needs, None)
    }

    /// [`Self::plan_request`], preferring `sticky` — the deployment the
    /// request's session last succeeded on — if, and only if, it survived
    /// both filters.
    ///
    /// On a hit the policy takes no turn for the first choice: no cursor value
    /// is drawn and no loads are compared for it. What follows the sticky
    /// deployment is the policy's own failover order of the rest. On a miss
    /// the plan is exactly the one [`Self::plan_request`] would make, and
    /// [`Plan::sticky`] says why the sticky deployment was passed over.
    pub fn plan_with_affinity(
        &self,
        topology: &Topology,
        route: &Route,
        health: &BTreeMap<NodeId, NodeStatus>,
        observed: &BTreeMap<DeploymentId, DeploymentObservation>,
        needs: &RequestRequirements,
        sticky: Option<&DeploymentId>,
    ) -> Result<Plan, RoutingFailure> {
        let eligible = eligible(topology, route, health)?;
        let eligible = crate::capability::filter(eligible, needs, observed)?;
        let state = topology
            .routes()
            .iter()
            .position(|r| r.name == route.name)
            .and_then(|index| self.routes.get(index));

        let standing = sticky.map(|id| sticky_standing(&eligible, id));
        if let Some(Ok(index)) = standing {
            return Ok(self.plan_sticky(route.policy, state, eligible, index, observed));
        }

        let mut plan = match (route.policy, state) {
            (RoutePolicy::RoundRobin, Some(state)) => {
                let cursor = state.cursor.fetch_add(1, Ordering::Relaxed);
                order_round_robin(eligible, cursor)
            }
            (RoutePolicy::LeastBusy, Some(state)) => {
                let _choosing = state
                    .least_busy
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let loads = self.loads(&eligible.candidates, observed);
                let mut plan = order_least_busy(eligible, &loads);
                // Reserved before the lock is released: the next request to
                // read the loads sees this one already counted.
                plan.reservation = plan
                    .candidates
                    .first()
                    .map(|first| self.load.acquire(&first.deployment));
                plan.sticky = standing.and_then(Result::err).map(Sticky::Broken);
                return Ok(plan);
            }
            // Priority, or a route the selector does not know (which
            // validation makes impossible): configured order.
            _ => order_priority(eligible),
        };
        plan.reservation = plan
            .candidates
            .first()
            .map(|first| self.load.acquire(&first.deployment));
        plan.sticky = standing.and_then(Result::err).map(Sticky::Broken);
        Ok(plan)
    }

    /// The plan for an affinity hit: the sticky candidate first, then the rest
    /// in the order the policy would fail over through.
    fn plan_sticky(
        &self,
        policy: RoutePolicy,
        state: Option<&RouteState>,
        mut eligible: Eligible,
        index: usize,
        observed: &BTreeMap<DeploymentId, DeploymentObservation>,
    ) -> Plan {
        let finish = |mut plan: Plan| {
            plan.sticky = Some(Sticky::Hit);
            plan.reservation = plan
                .candidates
                .first()
                .map(|first| self.load.acquire(&first.deployment));
            plan
        };
        match (policy, state) {
            (RoutePolicy::RoundRobin, Some(_)) => {
                // The ring from the sticky deployment on: its successors, in
                // ring order, are where a round-robin failover goes.
                eligible.candidates.rotate_left(index);
                finish(Plan::from(
                    eligible,
                    RoutePolicy::RoundRobin,
                    Selection::SessionAffinity,
                ))
            }
            (RoutePolicy::LeastBusy, Some(state)) => {
                // Under the route's lock, like any least-busy choice, so the
                // slot taken on the sticky deployment is counted before the
                // next request reads the loads.
                let _choosing = state
                    .least_busy
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let first = eligible.candidates.remove(index);
                let loads = self.loads(&eligible.candidates, observed);
                let mut rest = order_least_busy(eligible, &loads);
                rest.candidates.insert(0, first);
                rest.selection = Selection::SessionAffinity;
                finish(rest)
            }
            _ => {
                let first = eligible.candidates.remove(index);
                eligible.candidates.insert(0, first);
                finish(Plan::from(eligible, policy, Selection::SessionAffinity))
            }
        }
    }

    fn loads(
        &self,
        candidates: &[Candidate],
        observed: &BTreeMap<DeploymentId, DeploymentObservation>,
    ) -> Vec<Load> {
        candidates
            .iter()
            .map(|candidate| Load {
                active: self.load.active(&candidate.deployment),
                limit: observed
                    .get(&candidate.deployment)
                    .map(|seen| seen.max_concurrent_requests),
            })
            .collect()
    }
}

/// Where a sticky deployment stands after both filters: its index among the
/// candidates, or why it is not one.
fn sticky_standing(eligible: &Eligible, sticky: &DeploymentId) -> Result<usize, Reassignment> {
    if let Some(index) = eligible
        .candidates
        .iter()
        .position(|candidate| &candidate.deployment == sticky)
    {
        return Ok(index);
    }
    if eligible.unfit.iter().any(|(id, _)| id == sticky) {
        return Err(Reassignment::StickyCapabilityMismatch);
    }
    match eligible.skipped.iter().find(|(id, _)| id == sticky) {
        Some((_, UnavailableReason::NodeUnhealthy | UnavailableReason::NodeUnknown)) => {
            Err(Reassignment::StickyUnhealthy)
        }
        // Disabled, swapped to another model, or (after a configuration
        // change, which this router does not do live) no longer in the route.
        _ => Err(Reassignment::StickyUnavailable),
    }
}

/// What a route can promise a client right now.
#[derive(Clone, Debug)]
pub struct RouteSummary {
    pub available: bool,
    /// The smallest context among the deployments a request could be sent to
    /// now. `None` when that is no deployment, or when any of them has no
    /// observed context: a number the router cannot vouch for is not
    /// advertised.
    pub context_length: Option<u32>,
    /// What every deployment a request could be sent to now supports.
    pub capabilities: CapabilitySet,
    pub max_concurrent_requests: Option<u32>,
}

/// Summarize a route over **exactly** the [`eligible`] deployments — the set
/// every policy chooses from.
///
/// One eligible set for routing and for reporting, so the invariant holds by
/// construction: a route never advertises more context, or a feature, than a
/// deployment it might currently send the request to can serve. The
/// per-deployment observations are read, never narrowed: the summary is
/// computed on each call and stored nowhere.
pub fn summarize(
    topology: &Topology,
    route: &Route,
    health: &BTreeMap<NodeId, NodeStatus>,
    observed: &BTreeMap<DeploymentId, DeploymentObservation>,
) -> RouteSummary {
    let Ok(plan) = eligible(topology, route, health) else {
        return RouteSummary {
            available: false,
            context_length: None,
            capabilities: CapabilitySet::none(),
            max_concurrent_requests: None,
        };
    };
    let mut context: Option<u32> = None;
    let mut capabilities: Option<CapabilitySet> = None;
    let mut concurrency: Option<u32> = None;
    let mut every_one_observed = true;
    for candidate in &plan.candidates {
        let Some(seen) = observed.get(&candidate.deployment) else {
            every_one_observed = false;
            continue;
        };
        context = Some(context.map_or(seen.context_length, |c| c.min(seen.context_length)));
        capabilities = Some(match capabilities {
            Some(so_far) => so_far.intersect(&seen.capabilities),
            None => seen.capabilities.clone(),
        });
        concurrency = Some(concurrency.map_or(seen.max_concurrent_requests, |c| {
            c.min(seen.max_concurrent_requests)
        }));
    }
    if !every_one_observed {
        // An eligible deployment the router has no figures for could be sent
        // the request, so nothing narrower than "unknown" is safe to claim.
        return RouteSummary {
            available: true,
            context_length: None,
            capabilities: CapabilitySet::none(),
            max_concurrent_requests: None,
        };
    }
    RouteSummary {
        available: true,
        context_length: context,
        capabilities: capabilities.unwrap_or_else(CapabilitySet::none),
        max_concurrent_requests: concurrency,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RouterFile, validate};
    use crate::domain::CapabilitySet;
    use crate::health::{Observation, Outcome, ServedModel, apply};
    use serde_json::json;
    use std::time::SystemTime;

    fn topology(nodes_enabled: [bool; 2]) -> Topology {
        let file: RouterFile = serde_json::from_value(json!({
            "default_route": "Fast",
            "nodes": [
                {"id": "dell", "url": "http://192.0.2.10:11434", "enabled": nodes_enabled[0]},
                {"id": "t420", "url": "http://192.0.2.11:11434", "enabled": nodes_enabled[1]}
            ],
            "routes": [
                {"name": "Coder", "deployments": [
                    {"node": "dell", "model": "QwenCoder"},
                    {"node": "t420", "model": "CoderBackup"}
                ]},
                {"name": "Fast", "deployments": [{"node": "t420", "model": "Fast"}]}
            ]
        }))
        .unwrap();
        validate(file, &|_| None).expect("valid").topology
    }

    fn serving(model: &str) -> NodeStatus {
        let mut status = NodeStatus::default();
        apply(
            &mut status,
            Outcome::Success(Observation {
                served: Some(ServedModel {
                    id: model.into(),
                    context_length: 4096,
                }),
                features: CapabilitySet::none(),
                max_concurrent_requests: 1,
                version: "0.4.1".into(),
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
            Outcome::Failure("could not connect".into()),
            1,
            SystemTime::now(),
        );
        status
    }

    fn health(dell: NodeStatus, t420: NodeStatus) -> BTreeMap<NodeId, NodeStatus> {
        BTreeMap::from([
            (NodeId::parse("dell").unwrap(), dell),
            (NodeId::parse("t420").unwrap(), t420),
        ])
    }

    fn route<'a>(topology: &'a Topology, name: &str) -> &'a Route {
        topology.resolve(Some(name)).expect("route")
    }

    #[test]
    fn a_single_healthy_deployment_is_explicit() {
        let t = topology([true, true]);
        let plan = plan(&t, route(&t, "Fast"), &health(down(), serving("Fast"))).unwrap();
        let decision = plan.decision(0).unwrap();
        assert_eq!(decision.deployment.as_str(), "t420/Fast");
        assert_eq!(decision.reason, RoutingReason::ExplicitSingleDeployment);
    }

    #[test]
    fn a_healthy_primary_is_chosen_first() {
        let t = topology([true, true]);
        let plan = plan(
            &t,
            route(&t, "Coder"),
            &health(serving("QwenCoder"), serving("CoderBackup")),
        )
        .unwrap();
        assert_eq!(plan.candidates.len(), 2);
        let decision = plan.decision(0).unwrap();
        assert_eq!(decision.node.as_str(), "dell");
        assert_eq!(decision.reason, RoutingReason::PrimaryHealthy);
        // If the primary fails before answering, the backup is next, and says
        // why it was reached.
        let fallback = plan.decision(1).unwrap();
        assert_eq!(fallback.node.as_str(), "t420");
        assert_eq!(fallback.reason, RoutingReason::PrimaryFailedFallback);
    }

    #[test]
    fn an_unhealthy_primary_falls_back() {
        let t = topology([true, true]);
        let plan = plan(
            &t,
            route(&t, "Coder"),
            &health(down(), serving("CoderBackup")),
        )
        .unwrap();
        assert_eq!(plan.candidates.len(), 1);
        let decision = plan.decision(0).unwrap();
        assert_eq!(decision.deployment.as_str(), "t420/CoderBackup");
        assert_eq!(decision.reason, RoutingReason::PrimaryUnavailableFallback);
        assert_eq!(
            plan.skipped,
            [(
                DeploymentId::of(&NodeId::parse("dell").unwrap(), "QwenCoder"),
                UnavailableReason::NodeUnhealthy
            )]
        );
    }

    #[test]
    fn a_primary_serving_another_model_falls_back() {
        let t = topology([true, true]);
        let plan = plan(
            &t,
            route(&t, "Coder"),
            &health(serving("SomethingElse"), serving("CoderBackup")),
        )
        .unwrap();
        assert_eq!(plan.decision(0).unwrap().node.as_str(), "t420");
        assert_eq!(plan.skipped[0].1, UnavailableReason::ModelNotServed);
    }

    #[test]
    fn all_unhealthy_is_route_unavailable_not_model_not_found() {
        let t = topology([true, true]);
        let failure = plan(&t, route(&t, "Coder"), &health(down(), down())).unwrap_err();
        assert!(matches!(
            failure,
            RoutingFailure::RouteUnavailable { ref route } if route.as_str() == "Coder"
        ));
    }

    #[test]
    fn a_disabled_node_is_skipped_even_when_it_would_answer() {
        let t = topology([false, true]);
        let plan = plan(
            &t,
            route(&t, "Coder"),
            &health(serving("QwenCoder"), serving("CoderBackup")),
        )
        .unwrap();
        assert_eq!(plan.decision(0).unwrap().node.as_str(), "t420");
        assert_eq!(plan.skipped[0].1, UnavailableReason::NodeDisabled);
    }

    #[test]
    fn a_node_never_probed_is_not_eligible() {
        let t = topology([true, true]);
        let failure = plan(&t, route(&t, "Fast"), &BTreeMap::new()).unwrap_err();
        assert!(matches!(failure, RoutingFailure::RouteUnavailable { .. }));
    }

    #[test]
    fn a_recovered_primary_is_first_again() {
        let t = topology([true, true]);
        let while_down = plan(
            &t,
            route(&t, "Coder"),
            &health(down(), serving("CoderBackup")),
        )
        .unwrap();
        assert_eq!(while_down.decision(0).unwrap().node.as_str(), "t420");
        let recovered = plan(
            &t,
            route(&t, "Coder"),
            &health(serving("QwenCoder"), serving("CoderBackup")),
        )
        .unwrap();
        assert_eq!(recovered.decision(0).unwrap().node.as_str(), "dell");
    }

    #[test]
    fn unknown_routes_and_the_default_resolve_without_guessing() {
        let t = topology([true, true]);
        assert!(matches!(
            t.resolve(Some("Research")),
            Err(RoutingFailure::UnknownRoute { ref requested }) if requested == "Research"
        ));
        assert_eq!(t.resolve(Some("default")).unwrap().name.as_str(), "Fast");
        assert_eq!(t.resolve(None).unwrap().name.as_str(), "Fast");
        assert_eq!(t.resolve(Some("  ")).unwrap().name.as_str(), "Fast");
        assert_eq!(t.resolve(Some("coder")).unwrap().name.as_str(), "Coder");

        let mut without_default = t.clone();
        without_default.default_route = None;
        assert_eq!(
            without_default.resolve(Some("default")).unwrap_err(),
            RoutingFailure::NoDefaultRoute
        );
        assert_eq!(
            without_default.resolve(None).unwrap_err(),
            RoutingFailure::NoDefaultRoute
        );
    }

    fn observation(tools: bool, context_length: u32) -> DeploymentObservation {
        let mut capabilities = CapabilitySet::none();
        capabilities.0.streaming = true;
        capabilities.0.tools = tools;
        DeploymentObservation {
            capabilities,
            context_length,
            max_concurrent_requests: 1,
            observed_at: SystemTime::now(),
        }
    }

    fn observed() -> BTreeMap<DeploymentId, DeploymentObservation> {
        BTreeMap::from([
            (
                DeploymentId::of(&NodeId::parse("dell").unwrap(), "QwenCoder"),
                observation(true, 32_768),
            ),
            (
                DeploymentId::of(&NodeId::parse("t420").unwrap(), "CoderBackup"),
                observation(false, 8_192),
            ),
        ])
    }

    #[test]
    fn a_route_promises_only_what_every_eligible_deployment_can_serve() {
        let t = topology([true, true]);
        let both = summarize(
            &t,
            route(&t, "Coder"),
            &health(serving("QwenCoder"), serving("CoderBackup")),
            &observed(),
        );
        assert!(both.available);
        assert_eq!(both.context_length, Some(8_192), "the smaller of the two");
        assert!(!both.capabilities.0.tools, "the backup has no tools");
        assert!(both.capabilities.0.streaming);

        // With the backup out of the eligible set, the route promises what the
        // primary alone can do - context and tools both.
        let primary_only = summarize(
            &t,
            route(&t, "Coder"),
            &health(serving("QwenCoder"), down()),
            &observed(),
        );
        assert_eq!(primary_only.context_length, Some(32_768));
        assert!(primary_only.capabilities.0.tools);

        // The per-deployment figures the summary was built from are unchanged.
        let kept = observed();
        assert!(kept.values().any(|seen| seen.capabilities.0.tools));
    }

    #[test]
    fn a_route_with_nothing_eligible_or_unobserved_promises_nothing() {
        let t = topology([true, true]);
        let none = summarize(&t, route(&t, "Coder"), &health(down(), down()), &observed());
        assert!(!none.available);
        assert_eq!(none.context_length, None);
        assert!(!none.capabilities.0.streaming);

        let unobserved = summarize(
            &t,
            route(&t, "Coder"),
            &health(serving("QwenCoder"), serving("CoderBackup")),
            &BTreeMap::new(),
        );
        assert!(unobserved.available);
        assert_eq!(
            unobserved.context_length, None,
            "no number it cannot vouch for"
        );
    }
    // --- R4: round-robin and least-busy ------------------------------------

    use crate::load::LoadBook;
    use std::sync::Arc;

    /// Three nodes, one route of three deployments under `strategy`.
    fn ring(strategy: &str) -> (Topology, Arc<Selector>) {
        let file: RouterFile = serde_json::from_value(json!({
            "nodes": [
                {"id": "a", "url": "http://192.0.2.10:11434"},
                {"id": "b", "url": "http://192.0.2.11:11434"},
                {"id": "c", "url": "http://192.0.2.12:11434", "enabled": true}
            ],
            "routes": [{"name": "Coder", "strategy": strategy, "deployments": [
                {"node": "a", "model": "A"},
                {"node": "b", "model": "B"},
                {"node": "c", "model": "C"}
            ]}]
        }))
        .unwrap();
        let topology = validate(file, &|_| None).expect("valid").topology;
        let selector = Arc::new(Selector::new(&topology, Arc::new(LoadBook::new(&topology))));
        (topology, selector)
    }

    fn all_up() -> BTreeMap<NodeId, NodeStatus> {
        BTreeMap::from([
            (NodeId::parse("a").unwrap(), serving("A")),
            (NodeId::parse("b").unwrap(), serving("B")),
            (NodeId::parse("c").unwrap(), serving("C")),
        ])
    }

    fn with(
        mut health: BTreeMap<NodeId, NodeStatus>,
        node: &str,
        status: NodeStatus,
    ) -> BTreeMap<NodeId, NodeStatus> {
        health.insert(NodeId::parse(node).unwrap(), status);
        health
    }

    fn limits(
        a: Option<u32>,
        b: Option<u32>,
        c: Option<u32>,
    ) -> BTreeMap<DeploymentId, DeploymentObservation> {
        [("a", "A", a), ("b", "B", b), ("c", "C", c)]
            .into_iter()
            .filter_map(|(node, model, limit)| {
                let limit = limit?;
                Some((
                    DeploymentId::of(&NodeId::parse(node).unwrap(), model),
                    DeploymentObservation {
                        capabilities: CapabilitySet::none(),
                        context_length: 4096,
                        max_concurrent_requests: limit,
                        observed_at: SystemTime::now(),
                    },
                ))
            })
            .collect()
    }

    /// Plan once and say which node was chosen. The reservation is dropped
    /// with the plan.
    fn choose(
        topology: &Topology,
        selector: &Selector,
        health: &BTreeMap<NodeId, NodeStatus>,
        observed: &BTreeMap<DeploymentId, DeploymentObservation>,
    ) -> String {
        let plan = selector
            .plan(topology, &topology.routes()[0], health, observed)
            .expect("a plan");
        plan.candidates[0].node.as_str().to_owned()
    }

    #[test]
    fn round_robin_rotates_in_configured_order() {
        let (t, selector) = ring("round_robin");
        let picks: Vec<String> = (0..6)
            .map(|_| choose(&t, &selector, &all_up(), &BTreeMap::new()))
            .collect();
        assert_eq!(picks, ["a", "b", "c", "a", "b", "c"]);
        assert_eq!(selector.cursor(&t, &t.routes()[0].name), Some(6));
    }

    #[test]
    fn round_robin_skips_an_unhealthy_deployment_and_takes_it_back_on_recovery() {
        let (t, selector) = ring("round_robin");
        let b_down = with(all_up(), "b", down());
        let picks: Vec<String> = (0..4)
            .map(|_| choose(&t, &selector, &b_down, &BTreeMap::new()))
            .collect();
        assert_eq!(
            picks,
            ["a", "c", "a", "c"],
            "b is never its turn while down"
        );

        // b recovers: it is back in the ring from the next request on.
        let picks: Vec<String> = (0..3)
            .map(|_| choose(&t, &selector, &all_up(), &BTreeMap::new()))
            .collect();
        let mut sorted = picks.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            ["a", "b", "c"],
            "one full turn includes b again: {picks:?}"
        );
    }

    #[test]
    fn round_robin_failover_continues_round_the_ring_without_another_step() {
        let (t, selector) = ring("round_robin");
        let _first = choose(&t, &selector, &all_up(), &BTreeMap::new());
        let plan = selector
            .plan(&t, &t.routes()[0], &all_up(), &BTreeMap::new())
            .unwrap();
        let order: Vec<&str> = plan.candidates.iter().map(|c| c.node.as_str()).collect();
        assert_eq!(
            order,
            ["b", "c", "a"],
            "the turn, then the rest of the ring"
        );
        assert_eq!(plan.decision(0).unwrap().reason, RoutingReason::RoundRobin);
        assert_eq!(
            plan.decision(1).unwrap().reason,
            RoutingReason::RoundRobinFailover
        );
        // Planning is the only thing that advances the cursor; walking the
        // plan's failover candidates does not.
        assert_eq!(selector.cursor(&t, &t.routes()[0].name), Some(2));
    }

    #[test]
    fn round_robin_under_concurrency_shares_turns_exactly_and_never_picks_the_unhealthy() {
        let (t, selector) = ring("round_robin");
        let t = Arc::new(t);
        let b_down = Arc::new(with(all_up(), "b", down()));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (t, selector, health) =
                    (Arc::clone(&t), Arc::clone(&selector), Arc::clone(&b_down));
                std::thread::spawn(move || {
                    (0..300)
                        .map(|_| choose(&t, &selector, &health, &BTreeMap::new()))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut counts = BTreeMap::<String, usize>::new();
        for thread in threads {
            for pick in thread.join().unwrap() {
                *counts.entry(pick).or_default() += 1;
            }
        }
        // 2400 cursor values, each drawn once, over a ring of two.
        assert_eq!(counts.get("a"), Some(&1200));
        assert_eq!(counts.get("c"), Some(&1200));
        assert_eq!(counts.get("b"), None);
        assert_eq!(selector.cursor(&t, &t.routes()[0].name), Some(2400));
    }

    #[test]
    fn least_busy_compares_normalized_load_not_raw_counts() {
        let loads = [
            Load {
                active: 2,
                limit: Some(8),
            }, // 25%
            Load {
                active: 1,
                limit: Some(2),
            }, // 50%
        ];
        let (t, _) = ring("least_busy");
        let plan = order_least_busy(
            eligible(&t, &t.routes()[0], &with(all_up(), "c", down())).unwrap(),
            &loads,
        );
        assert_eq!(plan.candidates[0].node.as_str(), "a");
        assert_eq!(plan.decision(0).unwrap().reason, RoutingReason::LeastBusy);
        assert_eq!(
            plan.selection,
            Selection::LeastBusy {
                active_before: 2,
                concurrency_limit: Some(8),
                tied: false
            }
        );
        assert_eq!(
            plan.decision(1).unwrap().reason,
            RoutingReason::LeastBusyFailover
        );
    }

    #[test]
    fn least_busy_breaks_an_exact_tie_by_configured_order() {
        let (t, _) = ring("least_busy");
        let two = eligible(&t, &t.routes()[0], &with(all_up(), "c", down())).unwrap();
        // 1/4 and 2/8 are the same 25%.
        let plan = order_least_busy(
            two.clone(),
            &[
                Load {
                    active: 1,
                    limit: Some(4),
                },
                Load {
                    active: 2,
                    limit: Some(8),
                },
            ],
        );
        assert_eq!(plan.candidates[0].node.as_str(), "a");
        assert_eq!(
            plan.decision(0).unwrap().reason,
            RoutingReason::LeastBusyTiebreak
        );
        // Both full is a tie too: no router-side refusal, configured order.
        let plan = order_least_busy(
            two,
            &[
                Load {
                    active: 4,
                    limit: Some(4),
                },
                Load {
                    active: 2,
                    limit: Some(2),
                },
            ],
        );
        assert_eq!(plan.candidates[0].node.as_str(), "a");
    }

    #[test]
    fn unknown_or_zero_capacity_is_never_assumed_to_have_room() {
        let (t, _) = ring("least_busy");
        let three = eligible(&t, &t.routes()[0], &all_up()).unwrap();
        let plan = order_least_busy(
            three.clone(),
            &[
                Load {
                    active: 0,
                    limit: None,
                }, // unknown
                Load {
                    active: 0,
                    limit: Some(0),
                }, // zero
                Load {
                    active: 7,
                    limit: Some(8),
                }, // known, nearly full
            ],
        );
        let order: Vec<&str> = plan.candidates.iter().map(|c| c.node.as_str()).collect();
        assert_eq!(
            order,
            ["c", "a", "b"],
            "known capacity first; the rest stay as fallbacks"
        );

        // Among deployments with no usable capacity, fewer in flight first.
        let plan = order_least_busy(
            three,
            &[
                Load {
                    active: 3,
                    limit: None,
                },
                Load {
                    active: 1,
                    limit: Some(0),
                },
                Load {
                    active: 2,
                    limit: None,
                },
            ],
        );
        let order: Vec<&str> = plan.candidates.iter().map(|c| c.node.as_str()).collect();
        assert_eq!(order, ["b", "c", "a"]);
        assert_eq!(
            plan.selection,
            Selection::LeastBusy {
                active_before: 1,
                concurrency_limit: None,
                tied: false
            }
        );
    }

    #[test]
    fn least_busy_reserves_as_it_chooses_so_load_spreads_by_capacity() {
        let (t, selector) = ring("least_busy");
        let health = with(all_up(), "c", down());
        let observed = limits(Some(4), Some(2), Some(1));
        let route = &t.routes()[0];
        // Plans are held, so each one's reservation is still in flight when
        // the next request is planned.
        let mut held = Vec::new();
        let mut picks = Vec::new();
        for _ in 0..3 {
            let plan = selector.plan(&t, route, &health, &observed).unwrap();
            picks.push(plan.candidates[0].node.as_str().to_owned());
            held.push(plan);
        }
        // 0/4 vs 0/2: tie, a. Then 1/4 vs 0/2: b. Then 1/4 vs 1/2: a.
        assert_eq!(picks, ["a", "b", "a"]);
        let load = selector.load();
        assert_eq!(
            load.active(&DeploymentId::of(&NodeId::parse("a").unwrap(), "A")),
            2
        );
        drop(held);
        assert!(
            load.snapshot().values().all(|active| *active == 0),
            "every slot returned"
        );
    }

    #[test]
    fn least_busy_under_concurrency_gives_the_bigger_node_more_in_flight_work() {
        let (t, selector) = ring("least_busy");
        let t = Arc::new(t);
        let health = Arc::new(with(all_up(), "c", down()));
        let observed = Arc::new(limits(Some(4), Some(1), None));
        let barrier = Arc::new(std::sync::Barrier::new(10));
        let threads: Vec<_> = (0..10)
            .map(|_| {
                let (t, selector, health, observed, barrier) = (
                    Arc::clone(&t),
                    Arc::clone(&selector),
                    Arc::clone(&health),
                    Arc::clone(&observed),
                    Arc::clone(&barrier),
                );
                std::thread::spawn(move || {
                    barrier.wait();
                    let plan = selector
                        .plan(&t, &t.routes()[0], &health, &observed)
                        .unwrap();
                    let pick = plan.candidates[0].node.as_str().to_owned();
                    // Hold the slot until every thread has chosen.
                    barrier.wait();
                    pick
                })
            })
            .collect();
        let mut counts = BTreeMap::<String, usize>::new();
        for thread in threads {
            *counts.entry(thread.join().unwrap()).or_default() += 1;
        }
        // Ten held at once over 4 + 1 slots: the selection under the route
        // lock sees every earlier reservation, so it lands 8/4 vs 2/1 - in
        // proportion to capacity, whatever order the threads ran in.
        assert_eq!(counts.get("a"), Some(&8), "{counts:?}");
        assert_eq!(counts.get("b"), Some(&2), "{counts:?}");
        assert!(
            selector
                .load()
                .snapshot()
                .values()
                .all(|active| *active == 0)
        );
    }

    #[test]
    fn neither_new_policy_ever_selects_an_ineligible_deployment() {
        for strategy in ["round_robin", "least_busy"] {
            let (t, selector) = ring(strategy);
            // a unknown (never probed), b unhealthy, c serving another model:
            // nothing is eligible, and an idle count does not change that.
            let mut health = BTreeMap::new();
            health.insert(NodeId::parse("b").unwrap(), down());
            health.insert(NodeId::parse("c").unwrap(), serving("Other"));
            let failure = selector
                .plan(
                    &t,
                    &t.routes()[0],
                    &health,
                    &limits(Some(8), Some(8), Some(8)),
                )
                .unwrap_err();
            assert!(
                matches!(failure, RoutingFailure::RouteUnavailable { .. }),
                "{strategy}"
            );
            assert!(
                selector
                    .load()
                    .snapshot()
                    .values()
                    .all(|active| *active == 0)
            );

            // Only c recovers: every request goes to c, however idle a and b look.
            let health = with(health, "c", serving("C"));
            for _ in 0..5 {
                assert_eq!(
                    choose(&t, &selector, &health, &limits(Some(8), Some(8), Some(8))),
                    "c",
                    "{strategy}"
                );
            }
        }

        // And a disabled node, for both.
        for strategy in ["round_robin", "least_busy"] {
            let file: RouterFile = serde_json::from_value(json!({
                "nodes": [
                    {"id": "a", "url": "http://192.0.2.10:11434", "enabled": false},
                    {"id": "b", "url": "http://192.0.2.11:11434"}
                ],
                "routes": [{"name": "Coder", "strategy": strategy, "deployments": [
                    {"node": "a", "model": "A"}, {"node": "b", "model": "B"}
                ]}]
            }))
            .unwrap();
            let t = validate(file, &|_| None).unwrap().topology;
            let selector = Selector::new(&t, Arc::new(LoadBook::new(&t)));
            let health = BTreeMap::from([
                (NodeId::parse("a").unwrap(), serving("A")),
                (NodeId::parse("b").unwrap(), serving("B")),
            ]);
            for _ in 0..4 {
                assert_eq!(
                    choose(&t, &selector, &health, &BTreeMap::new()),
                    "b",
                    "{strategy}"
                );
            }
        }
    }

    #[test]
    fn priority_is_unchanged_through_the_selector() {
        let (t, selector) = ring("priority");
        for _ in 0..3 {
            assert_eq!(choose(&t, &selector, &all_up(), &BTreeMap::new()), "a");
        }
        assert_eq!(
            choose(
                &t,
                &selector,
                &with(all_up(), "a", down()),
                &BTreeMap::new()
            ),
            "b"
        );
        assert_eq!(
            selector.cursor(&t, &t.routes()[0].name),
            Some(0),
            "priority keeps no cursor"
        );
    }

    // --- R5: capability filtering before policy --------------------------

    use crate::proxy::Endpoint;
    use crate::requirements::{RequestRequirements, ToolChoiceRequirement};

    /// What one deployment offers, in the ring of a, b, c.
    #[derive(Clone, Copy)]
    struct Offers {
        tools: bool,
        reasoning: bool,
        context: u32,
        limit: u32,
    }

    const FULL: Offers = Offers {
        tools: true,
        reasoning: true,
        context: 32_768,
        limit: 4,
    };

    fn offering(a: Offers, b: Offers, c: Offers) -> BTreeMap<DeploymentId, DeploymentObservation> {
        [("a", "A", a), ("b", "B", b), ("c", "C", c)]
            .into_iter()
            .map(|(node, model, offers)| {
                let mut capabilities = CapabilitySet::none();
                let f = &mut capabilities.0;
                f.streaming = true;
                f.chat_completions = true;
                f.completions = true;
                f.tools = offers.tools;
                f.tool_choice = offers.tools;
                f.reasoning_content = offers.reasoning;
                (
                    DeploymentId::of(&NodeId::parse(node).unwrap(), model),
                    DeploymentObservation {
                        capabilities,
                        context_length: offers.context,
                        max_concurrent_requests: offers.limit,
                        observed_at: SystemTime::now(),
                    },
                )
            })
            .collect()
    }

    fn chat_needing(tools: bool, reasoning: bool, prompt_tokens: u32) -> RequestRequirements {
        RequestRequirements {
            endpoint: Some(Endpoint::ChatCompletions),
            tools,
            reasoning,
            prompt_tokens: Some(prompt_tokens),
            ..RequestRequirements::none()
        }
    }

    fn pick(
        t: &Topology,
        selector: &Selector,
        health: &BTreeMap<NodeId, NodeStatus>,
        observed: &BTreeMap<DeploymentId, DeploymentObservation>,
        needs: &RequestRequirements,
    ) -> Plan {
        selector
            .plan_request(t, &t.routes()[0], health, observed, needs)
            .expect("a plan")
    }

    fn nodes(plan: &Plan) -> Vec<&str> {
        plan.candidates.iter().map(|c| c.node.as_str()).collect()
    }

    #[test]
    fn priority_goes_straight_to_the_first_capable_deployment() {
        let (t, selector) = ring("priority");
        let no_tools = Offers {
            tools: false,
            ..FULL
        };
        let observed = offering(no_tools, FULL, FULL);
        let plan = pick(
            &t,
            &selector,
            &all_up(),
            &observed,
            &chat_needing(true, false, 10),
        );
        // a was never eligible for this request, so b is a first choice, not a
        // fallback from a failure, and a is not even a failover.
        assert_eq!(nodes(&plan), ["b", "c"]);
        assert_eq!(
            plan.decision(0).unwrap().reason,
            RoutingReason::PrimaryUnavailableFallback
        );
        assert_eq!(
            plan.unfit,
            [(
                DeploymentId::of(&NodeId::parse("a").unwrap(), "A"),
                vec![CapabilityGap::ToolsUnsupported]
            )]
        );
        // An ordinary request still starts at a.
        let plan = pick(
            &t,
            &selector,
            &all_up(),
            &observed,
            &chat_needing(false, false, 10),
        );
        assert_eq!(nodes(&plan), ["a", "b", "c"]);
        assert!(plan.unfit.is_empty());
    }

    #[test]
    fn round_robin_rotates_only_over_the_capable_ring() {
        let (t, selector) = ring("round_robin");
        let observed = offering(
            FULL,
            Offers {
                tools: false,
                ..FULL
            },
            FULL,
        );
        let tools = chat_needing(true, false, 10);
        let picks: Vec<String> = (0..6)
            .map(|_| nodes(&pick(&t, &selector, &all_up(), &observed, &tools))[0].to_owned())
            .collect();
        assert_eq!(picks, ["a", "c", "a", "c", "a", "c"], "b is never its turn");

        // Mixed traffic: the ring is each request's own eligible set, so its
        // length changes between requests, and nothing panics or picks b for
        // a tool request.
        for round in 0..30 {
            let needs = chat_needing(round % 2 == 0, false, 10);
            let plan = pick(&t, &selector, &all_up(), &observed, &needs);
            if needs.tools {
                assert_ne!(nodes(&plan)[0], "b");
                assert_eq!(plan.candidates.len(), 2);
            } else {
                assert_eq!(plan.candidates.len(), 3);
            }
        }
    }

    #[test]
    fn least_busy_compares_only_capable_deployments_however_idle_the_others() {
        let (t, selector) = ring("least_busy");
        // a 2/4 = 50%, b idle but without tools, c 1/4 = 25%.
        let observed = offering(
            FULL,
            Offers {
                tools: false,
                ..FULL
            },
            FULL,
        );
        let load = selector.load();
        let (a, c) = (
            DeploymentId::of(&NodeId::parse("a").unwrap(), "A"),
            DeploymentId::of(&NodeId::parse("c").unwrap(), "C"),
        );
        let _held = [load.acquire(&a), load.acquire(&a), load.acquire(&c)];
        let plan = pick(
            &t,
            &selector,
            &all_up(),
            &observed,
            &chat_needing(true, false, 10),
        );
        assert_eq!(nodes(&plan), ["c", "a"], "c, then a; never b");
        assert_eq!(
            plan.selection,
            Selection::LeastBusy {
                active_before: 1,
                concurrency_limit: Some(4),
                tied: false
            }
        );
        // Without tools, the idle b wins as before.
        drop(plan);
        let plan = pick(
            &t,
            &selector,
            &all_up(),
            &observed,
            &chat_needing(false, false, 10),
        );
        assert_eq!(nodes(&plan)[0], "b");
    }

    #[test]
    fn requirements_are_intersected_and_more_capability_is_never_preferred() {
        // The all-capabilities example: only c offers tools, reasoning and
        // 20K together.
        let observed = offering(
            Offers {
                reasoning: false,
                ..FULL
            },
            Offers {
                context: 8_192,
                ..FULL
            },
            FULL,
        );
        for strategy in ["priority", "round_robin", "least_busy"] {
            let (t, selector) = ring(strategy);
            for _ in 0..3 {
                let plan = pick(
                    &t,
                    &selector,
                    &all_up(),
                    &observed,
                    &chat_needing(true, true, 20_000),
                );
                assert_eq!(nodes(&plan), ["c"], "{strategy}");
                assert_eq!(plan.unfit.len(), 2);
            }
            // Pairs of requirements narrow to the deployments meeting both.
            let tools_and_large = pick(
                &t,
                &selector,
                &all_up(),
                &observed,
                &chat_needing(true, false, 20_000),
            );
            let mut got = nodes(&tools_and_large);
            got.sort_unstable();
            assert_eq!(got, ["a", "c"], "{strategy}");
            let reasoning_and_small = pick(
                &t,
                &selector,
                &all_up(),
                &observed,
                &chat_needing(false, true, 100),
            );
            let mut got = nodes(&reasoning_and_small);
            got.sort_unstable();
            assert_eq!(got, ["b", "c"], "{strategy}");
        }

        // A plain request keeps a plain deployment: capability is a floor.
        let (t, selector) = ring("priority");
        let plain = Offers {
            tools: false,
            reasoning: false,
            ..FULL
        };
        let plan = pick(
            &t,
            &selector,
            &all_up(),
            &offering(plain, FULL, FULL),
            &chat_needing(false, false, 10),
        );
        assert_eq!(nodes(&plan), ["a", "b", "c"]);
    }

    #[test]
    fn a_request_no_available_deployment_can_serve_is_a_mismatch_naming_what_was_missing() {
        let (t, selector) = ring("round_robin");
        let observed = offering(
            Offers {
                tools: false,
                ..FULL
            },
            Offers {
                tools: false,
                context: 1024,
                ..FULL
            },
            Offers {
                tools: false,
                ..FULL
            },
        );
        let failure = selector
            .plan_request(
                &t,
                &t.routes()[0],
                &all_up(),
                &observed,
                &chat_needing(true, false, 4096),
            )
            .unwrap_err();
        let RoutingFailure::CapabilityMismatch {
            route,
            unmet,
            unfit,
        } = failure
        else {
            panic!("expected a mismatch, got {failure:?}");
        };
        assert_eq!(route.as_str(), "Coder");
        assert_eq!(
            unmet,
            [
                CapabilityGap::ToolsUnsupported,
                CapabilityGap::ContextTooSmall
            ]
        );
        assert_eq!(unfit.len(), 3);
        // Nothing was reserved, and the cursor did not move.
        assert!(selector.load().snapshot().values().all(|n| *n == 0));
        assert_eq!(selector.cursor(&t, &t.routes()[0].name), Some(0));

        // All down is still route_unavailable, whatever the request needs.
        let all_down = with(with(with(all_up(), "a", down()), "b", down()), "c", down());
        assert!(matches!(
            selector.plan_request(
                &t,
                &t.routes()[0],
                &all_down,
                &observed,
                &chat_needing(true, false, 10)
            ),
            Err(RoutingFailure::RouteUnavailable { .. })
        ));
    }

    #[test]
    fn when_only_a_down_deployment_could_serve_it_the_route_is_unavailable_not_mismatched() {
        let (t, selector) = ring("priority");
        // Only c has tools, and c is down: waiting fixes this, changing the
        // request does not.
        let no_tools = Offers {
            tools: false,
            ..FULL
        };
        let observed = offering(no_tools, no_tools, FULL);
        let failure = selector
            .plan_request(
                &t,
                &t.routes()[0],
                &with(all_up(), "c", down()),
                &observed,
                &chat_needing(true, false, 10),
            )
            .unwrap_err();
        assert!(
            matches!(failure, RoutingFailure::RouteUnavailable { .. }),
            "{failure:?}"
        );
    }

    #[test]
    fn a_capability_change_is_seen_on_the_next_observation() {
        let (t, selector) = ring("priority");
        let needs = chat_needing(true, false, 10);
        let before = offering(
            Offers {
                tools: false,
                ..FULL
            },
            FULL,
            FULL,
        );
        assert_eq!(
            nodes(&pick(&t, &selector, &all_up(), &before, &needs))[0],
            "b"
        );
        // a's node swapped to a model with tools; the next probe files it.
        let after = offering(FULL, FULL, FULL);
        assert_eq!(
            nodes(&pick(&t, &selector, &all_up(), &after, &needs))[0],
            "a"
        );
    }

    #[test]
    fn a_plan_outside_a_request_requires_nothing() {
        let (t, selector) = ring("priority");
        // No observations at all: plan() behaves exactly as before R5.
        let plan = selector
            .plan(&t, &t.routes()[0], &all_up(), &BTreeMap::new())
            .unwrap();
        assert_eq!(nodes(&plan), ["a", "b", "c"]);
        // A real request must be vouched for: an unobserved deployment is not.
        let failure = selector
            .plan_request(
                &t,
                &t.routes()[0],
                &all_up(),
                &BTreeMap::new(),
                &chat_needing(false, false, 1),
            )
            .unwrap_err();
        assert!(matches!(
            failure,
            RoutingFailure::CapabilityMismatch { ref unmet, .. } if unmet == &[CapabilityGap::Unobserved]
        ));
    }

    #[test]
    fn a_forced_tool_choice_skips_a_deployment_that_takes_tools_but_not_tool_choice() {
        let (t, selector) = ring("round_robin");
        let mut observed = offering(FULL, FULL, FULL);
        let b = DeploymentId::of(&NodeId::parse("b").unwrap(), "B");
        observed.get_mut(&b).unwrap().capabilities.0.tool_choice = false;
        let forced = RequestRequirements {
            tool_choice: ToolChoiceRequirement::Required,
            ..chat_needing(true, false, 10)
        };
        let picks: Vec<String> = (0..4)
            .map(|_| nodes(&pick(&t, &selector, &all_up(), &observed, &forced))[0].to_owned())
            .collect();
        assert_eq!(picks, ["a", "c", "a", "c"]);
        // `auto` is what b does with tools anyway: b is back in the ring.
        let auto = RequestRequirements {
            tool_choice: ToolChoiceRequirement::Auto,
            ..chat_needing(true, false, 10)
        };
        assert_eq!(
            pick(&t, &selector, &all_up(), &observed, &auto)
                .candidates
                .len(),
            3
        );
    }

    #[test]
    fn an_unknown_strategy_is_refused_rather_than_read_as_priority() {
        let error = serde_json::from_value::<RouterFile>(json!({
            "nodes": [{"id": "a", "url": "http://192.0.2.10:11434"}],
            "routes": [{"name": "Coder", "strategy": "fastest", "deployments": [{"node": "a", "model": "A"}]}]
        }))
        .unwrap_err();
        assert!(error.to_string().contains("fastest"), "{error}");
    }

    // --- R6: session affinity as a preference over valid candidates -------

    fn sticky_to(node: &str) -> DeploymentId {
        DeploymentId::of(&NodeId::parse(node).unwrap(), &node.to_uppercase())
    }

    fn plan_sticky(
        t: &Topology,
        selector: &Selector,
        health: &BTreeMap<NodeId, NodeStatus>,
        observed: &BTreeMap<DeploymentId, DeploymentObservation>,
        needs: &RequestRequirements,
        sticky: Option<&DeploymentId>,
    ) -> Plan {
        selector
            .plan_with_affinity(t, &t.routes()[0], health, observed, needs, sticky)
            .expect("a plan")
    }

    #[test]
    fn without_a_sticky_deployment_every_policy_plans_exactly_as_before() {
        for strategy in ["priority", "round_robin", "least_busy"] {
            let (t, with_none) = ring(strategy);
            let (_, plain) = ring(strategy);
            let observed = offering(FULL, FULL, FULL);
            let needs = chat_needing(false, false, 10);
            for _ in 0..5 {
                let a = plan_sticky(&t, &with_none, &all_up(), &observed, &needs, None);
                let b = pick(&t, &plain, &all_up(), &observed, &needs);
                assert_eq!(nodes(&a), nodes(&b), "{strategy}");
                assert_eq!(a.selection, b.selection, "{strategy}");
                assert_eq!(a.sticky, None);
                // Held concurrently, as live requests would be.
                std::mem::forget((a.reservation, b.reservation));
            }
        }
    }

    #[test]
    fn priority_prefers_a_valid_sticky_deployment_and_falls_back_in_configured_order() {
        let (t, selector) = ring("priority");
        let observed = offering(FULL, FULL, FULL);
        let needs = chat_needing(false, false, 10);
        let b = sticky_to("b");
        let plan = plan_sticky(&t, &selector, &all_up(), &observed, &needs, Some(&b));
        assert_eq!(nodes(&plan), ["b", "a", "c"]);
        assert_eq!(plan.sticky, Some(Sticky::Hit));
        assert_eq!(plan.selection, Selection::SessionAffinity);
        assert_eq!(
            plan.decision(0).unwrap().reason,
            RoutingReason::SessionAffinity
        );
        assert_eq!(
            plan.decision(1).unwrap().reason,
            RoutingReason::PrimaryFailedFallback
        );

        // b goes down: ordinary priority, and the plan says why b was passed.
        let plan = plan_sticky(
            &t,
            &selector,
            &with(all_up(), "b", down()),
            &observed,
            &needs,
            Some(&b),
        );
        assert_eq!(nodes(&plan), ["a", "c"]);
        assert_eq!(
            plan.sticky,
            Some(Sticky::Broken(Reassignment::StickyUnhealthy))
        );
        assert_eq!(
            plan.decision(0).unwrap().reason,
            RoutingReason::PrimaryHealthy
        );
    }

    #[test]
    fn round_robin_takes_no_turn_on_an_affinity_hit() {
        let (t, selector) = ring("round_robin");
        let observed = offering(FULL, FULL, FULL);
        let needs = chat_needing(false, false, 10);
        let c = sticky_to("c");
        for _ in 0..4 {
            let plan = plan_sticky(&t, &selector, &all_up(), &observed, &needs, Some(&c));
            assert_eq!(
                nodes(&plan),
                ["c", "a", "b"],
                "the sticky one, then the ring from it"
            );
            assert_eq!(
                plan.decision(1).unwrap().reason,
                RoutingReason::RoundRobinFailover
            );
        }
        assert_eq!(
            selector.cursor(&t, &t.routes()[0].name),
            Some(0),
            "no cursor value drawn"
        );
        // Requests without a session still rotate from where the ring was.
        let picks: Vec<String> = (0..3)
            .map(|_| nodes(&pick(&t, &selector, &all_up(), &observed, &needs))[0].to_owned())
            .collect();
        assert_eq!(picks, ["a", "b", "c"]);
    }

    #[test]
    fn least_busy_keeps_a_sticky_deployment_even_when_another_is_idler() {
        let (t, selector) = ring("least_busy");
        let observed = offering(FULL, FULL, FULL);
        let needs = chat_needing(false, false, 10);
        let a = sticky_to("a");
        // a is the busiest by far.
        let _held: Vec<Lease> = (0..3).map(|_| selector.load().acquire(&a)).collect();
        let plan = plan_sticky(&t, &selector, &all_up(), &observed, &needs, Some(&a));
        assert_eq!(nodes(&plan)[0], "a", "stickiness is not soft");
        assert_eq!(selector.load().active(&a), 4, "the slot is reserved on a");
        // Failover after a is least-busy over the rest.
        assert_eq!(&nodes(&plan)[1..], ["b", "c"]);
        assert_eq!(
            plan.decision(1).unwrap().reason,
            RoutingReason::LeastBusyFailover
        );
        drop(plan);

        // a unhealthy: stickiness breaks and least-busy resumes.
        let plan = plan_sticky(
            &t,
            &selector,
            &with(all_up(), "a", down()),
            &observed,
            &needs,
            Some(&a),
        );
        assert_eq!(nodes(&plan), ["b", "c"]);
        assert!(matches!(plan.selection, Selection::LeastBusy { .. }));
    }

    #[test]
    fn a_sticky_deployment_that_cannot_serve_the_request_is_never_resurrected() {
        for strategy in ["priority", "round_robin", "least_busy"] {
            let (t, selector) = ring(strategy);
            let no_tools = Offers {
                tools: false,
                ..FULL
            };
            let observed = offering(no_tools, FULL, FULL);
            let a = sticky_to("a");
            let plan = plan_sticky(
                &t,
                &selector,
                &all_up(),
                &observed,
                &chat_needing(true, false, 10),
                Some(&a),
            );
            assert!(!nodes(&plan).contains(&"a"), "{strategy}: a has no tools");
            assert_eq!(
                plan.sticky,
                Some(Sticky::Broken(Reassignment::StickyCapabilityMismatch)),
                "{strategy}"
            );
            // The same session's next request without tools may use a again.
            let plan = plan_sticky(
                &t,
                &selector,
                &all_up(),
                &observed,
                &chat_needing(false, false, 10),
                Some(&a),
            );
            assert_eq!(nodes(&plan)[0], "a", "{strategy}");
        }
    }

    #[test]
    fn a_disabled_or_swapped_sticky_deployment_is_unavailable_not_unhealthy() {
        let (t, selector) = ring("priority");
        let observed = offering(FULL, FULL, FULL);
        let needs = chat_needing(false, false, 10);
        let b = sticky_to("b");
        let plan = plan_sticky(
            &t,
            &selector,
            &with(all_up(), "b", serving("SomethingElse")),
            &observed,
            &needs,
            Some(&b),
        );
        assert_eq!(
            plan.sticky,
            Some(Sticky::Broken(Reassignment::StickyUnavailable))
        );
        assert_eq!(nodes(&plan)[0], "a");
    }
}
