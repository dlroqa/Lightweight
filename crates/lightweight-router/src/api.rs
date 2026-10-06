//! The router's HTTP surface.
//!
//! Two surfaces, kept apart as the gateway keeps its own apart:
//!
//! * **`/v1`** is the OpenAI-compatible surface a client like Lightagent talks
//!   to. It names routes and nothing else: no node, no address, no node-local
//!   model name, no file.
//! * **`/api/router/v1`** is the operator's read-only view of what is behind
//!   the routes — nodes, deployments, health, session affinity, `Auto`'s
//!   rules and recent routing traces. It shares `/v1`'s credential and never shows a node's
//!   key, a session id (only a keyed fingerprint) or any request content. Its
//!   few `POST`s act on the router's own bookkeeping — run a placement pass,
//!   check the classifier, reset adaptive scoring's route history — never on
//!   the configuration.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use lightweight_api::capabilities::{PROTOCOL_NAME, PROTOCOL_VERSION};
use lightweight_api::error::ErrorEnvelope;
use serde_json::{Value, json};

use crate::RouterState;
use crate::domain::{CapabilitySet, DeploymentHealth, NodeId, Route};
use crate::error::{json_error, unauthorized};
use crate::health::NodeStatus;
use crate::proxy::{self, Endpoint};
use crate::select::RouteSummary as RouteView;

/// The `owned_by` every route row carries.
pub const OWNED_BY: &str = "lightweight-router";

pub fn app(state: Arc<RouterState>) -> Router {
    app_with_panel(state, None)
}

/// The router's HTTP surface, optionally also serving the control panel's
/// built files from `web_root` (`hermes router --web-root`).
///
/// The panel is served for the same reason the gateway serves it: the page
/// and the API it calls then share an origin, so no cross-origin policy is
/// ever written. Every endpoint is matched first, the panel's files need no
/// credential (they carry none), and the API keeps its own: an unknown path
/// under `/api` or `/v1` still gets this router's JSON `not_found`, never the
/// panel's document. Without a web root nothing here changes.
pub fn app_with_panel(state: Arc<RouterState>, web_root: Option<PathBuf>) -> Router {
    let web_root = web_root.map(Arc::new);
    let fallback = move |uri: Uri| {
        let web_root = web_root.clone();
        async move {
            match web_root {
                Some(root) if !is_api_path(uri.path()) => {
                    lightweight_gateway::web::serve_root(&root, &uri).await
                }
                _ => not_found().await,
            }
        }
    };
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/metrics", get(metrics))
        .route("/v1/models", get(models))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        .route("/api/router/v1/nodes", get(nodes))
        .route("/api/router/v1/routes", get(routes))
        .route("/api/router/v1/deployments", get(deployments))
        .route("/api/router/v1/health", get(health_detail))
        .route("/api/router/v1/sessions", get(sessions))
        .route("/api/router/v1/traces", get(traces))
        .route("/api/router/v1/auto", get(auto_rules))
        .route("/api/router/v1/classifier/check", post(classifier_check))
        .route(
            "/api/router/v1/adaptive-scoring/reset",
            post(adaptive_scoring_reset),
        )
        .route("/api/router/v1/placement", get(placement))
        .route("/api/router/v1/placement/reconcile", post(reconcile))
        .fallback(fallback)
        .with_state(state)
}

/// A path that belongs to an API surface, where a missing endpoint must be a
/// JSON error and never the panel's document.
fn is_api_path(path: &str) -> bool {
    ["/api", "/v1"]
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
}

/// Check the client's credential against the router's own policy.
fn authorize(state: &RouterState, headers: &HeaderMap) -> Option<Response> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    state.auth.check(presented).err().map(unauthorized)
}

async fn chat_completions(
    State(state): State<Arc<RouterState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    proxy::forward(state, Endpoint::ChatCompletions, &headers, &body).await
}

