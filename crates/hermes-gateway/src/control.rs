//! The control API: `/api/v1`.
//!
//! Deliberately not under `/v1`. That prefix is the OpenAI surface, and a
//! client walking it must find only what OpenAI defines there — which is why
//! `/api/v1/metrics` has lived under its own prefix since M5, before there was
//! anything else to keep it company.
//!
//! `/v1/models` still answers from the *resident* model alone. A client can
//! only use what is loaded, and offering it a list of things it would have to
//! ask us to load first would make every id in that list a trap.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use hermes_api::error::ErrorEnvelope;
use hermes_catalog::install::AddModel;
use hermes_catalog::{InstalledModel, manifest};
use hermes_core::{Actionable, GgmlType};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast;

use crate::jobs::{Job, JobKind, JobState};
use crate::manager::{self, LoadOptions};
use crate::routes::authorize;
use crate::state::GatewayState;

/// One row of `GET /api/v1/models`.
///
/// The catalog record plus the two things only a running gateway knows: whether
/// this model is the one loaded, and whether its file is still there.
#[derive(Debug, Serialize)]
pub struct CatalogRow {
    #[serde(flatten)]
    model: InstalledModel,
    /// `loaded`, `available`, or `missing`.
    state: &'static str,
    /// Whether the digest was checked against something, or merely recorded.
    verified: bool,
    integrity_label: &'static str,
}

/// `GET /api/v1/models` — everything installed.
pub async fn models(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(manager) = state.manager() else {
        return no_manager();
    };

    let resident = state.catalog.resident().await;
    let rows: Vec<CatalogRow> = manager
        .models()
        .await
        .into_iter()
        .map(|model| {
            let loaded = resident
                .as_ref()
                .is_some_and(|current| current.id.slug() == model.id);
            CatalogRow {
                state: if loaded {
                    "loaded"
                } else if model.is_present() {
                    "available"
                } else {
                    // Kept, not forgotten: the drive may not be mounted.
                    "missing"
                },
                verified: model.integrity.verified(),
                integrity_label: model.integrity.label(),
                model,
            }
        })
        .collect();

    axum::Json(json!({ "object": "list", "data": rows })).into_response()
}

/// `GET /api/v1/catalog` — the pinned models, and what is already installed.
pub async fn pinned(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let installed: Vec<String> = match state.manager() {
        Some(manager) => manager.models().await.into_iter().map(|m| m.id).collect(),
        None => Vec::new(),
    };

    let rows: Vec<serde_json::Value> = manifest::MODELS
        .iter()
        .map(|model| {
            json!({
                "id": model.id,
                "name": model.name,
                "repo": model.repo,
                "file": model.file,
                "url": model.url(),
                "sha256": model.sha256,
                "size": model.size,
                "parameters": model.parameters,
                "quantization": model.quantization,
                "summary": model.summary,
                "installed": installed.iter().any(|id| id == model.id),
            })
        })
        .collect();

    axum::Json(json!({
        "object": "list",
        "data": rows,
        // Said in the payload rather than only in the docs: a UI that offers
        // only this list would be hiding the more useful half of the feature.
        "note": "Any direct https link to a .gguf can be added as well.",
    }))
    .into_response()
}

/// Body of `POST /api/v1/models/download`.
#[derive(Debug, Deserialize)]
pub struct DownloadBody {
    /// One of the pinned ids.
    #[serde(default)]
    pub id: Option<String>,
    /// A direct https link.
    #[serde(default)]
    pub url: Option<String>,
    /// Expected digest, for a link whose host publishes none.
    #[serde(default)]
    pub sha256: Option<String>,
}

/// `POST /api/v1/models/download` — start fetching, and return a job.
pub async fn download(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(manager) = state.manager() else {
        return no_manager();
    };
    let body: DownloadBody = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(err) => return bad_request("body", &err.to_string()),
    };

    let request = match (body.id, body.url) {
        (Some(id), None) => AddModel::Pinned { id },
        (None, Some(url)) => AddModel::Link {
            url,
            sha256: body.sha256,
        },
        (Some(_), Some(_)) => {
            return bad_request("id", "give either a pinned id or a url, not both");
        }
        (None, None) => return bad_request("id", "name a pinned id or give a url"),
    };

    let job = state
        .jobs()
        .start(JobKind::Download, &state.shutdown_token());
    let background = Arc::clone(&job);
    let manager = Arc::clone(manager);
    // Spawned rather than awaited: this is minutes of work, and the caller gets
    // a job id now and watches it.
    tokio::spawn(async move {
        finish(&background, manager.install(&request, &background).await);
    });
    accepted(&job)
}

/// Body of `POST /api/v1/models/import`.
#[derive(Debug, Deserialize)]
pub struct ImportBody {
    pub path: PathBuf,
}

