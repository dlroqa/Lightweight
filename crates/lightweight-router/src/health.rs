//! What the router currently believes about each node.
//!
//! One probe per node per interval, against the node's own
//! `GET /v1/capabilities`. That single call answers everything routing needs,
//! and nothing it does not:
//!
//! * **Is it reachable?** It answered.
//! * **Is it Lightweight?** Its body names the public inference protocol and a
//!   version this router speaks. Anything else — another server on that port,
//!   a captive portal, an older gateway — is a failed probe, not a healthy one.
//! * **What can it serve right now?** `state.model.id`: the identity the node
//!   advertises, alias first, exactly as its `/v1/models` lists it.
//! * **What does it support?** `features`, and the context it is serving.
//!
//! It is cheap on the node — no catalog read, no engine call — so it is the
//! only thing polled. The request path never probes; it reads this book.
//!
//! The verdict is deliberately simple and deterministic:
//!
//! * a success makes a node healthy at once;
//! * a failure counts, and only `failure_threshold` consecutive failures make
//!   it unhealthy, so one lost packet does not move traffic;
//! * a node never yet seen stays unknown, and unknown is not eligible.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use lightweight_api::capabilities::{CapabilitiesBody, PROTOCOL_NAME, PROTOCOL_VERSION};
use lightweight_catalog::alias;
use lightweight_observability::targets;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::domain::{
    CapabilitySet, Deployment, DeploymentHealth, DeploymentId, Node, NodeHealth, NodeId, Topology,
    UnavailableReason,
};

/// The model a node said it is serving.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServedModel {
    /// The node's public identity for it — its alias when it has one.
    pub id: String,
    /// The effective context the node serves it at.
    pub context_length: u32,
}

/// What one successful probe learned.
#[derive(Clone, Debug)]
pub struct Observation {
    pub served: Option<ServedModel>,
    pub features: CapabilitySet,
    pub max_concurrent_requests: u32,
    pub version: String,
}

/// Everything known about one node.
#[derive(Clone, Debug, Default)]
pub struct NodeStatus {
    pub health: NodeHealth,
    pub consecutive_failures: u32,
    pub last_checked: Option<SystemTime>,
    /// The last time the node answered as a Lightweight gateway.
    pub last_seen: Option<SystemTime>,
    /// Why the last failure happened. Never carries a credential: it is built
    /// from status codes and transport error kinds, not from response bodies.
    pub last_error: Option<String>,
    /// From the last successful probe. Kept through failures so the control
    /// API can say what the node *was* serving, but never consulted for an
    /// unhealthy node.
    pub observed: Option<Observation>,
}

impl NodeStatus {
    /// The model the node is serving, as far as the router may rely on it.
    pub fn served(&self) -> Option<&ServedModel> {
        self.observed.as_ref()?.served.as_ref()
    }
}

/// The two things that can happen to a node's status.
#[derive(Clone, Debug)]
pub enum Outcome {
    Success(Observation),
    Failure(String),
}

/// Apply one outcome to one status. Pure, so the rules can be tested exactly.
pub fn apply(status: &mut NodeStatus, outcome: Outcome, threshold: u32, at: SystemTime) {
    status.last_checked = Some(at);
    match outcome {
        Outcome::Success(observation) => {
            status.health = NodeHealth::Healthy;
            status.consecutive_failures = 0;
            status.last_seen = Some(at);
            status.last_error = None;
            status.observed = Some(observation);
        }
        Outcome::Failure(reason) => {
            status.consecutive_failures = status.consecutive_failures.saturating_add(1);
            status.last_error = Some(reason);
            if status.consecutive_failures >= threshold {
                status.health = NodeHealth::Unhealthy;
            }
        }
    }
}

/// What was last observed about one deployment, kept for that deployment
/// alone.
///
/// Recorded whenever a probe finds the deployment's node serving the
/// deployment's model, and never merged with another deployment's. A route's
/// public answer is a conservative combination of these, computed on read; the
/// per-deployment values themselves are never narrowed to it. That is what a
/// later selector needs to say "this request uses tools, so only the
/// deployments that support tools are eligible".
#[derive(Clone, Debug, Serialize)]
pub struct DeploymentObservation {
    pub capabilities: CapabilitySet,
    pub context_length: u32,
    pub max_concurrent_requests: u32,
    #[serde(skip)]
    pub observed_at: SystemTime,
}

