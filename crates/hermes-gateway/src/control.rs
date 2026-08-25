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

use axum::extract::{Path, Query, State};
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
use crate::system::Probed;

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

impl CatalogRow {
    /// Describe one installed model, given whether it is the resident one.
    ///
    /// Shared by the list and the detail endpoint rather than written twice.
    /// Two copies of "is this loaded, is its file there, was its digest
    /// checked?" are two answers waiting to disagree, and the list and the
    /// detail view of the same model disagreeing is the kind of thing a user
    /// reports as the catalog being broken.
    fn describe(model: InstalledModel, loaded: bool) -> Self {
        Self {
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
    }
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
            CatalogRow::describe(model, loaded)
        })
        .collect();

    axum::Json(ListBody::of(rows)).into_response()
}

/// An OpenAI-shaped list body.
///
/// Typed rather than `json!({"object": "list", "data": rows})`, because that
/// macro resolves to `to_value(..).unwrap()` and a `CatalogRow` carries a
/// `PathBuf`. A model stored under a path that is not valid UTF-8 - legal on
/// Linux, where a path is bytes - would fail to serialize, and under the
/// release profile's `panic = "abort"` that unwrap takes the gateway down on a
/// request that should have been a 500.
#[derive(Debug, Serialize)]
pub struct ListBody<T> {
    object: &'static str,
    data: Vec<T>,
}

