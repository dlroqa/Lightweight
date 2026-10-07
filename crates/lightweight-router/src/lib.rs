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
//! then the request's requirements, then — when the request names a session —
//! that session's last deployment is preferred if it survived both, and
//! otherwise the route's policy — priority, round-robin or least-busy — orders
//! them. Latency, TTFT and estimator accuracy are measured and never consulted.
//! See `docs/ROUTER.md` for the roadmap beyond it.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod affinity;
pub mod api;
pub mod auto_route;
pub mod budget;
pub mod capability;
pub mod classifier;
pub mod config;
pub mod controller;
pub mod domain;
pub mod error;
pub mod fallback;
pub mod health;
pub mod load;
pub mod metrics;
pub mod placement;
pub mod proxy;
pub mod requirements;
pub mod scoring;
pub mod select;
pub mod sse;
pub mod trace;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;

use lightweight_gateway::AuthPolicy;
use lightweight_observability::targets;
use tokio_util::sync::CancellationToken;

pub use config::{RouterConfig, load, validate};
pub use domain::Topology;

use crate::affinity::AffinityBook;
use crate::config::HealthPolicy;
use crate::health::HealthBook;
use crate::load::LoadBook;
use crate::metrics::RouterMetrics;
use crate::select::Selector;
use crate::trace::TraceBook;

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
    /// Each client request's pre-commit budget (R9.3.2), when configured.
    /// Only the duration: the deadline is the request's own, made when it
    /// arrives, and never kept here.
    pub request_budget: Option<std::time::Duration>,
    /// Per-route policy state and the per-deployment in-flight counts.
    pub selector: Selector,
    /// The `auto_route` section, if the file has one. Only ever chooses a
    /// route; everything after that is the route's own.
    pub auto: Option<crate::auto_route::AutoRoute>,
    /// Each route's success history, for adaptive route scoring (R9.2).
    /// Records nothing unless scoring is configured and on.
    pub route_history: crate::scoring::HistoryBook,
    /// When the classifier last answered, last failed, and was last checked.
    /// Read by the admin view only.
    pub classifier_status: crate::classifier::ClassifierStatus,
    /// Which deployment each live session prefers. Empty, and never written,
    /// while affinity is off.
    pub affinity: AffinityBook,
    /// The most recent routing traces.
    pub traces: TraceBook,
    /// Test-only pauses around the planning window. Always zero in a router
    /// started from a configuration file.
    #[doc(hidden)]
    pub phase_delays: crate::proxy::PhaseDelays,
    /// Placement's loads in progress, last results and backoff. Read by the
    /// placement controller and the admin view, never by a request.
    pub placement: crate::placement::PlacementBook,
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
            request_budget: config.pre_commit_budget,
            selector,
            auto: config.auto.clone(),
            route_history: crate::scoring::HistoryBook::new(
                config
                    .topology
                    .routes()
                    .iter()
                    .map(|route| route.name.clone())
                    .collect(),
                config
                    .auto
                    .as_ref()
                    .and_then(|auto| auto.scoring.as_ref())
                    .filter(|scoring| scoring.enabled)
                    .map(|scoring| scoring.history),
            ),
            classifier_status: crate::classifier::ClassifierStatus::default(),
            affinity: AffinityBook::new(config.affinity.clone()),
            traces: TraceBook::new(config.trace_capacity),
            phase_delays: crate::proxy::PhaseDelays::default(),
            placement: crate::placement::PlacementBook::new(config.placement),
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
    web_root: Option<std::path::PathBuf>,
}

impl BoundRouter {
    /// Also serve the control panel's built files at `/` (see
    /// [`api::app_with_panel`]). `None` serves no panel, as before.
    #[must_use]
    pub fn with_web_root(mut self, web_root: Option<std::path::PathBuf>) -> Self {
        self.web_root = web_root;
        self
    }

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

        // Expired affinities are also dropped when looked up and when the book
        // is full; this only stops sessions that never return from sitting in
        // memory until then.
        let sweeper = state.affinity.enabled().then(|| {
            let state = Arc::clone(&state);
            let stopping = stop.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(state.affinity.sweep_interval());
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        () = stopping.cancelled() => break,
                        _ = tick.tick() => {
                            state.affinity.sweep();
                        }
                    }
                }
            })
        });
        if state.affinity.enabled() {
            let policy = state.affinity.policy();
            tracing::info!(
                target: targets::ROUTER,
                header = policy.header.as_str(),
                idle_ttl_secs = policy.idle_ttl.as_secs(),
                max_entries = policy.max_entries,
                "session affinity is on"
            );
        }

        // An external classifier is checked once at start — key accepted, model
        // listed — in the background: a provider that is down must not stop
        // the router starting, and Auto falls back without it anyway.
        if let Some(jev) = state
            .auto
            .as_ref()
            .and_then(|auto| auto.classifier.as_ref())
            .and_then(|classifier| match &classifier.provider {
                crate::classifier::ClassifierProvider::Jev(jev) => Some(jev.clone()),
                crate::classifier::ClassifierProvider::Lightweight(_) => None,
            })
        {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let report = crate::classifier::jev::check(&state.client, &jev).await;
                tracing::info!(
                    target: targets::ROUTER,
                    provider = "jev",
                    model = report.model.as_deref(),
                    status = report.status,
                    model_listed = report.model_listed,
                    http_status = report.http_status,
                    "classifier provider checked"
                );
                state.classifier_status.record_check(report);
            });
        }

        // Off unless a route has a placement target. It shares the health book
        // with the monitor and the client with requests, and nothing else.
        let placer = controller::spawn(Arc::clone(&state), stop.clone());

        let mut servers = Vec::with_capacity(self.listeners.len());
        for listener in self.listeners {
            let app = api::app_with_panel(Arc::clone(&state), self.web_root.clone());
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
        if let Some(sweeper) = sweeper {
            let _ = sweeper.await;
        }
        if let Some(placer) = placer {
            let _ = placer.await;
        }
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
    Ok(BoundRouter {
        state,
        listeners,
        web_root: None,
    })
}