/// The router's current belief about every configured node.
#[derive(Debug)]
pub struct HealthBook {
    threshold: u32,
    nodes: RwLock<BTreeMap<NodeId, NodeStatus>>,
    /// Each node's deployments and the model each one names. Fixed by the
    /// topology; used to file an observation under every deployment it is
    /// about.
    deployments_of: BTreeMap<NodeId, Vec<(DeploymentId, String)>>,
    deployments: RwLock<BTreeMap<DeploymentId, DeploymentObservation>>,
}

impl HealthBook {
    pub fn new(topology: &Topology, threshold: u32) -> Self {
        let nodes = topology
            .nodes()
            .iter()
            .map(|node| (node.id.clone(), NodeStatus::default()))
            .collect();
        let mut deployments_of: BTreeMap<NodeId, Vec<(DeploymentId, String)>> = BTreeMap::new();
        for deployment in topology.deployments() {
            deployments_of
                .entry(deployment.node.clone())
                .or_default()
                .push((deployment.id.clone(), deployment.remote_model.clone()));
        }
        Self {
            threshold,
            nodes: RwLock::new(nodes),
            deployments_of,
            deployments: RwLock::new(BTreeMap::new()),
        }
    }

    /// What was last observed about one deployment, if it has ever been seen
    /// served.
    pub fn deployment(&self, id: &DeploymentId) -> Option<DeploymentObservation> {
        self.deployments
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    pub fn deployment_snapshot(&self) -> BTreeMap<DeploymentId, DeploymentObservation> {
        self.deployments
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// File a successful probe under the deployments it describes: those on
    /// `node` whose model is the one the node said it serves.
    fn observe_deployments(&self, node: &NodeId, observation: &Observation, at: SystemTime) {
        let Some(served) = &observation.served else {
            return;
        };
        let Some(listed) = self.deployments_of.get(node) else {
            return;
        };
        let mut deployments = self
            .deployments
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        for (id, model) in listed {
            if alias::same_name(model, &served.id) {
                deployments.insert(
                    id.clone(),
                    DeploymentObservation {
                        capabilities: observation.features.clone(),
                        context_length: served.context_length,
                        max_concurrent_requests: observation.max_concurrent_requests,
                        observed_at: at,
                    },
                );
            }
        }
    }

    /// Record an outcome for a node. A node not in the topology is ignored.
    pub fn record(&self, node: &NodeId, outcome: Outcome) {
        if let Outcome::Success(observation) = &outcome {
            self.observe_deployments(node, observation, SystemTime::now());
        }
        let mut nodes = self.nodes.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(status) = nodes.get_mut(node) {
            let was = status.health;
            apply(status, outcome, self.threshold, SystemTime::now());
            if was != status.health {
                tracing::info!(
                    target: targets::ROUTER,
                    node = %node,
                    from = was.as_str(),
                    to = status.health.as_str(),
                    reason = status.last_error.as_deref().unwrap_or(""),
                    "node health changed"
                );
            }
        }
    }

    /// Forget which model a node is serving, until the next probe says.
    ///
    /// For when a request is refused with `model_not_found` by a node the book
    /// believed was serving it: the node swapped models between probes, and
    /// sending the next request there too would only fail the same way.
    pub fn forget_model(&self, node: &NodeId) {
        let mut nodes = self.nodes.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(observed) = nodes
            .get_mut(node)
            .and_then(|status| status.observed.as_mut())
        {
            observed.served = None;
        }
    }

    pub fn status(&self, node: &NodeId) -> NodeStatus {
        self.nodes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(node)
            .cloned()
            .unwrap_or_default()
    }

    pub fn snapshot(&self) -> BTreeMap<NodeId, NodeStatus> {
        self.nodes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether a deployment can take traffic now.
    pub fn availability(&self, node: &Node, deployment: &Deployment) -> DeploymentHealth {
        availability(node, deployment, &self.status(&node.id))
    }
}

/// Whether a deployment on `node`, whose status is `status`, can take traffic.
///
/// Only a node the router has seen healthy, serving exactly the model the
/// deployment names, is eligible. The model is compared the way the node
/// compares aliases — ignoring case — so a deployment written `qwencoder`
/// matches a node serving `QwenCoder`, as a request for either would.
pub fn availability(node: &Node, deployment: &Deployment, status: &NodeStatus) -> DeploymentHealth {
    if !node.enabled {
        return DeploymentHealth::Unavailable(UnavailableReason::NodeDisabled);
    }
    match status.health {
        NodeHealth::Unknown => DeploymentHealth::Unavailable(UnavailableReason::NodeUnknown),
        NodeHealth::Unhealthy => DeploymentHealth::Unavailable(UnavailableReason::NodeUnhealthy),
        NodeHealth::Healthy => match status.served() {
            Some(served) if alias::same_name(&served.id, &deployment.remote_model) => {
                DeploymentHealth::Available
            }
            _ => DeploymentHealth::Unavailable(UnavailableReason::ModelNotServed),
        },
    }
}

/// Ask one node what it is.
pub async fn probe(client: &reqwest::Client, node: &Node, timeout: Duration) -> Outcome {
    let mut request = client
        .get(node.endpoint("/v1/capabilities"))
        .timeout(timeout);
    if let Some(value) = node.auth.header_value() {
        request = request.header(reqwest::header::AUTHORIZATION, value);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => return Outcome::Failure(describe_transport(&err)),
    };
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Outcome::Failure(
            "the node refused the router's credential (401); check this node's api_key_env"
                .to_owned(),
        );
    }
    if !status.is_success() {
        return Outcome::Failure(format!("/v1/capabilities answered {}", status.as_u16()));
    }
    let body: CapabilitiesBody = match response.json().await {
        Ok(body) => body,
        Err(_) => {
            return Outcome::Failure(
                "/v1/capabilities did not answer with the Lightweight capabilities contract"
                    .to_owned(),
            );
        }
    };
    match observe(body) {
        Ok(observation) => Outcome::Success(observation),
        Err(reason) => Outcome::Failure(reason),
    }
}

/// Turn a capabilities body into an observation, refusing one this router does
/// not speak.
pub fn observe(body: CapabilitiesBody) -> Result<Observation, String> {
    if body.protocol.name != PROTOCOL_NAME {
        return Err(format!(
            "the node speaks {:?}, not {PROTOCOL_NAME:?}",
            body.protocol.name
        ));
    }
    if !body
        .protocol
        .compatible_versions
        .contains(&PROTOCOL_VERSION)
    {
        return Err(format!(
            "the node's protocol versions {:?} do not include {PROTOCOL_VERSION}",
            body.protocol.compatible_versions
        ));
    }
    let served = body
        .state
        .model_loaded
        .then_some(body.state.model)
        .flatten()
        .map(|model| ServedModel {
            id: model.id,
            context_length: model.context_length,
        });
    Ok(Observation {
        served,
        features: CapabilitySet(body.features),
        max_concurrent_requests: body.limits.max_concurrent_requests,
        version: body.server.version,
    })
}

/// A transport failure, said without the URL (which `reqwest` would include,
/// and which is the control API's to show, not every log line's).
pub fn describe_transport(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "timed out".to_owned()
    } else if err.is_connect() {
        "could not connect".to_owned()
    } else if err.is_request() {
        "the request could not be sent".to_owned()
    } else if err.is_body() || err.is_decode() {
        "the response could not be read".to_owned()
    } else {
        "transport error".to_owned()
    }
}

/// Probe every enabled node once, concurrently, and record what was found.
pub async fn probe_all(
    client: &reqwest::Client,
    topology: &Topology,
    book: &HealthBook,
    timeout: Duration,
) {
    let probes = topology
        .nodes()
        .iter()
        .filter(|node| node.enabled)
        .map(|node| async move { (&node.id, probe(client, node, timeout).await) });
    for (node, outcome) in futures_util::future::join_all(probes).await {
        book.record(node, outcome);
    }
}

/// Keep probing until `stop` is cancelled.
pub fn spawn_monitor(
    client: reqwest::Client,
    topology: Arc<Topology>,
    book: Arc<HealthBook>,
    interval: Duration,
    timeout: Duration,
    stop: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                () = tokio::time::sleep(interval) => {}
            }
            tokio::select! {
                () = stop.cancelled() => return,
                () = probe_all(&client, &topology, &book, timeout) => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DeploymentId, NodeAuth};
    use lightweight_api::capabilities::CapabilityModel;