impl<T> ListBody<T> {
    fn of(data: Vec<T>) -> Self {
        Self {
            object: "list",
            data,
        }
    }
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
    /// Physical batch size. Priced by `GET /api/v1/models/{id}?ubatch=`.
    #[serde(default)]
    pub ubatch: Option<u32>,
    /// Threads for prompt processing. Absent means the same as `threads`.
    #[serde(default)]
    pub threads_batch: Option<u32>,
    /// `auto`, `none`, `mmap`, `mlock` or `mmap+mlock`.
    #[serde(default)]
    pub load_mode: Option<String>,
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
                "unknown KV cache type; GET /api/v1/gateway lists what this engine accepts",
            );
        }
    };

    let load_mode = match body
        .load_mode
        .as_deref()
        .map(hermes_core::LoadMode::from_name)
    {
        None => None,
        Some(Some(parsed)) => Some(parsed),
        Some(None) => {
            return bad_request(
                "load_mode",
                "unknown load mode; GET /api/v1/gateway lists what this engine accepts",
            );
        }
    };
    if let Some(ubatch) = body.ubatch
        && ubatch == 0
    {
        return bad_request("ubatch", "a physical batch size of zero processes nothing");
    }

    let options = LoadOptions {
        n_ctx: body.ctx,
        kv_type,
        threads: body.threads,
        ubatch: body.ubatch,
        threads_batch: body.threads_batch,
        load_mode,
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

/// One address this gateway is answering on.
#[derive(Debug, Serialize)]
pub struct ListenerReport {
    address: String,
    port: u16,
    /// Said plainly because it is what decided whether auth was required: a
    /// purely local bind is allowed to have no key.
    loopback: bool,
}

#[derive(Debug, Serialize)]
pub struct AuthReport {
    required: bool,
}

#[derive(Debug, Serialize)]
pub struct ConcurrencyReport {
    /// Slots the gateway is handing out right now.
    max_concurrent_requests: u32,
    queue_timeout_seconds: u64,
    /// What was asked for on the command line, or `null` for `auto`.
    ///
    /// Beside the live number rather than instead of it: "four, because you
    /// said four" and "four, because this machine can" are different answers
    /// to "why four?", and only one of them changes when a model is swapped.
    requested: Option<u32>,
}

/// Where this gateway reads and writes.
///
/// Typed, and serialized through `axum::Json` rather than built with `json!`.
/// That macro resolves to `to_value(..).unwrap()`, and `PathBuf` fails to
/// serialize when a path is not valid UTF-8 — which on Linux is a legal path.
/// Under the release profile's `panic = "abort"` that unwrap would take the
/// whole gateway down on one request. Serializing the response as a value lets
/// the same failure become a 500.
#[derive(Debug, Serialize)]
pub struct PathsReport {
    data: PathBuf,
    models: PathBuf,
    logs: PathBuf,
}

/// The reply to `GET /api/v1/gateway`.
#[derive(Debug, Serialize)]
pub struct GatewayReport {
    version: &'static str,
    backend: String,
    engine: hermes_inference::BackendHealth,
    model: Option<String>,
    listeners: Vec<ListenerReport>,
    /// What cannot be changed without restarting the listener, named so the
    /// panel can render those fields as read-only and say why.
    restart_required: Vec<&'static str>,
    auth: AuthReport,
    concurrency: ConcurrencyReport,
    /// What this engine accepts, straight from the backend.
    ///
    /// Here rather than on `/health`, which is probed by health checks and by
    /// the desktop shell and is deliberately left alone. It was advertised on
    /// the backend trait since M2 and serialized by no route, while a 400 told
    /// the user to look for it somewhere it had never been.
    engine_capabilities: hermes_inference::BackendCapabilities,
    /// The values a load uses when a request names none.
    ///
    /// Without these the panel cannot pre-select what the gateway would
    /// actually do, and would have to show a control whose starting position
    /// is a guess.
    defaults: DefaultsReport,
    queue: crate::scheduler::QueueSnapshot,
    paths: Option<PathsReport>,
}

/// The load defaults this gateway was started with.
#[derive(Debug, Serialize)]
pub struct DefaultsReport {
    kv_type: GgmlType,
    #[serde(skip_serializing_if = "Option::is_none")]
    threads: Option<u32>,
    /// `None` where the operator asked for `auto`.
    concurrency: Option<u32>,
    /// The physical batch a load uses when a request names none.
    ubatch: u32,
    /// The load modes this engine accepts, in its own spelling.
    ///
    /// Served here rather than on `/health` for the same reason the KV cache
    /// types are: `/health` is probed by health checks and by the desktop
    /// shell, and is left exactly as it is.
    load_modes: Vec<&'static str>,
}

/// What the GGUF header says, beyond what the catalog keeps.
///
/// The catalog stores what it needs to *list* a model — architecture, params,
/// quantization, context. The Models screen shows the shape of the network as
/// well, and those fields are read from the file rather than copied into the
/// catalog: copying them would mean a migration for every catalog already on
/// disk, and a second place for them to be wrong.
#[derive(Debug, Serialize)]
pub struct HeaderDetail {
    block_count: Option<u64>,
    embedding_length: Option<u64>,
    feed_forward_length: Option<u64>,
    head_count: Option<u64>,
    head_count_kv: Option<Vec<u64>>,
    vocab_size: Option<u64>,
    context_length: Option<u64>,
    rope_freq_base: Option<f64>,
    sliding_window: Option<u64>,
    tensor_count: u64,
    gguf_version: u32,
    /// The context sizes this model can be loaded at, smallest first.
    ///
    /// Serialized rather than derived by the caller: the ladder is a decision
    /// about which windows are worth offering, and a second copy of it in the
    /// panel would be a second answer to the same question. It ends at what the
    /// model was trained for, so nothing here is a size the engine would refuse.
    context_presets: Vec<u32>,
    /// Keys that were looked for and not found. Anything here means the
    /// estimate beside it is incomplete rather than wrong.
    missing: Vec<String>,
}

impl From<&hermes_gguf::ModelMetadata> for HeaderDetail {
    fn from(metadata: &hermes_gguf::ModelMetadata) -> Self {
        Self {
            block_count: metadata.block_count,
            embedding_length: metadata.embedding_length,
            feed_forward_length: metadata.feed_forward_length,
            head_count: metadata.head_count,
            head_count_kv: metadata.head_count_kv.clone(),
            vocab_size: metadata.vocab_size,
            context_length: metadata.context_length,
            rope_freq_base: metadata.rope_freq_base,
            sliding_window: metadata.sliding_window,
            tensor_count: metadata.tensor_count,
            gguf_version: metadata.gguf_version,
            context_presets: hermes_core::RuntimeParams::context_presets_for(
                metadata.context_length,
            ),
            missing: metadata.missing.clone(),
        }
    }
}

/// `GET /api/v1/models/{id}` — one model, in full.
///
/// The list endpoint stays a list: it answers from the catalog alone and opens
/// no files, because a panel polls it and a header read per row per poll would
/// make watching the model list cost more than using it. Everything that needs
/// the file — the network's shape, and the RAM estimate — is here, where it is
/// paid for once when someone actually selects a model.
///
/// The estimate is for the context this model would **actually** be loaded
/// with: its last context if it has one, and otherwise the largest this machine
/// can safely give it, exactly as `load` decides. An estimate for some other
/// context would be a number that no button on the screen produces.
pub async fn model_detail(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    Query(query): Query<DetailQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(manager) = state.manager() else {
        return no_manager();
    };

    let kv_type = match query.kv_type.as_deref().map(str::parse::<GgmlType>) {
        None => None,
        Some(Ok(parsed)) => Some(parsed),
        Some(Err(_)) => {
            return bad_request(
                "kv_type",
                "unknown KV cache type; GET /api/v1/gateway lists what this engine accepts",
            );
        }
    };

    let Some(model) = manager.get(&id).await else {
        return error_response(&hermes_catalog::CatalogError::UnknownModel { id });
    };
    let resident = state.catalog.resident().await;
    let loaded = resident
        .as_ref()
        .is_some_and(|current| current.id.slug() == model.id);
    let row = CatalogRow::describe(model, loaded);

    // The file is only opened when it is there. A model on a drive that is not
    // mounted is still a model, and saying so beats an I/O error.
    if !row.model.is_present() {
        return axum::Json(ModelDetail {
            row,
            header: None,
            estimate: None,
            context_source: None,
        })
        .into_response();
    }

    let defaults = manager.defaults();
    let path = row.model.path.clone();
    // Exactly the inputs `load` uses, so the number on screen is the number
    // that request would produce. `last_n_ctx` is not among them.
    let stored_default = crate::store_api::gateway_settings(&state).default_n_ctx;
    let asked = DetailOptions {
        n_ctx: query.ctx,
        kv_type,
        ubatch: query.ubatch,
        stored_default,
    };

    // Reading a header and probing memory are both blocking.
    let memory = state.memory_probe();
    let probed =
        tokio::task::spawn_blocking(move || describe_file(&path, defaults, asked, memory.as_ref()))
            .await;
    let (header, estimate, context_source) = match probed {
        Ok(described) => described,
        Err(err) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({
                    "error": {
                        "message": format!("the model file could not be read: {err}"),
                        "type": "server_error",
                        "code": "probe_unavailable",
                    }
                })),
            )
                .into_response();
        }
    };

    axum::Json(ModelDetail {
        row,
        header,
        estimate,
        context_source,
    })
    .into_response()
}

