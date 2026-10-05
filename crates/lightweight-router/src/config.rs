//! The router's configuration file, and the validation that stands between it
//! and a running router.
//!
//! JSON, like `fleet.json`, and for the same reason: it is what the rest of
//! the workspace already reads, so no new parser joins the dependency graph.
//! Unknown keys are refused rather than ignored, because a misspelt
//! `"enabeld": false` that is silently dropped leaves a node the operator
//! meant to switch off taking traffic.
//!
//! Secrets are never in the file. A node names the environment variable its
//! key is in (`api_key_env`), and the router reads it at startup. A file can
//! therefore be committed, shared and pasted into an issue without leaking a
//! credential — and a literal `api_key` key is refused as unknown.
//!
//! Validation reports **every** problem at once, then refuses to start. A
//! router that came up with some routes and not others would answer
//! `model_not_found` for a route the operator believes exists, which is a
//! worse failure than not starting.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use lightweight_catalog::alias;
use serde::Deserialize;

use crate::domain::{
    Deployment, DeploymentId, Node, NodeAuth, NodeId, Route, RouteName, RoutePlacement,
    RoutePolicy, Secret, Topology,
};

/// The port a router listens on when the file names none.
///
/// Beside the gateway's 11434 rather than on it, so a router and a gateway can
/// share a machine without either being moved.
pub const DEFAULT_PORT: u16 = 11500;

/// How often a node is probed, by default.
pub const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// How long one probe may take, by default.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Consecutive failures that make a healthy node unhealthy, by default.
///
/// Two rather than one so a single dropped packet does not move traffic; a
/// node that is really gone is out of rotation within two intervals.
pub const DEFAULT_FAILURE_THRESHOLD: u32 = 2;
/// How long the router waits to connect to a node during a request, by default.
///
/// Connecting only. A generation has no timeout here, for the gateway's own
/// reason: a CPU prefill can take minutes, and a number chosen on a fast
/// machine must not cut one off.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The header a client names its session in, when session affinity is on and
/// the file names no other.
pub const DEFAULT_SESSION_HEADER: &str = "x-lightweight-session";
/// How long a session may sit idle before its affinity is forgotten, by
/// default. Long enough to cover a person reading an answer and typing the
/// next turn; short enough that an affinity is an operational hint and not a
/// record of who used the router.
pub const DEFAULT_SESSION_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
/// How many sessions' affinities are held at once, by default.
pub const DEFAULT_SESSION_MAX_ENTRIES: usize = 10_000;
/// The most a configuration may ask to hold. An entry is a few dozen bytes;
/// this bounds the map at tens of megabytes whatever the file says.
pub const MAX_SESSION_ENTRIES: usize = 1_000_000;
/// How often the placement controller compares each route with its target,
/// by default. Slower than health probing: a load takes seconds to minutes,
/// and re-deciding faster than one can finish only adds control traffic.
pub const DEFAULT_PLACEMENT_INTERVAL: Duration = Duration::from_secs(15);
/// How long one load may take, from the request to observed readiness, by
/// default. A CPU node can spend minutes starting a large model.
pub const DEFAULT_LOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// The first wait after a failed placement action, by default. Doubled on
/// each further failure of the same deployment.
pub const DEFAULT_PLACEMENT_BACKOFF: Duration = Duration::from_secs(30);
/// The longest wait between attempts on a failing deployment, by default.
pub const DEFAULT_PLACEMENT_BACKOFF_MAX: Duration = Duration::from_secs(600);

/// How many recent routing traces are kept for `/api/router/v1/traces`, by
/// default.
pub const DEFAULT_TRACE_CAPACITY: usize = 200;
/// The most traces a configuration may ask to keep.
pub const MAX_TRACE_CAPACITY: usize = 10_000;

/// Headers a session may not be read from: each already means something to
/// the router or to a node, and reusing one would either leak a credential
/// into the affinity map or tie affinity to something that is not a session.
const RESERVED_SESSION_HEADERS: [&str; 8] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "x-request-id",
    "content-type",
    "content-length",
    "accept",
    "host",
];

/// The file as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterFile {
    /// `host:port` socket addresses. Loopback on [`DEFAULT_PORT`] when empty.
    #[serde(default)]
    pub listen: Vec<String>,
    /// The environment variable holding the key clients must present to the
    /// router. Required when any listener is off loopback.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// The route that `default`, or no `model` at all, resolves to. Without
    /// one, such a request is refused rather than routed somewhere arbitrary.
    #[serde(default)]
    pub default_route: Option<String>,
    #[serde(default)]
    pub health: HealthFile,
    #[serde(default)]
    pub request: RequestFile,
    /// Optional. Absent means no affinity: every request is routed by its
    /// route's policy alone, exactly as before sessions existed.
    #[serde(default)]
    pub session_affinity: SessionAffinityFile,
    #[serde(default)]
    pub traces: TracesFile,
    /// How the placement controller runs. It runs only when some route has a
    /// `placement` target; these settings alone start nothing.
    #[serde(default)]
    pub placement: PlacementFile,
    #[serde(default)]
    pub nodes: Vec<NodeFile>,
    #[serde(default)]
    pub routes: Vec<RouteFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthFile {
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_threshold")]
    pub failure_threshold: u32,
}

impl Default for HealthFile {
    fn default() -> Self {
        Self {
            interval_secs: default_interval_secs(),
            timeout_secs: default_timeout_secs(),
            failure_threshold: default_threshold(),
        }
    }
}