    fn observation(model: &str) -> Observation {
        Observation {
            served: Some(ServedModel {
                id: model.to_owned(),
                context_length: 4096,
            }),
            features: CapabilitySet::none(),
            max_concurrent_requests: 1,
            version: "0.4.1".into(),
        }
    }

    fn node(enabled: bool) -> Node {
        Node {
            id: NodeId::parse("dell").unwrap(),
            base_url: reqwest::Url::parse("http://192.0.2.10:11434").unwrap(),
            enabled,
            auth: NodeAuth::None,
        }
    }

    fn deployment(model: &str) -> Deployment {
        let node = NodeId::parse("dell").unwrap();
        Deployment {
            id: DeploymentId::of(&node, model),
            node,
            remote_model: model.to_owned(),
        }
    }

    #[test]
    fn a_success_makes_a_node_healthy_at_once() {
        let mut status = NodeStatus::default();
        assert_eq!(status.health, NodeHealth::Unknown);
        apply(
            &mut status,
            Outcome::Success(observation("QwenCoder")),
            2,
            SystemTime::now(),
        );
        assert_eq!(status.health, NodeHealth::Healthy);
        assert!(status.last_seen.is_some());
    }

    #[test]
    fn one_failure_is_not_enough_to_move_traffic() {
        let mut status = NodeStatus::default();
        let now = SystemTime::now();
        apply(&mut status, Outcome::Success(observation("m")), 2, now);
        apply(&mut status, Outcome::Failure("timed out".into()), 2, now);
        assert_eq!(status.health, NodeHealth::Healthy, "a single blip");
        apply(&mut status, Outcome::Failure("timed out".into()), 2, now);
        assert_eq!(status.health, NodeHealth::Unhealthy, "the threshold");
        assert_eq!(status.consecutive_failures, 2);
    }