/// One model with everything known about it.
#[derive(Debug, Serialize)]
pub struct ModelDetail {
    #[serde(flatten)]
    row: CatalogRow,
    /// `None` when the file is not there to be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    header: Option<HeaderDetail>,
    /// `None` only when there was no header to estimate against; an estimate
    /// that could not be *computed* says why instead of going missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    estimate: Option<Probed<hermes_memory::Estimate>>,
    /// Why the estimate is for the context it is for.
    #[serde(skip_serializing_if = "Option::is_none")]
    context_source: Option<manager::ContextSource>,
}

/// What the caller is weighing, as query parameters.
///
/// Absent, the response is byte-identical to what it was before they existed,
/// so no client that already reads this endpoint changes.
///
/// `threads` is deliberately not here. It appears nowhere in the estimator's
/// compute arithmetic, so a threads parameter would report a memory effect it
/// does not have — the same reason the panel shows no batch-size slider.
#[derive(Debug, Default, serde::Deserialize)]
pub struct DetailQuery {
    /// Estimate for this context rather than the one a load would pick.
    pub ctx: Option<u32>,
    /// Estimate for this KV cache type rather than the gateway's default.
    pub kv_type: Option<String>,
    /// Estimate for this physical batch size rather than the default.
    ///
    /// Here, where `threads` is deliberately not, because compute buffers
    /// scale with `n_ubatch` and with nothing else on this list. A knob that
    /// changed no number in the estimate would be reporting a memory effect it
    /// does not have.
    pub ubatch: Option<u32>,
}