/// `POST /api/v1/models/import` — register a file already on this machine.
pub async fn import(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(manager) = state.manager() else {
        return no_manager();
    };
    let body: ImportBody = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(err) => return bad_request("path", &err.to_string()),
    };

    let job = state.jobs().start(JobKind::Import, &state.shutdown_token());
    let background = Arc::clone(&job);
    let manager = Arc::clone(manager);
    // Hashing a multi-gigabyte file is minutes on this hardware, so an import
    // is a job too.
    tokio::spawn(async move {
        finish(&background, manager.import(body.path, &background).await);
    });
    accepted(&job)
}

/// Body of `POST /api/v1/models/{id}/load`.
#[derive(Debug, Default, Deserialize)]
pub struct LoadBody {
    #[serde(default)]
    pub ctx: Option<u32>,
    #[serde(default)]
    pub kv_type: Option<String>,
    #[serde(default)]
    pub threads: Option<u32>,
    #[serde(default)]
    pub force: bool,
}

/// `POST /api/v1/models/{id}/load` — swap the resident model.
pub async fn load(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    if state.manager().is_none() {
        return no_manager();
    }
    // An empty body is the ordinary case: load it as it is.
    let body: LoadBody = if body.is_empty() {
        LoadBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(body) => body,
            Err(err) => return bad_request("body", &err.to_string()),
        }
    };

    let kv_type = match body.kv_type.as_deref().map(str::parse::<GgmlType>) {
        None => None,
        Some(Ok(parsed)) => Some(parsed),
        Some(Err(_)) => {
            return bad_request(
                "kv_type",
                "unknown KV cache type; /health lists what this engine accepts",
            );
        }
    };

    let options = LoadOptions {
        n_ctx: body.ctx,
        kv_type,
        threads: body.threads,
        force: body.force,
    };

    let job = state.jobs().start(JobKind::Load, &state.shutdown_token());
    let background = Arc::clone(&job);
    let state = Arc::clone(&state);
    tokio::spawn(async move {
        let result = manager::load_model(&state, &id, options, &background).await;
        // `load_model` marks its own success, because only it knows the catalog
        // id that was loaded.
        if let Err(err) = result {
            background.set(JobState::Failed {
                error: hermes_core::ErrorReport::capture(&err),
            });
        }
    });
    accepted(&job)
}

/// `POST /api/v1/models/unload`.
pub async fn unload(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    if state.manager().is_none() {
        return no_manager();
    }
    match manager::unload_model(&state).await {
        Ok(unloaded) => axum::Json(json!({
            "unloaded": unloaded.map(|id| id.to_string()),
        }))
        .into_response(),
        Err(err) => error_response(&err),
    }
}

/// Query for `DELETE /api/v1/models/{id}`.
#[derive(Debug, Default, Deserialize)]
pub struct RemoveQuery {
    /// Also delete the file. Only ever applies to a model we downloaded.
    #[serde(default)]
    pub delete_file: bool,
}

/// `DELETE /api/v1/models/{id}`.
pub async fn remove(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<RemoveQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(manager) = state.manager() else {
        return no_manager();
    };

    let resident = state.catalog.resident().await;
    match manager
        .remove(&id, query.delete_file, resident.as_ref().map(|m| &m.id))
        .await
    {
        // What is reported is what the manager actually did, not the same
        // predicate evaluated a second time here. Two copies of "was this file
        // ours to delete?" are two answers waiting to disagree.
        Ok(removal) => axum::Json(json!({
            "removed": removal.model.id,
            "file_deleted": removal.file_deleted,
        }))
        .into_response(),
        Err(err) => error_response(&err),
    }
}

/// `GET /api/v1/jobs`.
pub async fn jobs(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let rows: Vec<serde_json::Value> = state
        .jobs()
        .recent()
        .into_iter()
        .map(|job| describe_job(&job))
        .collect();
    axum::Json(json!({ "object": "list", "data": rows })).into_response()
}

/// `GET /api/v1/jobs/{id}`.
pub async fn job(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    match state.jobs().get(id) {
        Some(job) => axum::Json(describe_job(&job)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(
                json!({"error": {"message": format!("no job {id}"), "code": "unknown_job"}}),
            ),
        )
            .into_response(),
    }
}

