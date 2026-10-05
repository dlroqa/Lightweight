//! The router's vocabulary.
//!
//! Three layers of identity meet here, and keeping them apart is the point of
//! the whole crate:
//!
//! * A **route** is what a client sees and sends as `model`: `Coder`, `Fast`.
//!   It is the router's own name and belongs to no node.
//! * A **deployment** is one model on one node, named by the identity *that
//!   node* advertises — usually its local alias, `QwenCoder`. The router
//!   forwards that name and never looks behind it.
//! * A **node** is one Lightweight gateway, reached over HTTP with its own
//!   credential.
//!
//! What lies behind a node's alias — its canonical id, the GGUF, the engine —
//! is the node's business. Nothing in this module can name it.
//!
//! The configuration types here are immutable once validated. Health, which
//! changes every few seconds, lives in [`crate::health`] and refers to these by
//! id, so a probe can never rewrite what the operator configured.

use std::fmt;

use lightweight_api::capabilities::FeatureSet;
use lightweight_catalog::alias;
use serde::{Deserialize, Serialize};

/// The longest node id accepted.
const MAX_NODE_ID_CHARS: usize = 64;

/// A node's identity, unique within one router.
///
/// Restricted to `[A-Za-z0-9._-]` so that a [`DeploymentId`] — the node id, a
/// `/`, then the model — splits back into exactly one node, and so the id is
/// safe in a log field, a metric label and a URL path without escaping.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl NodeId {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let id = raw.trim();
        if id.is_empty() {
            return Err("a node id cannot be empty".to_owned());
        }
        if id.chars().count() > MAX_NODE_ID_CHARS {
            return Err(format!(
                "a node id can be at most {MAX_NODE_ID_CHARS} characters"
            ));
        }
        if let Some(bad) = id
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        {
            return Err(format!(
                "a node id may contain only letters, digits, `.`, `_` and `-`, not {bad:?}"
            ));
        }
        Ok(Self(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One model on one node: `dell-7820/QwenCoder`.
///
/// Derived, never configured, so it cannot disagree with the node and model it
/// names. Two routes that list the same node and model share one deployment —
/// and therefore one health verdict.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct DeploymentId(String);

impl DeploymentId {
    pub fn of(node: &NodeId, remote_model: &str) -> Self {
        Self(format!("{node}/{remote_model}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeploymentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A route's public name, as the operator spelled it.
///
/// Held to the same rules as a node-local alias — trimmed, no `/`, `\` or `@`,
/// never `default` — because it travels the same places an alias does: a
/// client's `model` field, a model picker, a URL path. Matched ignoring case,
/// as aliases are, so `coder` reaches `Coder`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RouteName(String);

impl RouteName {
    pub fn parse(raw: &str) -> Result<Self, alias::AliasProblem> {
        alias::validate_alias(raw).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether a client's `model` value names this route.
    pub fn matches(&self, requested: &str) -> bool {
        alias::same_name(&self.0, requested)
    }
}

impl fmt::Display for RouteName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A credential value.
///
/// `Debug` is written by hand for the same reason the gateway's `AuthPolicy`
/// writes its own: this type is reachable from the router's state, and a
/// derived implementation would put the key in any log line formatted with
/// `?`. Nothing serializes it either.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// How the router authenticates to one node.
///
/// Always this node's own credential, read from an environment variable the
/// operator named for it. There is deliberately no fallback to the router's
/// client-facing key or to another node's: a credential that silently applied
/// to every node would turn one leaked key into access to all of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeAuth {
    /// The node accepts unauthenticated requests — a loopback gateway.
    None,
    /// Send `Authorization: Bearer <secret>`.
    Bearer {
        /// Where the secret came from, for messages. Not secret itself.
        env: String,
        secret: Secret,
    },
}

impl NodeAuth {
    /// The `Authorization` value to send, if any.
    pub fn header_value(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Bearer { secret, .. } => Some(format!("Bearer {}", secret.expose())),
        }
    }

    /// What the control API may say about this credential: whether there is
    /// one, never what it is.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bearer { .. } => "bearer",
        }
    }
}

/// One remote Lightweight gateway.
#[derive(Clone, Debug)]
pub struct Node {
    pub id: NodeId,
    /// Validated at load: `http` or `https`, a host, no credentials, query or
    /// fragment. Any path is kept, so a gateway behind a path-routing proxy
    /// is reachable.
    pub base_url: reqwest::Url,
    /// A disabled node is never probed and never sent traffic. The operator's
    /// off switch, distinct from a node that is merely unhealthy.
    pub enabled: bool,
    pub auth: NodeAuth,
}

impl Node {
    /// The absolute URL of one of the node's endpoints.
    pub fn endpoint(&self, path: &str) -> String {
        let base = self.base_url.as_str().trim_end_matches('/');
        format!("{base}{path}")
    }
}

/// One model available on one node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deployment {
    pub id: DeploymentId,
    pub node: NodeId,
    /// The model identity *that node* expects — normally its local alias, and
    /// always one its `/v1/models` advertises. Sent upstream as `model`; never
    /// shown to a client.
    pub remote_model: String,
}

/// How a route chooses among its deployments.
///
/// One strategy for now. An enum rather than a flag so that round-robin,
/// weighted or least-busy selection is a new variant with its own arm in
/// [`crate::select`], not a change to what a route is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutePolicy {
    /// The first eligible deployment in configured order. Deterministic: the
    /// same health always produces the same choice.
    #[default]
    Priority,
    /// Equal steps around the eligible deployments, in configured order. One
    /// step per client request, however many failover attempts it takes.
    RoundRobin,
    /// The eligible deployment with the lowest `active / concurrency limit`,
    /// ties to configured order. Load is the router's own in-flight count;
    /// latency plays no part.
    LeastBusy,
}

impl RoutePolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::RoundRobin => "round_robin",
            Self::LeastBusy => "least_busy",
        }
    }
}