/// The inputs the estimate is computed from, resolved.
#[derive(Debug, Clone, Copy, Default)]
struct DetailOptions {
    n_ctx: Option<u32>,
    kv_type: Option<GgmlType>,
    ubatch: Option<u32>,
    stored_default: Option<u32>,
}

/// Read the header and estimate what loading it would cost.
///
/// Both or neither: the estimate is computed *from* the header, so a header
/// that could not be read leaves nothing to estimate against. Returning a
/// verdict anyway would be a verdict about a model we could not open.
fn describe_file(
    path: &std::path::Path,
    defaults: manager::RuntimeDefaults,
    asked: DetailOptions,
    memory: &dyn hermes_system_info::MemoryProbe,
) -> (
    Option<HeaderDetail>,
    Option<Probed<hermes_memory::Estimate>>,
    Option<manager::ContextSource>,
) {
    // The catalog's own reader, not a second copy: "is this a model?" must have
    // one answer.
    let Ok(metadata) = hermes_catalog::read_header(path) else {
        return (None, None, None);
    };
    let detail = HeaderDetail::from(&metadata);

    let snapshot = match memory.snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            // The header is still worth having; only the verdict needs the
            // machine. But "no verdict" and "no verdict, and here is why" are
            // different answers, and the panel can only act on the second.
            return (Some(detail), Some(Probed::from_probe(Err(err))), None);
        }
    };

    let cache_type = asked.kv_type.unwrap_or(defaults.kv_type);
    let base = hermes_core::RuntimeParams {
        cache_type_k: cache_type,
        cache_type_v: cache_type,
        threads: Some(
            defaults
                .threads
                .unwrap_or_else(|| hermes_system_info::CpuInfo::detect().default_threads()),
        ),
        // The slot count this model would be loaded with, chosen the way the
        // load itself chooses it - the price on screen has to be the price the
        // button pays.
        n_parallel: 1,
        ..hermes_core::RuntimeParams::default()
    };
    let base = match asked.ubatch {
        Some(ubatch) if ubatch > 0 => base.with_ubatch(ubatch),
        _ => base,
    };

    // Exactly how `load` chooses, through the same functions it calls, so the
    // number on screen is the number that request would produce.
    let estimator = hermes_memory::Estimator::headless();
    let slots = estimator.choose_concurrency(
        defaults.concurrency,
        hermes_system_info::CpuInfo::detect().logical_cores,
        Some(&metadata),
        base,
        hermes_memory::Budget::of(snapshot),
    );
    let base = hermes_core::RuntimeParams {
        n_parallel: slots.slots,
        ..base
    };
    let chosen = manager::choose_context(
        asked.n_ctx,
        asked.stored_default,
        &metadata,
        base,
        // Nothing is reclaimed here: this describes a load, and a description
        // must not promise memory that only a real swap would release.
        hermes_memory::Budget::of(snapshot),
        &estimator,
    );
    let estimate = estimator.estimate(&metadata, base.with_context(chosen.n_ctx), snapshot);
    (
        Some(detail),
        Some(Probed::Read { reading: estimate }),
        Some(chosen.source),
    )
}

/// `GET /api/v1/requests` — what is being served right now, and what is queued.
///
/// The one thing the metrics could never say. `/api/v1/events` reports a
/// generation once it has *finished*, and the queue snapshot reports how many
/// are running as a bare number — so a gateway serving four clients for two
/// minutes each looked, from outside, much like a gateway doing nothing, until
/// they all finished at once.
///
/// It carries what the live feed carries and nothing more: the completion id
/// the client already has, the model `/v1/models` already advertises, the band,
/// and the prompt the engine counted. **Not the caller's address** — that is a
/// scheduling key, and this is a display.
pub async fn requests(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    axum::Json(state.scheduler().roster()).into_response()
}