async fn completions(
    State(state): State<Arc<RouterState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    proxy::forward(state, Endpoint::Completions, &headers, &body).await
}

/// The route as a client may be told about it: computed over the same
/// eligible set routing uses. See [`crate::select::summarize`].
fn view(state: &RouterState, route: &Route, health: &BTreeMap<NodeId, NodeStatus>) -> RouteView {
    crate::select::summarize(
        &state.topology,
        route,
        health,
        &state.health.deployment_snapshot(),
    )
}

/// `GET /v1/models`: every configured route, whether or not it is available
/// this second.
///
/// A route is the router's stable identity, so it is listed even while its
/// nodes are down: a client that configured `Coder` should be told `Coder` is
/// unavailable when it asks for it, not that `Coder` does not exist.
async fn models(State(state): State<Arc<RouterState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let health = state.health.snapshot();
    let created = unix(state.started);
    let data: Vec<Value> = state
        .topology
        .routes()
        .iter()
        .map(|route| {
            let mut row = json!({
                "id": route.name.as_str(),
                "object": "model",
                "created": created,
                "owned_by": OWNED_BY,
            });
            // The gateway's context spellings, one number under each, so a
            // client that sizes prompts from this list finds it where it looks.
            if let (Some(context), Some(object)) = (
                view(&state, route, &health).context_length,
                row.as_object_mut(),
            ) {
                for key in ["context_length", "n_ctx", "max_tokens", "max_output_tokens"] {
                    object.insert(key.into(), json!(context));
                }
            }
            row
        })
        .chain(enabled_auto(&state).map(|_| {
            // The router's own choice, not a model: no context, because that
            // belongs to whichever route a request resolves to.
            json!({
                "id": crate::auto_route::AUTO_ROUTE,
                "object": "model",
                "created": created,
                "owned_by": OWNED_BY,
            })
        }))
        .collect();
    axum::Json(json!({ "object": "list", "data": data })).into_response()
}

/// `Auto`, when it is configured and on.
fn enabled_auto(state: &RouterState) -> Option<&crate::auto_route::AutoRoute> {
    state.auto.as_ref().filter(|auto| auto.enabled)
}