/// A logical model: the name a client discovers and sends.
#[derive(Clone, Debug)]
pub struct Route {
    pub name: RouteName,
    pub policy: RoutePolicy,
    /// In configured order: the order tried under [`RoutePolicy::Priority`],
    /// the ring under [`RoutePolicy::RoundRobin`], and the tie-break under
    /// [`RoutePolicy::LeastBusy`]. Never empty.
    pub deployments: Vec<DeploymentId>,
}

/// Whether a node is answering as a Lightweight gateway.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeHealth {
    /// Not yet probed, or failing but not yet past the threshold from a start
    /// with no success. Not eligible: traffic goes only where a probe has
    /// succeeded.
    #[default]
    Unknown,
    Healthy,
    Unhealthy,
}

impl NodeHealth {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
        }
    }
}

/// Why a deployment cannot take traffic right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    NodeDisabled,
    NodeUnknown,
    NodeUnhealthy,
    /// The node is healthy but is not serving this deployment's model. The
    /// router never loads one to make it so: placement is not its job, and a
    /// load in the request path would stall the request behind it.
    ModelNotServed,
}

impl UnavailableReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NodeDisabled => "node_disabled",
            Self::NodeUnknown => "node_unknown",
            Self::NodeUnhealthy => "node_unhealthy",
            Self::ModelNotServed => "model_not_served",
        }
    }
}

/// Why an available deployment cannot serve one particular request.
///
/// Distinct from [`UnavailableReason`]: that one is about the deployment and
/// holds for every request; this one is about a request and the deployment's
/// last-observed capabilities. A deployment can have several at once. Ordered
/// so a set of them always reads in the same order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityGap {
    /// A chat request, and the node does not offer `/v1/chat/completions`.
    ChatUnsupported,
    /// A text completion, and the node does not offer `/v1/completions`.
    CompletionUnsupported,
    /// The request declares tools, and the node does not take them.
    ToolsUnsupported,
    /// The request's `tool_choice` needs the node to honour it, and the node
    /// does not say it does.
    ToolChoiceUnsupported,
    /// The request asks for reasoning, and the node does not offer it.
    ReasoningUnsupported,
    /// The prompt cannot fit in the context this deployment is served at.
    ContextTooSmall,
    /// The router has never observed this deployment's capabilities, so it
    /// cannot vouch for any of them.
    Unobserved,
}

impl CapabilityGap {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChatUnsupported => "chat_unsupported",
            Self::CompletionUnsupported => "completion_unsupported",
            Self::ToolsUnsupported => "tools_unsupported",
            Self::ToolChoiceUnsupported => "tool_choice_unsupported",
            Self::ReasoningUnsupported => "reasoning_unsupported",
            Self::ContextTooSmall => "context_too_small",
            Self::Unobserved => "unobserved",
        }
    }

    /// The requirement in words, for a client's error message. Names a kind of
    /// capability, never a node or a model.
    pub const fn requirement(self) -> &'static str {
        match self {
            Self::ChatUnsupported => "chat completions",
            Self::CompletionUnsupported => "text completions",
            Self::ToolsUnsupported => "tool calling",
            Self::ToolChoiceUnsupported => "the requested tool_choice",
            Self::ReasoningUnsupported => "reasoning (reasoning_effort)",
            Self::ContextTooSmall => "a context window large enough for this prompt",
            Self::Unobserved => "capabilities the router has observed",
        }
    }
}