/// The machine defaults a load would use.
///
/// The manager owns them; a gateway without one has none to report, and the
/// shipped defaults are what a load would use anyway.
fn defaults_of(state: &GatewayState) -> crate::manager::RuntimeDefaults {
    state
        .manager()
        .map(|manager| manager.defaults())
        .unwrap_or_default()
}

/// `GET /api/v1/gateway` — how this gateway is configured, and what it is doing.
///
/// The API Gateway screen asks four questions no existing endpoint answers:
/// where are we serving, is a key required, how many requests run at once, and
/// what is the queue doing. `/health` and `/version` answer two adjacent ones
/// and are deliberately left alone — they are probed by clients and scrapers
/// that must keep seeing exactly what they see today.
///
/// **Read-only, and honest about why.** The address, the port and the key are
/// settled before the first listener is bound, and changing any of them means
/// restarting the listener — which nothing inside this process can do to
/// itself. Rather than offering a control that would silently fail, each of
/// those is named in `restart_required`.
///
/// The key itself is never here. It is not logged, it is not in the engine's
/// argv, and an endpoint that returned it would undo both.
pub async fn gateway(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    axum::Json(describe_gateway(&state).await).into_response()
}

/// Read the gateway's own description.
///
/// Split from the handler so it can be asserted on without a server or a key,
/// as `system::report` is.
pub async fn describe_gateway(state: &GatewayState) -> GatewayReport {
    let config = &state.config;
    let resident = state.catalog.resident().await;

    GatewayReport {
        version: env!("CARGO_PKG_VERSION"),
        backend: state.backend.id().to_string(),
        engine: state.backend.health().await,
        model: resident.as_ref().map(|model| model.id.to_string()),
        listeners: config
            .bound_addresses
            .iter()
            .map(|address| ListenerReport {
                address: address.to_string(),
                port: address.port(),
                loopback: address.ip().is_loopback(),
            })
            .collect(),
        restart_required: vec!["listeners", "auth", "concurrency"],
        auth: AuthReport {
            required: config.auth.is_enabled(),
        },
        concurrency: ConcurrencyReport {
            // What is being handed out right now, which follows the loaded
            // engine, rather than what the command line asked for at startup.
            max_concurrent_requests: state.scheduler().capacity(),
            queue_timeout_seconds: config.queue_timeout.as_secs(),
            requested: defaults_of(state).concurrency,
        },
        engine_capabilities: state.backend.capabilities(),
        defaults: {
            // The manager owns them; a gateway without one has none to report,
            // and the shipped defaults are what a load would use anyway.
            let defaults = defaults_of(state);
            DefaultsReport {
                kv_type: defaults.kv_type,
                threads: defaults.threads,
                concurrency: defaults.concurrency,
                ubatch: hermes_core::RuntimeParams::default().n_ubatch,
                load_modes: hermes_core::LoadMode::ALL
                    .iter()
                    .map(|mode| mode.as_str())
                    .collect(),
            }
        },
        queue: state.scheduler().snapshot(),
        paths: config.paths.as_ref().map(|paths| PathsReport {
            data: paths.data_dir().to_path_buf(),
            models: paths.models_dir(),
            logs: paths.logs_dir(),
        }),
    }
}

