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

use crate::domain::{
    DeploymentId, NodeId, Route, RouteName, RoutePolicy, RoutingDecision, RoutingFailure,
    RoutingReason, Topology, UnavailableReason,
};
use crate::health::{NodeStatus, availability};

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
}