const fn default_interval_secs() -> u64 {
    DEFAULT_PROBE_INTERVAL.as_secs()
}
const fn default_timeout_secs() -> u64 {
    DEFAULT_PROBE_TIMEOUT.as_secs()
}
const fn default_threshold() -> u32 {
    DEFAULT_FAILURE_THRESHOLD
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestFile {
    #[serde(default = "default_connect_secs")]
    pub connect_timeout_secs: u64,
}

impl Default for RequestFile {
    fn default() -> Self {
        Self {
            connect_timeout_secs: default_connect_secs(),
        }
    }
}

const fn default_connect_secs() -> u64 {
    DEFAULT_CONNECT_TIMEOUT.as_secs()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionAffinityFile {
    /// Off unless the operator turns it on.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_session_header")]
    pub header: String,
    #[serde(default = "default_session_ttl_secs")]
    pub idle_ttl_secs: u64,
    #[serde(default = "default_session_max_entries")]
    pub max_entries: usize,
}

impl Default for SessionAffinityFile {
    fn default() -> Self {
        Self {
            enabled: false,
            header: default_session_header(),
            idle_ttl_secs: default_session_ttl_secs(),
            max_entries: default_session_max_entries(),
        }
    }
}

fn default_session_header() -> String {
    DEFAULT_SESSION_HEADER.to_owned()
}
const fn default_session_ttl_secs() -> u64 {
    DEFAULT_SESSION_IDLE_TTL.as_secs()
}
const fn default_session_max_entries() -> usize {
    DEFAULT_SESSION_MAX_ENTRIES
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TracesFile {
    /// Recent routing traces kept in memory. `0` keeps none.
    #[serde(default = "default_trace_capacity")]
    pub capacity: usize,
}

impl Default for TracesFile {
    fn default() -> Self {
        Self {
            capacity: default_trace_capacity(),
        }
    }
}

const fn default_trace_capacity() -> usize {
    DEFAULT_TRACE_CAPACITY
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementFile {
    #[serde(default = "default_placement_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_load_timeout_secs")]
    pub load_timeout_secs: u64,
    #[serde(default = "default_backoff_secs")]
    pub backoff_secs: u64,
    #[serde(default = "default_backoff_max_secs")]
    pub backoff_max_secs: u64,
}

impl Default for PlacementFile {
    fn default() -> Self {
        Self {
            interval_secs: default_placement_interval_secs(),
            load_timeout_secs: default_load_timeout_secs(),
            backoff_secs: default_backoff_secs(),
            backoff_max_secs: default_backoff_max_secs(),
        }
    }
}

const fn default_placement_interval_secs() -> u64 {
    DEFAULT_PLACEMENT_INTERVAL.as_secs()
}
const fn default_load_timeout_secs() -> u64 {
    DEFAULT_LOAD_TIMEOUT.as_secs()
}
const fn default_backoff_secs() -> u64 {
    DEFAULT_PLACEMENT_BACKOFF.as_secs()
}
const fn default_backoff_max_secs() -> u64 {
    DEFAULT_PLACEMENT_BACKOFF_MAX.as_secs()
}

/// A route's placement target, as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutePlacementFile {
    #[serde(default = "one")]
    pub min_ready: u32,
    #[serde(default)]
    pub warm_standby: u32,
    /// Required: the controller never loads a model on a node the operator
    /// did not name for this route.
    pub allowed_nodes: Vec<String>,
}

const fn one() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeFile {
    pub id: String,
    pub url: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

const fn enabled_by_default() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteFile {
    pub name: String,
    #[serde(default)]
    pub strategy: RoutePolicy,
    #[serde(default)]
    pub deployments: Vec<DeploymentFile>,
    /// Optional. Absent: no placement for this route.
    #[serde(default)]
    pub placement: Option<RoutePlacementFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentFile {
    pub node: String,
    /// The model identity the node advertises in its own `/v1/models`.
    pub model: String,
}

/// How nodes are watched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HealthPolicy {
    pub interval: Duration,
    pub timeout: Duration,
    pub failure_threshold: u32,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            interval: DEFAULT_PROBE_INTERVAL,
            timeout: DEFAULT_PROBE_TIMEOUT,
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
        }
    }
}

/// How related requests are kept on one deployment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AffinityPolicy {
    pub enabled: bool,
    /// Lowercase, as HTTP header names are compared.
    pub header: String,
    pub idle_ttl: Duration,
    pub max_entries: usize,
}

impl Default for AffinityPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            header: DEFAULT_SESSION_HEADER.to_owned(),
            idle_ttl: DEFAULT_SESSION_IDLE_TTL,
            max_entries: DEFAULT_SESSION_MAX_ENTRIES,
        }
    }
}

/// How the placement controller runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacementPolicy {
    pub interval: Duration,
    pub load_timeout: Duration,
    pub backoff: Duration,
    pub backoff_max: Duration,
}

impl Default for PlacementPolicy {
    fn default() -> Self {
        Self {
            interval: DEFAULT_PLACEMENT_INTERVAL,
            load_timeout: DEFAULT_LOAD_TIMEOUT,
            backoff: DEFAULT_PLACEMENT_BACKOFF,
            backoff_max: DEFAULT_PLACEMENT_BACKOFF_MAX,
        }
    }
}

/// A configuration that passed every check.
#[derive(Clone, Debug)]
pub struct RouterConfig {
    pub topology: Topology,
    pub listen: Vec<SocketAddr>,
    /// The key clients present to the router. Never sent to a node.
    pub client_key: Option<Secret>,
    pub health: HealthPolicy,
    pub connect_timeout: Duration,
    pub affinity: AffinityPolicy,
    /// How many recent routing traces to keep. `0` keeps none.
    pub trace_capacity: usize,
    pub placement: PlacementPolicy,
}