/// `GET /v1/capabilities`: the gateway's contract, answered for routes.
///
/// The same protocol name and version, so a client that checks them proceeds;
/// the same top-level fields, so a client that reads them finds them. Where
/// the gateway describes its one model, the router describes its default
/// route, and every route is listed under `routes`.
///
/// The rule for features is conservative: a route claims a feature only when
/// **every deployment it could send a request to right now** supports it, and
/// the router as a whole only what every available route supports. A route
/// with nothing available claims nothing.
///
/// `Auto` is not a route and is not listed under `routes`: what it can serve
/// is whatever the route a request resolves to can. It is described under
/// `auto` by the routes it can resolve to, and claims nothing of its own.
async fn capabilities(State(state): State<Arc<RouterState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let health = state.health.snapshot();
    let views: Vec<(&Route, RouteView)> = state
        .topology
        .routes()
        .iter()
        .map(|route| (route, view(&state, route, &health)))
        .collect();

    let mut features: Option<CapabilitySet> = None;
    let mut max_concurrent: Option<u32> = None;
    for (_, view) in views.iter().filter(|(_, view)| view.available) {
        features = Some(match features {
            Some(so_far) => so_far.intersect(&view.capabilities),
            None => view.capabilities.clone(),
        });
        if let Some(limit) = view.max_concurrent_requests {
            max_concurrent = Some(max_concurrent.map_or(limit, |m| m.min(limit)));
        }
    }

    let default_model = state.topology.default_route().and_then(|route| {
        let (_, view) = views.iter().find(|(r, _)| r.name == route.name)?;
        Some(json!({
            "id": route.name.as_str(),
            "context_length": view.available.then_some(view.context_length).flatten()?,
        }))
    });

    let routes: Vec<Value> = views
        .iter()
        .map(|(route, view)| {
            json!({
                "id": route.name.as_str(),
                "available": view.available,
                "context_length": view.context_length,
                "features": view.capabilities,
            })
        })
        .collect();

    let mut state_body = json!({
        "model_loaded": views.iter().any(|(_, view)| view.available),
    });
    if let (Some(model), Some(object)) = (default_model, state_body.as_object_mut()) {
        object.insert("model".into(), model);
    }

    let mut body = json!({
        "object": "capability.list",
        "protocol": {
            "name": PROTOCOL_NAME,
            "version": PROTOCOL_VERSION,
            "compatible_versions": [PROTOCOL_VERSION],
        },
        "server": {
            "name": "Lightweight Router",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "endpoints": {
            "models": "/v1/models",
            "chat_completions": "/v1/chat/completions",
            "completions": "/v1/completions",
        },
        "features": features.unwrap_or_else(CapabilitySet::none),
        "state": state_body,
        "limits": {
            "max_concurrent_requests": max_concurrent.unwrap_or(0),
        },
        "routes": routes,
    });
    if let (Some(auto), Some(object)) = (enabled_auto(&state), body.as_object_mut()) {
        object.insert(
            "auto".into(),
            json!({
                "id": crate::auto_route::AUTO_ROUTE,
                "router_resolved": true,
                "routes": auto.targets().iter().map(|route| route.as_str()).collect::<Vec<_>>(),
            }),
        );
    }
    axum::Json(body).into_response()
}

/// `GET /health`: never refused, and says only whether routes are available.
async fn health(State(state): State<Arc<RouterState>>) -> Response {
    let health = state.health.snapshot();
    let total = state.topology.routes().len();
    let available = state
        .topology
        .routes()
        .iter()
        .filter(|route| view(&state, route, &health).available)
        .count();
    let status = match available {
        0 => "unavailable",
        n if n == total => "ok",
        _ => "degraded",
    };
    axum::Json(json!({
        "status": status,
        "routes_available": available,
        "routes": total,
    }))
    .into_response()
}

async fn version() -> Response {
    axum::Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "build": concat!("lightweight-router-", env!("CARGO_PKG_VERSION")),
    }))
    .into_response()
}

async fn metrics(State(state): State<Arc<RouterState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        {
            let mut text = state
                .metrics
                .to_prometheus(&state.health.snapshot(), &state.selector.load().snapshot());
            text.push_str(&crate::metrics::RouterMetrics::affinity_to_prometheus(
                &state.affinity,
            ));
            text.push_str(&crate::metrics::RouterMetrics::placement_to_prometheus(
                &state.topology,
                &state.health.snapshot(),
                &state.placement.loading(),
            ));
            text.push_str(&crate::metrics::RouterMetrics::route_history_to_prometheus(
                &state.route_history,
                std::time::Instant::now(),
            ));
            text
        },
    )
        .into_response()
}

/// `GET /api/router/v1/sessions`: session affinity at a glance.
///
/// Operator-only, behind the same key as the rest of `/api/router/v1`. Each
/// entry is named by its route and a keyed fingerprint of the session — never
/// the id the client sent, which the router does not keep.
async fn sessions(State(state): State<Arc<RouterState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let policy = state.affinity.policy();
    let (expired, capacity) = state.affinity.evictions();
    let entries = state.affinity.entries();
    axum::Json(json!({
        "enabled": policy.enabled,
        "header": policy.header,
        "idle_ttl_secs": policy.idle_ttl.as_secs(),
        "max_entries": policy.max_entries,
        "active": entries.len(),
        "evicted": {"expired": expired, "capacity": capacity},
        "data": entries,
    }))
    .into_response()
}