/// Whether a deployment can take traffic right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeploymentHealth {
    Available,
    Unavailable(UnavailableReason),
}

impl DeploymentHealth {
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

/// The public protocol features a deployment offers, as its node reported them.
///
/// The node's own `features` object from `/v1/capabilities`, carried whole so
/// a route can claim only what every deployment it might use would honour.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct CapabilitySet(pub FeatureSet);

impl CapabilitySet {
    /// What two deployments both support.
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Self {
        let (a, b) = (&self.0, &other.0);
        Self(FeatureSet {
            streaming: a.streaming && b.streaming,
            sse_done: a.sse_done && b.sse_done,
            usage_chunk: a.usage_chunk && b.usage_chunk,
            chat_completions: a.chat_completions && b.chat_completions,
            completions: a.completions && b.completions,
            tools: a.tools && b.tools,
            tool_call_deltas: a.tool_call_deltas && b.tool_call_deltas,
            tool_choice: a.tool_choice && b.tool_choice,
            parallel_tool_calls: a.parallel_tool_calls && b.parallel_tool_calls,
            reasoning_content: a.reasoning_content && b.reasoning_content,
        })
    }

    /// Nothing at all: what a route with no available deployment can promise.
    pub fn none() -> Self {
        Self(FeatureSet {
            streaming: false,
            sse_done: false,
            usage_chunk: false,
            chat_completions: false,
            completions: false,
            tools: false,
            tool_call_deltas: false,
            tool_choice: false,
            parallel_tool_calls: false,
            reasoning_content: false,
        })
    }
}

/// Why a deployment was chosen. Logged with every routed request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutingReason {
    /// The route has one deployment and it was available.
    ExplicitSingleDeployment,
    /// The first deployment in priority order was available.
    PrimaryHealthy,
    /// Higher-priority deployments were unavailable by health, so this one was
    /// chosen before anything was sent.
    PrimaryUnavailableFallback,
    /// A higher-priority deployment was tried and failed before it had sent
    /// anything, so the request moved on.
    PrimaryFailedFallback,
    /// The round-robin rotation's turn.
    RoundRobin,
    /// The rotation's choice failed before answering; this is the next
    /// deployment after it in the rotation.
    RoundRobinFailover,
    /// The lowest normalized load, strictly.
    LeastBusy,
    /// The lowest normalized load, shared with another deployment; configured
    /// order broke the tie.
    LeastBusyTiebreak,
    /// The least-busy choice failed before answering; this is the next in the
    /// load order observed when the request was planned.
    LeastBusyFailover,
}

impl RoutingReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitSingleDeployment => "explicit_single_deployment",
            Self::PrimaryHealthy => "primary_healthy",
            Self::PrimaryUnavailableFallback => "primary_unavailable_fallback",
            Self::PrimaryFailedFallback => "primary_failed_fallback",
            Self::RoundRobin => "round_robin",
            Self::RoundRobinFailover => "round_robin_failover",
            Self::LeastBusy => "least_busy",
            Self::LeastBusyTiebreak => "least_busy_tiebreak",
            Self::LeastBusyFailover => "least_busy_failover",
        }
    }
}

/// Where one attempt of one request went, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutingDecision {
    pub route: RouteName,
    pub deployment: DeploymentId,
    pub node: NodeId,
    pub reason: RoutingReason,
}

/// Why a request could not be routed at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoutingFailure {
    /// No route has this name. Never answered by substituting another route.
    UnknownRoute { requested: String },
    /// The request asked for `default`, or named no model, and the operator
    /// configured no default route. Refused rather than guessed.
    NoDefaultRoute,
    /// The route exists, and no deployment of it can take the request.
    RouteUnavailable { route: RouteName },
    /// The route exists and has deployments that can take traffic, but none of
    /// them can serve what this request asks for. `unmet` is every gap seen,
    /// in a fixed order, so the client is told what to change; `unfit` is each
    /// deployment's own gaps, for the operator's log and never for the client.
    CapabilityMismatch {
        route: RouteName,
        unmet: Vec<CapabilityGap>,
        unfit: Vec<(DeploymentId, Vec<CapabilityGap>)>,
    },
}

/// Everything the operator configured, validated and immutable.
///
/// Built only by [`crate::config`], which refuses a configuration that breaks
/// any invariant below, so code holding a `Topology` may rely on them:
///
/// * node ids, deployment ids and route names (ignoring case) are unique;
/// * every deployment names a configured node;
/// * every route has at least one deployment, none listed twice;
/// * no route is called `default`, and the default route, if any, exists.
#[derive(Clone, Debug)]
pub struct Topology {
    pub(crate) nodes: Vec<Node>,
    pub(crate) deployments: Vec<Deployment>,
    pub(crate) routes: Vec<Route>,
    /// Index into `routes`.
    pub(crate) default_route: Option<usize>,
}