/// One reason a configuration was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("the configuration lists no nodes")]
    NoNodes,
    #[error("the configuration lists no routes")]
    NoRoutes,
    #[error("listen address {value:?} is not a host:port socket address")]
    BadListen { value: String },
    #[error("node {id:?}: {problem}")]
    BadNodeId { id: String, problem: String },
    #[error("node id {id:?} is used more than once")]
    DuplicateNode { id: String },
    #[error("node {node:?}: url {url:?} {problem}")]
    BadUrl {
        node: String,
        url: String,
        problem: String,
    },
    #[error("{owner}: api_key_env {var:?} is not a usable environment variable name")]
    BadEnvName { owner: String, var: String },
    #[error("{owner}: the environment variable {var:?} is not set (or is empty)")]
    MissingEnv { owner: String, var: String },
    #[error("route {name:?}: {problem}")]
    BadRouteName { name: String, problem: String },
    #[error(
        "route {name:?}: `default` is reserved; set \"default_route\" to choose the route it means"
    )]
    ReservedRouteName { name: String },
    #[error("route name {name:?} is used more than once (names are compared ignoring case)")]
    DuplicateRoute { name: String },
    #[error("route {route:?} has no deployments")]
    EmptyRoute { route: String },
    #[error("route {route:?}: deployment names node {node:?}, which is not configured")]
    UnknownNode { route: String, node: String },
    #[error("route {route:?}: a deployment on node {node:?} has an empty model")]
    EmptyModel { route: String, node: String },
    #[error("route {route:?}: a deployment on node {node:?} has a model with a control character")]
    BadModel { route: String, node: String },
    #[error("route {route:?} lists deployment {deployment:?} more than once")]
    DuplicateDeployment { route: String, deployment: String },
    #[error("default_route {name:?} is not one of the configured routes")]
    UnknownDefaultRoute { name: String },
    #[error("health.{field} must be at least {minimum}")]
    BadHealth { field: &'static str, minimum: u64 },
    #[error("health.timeout_secs must not exceed health.interval_secs")]
    TimeoutExceedsInterval,
    #[error("request.connect_timeout_secs must be at least 1")]
    BadConnectTimeout,
    #[error("session_affinity.header {header:?} {problem}")]
    BadSessionHeader {
        header: String,
        problem: &'static str,
    },
    #[error("session_affinity.{field} must be between {minimum} and {maximum}")]
    BadSessionLimit {
        field: &'static str,
        minimum: u64,
        maximum: u64,
    },
    #[error("traces.capacity must be at most {maximum}")]
    BadTraceCapacity { maximum: usize },
    #[error("placement.{field} must be at least {minimum}")]
    BadPlacementTiming { field: &'static str, minimum: u64 },
    #[error("placement.backoff_max_secs must not be less than placement.backoff_secs")]
    BackoffMaxBelowInitial,
    #[error("route {route:?}: placement {problem}")]
    BadRoutePlacement { route: String, problem: String },
}

/// Every reason a configuration was refused, in file order.
#[derive(Debug, PartialEq, Eq)]
pub struct ConfigErrors(pub Vec<ConfigError>);

impl std::fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index > 0 {
                writeln!(f)?;
            }
            write!(f, "  - {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigErrors {}

/// Why a file could not even be read as a configuration.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("{path} is not a valid router configuration: {source}")]
    Parse {
        path: String,
        source: serde_json::Error,
    },
    #[error("{path} was refused:\n{errors}")]
    Invalid { path: String, errors: ConfigErrors },
}

/// Read and validate a configuration file against the process environment.
pub fn load(path: &Path) -> Result<RouterConfig, LoadError> {
    let display = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|source| LoadError::Read {
        path: display.clone(),
        source,
    })?;
    let file: RouterFile = serde_json::from_str(&text).map_err(|source| LoadError::Parse {
        path: display.clone(),
        source,
    })?;
    validate(file, &|name| std::env::var(name).ok()).map_err(|errors| LoadError::Invalid {
        path: display,
        errors,
    })
}

