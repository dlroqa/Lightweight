//! A federated model router for Lightweight.
//!
//! One OpenAI-compatible endpoint in front of any number of Lightweight
//! gateways. A client — Lightagent, an SDK, `curl` — discovers and sends stable
//! logical names (`Coder`, `Fast`); the router decides which node answers and
//! what that node calls the model; the node decides how the model runs.
//!
//! ```text
//! client ──model="Coder"──▶ router ──model="QwenCoder"──▶ node A (or B)
//!        ◀─model="Coder"───        ◀─model="QwenCoder"──
//! ```
//!
//! What this crate owns: routes, the deployment registry, node health, the
//! priority policy, forwarding, stream relaying, pre-response failover, and its
//! own logs and metrics. What it deliberately does not: GGUF, memory
//! estimates, admission, scheduling, engine lifecycle, or loading anything.
//! Those stay on the node, and nothing here can reach them — the router only
//! speaks the node's public `/v1` surface.
//!
//! Selection is deterministic: health decides which deployments are eligible,
//! then the route's policy — priority, round-robin or least-busy — orders
//! them. See `docs/ROUTER.md` for the roadmap beyond it.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod api;
pub mod capability;
pub mod config;
pub mod domain;
pub mod error;
pub mod health;
pub mod load;
pub mod metrics;
pub mod proxy;
pub mod requirements;
pub mod select;
pub mod sse;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;

use lightweight_gateway::AuthPolicy;
use lightweight_observability::targets;
use tokio_util::sync::CancellationToken;

pub use config::{RouterConfig, load, validate};
pub use domain::Topology;

use crate::config::HealthPolicy;
use crate::health::HealthBook;
use crate::load::LoadBook;
use crate::metrics::RouterMetrics;
use crate::select::Selector;

/// Everything a request handler needs.
#[derive(Debug)]
pub struct RouterState {
    pub topology: Arc<Topology>,
    pub health: Arc<HealthBook>,
    pub policy: HealthPolicy,
    /// The client-facing policy. Built from the router's own key and its own
    /// listeners; it has never seen a node's key.
    pub auth: AuthPolicy,
    /// Shared by probes and requests, so connections to a node are pooled.
    pub client: reqwest::Client,
    pub metrics: RouterMetrics,
    /// Per-route policy state and the per-deployment in-flight counts.
    pub selector: Selector,
    pub started: SystemTime,
}

/// Why a router could not be started.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(
        "a listener is not on loopback, so the router requires a client key: \
         set \"api_key_env\" in the configuration to an environment variable holding one"
    )]
    NeedsKey,
    #[error("could not build the HTTP client: {0}")]
    Client(String),
    #[error("could not listen on {address}: {source}")]
    Bind {
        address: SocketAddr,
        source: std::io::Error,
    },
}

impl RouterState {
    pub fn new(config: &RouterConfig) -> Result<Self, StartError> {
        let addresses: Vec<_> = config.listen.iter().map(SocketAddr::ip).collect();
        // The gateway's own rule, for the same reason: loopback may run
        // without a key, and anything reachable from another machine may not.
        let auth = AuthPolicy::build(
            &addresses,
            config
                .client_key
                .as_ref()
                .map(|key| key.expose().to_owned()),
            Vec::new(),
        )
        .map_err(|_| StartError::NeedsKey)?;

        lightweight_download::ensure_provider();
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            // A node that redirects is misconfigured, and following it would
            // carry the request — and its credential — somewhere the operator
            // did not name.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("lightweight-router/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| StartError::Client(err.to_string()))?;

        let topology = Arc::new(config.topology.clone());
        let health = Arc::new(HealthBook::new(&topology, config.health.failure_threshold));
        let selector = Selector::new(&topology, Arc::new(LoadBook::new(&topology)));
        Ok(Self {
            topology,
            health,
            policy: config.health,
            auth,
            client,
            metrics: RouterMetrics::default(),
            selector,
            started: SystemTime::now(),
        })
    }

    /// Probe every enabled node now, rather than at the next interval.
    pub async fn probe_now(&self) {
        health::probe_all(
            &self.client,
            &self.topology,
            &self.health,
            self.policy.timeout,
        )
        .await;
    }
}

/// A router with its listeners claimed and nothing served yet.
pub struct BoundRouter {
    state: Arc<RouterState>,
    listeners: Vec<tokio::net::TcpListener>,
}

impl BoundRouter {
    /// The addresses actually bound — with port 0, the ones the kernel chose.
    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.listeners
            .iter()
            .filter_map(|listener| listener.local_addr().ok())
            .collect()
    }

    pub fn state(&self) -> Arc<RouterState> {
        Arc::clone(&self.state)
    }

    /// Probe once, then serve until `stop` is cancelled.
    ///
    /// The first probe runs before the first request is accepted, so a node
    /// that is up is routable from the first request rather than only after an
    /// interval. A node that is down does not hold up the start: the probe has
    /// a timeout, a down node is simply recorded as such, and the router
    /// serves every route that has somewhere to go.
    pub async fn serve(self, stop: CancellationToken) -> Result<(), String> {
        let state = self.state;
        state.probe_now().await;
        for node in state.topology.nodes() {
            let status = state.health.status(&node.id);
            tracing::info!(
                target: targets::ROUTER,
                node = %node.id,
                enabled = node.enabled,
                health = status.health.as_str(),
                serving = status.served().map_or("", |served| served.id.as_str()),
                "initial node state"
            );
        }

        let monitor = health::spawn_monitor(
            state.client.clone(),
            Arc::clone(&state.topology),
            Arc::clone(&state.health),
            state.policy.interval,
            state.policy.timeout,
            stop.clone(),
        );

        let mut servers = Vec::with_capacity(self.listeners.len());
        for listener in self.listeners {
            let app = api::app(Arc::clone(&state));
            let stopping = stop.clone();
            servers.push(tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move { stopping.cancelled().await })
                    .await
            }));
        }
        let mut failure = None;
        for server in servers {
            match server.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => failure = Some(format!("the router stopped serving: {err}")),
                Err(err) => failure = Some(format!("a listener task failed: {err}")),
            }
        }
        stop.cancel();
        let _ = monitor.await;
        failure.map_or(Ok(()), Err)
    }
}

/// Build the state and claim every listener, refusing to start on any failure.
///
/// Every address is bound before any is served, so a router that cannot hold
/// all of its listeners does not come up half-reachable.
pub async fn bind(config: &RouterConfig) -> Result<BoundRouter, StartError> {
    let state = Arc::new(RouterState::new(config)?);
    let mut listeners = Vec::with_capacity(config.listen.len());
    for address in &config.listen {
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(|source| StartError::Bind {
                address: *address,
                source,
            })?;
        listeners.push(listener);
    }
    Ok(BoundRouter { state, listeners })
}
