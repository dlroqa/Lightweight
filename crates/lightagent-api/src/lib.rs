//! The Lightagent HTTP API.
//!
//! A versioned surface (`/api/lightagent/v1`) over the one runtime: it starts and
//! observes agent runs, streams their canonical events as SSE, lists tools, and
//! reads and deletes saved sessions — all behind scoped bearer authentication.
//! It owns no agent logic; a [`RunManager`] drives the core loop and the handlers
//! only observe it.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod auth;
pub mod manager;
pub mod sse;

use std::collections::VecDeque;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use lightagent_core::AgentEvent;
use lightagent_store::{SessionId, SessionStore};
use lightagent_tools::ToolRegistry;
use serde::Deserialize;
use serde_json::json;

pub use auth::{AuthConfig, Scope};
pub use manager::{RunFactory, RunManager, RunState, RunStatus, StartRun};

/// The shared state every handler reads.
#[derive(Clone)]
pub struct AppState {
    /// Drives and tracks runs.
    pub manager: RunManager,
    /// The authentication policy.
    pub auth: AuthConfig,
    /// The session store for the active profile.
    pub sessions: SessionStore,
    /// When set, the panel is served from this directory (same-origin), so the
    /// WebUI and the API it calls share one origin and need no CORS.
    pub web_root: Option<PathBuf>,
}

/// Build the API router over `state`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/lightagent/v1/tools", get(list_tools))
        .route("/api/lightagent/v1/runs", post(create_run))
        .route("/api/lightagent/v1/runs/{id}", get(get_run))
        .route("/api/lightagent/v1/runs/{id}/events", get(run_events))
        .route("/api/lightagent/v1/runs/{id}/cancel", post(cancel_run))
        .route("/api/lightagent/v1/sessions", get(list_sessions))
        .route(
            "/api/lightagent/v1/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/api/lightagent/v1/approvals", get(list_approvals))
        .route("/api/lightagent/v1/approvals/{run}", post(respond_approval))
        .fallback(serve_static)
        .with_state(Arc::new(state))
}

/// Serve the WebUI from `web_root`, if configured.
///
/// Every API route is matched before this fallback. Path resolution is a
/// whitelist — each component must be an ordinary name, so `..` is refused
/// rather than resolved — and a path with no file extension that does not exist
/// is answered with `index.html`, so a client-side route deep-links, while a
/// missing asset is a 404 rather than the document.
async fn serve_static(State(state): State<Arc<AppState>>, uri: Uri) -> Response {
    let Some(root) = &state.web_root else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let relative = uri.path().trim_start_matches('/');
    let mut full = root.clone();
    let mut looks_like_asset = false;
    if relative.is_empty() {
        full.push("index.html");
    } else {
        for component in relative.split('/') {
            let ordinary = !component.is_empty()
                && component != "."
                && component != ".."
                && component
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
            if !ordinary {
                return (StatusCode::NOT_FOUND, "not found").into_response();
            }
            full.push(component);
        }
        looks_like_asset = std::path::Path::new(relative).extension().is_some();
    }

    match tokio::fs::read(&full).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, content_type(&full))], bytes).into_response(),
        Err(_) if looks_like_asset => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(_) => match tokio::fs::read(root.join("index.html")).await {
            Ok(bytes) => {
                ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], bytes).into_response()
            }
            Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
        },
    }
}

fn content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") | Some("map") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// Reject a request that failed authorization, else `None`.
fn deny(state: &AppState, headers: &HeaderMap, scope: Scope) -> Option<Response> {
    match state.auth.authorize(headers, scope) {
        Ok(()) => None,
        Err((status, message)) => Some((status, Json(json!({ "error": message }))).into_response()),
    }
}

async fn health() -> Response {
    Json(json!({ "status": "ok", "service": "lightagent" })).into_response()
}

async fn list_tools(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(rejection) = deny(&state, &headers, Scope::ToolsRead) {
        return rejection;
    }
    let registry = ToolRegistry::builtin();
    let tools: Vec<_> = registry
        .names()
        .into_iter()
        .filter_map(|name| registry.get(&name).map(|tool| tool.definition().clone()))
        .map(|definition| {
            json!({
                "name": definition.name,
                "risk": definition.risk.as_str(),
                "description": definition.description,
            })
        })
        .collect();
    Json(json!({ "tools": tools })).into_response()
}

#[derive(Deserialize)]
struct CreateRunBody {
    message: String,
    #[serde(default)]
    profile: Option<String>,
}

async fn create_run(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CreateRunBody>,
) -> Response {
    if let Some(rejection) = deny(&state, &headers, Scope::RunsWrite) {
        return rejection;
    }
    let run = state
        .manager
        .start(StartRun {
            message: body.message,
            profile: body.profile,
            cwd: None,
        })
        .await;
    (
        StatusCode::ACCEPTED,
        Json(json!({ "id": run.id(), "status": run.status().await })),
    )
        .into_response()
}