impl Topology {
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.iter().find(|node| &node.id == id)
    }

    pub fn deployments(&self) -> &[Deployment] {
        &self.deployments
    }

    pub fn deployment(&self, id: &DeploymentId) -> Option<&Deployment> {
        self.deployments
            .iter()
            .find(|deployment| &deployment.id == id)
    }

    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    pub fn default_route(&self) -> Option<&Route> {
        self.default_route.and_then(|index| self.routes.get(index))
    }

    /// The routes that list a deployment, in configured order.
    pub fn routes_using(&self, deployment: &DeploymentId) -> Vec<&RouteName> {
        self.routes
            .iter()
            .filter(|route| route.deployments.contains(deployment))
            .map(|route| &route.name)
            .collect()
    }

    /// Resolve a client's `model` field to a route.
    ///
    /// The same reading of `model` the gateway applies, through the same
    /// [`alias::ModelSelector`]: absent, blank and `default` all mean "the
    /// default", and the default is whatever the operator configured — never
    /// the first route, never a guess. Anything else must name a route, and a
    /// name that does not is refused rather than answered by another model.
    pub fn resolve(&self, requested: Option<&str>) -> Result<&Route, RoutingFailure> {
        match alias::ModelSelector::parse(requested) {
            alias::ModelSelector::Default => {
                self.default_route().ok_or(RoutingFailure::NoDefaultRoute)
            }
            alias::ModelSelector::Named(name) => self
                .routes
                .iter()
                .find(|route| route.name.matches(name))
                .ok_or_else(|| RoutingFailure::UnknownRoute {
                    requested: name.to_owned(),
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_id_that_could_blur_a_deployment_id_is_refused() {
        assert!(NodeId::parse("dell-7820").is_ok());
        assert!(NodeId::parse("t420.lan_2").is_ok());
        assert!(NodeId::parse("").is_err());
        assert!(NodeId::parse("a/b").is_err());
        assert!(NodeId::parse("has space").is_err());
        assert_eq!(NodeId::parse("  t420 ").map(|id| id.0), Ok("t420".into()));
    }

    #[test]
    fn a_deployment_id_is_the_node_and_the_model() {
        let node = NodeId::parse("dell-7820").unwrap();
        assert_eq!(
            DeploymentId::of(&node, "QwenCoder").as_str(),
            "dell-7820/QwenCoder"
        );
    }

    #[test]
    fn route_names_follow_the_alias_rules() {
        assert!(RouteName::parse("Coder").is_ok());
        assert!(RouteName::parse("default").is_err());
        assert!(RouteName::parse("DEFAULT").is_err());
        assert!(RouteName::parse("a/b").is_err());
        assert!(RouteName::parse("x@8k").is_err());
        let coder = RouteName::parse("Coder").unwrap();
        assert!(coder.matches("coder"));
        assert!(coder.matches(" CODER "));
        assert!(!coder.matches("Coder2"));
    }

    #[test]
    fn a_secret_never_prints() {
        let auth = NodeAuth::Bearer {
            env: "LIGHTWEIGHT_DELL_KEY".into(),
            secret: Secret::new("test-secret".into()),
        };
        let printed = format!("{auth:?}");
        assert!(!printed.contains("test-secret"), "{printed}");
        assert_eq!(auth.header_value().as_deref(), Some("Bearer test-secret"));
        assert_eq!(auth.kind(), "bearer");
    }

    #[test]
    fn an_endpoint_joins_without_doubling_the_slash() {
        let node = Node {
            id: NodeId::parse("a").unwrap(),
            base_url: reqwest::Url::parse("http://192.0.2.10:11434/").unwrap(),
            enabled: true,
            auth: NodeAuth::None,
        };
        assert_eq!(
            node.endpoint("/v1/models"),
            "http://192.0.2.10:11434/v1/models"
        );
        let proxied = Node {
            base_url: reqwest::Url::parse("https://gw.example.net/lightweight").unwrap(),
            ..node
        };
        assert_eq!(
            proxied.endpoint("/v1/models"),
            "https://gw.example.net/lightweight/v1/models"
        );
    }

    #[test]
    fn capabilities_intersect_to_what_both_honour() {
        let mut full = CapabilitySet::none();
        full.0.streaming = true;
        full.0.tools = true;
        let mut partial = CapabilitySet::none();
        partial.0.streaming = true;
        let both = full.intersect(&partial);
        assert!(both.0.streaming);
        assert!(!both.0.tools);
    }
}
