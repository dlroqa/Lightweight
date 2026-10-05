//! Choosing where a request goes.
//!
//! Pure: a route, the topology and a health snapshot in, an ordered plan out.
//! Nothing here sends a byte, so every selection rule is testable without a
//! network, and the same inputs always produce the same plan.
//!
//! Under [`RoutePolicy::Priority`] the plan is the route's deployments in
//! configured order, with every one that cannot take traffic removed. The
//! proxy tries them in that order and stops at the first that answers; health
//! decides who is *eligible*, configuration decides who is *first*. A recovered
//! primary is therefore first again on the very next request, with no state to
//! reset.

use std::collections::BTreeMap;

use crate::domain::CapabilitySet;
use crate::domain::{
    DeploymentId, NodeId, Route, RouteName, RoutePolicy, RoutingDecision, RoutingFailure,
    RoutingReason, Topology, UnavailableReason,
};
use crate::health::{DeploymentObservation, NodeStatus, availability};

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

/// The ordered deployments one request may be sent to.
#[derive(Clone, Debug)]
pub struct Plan {
    pub route: RouteName,
    pub candidates: Vec<Candidate>,
    /// Deployments left out, and why, for the log line.
    pub skipped: Vec<(DeploymentId, UnavailableReason)>,
    /// How many deployments the route has in total.
    route_size: usize,
}

impl Plan {
    /// Describe sending to `candidates[attempt]`, given that every earlier
    /// candidate in this plan was tried and failed.
    pub fn decision(&self, attempt: usize) -> Option<RoutingDecision> {
        let candidate = self.candidates.get(attempt)?;
        let reason = if self.route_size == 1 {
            RoutingReason::ExplicitSingleDeployment
        } else if attempt > 0 {
            RoutingReason::PrimaryFailedFallback
        } else if candidate.position == 0 {
            RoutingReason::PrimaryHealthy
        } else {
            RoutingReason::PrimaryUnavailableFallback
        };
        Some(RoutingDecision {
            route: self.route.clone(),
            deployment: candidate.deployment.clone(),
            node: candidate.node.clone(),
            reason,
        })
    }
}

/// Plan one request to `route`.
///
/// Refuses with [`RoutingFailure::RouteUnavailable`] when no deployment is
/// eligible — the route exists, so this is never `model_not_found`.
pub fn plan(
    topology: &Topology,
    route: &Route,
    health: &BTreeMap<NodeId, NodeStatus>,
) -> Result<Plan, RoutingFailure> {
    match route.policy {
        RoutePolicy::Priority => priority(topology, route, health),
    }
}

fn priority(
    topology: &Topology,
    route: &Route,
    health: &BTreeMap<NodeId, NodeStatus>,
) -> Result<Plan, RoutingFailure> {
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
    Ok(Plan {
        route: route.name.clone(),
        candidates,
        skipped,
        route_size: route.deployments.len(),
    })
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

/// Summarize a route over **exactly** the deployments [`plan`] would try.
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
    let Ok(plan) = plan(topology, route, health) else {
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
}