/// `GET /api/router/v1/placement`: each route's target against what is ready,
/// and where every deployment stands.
async fn placement(State(state): State<Arc<RouterState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let health = state.health.snapshot();
    let loading = state.placement.loading();
    let now = std::time::Instant::now();
    let policy = state.placement.policy();
    let routes: Vec<Value> = state
        .topology
        .routes()
        .iter()
        .filter_map(|route| crate::placement::assess(&state.topology, route, &health, &loading))
        .map(|assessment| {
            let deployments: Vec<Value> = assessment
                .deployments
                .iter()
                .map(|(id, node, deployment_state, allowed)| {
                    let record = state.placement.view(id, now);
                    json!({
                        "deployment": id,
                        "node": node,
                        "state": deployment_state.as_str(),
                        "allowed": allowed,
                        "loading_for_secs": record.loading_for.map(|d| d.as_secs()),
                        "last_result": record.last,
                        "consecutive_failures": record.failures,
                        "retry_in_secs": record.retry_in.map(|d| d.as_secs()),
                    })
                })
                .collect();
            json!({
                "route": assessment.route.as_str(),
                "min_ready": assessment.min_ready,
                "warm_standby": assessment.warm_standby,
                "target": assessment.target(),
                "ready": assessment.ready(),
                "ready_standby": assessment.standby(),
                "loading": assessment.loading(),
                "pending_loads": assessment.shortfall(),
                "status": assessment.status(),
                "deployments": deployments,
            })
        })
        .collect();
    axum::Json(json!({
        "enabled": crate::placement::configured(&state.topology),
        "interval_secs": policy.interval.as_secs(),
        "load_timeout_secs": policy.load_timeout.as_secs(),
        "last_pass": state.placement.last_pass().map(unix),
        "routes": routes,
    }))
    .into_response()
}

/// `POST /api/router/v1/placement/reconcile`: run a placement pass now rather
/// than at the next interval. It plans exactly what the interval would — it
/// cannot force a load, skip a backoff, or name a node.
async fn reconcile(State(state): State<Arc<RouterState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    if !crate::placement::configured(&state.topology) {
        return json_error(
            StatusCode::CONFLICT,
            &ErrorEnvelope::invalid_request(
                "no route has a placement target, so there is nothing to reconcile",
                "placement_not_configured",
            ),
        );
    }
    state.placement.wake.notify_one();
    (
        StatusCode::ACCEPTED,
        axum::Json(json!({"reconcile": "scheduled"})),
    )
        .into_response()
}

/// `GET /api/router/v1/auto`: `Auto`'s rules in the order they are tried,
/// each with the route it chooses and how often it has. Read-only: rules
/// change only with the configuration file.
async fn auto_rules(State(state): State<Arc<RouterState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(auto) = &state.auto else {
        return axum::Json(json!({"configured": false, "enabled": false})).into_response();
    };
    let rules: Vec<Value> = auto
        .rules
        .iter()
        .enumerate()
        .map(|(index, rule)| {
            json!({
                "position": index + 1,
                "name": rule.name,
                "when": rule.when,
                "condition": rule.when.summary(),
                "route": rule.route.as_str(),
                "classify": rule.classify,
                "decisions": state.metrics.auto_decisions(&rule.name),
            })
        })
        .collect();
    axum::Json(json!({
        "configured": true,
        "enabled": auto.enabled,
        "name": crate::auto_route::AUTO_ROUTE,
        "fallback_route": auto.fallback.as_str(),
        "fallback_decisions": state.metrics.auto_fallbacks(),
        "rules": rules,
        // The classifier's settings and state. Never a key: a provider that
        // has one says only whether it is configured.
        "classifier": auto.classifier.as_ref().map(|classifier| {
            let provider = classifier.provider.kind().as_str();
            let limits = classifier.limits();
            let mut view = json!({
                "provider": provider,
                "route": classifier.provider.route().map(|route| route.as_str()),
                "model": classifier.provider.model(),
                "candidates": classifier.candidates,
                "fallback_route": classifier.fallback.as_str(),
                "min_confidence": limits.min_confidence,
                "timeout_ms": u64::try_from(limits.timeout.as_millis()).unwrap_or(u64::MAX),
                "max_input_chars": limits.max_input_chars,
                "invoked_by": auto.rules.iter().filter(|rule| rule.classify)
                    .map(|rule| rule.name.as_str()).collect::<Vec<_>>(),
                "outcomes": state.metrics.classifier_outcomes(provider),
                "status": state.classifier_status.view(),
            });
            if let Some(object) = view.as_object_mut() {
                for configured in std::iter::once(&classifier.provider)
                    .chain(classifier.standby.as_ref())
                {
                    let mut block = configured.view();
                    if let Some(block) = block.as_object_mut() {
                        block.insert(
                            "active".into(),
                            json!(configured.kind() == classifier.provider.kind()),
                        );
                    }
                    object.insert(configured.kind().as_str().into(), block);
                }
            }
            view
        }),
        // Adaptive route scoring (R9.2): its settings, and each route's
        // history as numbers. Never a request, session, deployment or node.
        "adaptive_scoring": adaptive_scoring_view(&state, auto),
    }))
    .into_response()
}