async fn get_run(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(rejection) = deny(&state, &headers, Scope::RunsRead) {
        return rejection;
    }
    match state.manager.get(&id).await {
        Some(run) => Json(json!({
            "id": run.id(),
            "status": run.status().await,
            "events": run.events().await.len(),
            "pending_approval": run.pending().await,
        }))
        .into_response(),
        None => not_found(&id),
    }
}

async fn run_events(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(rejection) = deny(&state, &headers, Scope::RunsRead) {
        return rejection;
    }
    match state.manager.get(&id).await {
        Some(run) => Sse::new(event_stream(run))
            .keep_alive(KeepAlive::default())
            .into_response(),
        None => not_found(&id),
    }
}

async fn cancel_run(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(rejection) = deny(&state, &headers, Scope::RunsWrite) {
        return rejection;
    }
    match state.manager.get(&id).await {
        Some(run) => {
            run.cancel();
            Json(json!({ "id": run.id(), "cancelled": true })).into_response()
        }
        None => not_found(&id),
    }
}

async fn list_sessions(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(rejection) = deny(&state, &headers, Scope::SessionsRead) {
        return rejection;
    }
    match state.sessions.list() {
        Ok(list) => Json(json!({ "sessions": list })).into_response(),
        Err(error) => internal(&error.to_string()),
    }
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(rejection) = deny(&state, &headers, Scope::SessionsRead) {
        return rejection;
    }
    let id = match SessionId::parse(&id) {
        Ok(id) => id,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    };
    match state.sessions.load(&id) {
        Ok(session) => Json(session).into_response(),
        Err(error) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

async fn delete_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(rejection) = deny(&state, &headers, Scope::SessionsWrite) {
        return rejection;
    }
    let id = match SessionId::parse(&id) {
        Ok(id) => id,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    };
    match state.sessions.delete(&id) {
        Ok(removed) => Json(json!({ "deleted": removed })).into_response(),
        Err(error) => internal(&error.to_string()),
    }
}

async fn list_approvals(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(rejection) = deny(&state, &headers, Scope::ApprovalsWrite) {
        return rejection;
    }
    let mut waiting = Vec::new();
    for run in state.manager.awaiting_approval().await {
        waiting.push(json!({ "run": run.id(), "pending": run.pending().await }));
    }
    Json(json!({ "approvals": waiting })).into_response()
}

#[derive(Deserialize)]
struct RespondBody {
    #[serde(default)]
    approve: bool,
    #[serde(default)]
    remember_secs: Option<u64>,
}

async fn respond_approval(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(run): Path<String>,
    Json(body): Json<RespondBody>,
) -> Response {
    if let Some(rejection) = deny(&state, &headers, Scope::ApprovalsWrite) {
        return rejection;
    }
    use lightagent_core::{ApprovalDecision, ApprovalId};
    let Some(state_run) = state.manager.get(&run).await else {
        return not_found(&run);
    };
    let decision = if body.approve {
        match body.remember_secs {
            Some(secs) => {
                ApprovalDecision::grant_for(ApprovalId::new(), std::time::Duration::from_secs(secs))
            }
            None => ApprovalDecision::grant(ApprovalId::new()),
        }
    } else {
        ApprovalDecision::deny(ApprovalId::new())
    };
    let delivered = state_run.decide(decision);
    Json(json!({ "run": state_run.id(), "delivered": delivered })).into_response()
}

fn not_found(id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": format!("no run '{id}'") })),
    )
        .into_response()
}

fn internal(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": message })),
    )
        .into_response()
}

/// A run's events as an SSE stream: buffered history first, then the live tail,
/// ending when the run reaches a terminal state.
fn event_stream(run: Arc<RunState>) -> impl Stream<Item = Result<Event, Infallible>> {
    struct Cursor {
        run: Arc<RunState>,
        seen: usize,
        queue: VecDeque<AgentEvent>,
        finished: bool,
    }
    stream::unfold(
        Cursor {
            run,
            seen: 0,
            queue: VecDeque::new(),
            finished: false,
        },
        |mut cursor| async move {
            loop {
                if let Some(event) = cursor.queue.pop_front() {
                    return Some((Ok(sse::to_sse(&event)), cursor));
                }
                if cursor.finished {
                    return None;
                }
                let (new_events, status) = cursor.run.wait_from(cursor.seen).await;
                cursor.seen += new_events.len();
                cursor.queue.extend(new_events);
                if status.is_terminal() {
                    cursor.finished = true;
                }
                if cursor.queue.is_empty() && cursor.finished {
                    return None;
                }
            }
        },
    )
}