    #[test]
    fn one_success_recovers_an_unhealthy_node() {
        let mut status = NodeStatus::default();
        let now = SystemTime::now();
        for _ in 0..5 {
            apply(
                &mut status,
                Outcome::Failure("could not connect".into()),
                2,
                now,
            );
        }
        assert_eq!(status.health, NodeHealth::Unhealthy);
        apply(&mut status, Outcome::Success(observation("m")), 2, now);
        assert_eq!(status.health, NodeHealth::Healthy);
        assert_eq!(status.consecutive_failures, 0);
        assert_eq!(status.last_error, None);
    }

    #[test]
    fn a_node_never_seen_goes_from_unknown_to_unhealthy() {
        let mut status = NodeStatus::default();
        let now = SystemTime::now();
        apply(
            &mut status,
            Outcome::Failure("could not connect".into()),
            3,
            now,
        );
        assert_eq!(status.health, NodeHealth::Unknown);
        apply(
            &mut status,
            Outcome::Failure("could not connect".into()),
            3,
            now,
        );
        apply(
            &mut status,
            Outcome::Failure("could not connect".into()),
            3,
            now,
        );
        assert_eq!(status.health, NodeHealth::Unhealthy);
    }

    #[test]
    fn availability_needs_enabled_healthy_and_the_right_model() {
        let mut status = NodeStatus::default();
        assert_eq!(
            availability(&node(true), &deployment("QwenCoder"), &status),
            DeploymentHealth::Unavailable(UnavailableReason::NodeUnknown)
        );
        apply(
            &mut status,
            Outcome::Success(observation("QwenCoder")),
            1,
            SystemTime::now(),
        );
        assert_eq!(
            availability(&node(true), &deployment("QwenCoder"), &status),
            DeploymentHealth::Available
        );
        assert_eq!(
            availability(&node(true), &deployment("qwencoder"), &status),
            DeploymentHealth::Available,
            "compared as the node compares aliases"
        );
        assert_eq!(
            availability(&node(true), &deployment("Other"), &status),
            DeploymentHealth::Unavailable(UnavailableReason::ModelNotServed)
        );
        assert_eq!(
            availability(&node(false), &deployment("QwenCoder"), &status),
            DeploymentHealth::Unavailable(UnavailableReason::NodeDisabled)
        );
        apply(
            &mut status,
            Outcome::Failure("x".into()),
            1,
            SystemTime::now(),
        );
        assert_eq!(
            availability(&node(true), &deployment("QwenCoder"), &status),
            DeploymentHealth::Unavailable(UnavailableReason::NodeUnhealthy)
        );
    }