fn adaptive_scoring_view(state: &RouterState, auto: &crate::auto_route::AutoRoute) -> Value {
    let Some(scoring) = &auto.scoring else {
        return json!({"configured": false, "enabled": false});
    };
    let mut view = scoring.view();
    if let Some(object) = view.as_object_mut() {
        object.insert("configured".into(), json!(true));
        object.insert(
            "classifier_baseline".into(),
            json!(
                auto.classifier
                    .as_ref()
                    .map(crate::scoring::classifier_baseline)
            ),
        );
        let fallbacks: BTreeMap<&str, u64> = ["below_threshold", "no_verdict", "internal_error"]
            .into_iter()
            .map(|reason| (reason, state.metrics.scoring_fallbacks(reason)))
            .collect();
        object.insert("fallbacks".into(), json!(fallbacks));
        object.insert(
            "routes".into(),
            json!(state.route_history.view(std::time::Instant::now())),
        );
    }
    view
}

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetRequest {
    /// One logical route; absent resets every route.
    route: Option<String>,
}

/// `POST /api/router/v1/adaptive-scoring/reset`: forget adaptive scoring's
/// route history — every route's, or with `{"route": "Coder"}` one route's.
///
/// Operator-only, behind the router's key like every other mutation here. It
/// resets the history aggregates and nothing else: no route, rule, classifier
/// setting, session affinity, placement, node or loaded model is touched.
/// Prometheus counters stay monotonic; only the history scoring reads starts
/// over.
async fn adaptive_scoring_reset(
    State(state): State<Arc<RouterState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    if !state.route_history.is_recording() {
        return json_error(
            StatusCode::CONFLICT,
            &ErrorEnvelope::invalid_request(
                "adaptive scoring is not configured and on, so there is no history to reset",
                "adaptive_scoring_not_enabled",
            ),
        );
    }
    let request = if body.iter().all(u8::is_ascii_whitespace) {
        ResetRequest::default()
    } else {
        match serde_json::from_slice::<ResetRequest>(&body) {
            Ok(request) => request,
            Err(_) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    &ErrorEnvelope::invalid_request(
                        "the body must be empty, {}, or {\"route\": \"<a configured route>\"}",
                        "invalid_request_body",
                    ),
                );
            }
        }
    };
    let route = match request.route.as_deref() {
        None => None,
        Some(requested) => match state.route_history.route_named(requested) {
            Some(route) => Some(route.clone()),
            None => {
                return json_error(
                    StatusCode::NOT_FOUND,
                    &ErrorEnvelope::invalid_request(
                        "that is not one of the configured routes",
                        "route_not_found",
                    )
                    .with_param("route"),
                );
            }
        },
    };
    let reset = state.route_history.reset(route.as_ref());
    tracing::info!(
        target: lightweight_observability::targets::ROUTER,
        scope = if route.is_some() { "route" } else { "all" },
        routes = reset.iter().map(ToString::to_string).collect::<Vec<_>>().join(","),
        "adaptive scoring history reset"
    );
    axum::Json(json!({
        "reset": if route.is_some() { "route" } else { "all" },
        "routes": reset,
        "reset_at": crate::classifier::unix_now(),
    }))
    .into_response()
}