/// `GET /api/v1/events` — finished generations, as they finish.
///
/// One stream, deliberately. The Dashboard's live feed and the API Gateway
/// screen's recent-requests list are the same data rendered twice, and two
/// endpoints publishing it would be two chances to disagree about what a
/// request did.
///
/// It carries what the closing log line carries and nothing more — the prompt,
/// the completion and the key are as absent here as they are there. What this
/// adds over reading the log is only timeliness.
///
/// Unlike a job's event stream, this one never ends on its own: a gateway
/// always has more requests ahead of it. It ends when the client goes away or
/// the gateway shuts down.
pub async fn events(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }

    let receiver = state.metrics().watch_requests();
    let shutdown = state.shutdown_token();

    let stream = futures_util::stream::unfold(
        (receiver, shutdown),
        |(mut receiver, shutdown)| async move {
            loop {
                let received = tokio::select! {
                    () = shutdown.cancelled() => return None,
                    received = receiver.recv() => received,
                };
                match received {
                    Ok(event) => {
                        let payload = serde_json::to_string(&event).unwrap_or_else(|err| {
                            // A `RequestEvent` is plain owned data with no map
                            // keys and no non-finite numbers, so this cannot
                            // happen. Said rather than guessed at, because
                            // inventing an event would put a request in the
                            // feed that never ran.
                            tracing::error!(
                                target: hermes_observability::targets::API,
                                error = %err,
                                "a request event could not be encoded"
                            );
                            String::new()
                        });
                        if payload.is_empty() {
                            continue;
                        }
                        return Some((
                            Ok::<axum::body::Bytes, std::convert::Infallible>(
                                axum::body::Bytes::from(format!("data: {payload}\n\n")),
                            ),
                            (receiver, shutdown),
                        ));
                    }
                    // This watcher fell behind. Told rather than hidden: a feed
                    // that silently skips is a feed whose gaps get read as idle
                    // time.
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        return Some((
                            Ok(axum::body::Bytes::from(format!(
                                "data: {{\"missed\":{missed}}}\n\n"
                            ))),
                            (receiver, shutdown),
                        ));
                    }
                    // The gateway is going away.
                    Err(broadcast::error::RecvError::Closed) => return None,
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

/// A refusal with a stable code, in the shape every other error here has.
fn error_json(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        axum::Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "code": code,
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

/// Body for `POST /api/v1/benchmarks`.
///
/// Every field optional: the ordinary case is "measure what is loaded", and the
/// defaults are the runner's own.
#[derive(Debug, Default, Deserialize)]
pub struct BenchmarkBody {
    pub prompt_tokens: Option<u32>,
    pub generate_tokens: Option<u32>,
    pub repeat: Option<u32>,
}

/// `POST /api/v1/benchmarks`.
///
/// Measures the model that is **already resident**, at the parameters it was
/// already loaded with. It does not reload, does not change a setting and does
/// not pause the scheduler: it takes a slot like any other request, because
/// that is exactly what it is.
///
/// Varying the parameters means reloading the engine, which takes minutes and
/// would interrupt whoever is being served. That is `hermes bench`, which
/// brings its own engine.
pub async fn run_benchmark(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(store) = state.benchmarks() else {
        return error_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_data_directory",
            "this gateway has no data directory, so it cannot save a benchmark",
        );
    };
    let body: BenchmarkBody = if body.is_empty() {
        BenchmarkBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(body) => body,
            Err(err) => return bad_request("body", &err.to_string()),
        }
    };

    let Some(resident) = state.catalog.resident().await else {
        return error_json(
            StatusCode::CONFLICT,
            "no_model_loaded",
            "load a model before benchmarking; there is nothing resident to measure",
        );
    };

    let job = state
        .jobs()
        .start(JobKind::Benchmark, &state.shutdown_token());
    let background = Arc::clone(&job);
    let state = Arc::clone(&state);
    tokio::spawn(async move {
        match crate::benchmark::run(&state, &store, &resident, &body, &background).await {
            Ok(id) => background.set(JobState::Succeeded { model: Some(id) }),
            Err(err) => background.set(JobState::Failed {
                error: hermes_core::ErrorReport::capture(&err),
            }),
        }
    });
    accepted(&job)
}

/// `GET /api/v1/benchmarks`.
pub async fn benchmarks(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(store) = state.benchmarks() else {
        return axum::Json(json!({ "runs": [] })).into_response();
    };
    match store.list() {
        Ok(runs) => axum::Json(json!({ "runs": runs })).into_response(),
        Err(err) => error_response(&err),
    }
}

/// `GET /api/v1/benchmarks/{id}`.
pub async fn benchmark(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(store) = state.benchmarks() else {
        return error_json(
            StatusCode::NOT_FOUND,
            "no_data_directory",
            "this gateway has no data directory, so it has saved no benchmarks",
        );
    };
    match store.get(&id) {
        Ok(run) => axum::Json(run).into_response(),
        Err(err) => error_response(&err),
    }
}