/// Check a parsed file, resolving secrets through `env`.
///
/// `env` is a parameter so tests can supply an environment without mutating
/// the process's, which is shared by every test running beside them.
pub fn validate(
    file: RouterFile,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<RouterConfig, ConfigErrors> {
    let mut errors = Vec::new();

    let listen = validate_listen(&file.listen, &mut errors);
    let client_key = file
        .api_key_env
        .as_deref()
        .and_then(|var| read_secret("router", var, env, &mut errors));
    let health = validate_health(&file.health, &mut errors);
    if file.request.connect_timeout_secs == 0 {
        errors.push(ConfigError::BadConnectTimeout);
    }
    let affinity = validate_affinity(&file.session_affinity, &mut errors);
    let placement = validate_placement(&file.placement, &mut errors);
    if file.traces.capacity > MAX_TRACE_CAPACITY {
        errors.push(ConfigError::BadTraceCapacity {
            maximum: MAX_TRACE_CAPACITY,
        });
    }

    if file.nodes.is_empty() {
        errors.push(ConfigError::NoNodes);
    }
    if file.routes.is_empty() {
        errors.push(ConfigError::NoRoutes);
    }

    let nodes = validate_nodes(&file.nodes, env, &mut errors);
    let (routes, deployments) = validate_routes(&file.routes, &nodes, &mut errors);

    let default_route = file.default_route.as_deref().and_then(|name| {
        if alias::is_reserved(name) {
            errors.push(ConfigError::UnknownDefaultRoute {
                name: name.to_owned(),
            });
            return None;
        }
        let found = routes.iter().position(|route| route.name.matches(name));
        if found.is_none() {
            errors.push(ConfigError::UnknownDefaultRoute {
                name: name.to_owned(),
            });
        }
        found
    });

    if !errors.is_empty() {
        return Err(ConfigErrors(errors));
    }

    Ok(RouterConfig {
        topology: Topology {
            nodes,
            deployments,
            routes,
            default_route,
        },
        listen,
        client_key,
        health,
        connect_timeout: Duration::from_secs(file.request.connect_timeout_secs),
        affinity,
        trace_capacity: file.traces.capacity,
        placement,
    })
}

fn validate_placement(raw: &PlacementFile, errors: &mut Vec<ConfigError>) -> PlacementPolicy {
    for (field, value, minimum) in [
        ("interval_secs", raw.interval_secs, 1),
        ("load_timeout_secs", raw.load_timeout_secs, 1),
        ("backoff_secs", raw.backoff_secs, 1),
    ] {
        if value < minimum {
            errors.push(ConfigError::BadPlacementTiming { field, minimum });
        }
    }
    if raw.backoff_max_secs < raw.backoff_secs {
        errors.push(ConfigError::BackoffMaxBelowInitial);
    }
    PlacementPolicy {
        interval: Duration::from_secs(raw.interval_secs),
        load_timeout: Duration::from_secs(raw.load_timeout_secs),
        backoff: Duration::from_secs(raw.backoff_secs),
        backoff_max: Duration::from_secs(raw.backoff_max_secs),
    }
}

/// Check one route's placement against the deployments it actually has.
fn validate_route_placement(
    route: &RouteName,
    raw: &RoutePlacementFile,
    members: &[DeploymentId],
    deployments: &[Deployment],
    errors: &mut Vec<ConfigError>,
) -> Option<RoutePlacement> {
    let mut fail = |problem: String| {
        errors.push(ConfigError::BadRoutePlacement {
            route: route.as_str().to_owned(),
            problem,
        });
    };
    if raw.min_ready == 0 {
        fail("min_ready must be at least 1".into());
        return None;
    }
    if raw.allowed_nodes.is_empty() {
        fail("allowed_nodes must name at least one node".into());
        return None;
    }
    let mut allowed = Vec::new();
    let mut named = BTreeSet::new();
    for node in &raw.allowed_nodes {
        if !named.insert(node.to_ascii_lowercase()) {
            fail(format!("allowed_nodes lists {node:?} more than once"));
            return None;
        }
        // Only a node this route already has a deployment on: placement
        // loads the model the route names there, never a model on a node the
        // route does not use.
        let found: Vec<&DeploymentId> = members
            .iter()
            .filter(|id| {
                deployments
                    .iter()
                    .any(|d| &d.id == *id && d.node.as_str().eq_ignore_ascii_case(node))
            })
            .collect();
        if found.is_empty() {
            fail(format!(
                "allowed_nodes names {node:?}, which has no deployment in this route"
            ));
            return None;
        }
        allowed.extend(found.into_iter().cloned());
    }
    // Configured order, whatever order allowed_nodes was written in.
    allowed.sort_by_key(|id| members.iter().position(|member| member == id));
    let placement = RoutePlacement {
        min_ready: raw.min_ready,
        warm_standby: raw.warm_standby,
        allowed,
    };
    let reachable = members.len();
    if placement.target() as usize > reachable {
        fail(format!(
            "asks for {} ready deployments, but the route has only {reachable}",
            placement.target()
        ));
        return None;
    }
    Some(placement)
}

/// Check the affinity settings. They are checked even while affinity is off,
/// so turning it on later cannot be the moment a typo surfaces.
fn validate_affinity(raw: &SessionAffinityFile, errors: &mut Vec<ConfigError>) -> AffinityPolicy {
    let header = raw.header.trim().to_ascii_lowercase();
    let problem = if header.is_empty() {
        Some("is empty")
    } else if axum::http::HeaderName::from_bytes(header.as_bytes()).is_err() {
        Some("is not a valid HTTP header name")
    } else if RESERVED_SESSION_HEADERS.contains(&header.as_str()) {
        Some("already means something else and cannot carry a session")
    } else {
        None
    };
    if let Some(problem) = problem {
        errors.push(ConfigError::BadSessionHeader {
            header: raw.header.clone(),
            problem,
        });
    }
    // A day at most: an affinity is a hint for one conversation, not a
    // long-lived record of who talks to the router.
    const MAX_TTL_SECS: u64 = 24 * 60 * 60;
    if raw.idle_ttl_secs == 0 || raw.idle_ttl_secs > MAX_TTL_SECS {
        errors.push(ConfigError::BadSessionLimit {
            field: "idle_ttl_secs",
            minimum: 1,
            maximum: MAX_TTL_SECS,
        });
    }
    if raw.max_entries == 0 || raw.max_entries > MAX_SESSION_ENTRIES {
        errors.push(ConfigError::BadSessionLimit {
            field: "max_entries",
            minimum: 1,
            maximum: MAX_SESSION_ENTRIES as u64,
        });
    }
    AffinityPolicy {
        enabled: raw.enabled,
        header,
        idle_ttl: Duration::from_secs(raw.idle_ttl_secs),
        max_entries: raw.max_entries,
    }
}

fn validate_listen(raw: &[String], errors: &mut Vec<ConfigError>) -> Vec<SocketAddr> {
    if raw.is_empty() {
        return vec![SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT))];
    }
    raw.iter()
        .filter_map(|value| match value.trim().parse::<SocketAddr>() {
            Ok(address) => Some(address),
            Err(_) => {
                errors.push(ConfigError::BadListen {
                    value: value.clone(),
                });
                None
            }
        })
        .collect()
}