/// `POST /api/router/v1/classifier/check`: check the active classifier
/// provider now, record the result for `GET /api/router/v1/auto`, and return
/// it. For Jev: `GET /v1/models` — is the key accepted, is the model listed —
/// bounded by its timeout, never a classification and never a key or a
/// provider's error body in the answer. For the Lightweight provider: whether
/// its classifier route has a deployment available. It changes nothing.
async fn classifier_check(State(state): State<Arc<RouterState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(classifier) = state
        .auto
        .as_ref()
        .and_then(|auto| auto.classifier.as_ref())
    else {
        return json_error(
            StatusCode::CONFLICT,
            &ErrorEnvelope::invalid_request(
                "no classifier is configured, so there is nothing to check",
                "classifier_not_configured",
            ),
        );
    };
    let report = match &classifier.provider {
        crate::classifier::ClassifierProvider::Jev(jev) => {
            crate::classifier::jev::check(&state.client, jev).await
        }
        crate::classifier::ClassifierProvider::Lightweight(lightweight) => {
            let started = std::time::Instant::now();
            let health = state.health.snapshot();
            let available = state
                .topology
                .route(&lightweight.route)
                .is_some_and(|route| view(&state, route, &health).available);
            crate::classifier::CheckReport {
                provider: "lightweight",
                status: if available { "ok" } else { "route_unavailable" },
                model: None,
                model_listed: None,
                http_status: None,
                checked_at: crate::classifier::unix_now(),
                duration_ms: started.elapsed().as_secs_f64() * 1000.0,
            }
        }
    };
    state.classifier_status.record_check(report.clone());
    axum::Json(report).into_response()
}

#[derive(serde::Deserialize)]
struct TraceQuery {
    limit: Option<usize>,
}

/// `GET /api/router/v1/traces?limit=N`: the most recent routing traces,
/// newest first. Memory only and bounded by `traces.capacity`; no prompt,
/// message, credential or session id is ever in one.
async fn traces(
    State(state): State<Arc<RouterState>>,
    headers: HeaderMap,
    Query(query): Query<TraceQuery>,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let limit = query.limit.unwrap_or(50).min(state.traces.capacity());
    axum::Json(json!({
        "object": "list",
        "capacity": state.traces.capacity(),
        "data": state.traces.recent(limit),
    }))
    .into_response()
}

fn node_json(node: &crate::domain::Node, status: &NodeStatus) -> Value {
    json!({
        "id": node.id,
        "url": node.base_url.as_str(),
        "enabled": node.enabled,
        "auth": node.auth.kind(),
        "health": status.health,
        "consecutive_failures": status.consecutive_failures,
        "last_checked": status.last_checked.map(unix),
        "last_seen": status.last_seen.map(unix),
        "last_error": status.last_error,
        "serving": status.served(),
        "version": status.observed.as_ref().map(|observed| &observed.version),
    })
}

/// `GET /api/router/v1/nodes`.
async fn nodes(State(state): State<Arc<RouterState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let health = state.health.snapshot();
    let data: Vec<Value> = state
        .topology
        .nodes()
        .iter()
        .map(|node| node_json(node, health.get(&node.id).unwrap_or(&NodeStatus::default())))
        .collect();
    axum::Json(json!({ "object": "list", "data": data })).into_response()
}

