//! The router's HTTP surface.
//!
//! Two surfaces, kept apart as the gateway keeps its own apart:
//!
//! * **`/v1`** is the OpenAI-compatible surface a client like Lightagent talks
//!   to. It names routes and nothing else: no node, no address, no node-local
//!   model name, no file.
//! * **`/api/router/v1`** is the operator's read-only view of what is behind
//!   the routes — nodes, deployments, health. It shares `/v1`'s credential and
//!   never shows a node's key.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
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

/// The `owned_by` every route row carries.
pub const OWNED_BY: &str = "lightweight-router";

pub fn app(state: Arc<RouterState>) -> Router {
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
        .fallback(not_found)
        .with_state(state)
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

/// What the router can say about one route right now.
struct RouteView {
    available: bool,
    /// The smallest context any of the route's deployments was last seen
    /// serving. Smallest, because the router may send a request to any of
    /// them, and a prompt sized for the largest would overflow the others.
    context_length: Option<u32>,
    /// What every *available* deployment supports.
    features: CapabilitySet,
    max_concurrent_requests: Option<u32>,
}

fn view(state: &RouterState, route: &Route, health: &BTreeMap<NodeId, NodeStatus>) -> RouteView {
    let unknown = NodeStatus::default();
    let mut available = false;
    let mut context_length: Option<u32> = None;
    let mut features: Option<CapabilitySet> = None;
    let mut max_concurrent: Option<u32> = None;

    for id in &route.deployments {
        let Some(deployment) = state.topology.deployment(id) else {
            continue;
        };
        let Some(node) = state.topology.node(&deployment.node) else {
            continue;
        };
        let status = health.get(&node.id).unwrap_or(&unknown);
        if let Some(served) = status.served().filter(|served| {
            lightweight_catalog::alias::same_name(&served.id, &deployment.remote_model)
        }) {
            context_length = Some(
                context_length.map_or(served.context_length, |c| c.min(served.context_length)),
            );
        }
        if crate::health::availability(node, deployment, status) == DeploymentHealth::Available {
            available = true;
            if let Some(observed) = &status.observed {
                features = Some(match features {
                    Some(so_far) => so_far.intersect(&observed.features),
                    None => observed.features.clone(),
                });
                max_concurrent = Some(
                    max_concurrent.map_or(observed.max_concurrent_requests, |m| {
                        m.min(observed.max_concurrent_requests)
                    }),
                );
            }
        }
    }

    RouteView {
        available,
        context_length,
        features: features.unwrap_or_else(CapabilitySet::none),
        max_concurrent_requests: max_concurrent,
    }
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
        .collect();
    axum::Json(json!({ "object": "list", "data": data })).into_response()
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
            Some(so_far) => so_far.intersect(&view.features),
            None => view.features.clone(),
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
                "features": view.features,
            })
        })
        .collect();

    let mut state_body = json!({
        "model_loaded": views.iter().any(|(_, view)| view.available),
    });
    if let (Some(model), Some(object)) = (default_model, state_body.as_object_mut()) {
        object.insert("model".into(), model);
    }

    axum::Json(json!({
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
    }))
    .into_response()
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
        state.metrics.to_prometheus(&state.health.snapshot()),
    )
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
            json!({
                "id": deployment.id,
                "node": deployment.node,
                "model": deployment.remote_model,
                "routes": state.topology.routes_using(&deployment.id),
                "available": available,
                "unavailable_reason": reason,
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