fn validate_health(raw: &HealthFile, errors: &mut Vec<ConfigError>) -> HealthPolicy {
    if raw.interval_secs == 0 {
        errors.push(ConfigError::BadHealth {
            field: "interval_secs",
            minimum: 1,
        });
    }
    if raw.timeout_secs == 0 {
        errors.push(ConfigError::BadHealth {
            field: "timeout_secs",
            minimum: 1,
        });
    }
    if raw.failure_threshold == 0 {
        errors.push(ConfigError::BadHealth {
            field: "failure_threshold",
            minimum: 1,
        });
    }
    // A probe that may outlive its interval would overlap the next one, and
    // two verdicts on one node racing each other is the flapping the threshold
    // exists to prevent.
    if raw.timeout_secs > raw.interval_secs {
        errors.push(ConfigError::TimeoutExceedsInterval);
    }
    HealthPolicy {
        interval: Duration::from_secs(raw.interval_secs),
        timeout: Duration::from_secs(raw.timeout_secs),
        failure_threshold: raw.failure_threshold,
    }
}

/// Read one secret from the environment, recording why it could not be.
fn read_secret(
    owner: &str,
    var: &str,
    env: &dyn Fn(&str) -> Option<String>,
    errors: &mut Vec<ConfigError>,
) -> Option<Secret> {
    let usable_name = !var.is_empty()
        && var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !var.starts_with(|c: char| c.is_ascii_digit());
    if !usable_name {
        errors.push(ConfigError::BadEnvName {
            owner: owner.to_owned(),
            var: var.to_owned(),
        });
        return None;
    }
    match env(var).map(|value| value.trim().to_owned()) {
        Some(value) if !value.is_empty() => Some(Secret::new(value)),
        _ => {
            errors.push(ConfigError::MissingEnv {
                owner: owner.to_owned(),
                var: var.to_owned(),
            });
            None
        }
    }
}

fn validate_nodes(
    raw: &[NodeFile],
    env: &dyn Fn(&str) -> Option<String>,
    errors: &mut Vec<ConfigError>,
) -> Vec<Node> {
    let mut seen = BTreeSet::new();
    let mut nodes = Vec::with_capacity(raw.len());
    for entry in raw {
        let id = match NodeId::parse(&entry.id) {
            Ok(id) => id,
            Err(problem) => {
                errors.push(ConfigError::BadNodeId {
                    id: entry.id.clone(),
                    problem,
                });
                continue;
            }
        };
        // Ignoring case: `Dell` and `dell` side by side are a typo, not two
        // machines, and a log line naming one would be read as the other.
        if !seen.insert(id.as_str().to_ascii_lowercase()) {
            errors.push(ConfigError::DuplicateNode {
                id: id.as_str().to_owned(),
            });
            continue;
        }

        let base_url = match validate_url(&entry.url) {
            Ok(url) => Some(url),
            Err(problem) => {
                errors.push(ConfigError::BadUrl {
                    node: id.as_str().to_owned(),
                    url: entry.url.clone(),
                    problem: problem.to_owned(),
                });
                None
            }
        };

        // A disabled node is never contacted, so its key is not demanded: the
        // operator's off switch must not be blocked by the credential of the
        // machine being switched off.
        let auth = match (&entry.api_key_env, entry.enabled) {
            (Some(var), true) => {
                match read_secret(&format!("node {:?}", id.as_str()), var, env, errors) {
                    Some(secret) => NodeAuth::Bearer {
                        env: var.clone(),
                        secret,
                    },
                    None => NodeAuth::None,
                }
            }
            _ => NodeAuth::None,
        };

        if let Some(base_url) = base_url {
            nodes.push(Node {
                id,
                base_url,
                enabled: entry.enabled,
                auth,
            });
        }
    }
    nodes
}

/// Check a node URL, returning the reason it is unusable.
fn validate_url(raw: &str) -> Result<reqwest::Url, &'static str> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|_| "is not a valid URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("must use http or https");
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("has no host");
    }
    // A credential in the URL would be printed by the control API and by every
    // log line that names the node. Keys go in `api_key_env`.
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not carry credentials; name an environment variable in api_key_env");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("must not have a query or fragment");
    }
    Ok(url)
}