/// `GET /api/v1/jobs/{id}/events` — progress as server-sent events.
///
/// The stream ends when the job does, so a client can simply read to the end
/// rather than deciding for itself when to stop.
pub async fn job_events(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(job) = state.jobs().get(id) else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(
                json!({"error": {"message": format!("no job {id}"), "code": "unknown_job"}}),
            ),
        )
            .into_response();
    };

    // Current state and subsequent updates are taken together, so a job that
    // finishes between the two cannot leave this stream waiting forever.
    let (current, updates) = job.watch();
    let shutdown = state.shutdown_token();

    // `unfold` rather than a generator macro: the same shape the generation
    // stream uses, and no new dependency for one endpoint.
    let stream = futures_util::stream::unfold(
        EventStream {
            phase: Phase::Initial(current),
            updates,
            shutdown,
        },
        |mut stream| async move {
            loop {
                match std::mem::replace(&mut stream.phase, Phase::Done) {
                    Phase::Done => return None,
                    Phase::Initial(state) => {
                        stream.phase = next_phase(state.is_terminal());
                        // The error type is named once here; nothing in this
                        // stream can fail, but `Body::from_stream` wants a
                        // `Result`.
                        return Some((
                            Ok::<axum::body::Bytes, std::convert::Infallible>(sse_frame(&state)),
                            stream,
                        ));
                    }
                    Phase::Live => {
                        let update = tokio::select! {
                            () = stream.shutdown.cancelled() => None,
                            update = stream.updates.recv() => Some(update),
                        };
                        match update {
                            Some(Ok(state)) => {
                                stream.phase = next_phase(state.is_terminal());
                                return Some((Ok(sse_frame(&state)), stream));
                            }
                            // Lagged: intermediate updates were missed, which
                            // is harmless — the next one supersedes them, and
                            // the terminal state is held on the job itself.
                            Some(Err(broadcast::error::RecvError::Lagged(_))) => {
                                stream.phase = Phase::Live;
                                continue;
                            }
                            // Closed, or the gateway is shutting down: there is
                            // nothing further to report.
                            Some(Err(broadcast::error::RecvError::Closed)) | None => {
                                stream.phase = Phase::Closing;
                                continue;
                            }
                        }
                    }
                    Phase::Closing => {
                        stream.phase = Phase::Done;
                        return Some((
                            Ok(axum::body::Bytes::from_static(b"data: [DONE]\n\n")),
                            stream,
                        ));
                    }
                }
            }
        },
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(axum::body::Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Where a job's event stream has got to.
enum Phase {
    /// The state the watcher joined at, not yet sent.
    Initial(JobState),
    /// Following updates as they happen.
    Live,
    /// The terminal marker is still to send.
    Closing,
    Done,
}

struct EventStream {
    phase: Phase,
    updates: broadcast::Receiver<JobState>,
    shutdown: tokio_util::sync::CancellationToken,
}

const fn next_phase(terminal: bool) -> Phase {
    if terminal {
        Phase::Closing
    } else {
        Phase::Live
    }
}

fn sse_frame(state: &JobState) -> axum::body::Bytes {
    // A `JobState` cannot fail to serialize — it is plain owned data with no
    // map keys and no non-finite numbers. If that ever stops being true, say so
    // rather than inventing a "failed" state the job never reached, which would
    // report a completed download as an error.
    let payload = serde_json::to_string(state).unwrap_or_else(|err| {
        tracing::error!(
            target: hermes_observability::targets::API,
            error = %err,
            "a job state could not be encoded"
        );
        r#"{"state":"running","stage":{"of":"queued"}}"#.to_owned()
    });
    axum::body::Bytes::from(format!("data: {payload}\n\n"))
}

fn describe_job(job: &Arc<Job>) -> serde_json::Value {
    json!({
        "id": job.id.get(),
        "kind": job.kind,
        "started_at": job.started_at,
        "status": job.state(),
    })
}

/// 202 with the job to watch.
fn accepted(job: &Arc<Job>) -> Response {
    (
        StatusCode::ACCEPTED,
        axum::Json(json!({
            "job": job.id.get(),
            "events": format!("/api/v1/jobs/{}/events", job.id.get()),
        })),
    )
        .into_response()
}

/// Record a finished install on its job.
fn finish<E: Actionable>(job: &Arc<Job>, result: Result<InstalledModel, E>) {
    match result {
        Ok(model) => job.set(JobState::Succeeded {
            model: Some(model.id),
        }),
        Err(err) if err.kind() == hermes_core::ErrorKind::Cancelled => {
            job.set(JobState::Cancelled);
        }
        Err(err) => job.set(JobState::Failed {
            error: hermes_core::ErrorReport::capture(&err),
        }),
    }
}

fn error_response<E: Actionable>(err: &E) -> Response {
    let status =
        StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, axum::Json(ErrorEnvelope::from_error(err))).into_response()
}

fn bad_request(param: &str, message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "param": param,
                "code": "invalid_request",
            }
        })),
    )
        .into_response()
}

/// This gateway was started without a catalog.
fn no_manager() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        axum::Json(json!({
            "error": {
                "message": "this gateway was started without a model catalog",
                "type": "server_error",
                "code": "no_model_catalog",
            }
        })),
    )
        .into_response()
}