    #[test]
    fn only_the_lightweight_protocol_is_accepted() {
        let body = || {
            CapabilitiesBody::new(
                "0.4.1",
                Some(CapabilityModel {
                    id: "QwenCoder".into(),
                    context_length: 8192,
                }),
                2,
            )
        };
        let seen = observe(body()).expect("a Lightweight node");
        assert_eq!(
            seen.served,
            Some(ServedModel {
                id: "QwenCoder".into(),
                context_length: 8192
            })
        );
        assert_eq!(seen.max_concurrent_requests, 2);

        let mut other = body();
        other.protocol.name = "something-else".into();
        assert!(observe(other).is_err());

        let mut future = body();
        future.protocol.compatible_versions = vec![2];
        assert!(observe(future).is_err());

        let empty = CapabilitiesBody::new("0.4.1", None, 1);
        assert_eq!(observe(empty).expect("healthy").served, None);
    }

    fn observed_serving(model: &str, tools: bool, context_length: u32) -> Observation {
        let mut features = CapabilitySet::none();
        features.0.streaming = true;
        features.0.tools = tools;
        Observation {
            served: Some(ServedModel {
                id: model.to_owned(),
                context_length,
            }),
            features,
            max_concurrent_requests: 1,
            version: "0.4.1".into(),
        }
    }

    fn two_deployment_topology() -> Topology {
        let file: crate::config::RouterFile = serde_json::from_value(serde_json::json!({
            "nodes": [
                {"id": "a", "url": "http://192.0.2.10:11434"},
                {"id": "b", "url": "http://192.0.2.11:11434"}
            ],
            "routes": [{"name": "Coder", "deployments": [
                {"node": "a", "model": "QwenCoder"},
                {"node": "b", "model": "CoderBackup"}
            ]}]
        }))
        .unwrap();
        crate::config::validate(file, &|_| None).unwrap().topology
    }

    #[test]
    fn each_deployment_keeps_its_own_capabilities() {
        let topology = two_deployment_topology();
        let book = HealthBook::new(&topology, 2);
        let (a, b) = (NodeId::parse("a").unwrap(), NodeId::parse("b").unwrap());
        book.record(
            &a,
            Outcome::Success(observed_serving("QwenCoder", true, 32_768)),
        );
        book.record(
            &b,
            Outcome::Success(observed_serving("CoderBackup", false, 8_192)),
        );

        let primary = book.deployment(&DeploymentId::of(&a, "QwenCoder")).unwrap();
        let backup = book
            .deployment(&DeploymentId::of(&b, "CoderBackup"))
            .unwrap();
        assert!(primary.capabilities.0.tools);
        assert_eq!(primary.context_length, 32_768);
        assert!(!backup.capabilities.0.tools);
        assert_eq!(backup.context_length, 8_192);
    }

    #[test]
    fn a_deployment_observation_is_only_ever_written_by_its_own_model() {
        let topology = two_deployment_topology();
        let book = HealthBook::new(&topology, 1);
        let a = NodeId::parse("a").unwrap();
        let primary = DeploymentId::of(&a, "QwenCoder");
        book.record(
            &a,
            Outcome::Success(observed_serving("QwenCoder", true, 32_768)),
        );

        // The node swaps to a model no deployment names, then fails: the
        // deployment's own figures are neither overwritten nor erased.
        book.record(&a, Outcome::Success(observed_serving("Other", false, 512)));
        book.record(&a, Outcome::Failure("could not connect".into()));
        book.forget_model(&a);
        let kept = book.deployment(&primary).unwrap();
        assert!(kept.capabilities.0.tools);
        assert_eq!(kept.context_length, 32_768);
        // And it is not available, because availability is read from the node.
        assert_eq!(
            book.availability(&topology.nodes()[0], &topology.deployments()[0]),
            DeploymentHealth::Unavailable(UnavailableReason::NodeUnhealthy)
        );
    }
}