fn validate_routes(
    raw: &[RouteFile],
    nodes: &[Node],
    errors: &mut Vec<ConfigError>,
) -> (Vec<Route>, Vec<Deployment>) {
    let mut routes: Vec<Route> = Vec::with_capacity(raw.len());
    let mut deployments: Vec<Deployment> = Vec::new();

    for entry in raw {
        let name = match RouteName::parse(&entry.name) {
            Ok(name) => name,
            Err(alias::AliasProblem::Reserved) => {
                errors.push(ConfigError::ReservedRouteName {
                    name: entry.name.clone(),
                });
                continue;
            }
            Err(problem) => {
                errors.push(ConfigError::BadRouteName {
                    name: entry.name.clone(),
                    problem: problem.to_string(),
                });
                continue;
            }
        };
        if routes.iter().any(|route| route.name.matches(name.as_str())) {
            errors.push(ConfigError::DuplicateRoute {
                name: name.as_str().to_owned(),
            });
            continue;
        }
        if entry.deployments.is_empty() {
            errors.push(ConfigError::EmptyRoute {
                route: name.as_str().to_owned(),
            });
            continue;
        }

        let mut members: Vec<DeploymentId> = Vec::with_capacity(entry.deployments.len());
        for listed in &entry.deployments {
            let Some(node) = nodes
                .iter()
                .find(|node| node.id.as_str() == listed.node.trim())
            else {
                // A node that failed its own validation has already been
                // reported; naming it again as "unknown" would be noise.
                if NodeId::parse(&listed.node).is_ok()
                    && !errors.iter().any(|error| {
                        matches!(error, ConfigError::BadUrl { node, .. } if node == listed.node.trim())
                    })
                {
                    errors.push(ConfigError::UnknownNode {
                        route: name.as_str().to_owned(),
                        node: listed.node.clone(),
                    });
                }
                continue;
            };
            let model = listed.model.trim();
            if model.is_empty() {
                errors.push(ConfigError::EmptyModel {
                    route: name.as_str().to_owned(),
                    node: node.id.as_str().to_owned(),
                });
                continue;
            }
            if model.chars().any(char::is_control) {
                errors.push(ConfigError::BadModel {
                    route: name.as_str().to_owned(),
                    node: node.id.as_str().to_owned(),
                });
                continue;
            }

            // One deployment per node and model, compared the way a node
            // compares its aliases. A second route naming the same pair shares
            // the deployment rather than creating a twin with its own health.
            let existing = deployments.iter().find(|deployment| {
                deployment.node == node.id && alias::same_name(&deployment.remote_model, model)
            });
            let id = match existing {
                Some(deployment) => deployment.id.clone(),
                None => {
                    let deployment = Deployment {
                        id: DeploymentId::of(&node.id, model),
                        node: node.id.clone(),
                        remote_model: model.to_owned(),
                    };
                    let id = deployment.id.clone();
                    deployments.push(deployment);
                    id
                }
            };
            if members.contains(&id) {
                errors.push(ConfigError::DuplicateDeployment {
                    route: name.as_str().to_owned(),
                    deployment: id.as_str().to_owned(),
                });
                continue;
            }
            members.push(id);
        }

        let placement = entry
            .placement
            .as_ref()
            .and_then(|raw| validate_route_placement(&name, raw, &members, &deployments, errors));
        routes.push(Route {
            name,
            policy: entry.strategy,
            deployments: members,
            placement,
        });
    }

    (routes, deployments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: serde_json::Value) -> RouterFile {
        serde_json::from_value(value).expect("the file shape parses")
    }

    fn env_with(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn errors_of(value: serde_json::Value) -> Vec<ConfigError> {
        validate(parse(value), &no_env).expect_err("refused").0
    }

    fn valid() -> serde_json::Value {
        json!({
            "default_route": "Fast",
            "nodes": [
                {"id": "dell-7820", "url": "http://192.0.2.10:11434", "api_key_env": "LIGHTWEIGHT_DELL_KEY"},
                {"id": "t420", "url": "http://192.0.2.11:11434", "api_key_env": "LIGHTWEIGHT_T420_KEY"}
            ],
            "routes": [
                {"name": "Coder", "strategy": "priority", "deployments": [
                    {"node": "dell-7820", "model": "QwenCoder"},
                    {"node": "t420", "model": "CoderBackup"}
                ]},
                {"name": "Fast", "deployments": [{"node": "t420", "model": "Fast"}]}
            ]
        })
    }

    const KEYS: &[(&str, &str)] = &[
        ("LIGHTWEIGHT_DELL_KEY", "dell-secret"),
        ("LIGHTWEIGHT_T420_KEY", "t420-secret"),
    ];

    #[test]
    fn a_valid_priority_configuration_is_accepted() {
        let config = validate(parse(valid()), &env_with(KEYS)).expect("valid");
        let topology = &config.topology;
        assert_eq!(topology.nodes().len(), 2);
        assert_eq!(topology.routes().len(), 2);
        assert_eq!(topology.deployments().len(), 3);

        let coder = &topology.routes()[0];
        assert_eq!(coder.policy, RoutePolicy::Priority);
        assert_eq!(
            coder
                .deployments
                .iter()
                .map(DeploymentId::as_str)
                .collect::<Vec<_>>(),
            ["dell-7820/QwenCoder", "t420/CoderBackup"]
        );
        assert_eq!(
            topology.default_route().map(|r| r.name.as_str()),
            Some("Fast")
        );
        assert_eq!(
            config.listen,
            [SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT))]
        );
        assert_eq!(config.health, HealthPolicy::default());

        // Each node carries its own key, not a shared one.
        let keys: Vec<_> = topology
            .nodes()
            .iter()
            .map(|node| node.auth.header_value())
            .collect();
        assert_eq!(
            keys,
            [
                Some("Bearer dell-secret".to_owned()),
                Some("Bearer t420-secret".to_owned())
            ]
        );
    }

    #[test]
    fn duplicate_route_names_are_refused_ignoring_case() {
        let mut file = valid();
        file["routes"][1]["name"] = json!("coder");
        let errors = validate(parse(file), &env_with(KEYS)).unwrap_err().0;
        assert!(
            errors.contains(&ConfigError::DuplicateRoute {
                name: "coder".into()
            }),
            "{errors:?}"
        );
    }

    #[test]
    fn duplicate_node_ids_are_refused() {
        let mut file = valid();
        file["nodes"][1]["id"] = json!("dell-7820");
        let errors = validate(parse(file), &env_with(KEYS)).unwrap_err().0;
        assert!(errors.contains(&ConfigError::DuplicateNode {
            id: "dell-7820".into()
        }));
    }

    #[test]
    fn an_unknown_node_reference_is_refused() {
        let mut file = valid();
        file["routes"][1]["deployments"][0]["node"] = json!("ghost");
        let errors = validate(parse(file), &env_with(KEYS)).unwrap_err().0;
        assert_eq!(
            errors,
            [ConfigError::UnknownNode {
                route: "Fast".into(),
                node: "ghost".into()
            }]
        );
    }

    #[test]
    fn an_empty_route_is_refused() {
        let mut file = valid();
        file["routes"][1]["deployments"] = json!([]);
        let errors = validate(parse(file), &env_with(KEYS)).unwrap_err().0;
        assert!(errors.contains(&ConfigError::EmptyRoute {
            route: "Fast".into()
        }));
    }

    #[test]
    fn a_route_called_default_is_refused() {
        let mut file = valid();
        file["routes"][1]["name"] = json!("Default");
        file["default_route"] = json!("Coder");
        let errors = validate(parse(file), &env_with(KEYS)).unwrap_err().0;
        assert_eq!(
            errors,
            [ConfigError::ReservedRouteName {
                name: "Default".into()
            }]
        );
    }

    #[test]
    fn default_route_must_name_a_route_and_not_itself() {
        let mut file = valid();
        file["default_route"] = json!("Research");
        let errors = validate(parse(file), &env_with(KEYS)).unwrap_err().0;
        assert_eq!(
            errors,
            [ConfigError::UnknownDefaultRoute {
                name: "Research".into()
            }]
        );

        let mut file = valid();
        file["default_route"] = json!("default");
        assert!(validate(parse(file), &env_with(KEYS)).is_err());

        // Absent is allowed: such a router refuses `default` instead.
        let mut file = valid();
        file.as_object_mut().unwrap().remove("default_route");
        let config = validate(parse(file), &env_with(KEYS)).expect("valid");
        assert!(config.topology.default_route().is_none());
    }

    #[test]
    fn malformed_urls_are_refused_with_the_reason() {
        for (url, expected) in [
            ("not a url", "is not a valid URL"),
            ("ftp://192.0.2.10/", "must use http or https"),
            (
                "http://user:pw@192.0.2.10:11434",
                "must not carry credentials; name an environment variable in api_key_env",
            ),
            (
                "http://192.0.2.10:11434/?key=x",
                "must not have a query or fragment",
            ),
        ] {
            let mut file = valid();
            file["nodes"][0]["url"] = json!(url);
            let errors = validate(parse(file), &env_with(KEYS)).unwrap_err().0;
            assert_eq!(
                errors,
                [ConfigError::BadUrl {
                    node: "dell-7820".into(),
                    url: url.into(),
                    problem: expected.into()
                }],
                "{url}"
            );
        }
    }

    #[test]
    fn a_missing_environment_variable_is_refused_and_named() {
        let errors = errors_of(valid());
        assert!(errors.contains(&ConfigError::MissingEnv {
            owner: "node \"dell-7820\"".into(),
            var: "LIGHTWEIGHT_DELL_KEY".into()
        }));
        assert!(errors.contains(&ConfigError::MissingEnv {
            owner: "node \"t420\"".into(),
            var: "LIGHTWEIGHT_T420_KEY".into()
        }));
    }

    #[test]
    fn a_disabled_node_does_not_need_its_key() {
        let mut file = valid();
        file["nodes"][0]["enabled"] = json!(false);
        let config = validate(parse(file), &env_with(&[("LIGHTWEIGHT_T420_KEY", "k")]))
            .expect("the disabled node's key is not demanded");
        assert!(!config.topology.nodes()[0].enabled);
    }

    #[test]
    fn a_literal_key_in_the_file_is_refused() {
        let mut file = valid();
        file["nodes"][0]["api_key"] = json!("sk-lw-oops");
        let error = serde_json::from_value::<RouterFile>(file).unwrap_err();
        assert!(error.to_string().contains("api_key"), "{error}");
    }

    #[test]
    fn the_same_deployment_twice_in_one_route_is_refused() {
        let mut file = valid();
        file["routes"][0]["deployments"][1] = json!({"node": "dell-7820", "model": "qwencoder"});
        let errors = validate(parse(file), &env_with(KEYS)).unwrap_err().0;
        assert_eq!(
            errors,
            [ConfigError::DuplicateDeployment {
                route: "Coder".into(),
                deployment: "dell-7820/QwenCoder".into()
            }]
        );
    }

    #[test]
    fn two_routes_naming_one_node_and_model_share_one_deployment() {
        let mut file = valid();
        file["routes"][1]["deployments"][0] = json!({"node": "t420", "model": "CoderBackup"});
        let config = validate(parse(file), &env_with(KEYS)).expect("valid");
        assert_eq!(config.topology.deployments().len(), 2);
        let shared = DeploymentId::of(&NodeId::parse("t420").unwrap(), "CoderBackup");
        assert_eq!(config.topology.routes_using(&shared).len(), 2);
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let errors = errors_of(json!({
            "listen": ["nowhere"],
            "health": {"interval_secs": 0},
            "nodes": [],
            "routes": []
        }));
        assert!(errors.contains(&ConfigError::BadListen {
            value: "nowhere".into()
        }));
        assert!(errors.contains(&ConfigError::NoNodes));
        assert!(errors.contains(&ConfigError::NoRoutes));
        assert!(errors.contains(&ConfigError::BadHealth {
            field: "interval_secs",
            minimum: 1
        }));
    }

    #[test]
    fn the_router_key_is_read_from_its_own_variable() {
        let mut file = valid();
        file["api_key_env"] = json!("LIGHTWEIGHT_ROUTER_KEY");
        let errors = validate(parse(file), &env_with(KEYS)).unwrap_err().0;
        assert_eq!(
            errors,
            [ConfigError::MissingEnv {
                owner: "router".into(),
                var: "LIGHTWEIGHT_ROUTER_KEY".into()
            }]
        );
    }

    #[test]
    fn affinity_is_off_and_traces_are_bounded_when_the_file_says_nothing() {
        let config = validate(parse(valid()), &env_with(KEYS)).expect("valid");
        assert_eq!(config.affinity, AffinityPolicy::default());
        assert!(!config.affinity.enabled);
        assert_eq!(config.trace_capacity, DEFAULT_TRACE_CAPACITY);
    }

    #[test]
    fn affinity_settings_are_read_and_the_header_is_lowercased() {
        let mut file = valid();
        file["session_affinity"] = json!({
            "enabled": true,
            "header": "X-Conversation",
            "idle_ttl_secs": 60,
            "max_entries": 5
        });
        file["traces"] = json!({"capacity": 0});
        let config = validate(parse(file), &env_with(KEYS)).expect("valid");
        assert_eq!(
            config.affinity,
            AffinityPolicy {
                enabled: true,
                header: "x-conversation".into(),
                idle_ttl: Duration::from_secs(60),
                max_entries: 5,
            }
        );
        assert_eq!(config.trace_capacity, 0);
    }

    #[test]
    fn a_session_header_that_already_means_something_is_refused() {
        for header in ["Authorization", "x-request-id", "Cookie", "", "bad header"] {
            let mut file = valid();
            file["session_affinity"] = json!({"enabled": true, "header": header});
            let errors = validate(parse(file), &env_with(KEYS)).unwrap_err().0;
            assert!(
                matches!(errors.as_slice(), [ConfigError::BadSessionHeader { .. }]),
                "{header:?}: {errors:?}"
            );
        }
    }

    #[test]
    fn unbounded_affinity_and_trace_limits_are_refused() {
        let mut file = valid();
        file["session_affinity"] = json!({"idle_ttl_secs": 0, "max_entries": 0});
        file["traces"] = json!({"capacity": MAX_TRACE_CAPACITY + 1});
        let errors = validate(parse(file), &env_with(KEYS)).unwrap_err().0;
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors.contains(&ConfigError::BadTraceCapacity {
            maximum: MAX_TRACE_CAPACITY
        }));
        let mut file = valid();
        file["session_affinity"] = json!({"bogus": true});
        assert!(
            serde_json::from_value::<RouterFile>(file).is_err(),
            "unknown keys are refused"
        );
    }

    #[test]
    fn placement_is_absent_unless_a_route_asks_for_it() {
        let config = validate(parse(valid()), &env_with(KEYS)).expect("valid");
        assert!(
            config
                .topology
                .routes()
                .iter()
                .all(|route| route.placement.is_none())
        );
        assert_eq!(config.placement, PlacementPolicy::default());
    }

    #[test]
    fn a_route_placement_is_read_in_configured_order() {
        let mut file = valid();
        file["routes"][0]["placement"] =
            json!({"min_ready": 1, "warm_standby": 1, "allowed_nodes": ["t420", "dell-7820"]});
        file["placement"] = json!({"interval_secs": 5, "load_timeout_secs": 120,
                                   "backoff_secs": 10, "backoff_max_secs": 300});
        let config = validate(parse(file), &env_with(KEYS)).expect("valid");
        let placement = config.topology.routes()[0].placement.clone().unwrap();
        assert_eq!(placement.target(), 2);
        let allowed: Vec<&str> = placement.allowed.iter().map(DeploymentId::as_str).collect();
        assert_eq!(allowed, ["dell-7820/QwenCoder", "t420/CoderBackup"]);
        assert_eq!(config.placement.interval, Duration::from_secs(5));
        assert_eq!(config.placement.backoff_max, Duration::from_secs(300));
    }

    #[test]
    fn an_unreachable_or_unbounded_placement_is_refused() {
        let cases = [
            (json!({"allowed_nodes": []}), "allowed_nodes must name"),
            (
                json!({"min_ready": 0, "allowed_nodes": ["t420"]}),
                "min_ready",
            ),
            (
                json!({"allowed_nodes": ["nowhere"]}),
                "no deployment in this route",
            ),
            // Fast has one deployment; two ready can never happen.
            (
                json!({"warm_standby": 1, "allowed_nodes": ["t420"]}),
                "has only 1",
            ),
            (json!({"allowed_nodes": ["t420", "T420"]}), "more than once"),
        ];
        for (placement, expected) in cases {
            let mut file = valid();
            file["routes"][1]["placement"] = placement.clone();
            let errors = validate(parse(file), &env_with(KEYS)).unwrap_err().0;
            assert!(
                matches!(&errors[..], [ConfigError::BadRoutePlacement { problem, .. }] if problem.contains(expected)),
                "{placement}: {errors:?}"
            );
        }
        // dell-7820 has no deployment in Fast.
        let mut file = valid();
        file["routes"][1]["placement"] = json!({"allowed_nodes": ["dell-7820"]});
        assert!(validate(parse(file), &env_with(KEYS)).is_err());

        let mut file = valid();
        file["placement"] = json!({"interval_secs": 0, "backoff_secs": 60, "backoff_max_secs": 30});
        let errors = validate(parse(file), &env_with(KEYS)).unwrap_err().0;
        assert!(errors.contains(&ConfigError::BadPlacementTiming {
            field: "interval_secs",
            minimum: 1
        }));
        assert!(errors.contains(&ConfigError::BackoffMaxBelowInitial));
        let mut file = valid();
        file["routes"][0]["placement"] = json!({"allowed_nodes": ["t420"], "max": 3});
        assert!(
            serde_json::from_value::<RouterFile>(file).is_err(),
            "unknown keys are refused"
        );
    }
}