fn deployment_availability(
    state: &RouterState,
    deployment: &crate::domain::Deployment,
    health: &BTreeMap<NodeId, NodeStatus>,
) -> (bool, Option<&'static str>) {
    let Some(node) = state.topology.node(&deployment.node) else {
        return (false, None);
    };
    match crate::health::availability(
        node,
        deployment,
        health.get(&node.id).unwrap_or(&NodeStatus::default()),
    ) {
        DeploymentHealth::Available => (true, None),
        DeploymentHealth::Unavailable(reason) => (false, Some(reason.as_str())),
    }
}

/// `GET /api/router/v1/routes`.
async fn routes(State(state): State<Arc<RouterState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let health = state.health.snapshot();
    let data: Vec<Value> = state
        .topology
        .routes()
        .iter()
        .map(|route| {
            let deployments: Vec<Value> = route
                .deployments
                .iter()
                .enumerate()
                .filter_map(|(position, id)| {
                    let deployment = state.topology.deployment(id)?;
                    let (available, reason) = deployment_availability(&state, deployment, &health);
                    Some(json!({
                        "id": deployment.id,
                        "node": deployment.node,
                        "model": deployment.remote_model,
                        "priority": position + 1,
                        "available": available,
                        "unavailable_reason": reason,
                    }))
                })
                .collect();
            json!({
                "name": route.name.as_str(),
                // What a classifier is told the route is for; `null` when the
                // file gives none.
                "description": route.description,
                "strategy": route.policy.as_str(),
                "available": view(&state, route, &health).available,
                "deployments": deployments,
            })
        })
        .collect();
    axum::Json(json!({
        "object": "list",
        "default_route": state.topology.default_route().map(|route| route.name.as_str()),
        "data": data,
    }))
    .into_response()
}

/// `GET /api/router/v1/deployments`.
async fn deployments(State(state): State<Arc<RouterState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let health = state.health.snapshot();
    let data: Vec<Value> = state
        .topology
        .deployments()
        .iter()
        .map(|deployment| {
            let (available, reason) = deployment_availability(&state, deployment, &health);
            // This deployment's own last-observed figures, never a route's
            // combined ones.
            let observed = state.health.deployment(&deployment.id);
            json!({
                "id": deployment.id,
                "node": deployment.node,
                "model": deployment.remote_model,
                "routes": state.topology.routes_using(&deployment.id),
                "available": available,
                "unavailable_reason": reason,
                "observed": observed,
                // What least-busy reads: the router's own in-flight count, and
                // the slot count this deployment's node last advertised.
                "active_requests": state.selector.load().active(&deployment.id),
                "concurrency_limit": observed.as_ref().map(|seen| seen.max_concurrent_requests),
                "observed_at": observed.as_ref().map(|seen| unix(seen.observed_at)),
            })
        })
        .collect();
    axum::Json(json!({ "object": "list", "data": data })).into_response()
}

/// `GET /api/router/v1/health`: the whole picture in one read.
async fn health_detail(State(state): State<Arc<RouterState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let health = state.health.snapshot();
    let nodes: Vec<Value> = state
        .topology
        .nodes()
        .iter()
        .map(|node| {
            let status = health.get(&node.id).cloned().unwrap_or_default();
            json!({
                "id": node.id,
                "enabled": node.enabled,
                "health": status.health,
                "last_error": status.last_error,
            })
        })
        .collect();
    let routes: Vec<Value> = state
        .topology
        .routes()
        .iter()
        .map(|route| {
            json!({
                "name": route.name.as_str(),
                "available": view(&state, route, &health).available,
            })
        })
        .collect();
    axum::Json(json!({
        "probe_interval_secs": state.policy.interval.as_secs(),
        "failure_threshold": state.policy.failure_threshold,
        "active_requests": state.metrics.active(),
        "nodes": nodes,
        "routes": routes,
    }))
    .into_response()
}

async fn not_found() -> Response {
    json_error(
        StatusCode::NOT_FOUND,
        &ErrorEnvelope::invalid_request("this router has no such endpoint", "not_found"),
    )
}

fn unix(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}
