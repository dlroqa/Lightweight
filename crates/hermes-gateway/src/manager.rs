//! Loading, unloading and acquiring models on a gateway that is already
//! serving.
//!
//! This is the piece that makes the catalog more than a list. It owns the
//! ordering that a swap has to follow, and every step of it exists because
//! skipping it breaks something specific:
//!
//! 1. **Pause admission**, so nothing new starts against an engine that is
//!    about to be replaced.
//! 2. **Drain**, because nothing is preempted. The generation in flight when a
//!    swap is requested runs to its end.
//! 3. **Admit against this machine's free memory**, exactly as `hermes serve`
//!    does. A model that fits on the developer's box is not a model that fits.
//! 4. **Load**, which unloads the previous engine first — two models resident
//!    at once is the memory spike admission control exists to prevent.
//! 5. **Re-derive the band ceilings** from the context actually loaded. A swap
//!    that skipped this would schedule a new model by the old one's numbers.
//! 6. **Publish**, then resume.
//!
//! Steps 1 and 6 are a guard, so a failure anywhere between them cannot leave
//! the gateway refusing to admit anything.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hermes_catalog::install::{AddModel, InstallProgress, Installer};
use hermes_catalog::{CatalogError, CatalogStore, InstalledModel};
use hermes_core::{
    Actionable, ErrorKind, GgmlType, ModelId, Remedy, RemedyAction, RuntimeParams, SettingsSection,
};
use hermes_inference::{BackendError, LoadProgress, LoadRequest};
use hermes_memory::{Estimator, Verdict};
use hermes_observability::targets;
use hermes_system_info::{CpuInfo, MemoryProbe, SystemMemoryProbe};
use tokio::sync::{Mutex, mpsc};

use crate::catalog::ResidentModel;
use crate::jobs::{Job, JobState, Stage};
use crate::scheduler::BandLimits;
use crate::state::GatewayState;

/// How long a swap waits for the engine to go idle before giving up.
///
/// Generous on purpose: a turn on the slowest machine this targets runs to
/// minutes, and a swap that gave up after thirty seconds would fail on exactly
/// the hardware the feature matters most on. Bounded all the same, so a stuck
/// engine surfaces as an error rather than a gateway that never comes back.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(600);

/// What the machine's defaults are, from the command line that started us.
#[derive(Clone, Copy, Debug)]
pub struct RuntimeDefaults {
    pub kv_type: GgmlType,
    pub threads: Option<u32>,
    /// Requests that may run at once, which the engine is told as `--parallel`
    /// and the RAM estimate is computed for.
    pub concurrency: u32,
}

impl Default for RuntimeDefaults {
    fn default() -> Self {
        Self {
            kv_type: GgmlType::F16,
            threads: None,
            concurrency: 1,
        }
    }
}

/// What a caller asked for when loading a model.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadOptions {
    /// Context length. Absent, the largest that fits this machine is chosen.
    pub n_ctx: Option<u32>,
    pub kv_type: Option<GgmlType>,
    pub threads: Option<u32>,
    /// Load even though the estimate says it will not fit.
    pub force: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),

    #[error(transparent)]
    Backend(#[from] BackendError),

    #[error("another model operation is already running")]
    Busy,

    #[error("this gateway was started without a model catalog")]
    NoCatalog,

    #[error("the engine did not become idle within {seconds} seconds")]
    DrainTimedOut { seconds: u64 },

    #[error("{id} is in the catalog but its file is missing from {path}")]
    FileMissing { id: String, path: PathBuf },
}

impl Actionable for ManagerError {
    fn code(&self) -> &'static str {
        match self {
            Self::Catalog(err) => err.code(),
            Self::Backend(err) => err.code(),
            Self::Busy => "model_operation_in_progress",
            Self::NoCatalog => "no_model_catalog",
            Self::DrainTimedOut { .. } => "drain_timed_out",
            Self::FileMissing { .. } => "model_file_not_found",
        }
    }

    fn kind(&self) -> ErrorKind {
        match self {
            Self::Catalog(err) => err.kind(),
            Self::Backend(err) => err.kind(),
            // Retrying is exactly right here, and the operation ahead is
            // usually seconds away from finishing.
            Self::Busy => ErrorKind::RateLimited,
            // Not a transient condition and not the caller's mistake: this
            // build of the gateway simply has no catalog attached.
            Self::NoCatalog => ErrorKind::Internal,
            Self::DrainTimedOut { .. } => ErrorKind::Unavailable,
            Self::FileMissing { .. } => ErrorKind::NotFound,
        }
    }

    fn remedies(&self) -> Vec<Remedy> {
        match self {
            Self::Catalog(err) => err.remedies(),
            Self::Backend(err) => err.remedies(),
            Self::Busy => vec![Remedy::new(
                "Wait for the current model operation to finish",
                RemedyAction::RetryAfter { seconds: 5 },
            )],
            Self::NoCatalog => Vec::new(),
            Self::DrainTimedOut { .. } => vec![Remedy::new(
                "Wait for the running request to finish, or stop the client",
                RemedyAction::RetryAfter { seconds: 30 },
            )],
            Self::FileMissing { .. } => vec![Remedy::new(
                "Download or import the model again",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Models,
                },
            )],
        }
    }
}

/// What removing a model actually did.
///
/// Carries the outcome rather than leaving each caller to re-derive it: whether
/// the file was deleted depends on where it came from *and* on whether the
/// delete succeeded, and only this function knows both.
#[derive(Clone, Debug)]
pub struct Removal {
    pub model: InstalledModel,
    pub file_deleted: bool,
}

/// The catalog, and the operations that change it.
#[derive(Debug)]
pub struct ModelManager {
    catalog: Mutex<CatalogStore>,
    installer: Installer,
    /// Serialises install and load.
    ///
    /// Two clients loading two models at once would race the engine into an
    /// undefined state, and two downloads of the same model would fight over
    /// one partial file.
    operation: Mutex<()>,
    defaults: RuntimeDefaults,
}

impl ModelManager {
    pub fn new(catalog: CatalogStore, installer: Installer, defaults: RuntimeDefaults) -> Self {
        Self {
            catalog: Mutex::new(catalog),
            installer,
            operation: Mutex::new(()),
            defaults,
        }
    }

    pub fn defaults(&self) -> RuntimeDefaults {
        self.defaults
    }

    /// Every installed model.
    pub async fn models(&self) -> Vec<InstalledModel> {
        self.catalog.lock().await.models().cloned().collect()
    }

    pub async fn get(&self, id: &str) -> Option<InstalledModel> {
        self.catalog.lock().await.get(id).cloned()
    }

    /// Download or link a model, reporting into `job`.
    ///
    /// The catalog lock is taken twice, briefly, and **never across the
    /// transfer**: a download runs for minutes, and holding it would block
    /// `GET /api/v1/models` — the very listing a UI refreshes while watching
    /// the download it started.
    pub async fn install(
        &self,
        request: &AddModel,
        job: &Arc<Job>,
    ) -> Result<InstalledModel, ManagerError> {
        let _busy = self.operation.try_lock().map_err(|_| ManagerError::Busy)?;
        let (progress, pump) = install_progress(job);

        let outcome = self.install_phases(request, &progress, job).await;

        drop(progress);
        let _ = pump.await;
        outcome
    }

    /// The three phases of an install, so the lock discipline above is legible.
    async fn install_phases(
        &self,
        request: &AddModel,
        progress: &mpsc::Sender<InstallProgress>,
        job: &Arc<Job>,
    ) -> Result<InstalledModel, ManagerError> {
        let _ = progress.try_send(InstallProgress::Resolving);
        let plan = self.installer.plan(request).await?;

        if let Some(existing) = {
            let catalog = self.catalog.lock().await;
            Installer::already_installed(&catalog, &plan)
        } {
            return Ok(existing);
        }

        // No lock held here. This is the part that takes minutes.
        let scanned = self
            .installer
            .fetch(&plan, progress, &job.cancel_token())
            .await?;

        let mut catalog = self.catalog.lock().await;
        Ok(Installer::commit(&mut catalog, scanned)?)
    }

    /// Register a model already on this machine, reporting into `job`.
    ///
    /// Same discipline: hashing a multi-gigabyte file happens with no lock
    /// held, and the catalog is taken only to insert the result.
    pub async fn import(
        &self,
        path: PathBuf,
        job: &Arc<Job>,
    ) -> Result<InstalledModel, ManagerError> {
        let _busy = self.operation.try_lock().map_err(|_| ManagerError::Busy)?;
        let (progress, pump) = install_progress(job);

        let outcome = async {
            let scanned = self.installer.scan_local(path, &progress).await?;
            let mut catalog = self.catalog.lock().await;
            Installer::commit(&mut catalog, scanned)
        }
        .await;

        drop(progress);
        let _ = pump.await;
        Ok(outcome?)
    }

    /// Forget a model, and optionally delete the file.
    ///
    /// Refuses while the model is the one loaded: deleting the weights out from
    /// under a running engine is not a state worth supporting.
    pub async fn remove(
        &self,
        id: &str,
        delete_file: bool,
        resident: Option<&ModelId>,
    ) -> Result<Removal, ManagerError> {
        let mut catalog = self.catalog.lock().await;
        let Some(model) = catalog.get(id) else {
            return Err(CatalogError::UnknownModel { id: id.to_owned() }.into());
        };
        if resident.is_some_and(|loaded| loaded.slug() == model.id) {
            return Err(CatalogError::InUse {
                id: model.id.clone(),
            }
            .into());
        }

        let removed = catalog.remove(id)?;
        catalog.save()?;

        // Only a file we downloaded into our own directory is ours to delete.
        // An imported one belongs to the user and was never copied.
        let ours = matches!(
            removed.source,
            hermes_catalog::Source::Manifest { .. } | hermes_catalog::Source::Link { .. }
        );
        let file_deleted = delete_file && ours && std::fs::remove_file(&removed.path).is_ok();
        tracing::info!(
            target: targets::MODEL,
            id = %removed.id,
            file_deleted,
            "model removed from the catalog"
        );
        Ok(Removal {
            model: removed,
            file_deleted,
        })
    }

    /// Add the model named on the command line to the catalog.
    ///
    /// Tolerant on purpose. `hermes serve <model.gguf>` is about serving, and a
    /// model already in the catalog, or a file we cannot hash, must not be
    /// reported as a failure of the thing the user actually asked for. The
    /// installer already treats identical bytes as the model it already has.
    pub async fn register_at_startup(&self, path: PathBuf) -> Result<InstalledModel, ManagerError> {
        // A drained receiver, so progress is discarded rather than filling a
        // channel nobody reads. The sends are `try_send` regardless.
        let (progress, mut updates) = mpsc::channel(8);
        let drain = tokio::spawn(async move { while updates.recv().await.is_some() {} });

        let scanned = self.installer.scan_local(path, &progress).await;
        drop(progress);
        let _ = drain.await;

        let mut catalog = self.catalog.lock().await;
        Ok(Installer::commit(&mut catalog, scanned?)?)
    }

    /// Record that a model was loaded, for the default context next time.
    async fn mark_loaded(&self, id: &str, n_ctx: u32) {
        let mut catalog = self.catalog.lock().await;
        if let Some(model) = catalog.get_mut(id) {
            model.mark_loaded(n_ctx);
        }
        // A failure to persist this is not worth failing a load over: it costs
        // a remembered default, nothing more.
        let _ = catalog.save();
    }
}

/// Forward install progress into a job, one update per whole percent.
///
/// The throttle is not cosmetic. A download reports per chunk — 16 KB at a
/// time — so a 1 GB model would produce roughly 65,000 updates, each becoming a
/// broadcast send and an SSE frame per watcher. Measured on a 16 MB engine
/// download it was **1,010 frames** where 100 carry the same information, and
/// on a four-core 1.5 GHz box that is CPU taken from the work the user is
/// waiting for.
///
/// Every send is `try_send` inside the installer, so a slow consumer here can
/// never stall a download either way.
fn install_progress(
    job: &Arc<Job>,
) -> (mpsc::Sender<InstallProgress>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel(64);
    let job = Arc::clone(job);
    let pump = tokio::spawn(async move {
        let mut throttle = Throttle::default();
        while let Some(progress) = rx.recv().await {
            let step = match &progress {
                InstallProgress::Downloading { downloaded, total } => {
                    Step::Fraction("downloading", *downloaded, *total)
                }
                InstallProgress::Hashing { done, total } => {
                    Step::Fraction("hashing", *done, Some(*total))
                }
                InstallProgress::Resolving => Step::Stage("resolving"),
                InstallProgress::Reading => Step::Stage("reading"),
                InstallProgress::Done => Step::Stage("done"),
            };
            if throttle.should_emit(step) {
                job.advance(Stage::Install { progress });
            }
        }
    });
    (tx, pump)
}

/// One progress update, reduced to what decides whether it is worth sending.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    /// A named stage, always worth reporting when it changes.
    Stage(&'static str),
    /// A stage carrying a fraction, reported when the whole percent changes.
    Fraction(&'static str, u64, Option<u64>),
}

/// Drops progress updates that would tell a watcher nothing new.
#[derive(Default)]
struct Throttle {
    last_stage: Option<&'static str>,
    last_percent: Option<u64>,
}

impl Throttle {
    fn should_emit(&mut self, step: Step) -> bool {
        match step {
            Step::Stage(name) => {
                let changed = self.last_stage != Some(name);
                self.last_stage = Some(name);
                self.last_percent = None;
                changed
            }
            Step::Fraction(name, done, total) => {
                let percent = total.and_then(|total| done.saturating_mul(100).checked_div(total));
                let changed = self.last_stage != Some(name) || self.last_percent != percent;
                self.last_stage = Some(name);
                self.last_percent = percent;
                // A transfer whose size is unknown has no percent to change, so
                // it reports every update rather than none.
                changed || percent.is_none()
            }
        }
    }
}

/// Make a model resident, replacing whatever is loaded now.
///
/// See the module note for why the steps are in this order.
pub async fn load_model(
    state: &Arc<GatewayState>,
    id: &str,
    options: LoadOptions,
    job: &Arc<Job>,
) -> Result<ResidentModel, ManagerError> {
    let manager = state.manager().ok_or(ManagerError::NoCatalog)?;
    let _busy = manager
        .operation
        .try_lock()
        .map_err(|_| ManagerError::Busy)?;

    let model = manager
        .get(id)
        .await
        .ok_or_else(|| CatalogError::UnknownModel { id: id.to_owned() })?;
    if !model.is_present() {
        return Err(ManagerError::FileMissing {
            id: model.id.clone(),
            path: model.path.clone(),
        });
    }

    // The catalog's own reader, not a second copy: "is this a model?" must
    // have one answer.
    let metadata = hermes_catalog::read_header(&model.path)?;
    let defaults = manager.defaults();
    let cache_type = options.kv_type.unwrap_or(defaults.kv_type);
    let cpu = CpuInfo::detect();
    let base = RuntimeParams {
        cache_type_k: cache_type,
        cache_type_v: cache_type,
        threads: Some(
            options
                .threads
                .or(defaults.threads)
                .unwrap_or_else(|| cpu.default_threads()),
        ),
        n_parallel: defaults.concurrency.max(1),
        ..RuntimeParams::default()
    };

    // Admission control, exactly as `hermes serve` does it: never promise a
    // model will run because its weights fit.
    let estimator = Estimator::headless();
    let snapshot = SystemMemoryProbe
        .snapshot()
        .map_err(|err| BackendError::io("reading system memory", std::io::Error::other(err)))?;
    let n_ctx = match options.n_ctx {
        Some(requested) => requested,
        None => estimator
            .largest_safe_context(&metadata, base, snapshot, None)
            .unwrap_or(base.n_ctx),
    };
    let params = base.with_context(n_ctx);
    let estimate = estimator.estimate(&metadata, params, snapshot);
    if estimate.verdict == Verdict::Insufficient && !options.force {
        return Err(BackendError::InsufficientMemory {
            model: model.id.clone(),
            // Rendered rather than raw: this string reaches the user, and
            // "2.47 GiB" is the number they can act on.
            required: estimate.total.to_string(),
            available: estimate.budget.to_string(),
        }
        .into());
    }

    // Nothing new starts, and what is running is left to finish.
    let scheduler = state.scheduler();
    let _paused = scheduler.pause();
    if !scheduler.drain(DRAIN_TIMEOUT).await {
        return Err(ManagerError::DrainTimedOut {
            seconds: DRAIN_TIMEOUT.as_secs(),
        });
    }

    // The engine the previous model was in is stopped by `load` itself, before
    // it launches the new one, so two models are never resident at once.
    let (progress, pump) = load_progress(job);
    let request = LoadRequest {
        model: ModelId::with_context(&model.id, params.n_ctx),
        gguf_path: model.path.clone(),
        metadata: Arc::new(metadata),
        runtime: params,
    };
    let loaded = state
        .backend
        .load(request, progress, job.cancel_token())
        .await;
    let _ = pump.await;
    let loaded = loaded?;

    // The ceilings follow the context that is actually running.
    scheduler.set_band_limits(BandLimits::for_context(loaded.effective.n_ctx));

    let resident = ResidentModel {
        id: loaded.model.clone(),
        instance: loaded.instance,
        n_ctx: loaded.effective.n_ctx,
        architecture: model.architecture.clone(),
        param_count: model.param_count,
        quantization: model.quantization.clone(),
        model_max_context_length: model.context_length,
        ram_verdict: Some(estimate.verdict.label().to_owned()),
        backend: Some(state.backend.id().to_string()),
        model_path: model.path.display().to_string(),
    };
    state.catalog.set_resident(Some(resident.clone())).await;
    manager.mark_loaded(&model.id, loaded.effective.n_ctx).await;

    tracing::info!(
        target: targets::MODEL,
        id = %resident.id,
        n_ctx = resident.n_ctx,
        verdict = ?estimate.verdict,
        "model loaded on a running gateway"
    );
    job.set(JobState::Succeeded {
        model: Some(model.id.clone()),
    });
    Ok(resident)
}

/// Release whatever is loaded.
///
/// Idempotent: unloading nothing succeeds, so a UI need not know the state
/// before asking.
pub async fn unload_model(state: &Arc<GatewayState>) -> Result<Option<ModelId>, ManagerError> {
    let Some(resident) = state.catalog.resident().await else {
        return Ok(None);
    };

    let scheduler = state.scheduler();
    let _paused = scheduler.pause();
    if !scheduler.drain(DRAIN_TIMEOUT).await {
        return Err(ManagerError::DrainTimedOut {
            seconds: DRAIN_TIMEOUT.as_secs(),
        });
    }

    state.backend.unload(resident.instance).await?;
    state.catalog.set_resident(None).await;
    tracing::info!(target: targets::MODEL, id = %resident.id, "model unloaded");
    Ok(Some(resident.id))
}

/// Forward load progress into a job, throttled the same way.
///
/// A first load on a fresh profile downloads the engine, which reports per
/// chunk exactly as a model download does.
fn load_progress(job: &Arc<Job>) -> (mpsc::Sender<LoadProgress>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel(32);
    let job = Arc::clone(job);
    let pump = tokio::spawn(async move {
        let mut throttle = Throttle::default();
        while let Some(progress) = rx.recv().await {
            let step = match &progress {
                LoadProgress::AcquiringRuntime { downloaded, total } => {
                    Step::Fraction("acquiring_runtime", *downloaded, *total)
                }
                LoadProgress::VerifyingRuntime => Step::Stage("verifying_runtime"),
                LoadProgress::StartingEngine => Step::Stage("starting_engine"),
                LoadProgress::LoadingWeights => Step::Stage("loading_weights"),
                LoadProgress::Ready => Step::Stage("ready"),
            };
            if throttle.should_emit(step) {
                job.advance(Stage::Load { progress });
            }
        }
    });
    (tx, pump)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::jobs::{JobKind, Jobs};
    use hermes_catalog::CatalogStore;
    use hermes_catalog::install::Installer;
    use tokio_util::sync::CancellationToken;

    fn manager() -> ModelManager {
        let root = std::env::temp_dir().join(format!(
            "hermes-manager-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        ModelManager::new(
            CatalogStore::in_memory(),
            Installer::new(root.join("models"), root.join("downloads")).expect("installer"),
            RuntimeDefaults::default(),
        )
    }

    #[tokio::test]
    async fn a_second_operation_is_refused_while_one_is_running() {
        // One at a time: two clients loading two models at once would race the
        // engine, and two downloads would fight over one partial file.
        //
        // The guard is taken directly rather than by starting a real install.
        // An earlier version of this test raced two installs and, when the
        // first failed fast, the second went to the network and downloaded a
        // 100 MB model *inside a unit test* — the suite promises no network and
        // no model downloads, and the test proved nothing either way.
        let manager = manager();
        let jobs = Jobs::new();
        let job = jobs.start(JobKind::Download, &CancellationToken::new());

        let held = manager
            .operation
            .try_lock()
            .expect("nothing else is running");

        let refused = manager
            .install(
                &AddModel::Pinned {
                    id: "smollm2-135m-instruct-q4_k_m".into(),
                },
                &job,
            )
            .await;
        match refused {
            Err(ManagerError::Busy) => {}
            other => panic!("a concurrent install was not refused: {other:?}"),
        }

        // And it is a wait, not a wall: the next one goes through.
        drop(held);
        assert!(
            manager.operation.try_lock().is_ok(),
            "the operation lock was not released"
        );
    }

    #[tokio::test]
    async fn an_import_of_a_file_that_is_not_there_says_so_without_touching_the_catalog() {
        let manager = manager();
        let jobs = Jobs::new();
        let job = jobs.start(JobKind::Import, &CancellationToken::new());

        let err = manager
            .import(PathBuf::from("/definitely/not/here.gguf"), &job)
            .await
            .expect_err("a missing file must not be imported");
        assert_eq!(err.code(), "model_file_not_found");
        assert!(manager.models().await.is_empty());
    }

    #[test]
    fn a_gateway_with_no_catalog_says_so_rather_than_claiming_to_be_busy() {
        // These are different things, and a client retrying a "busy" that will
        // never clear is the cost of confusing them.
        let err = ManagerError::NoCatalog;
        assert_eq!(err.code(), "no_model_catalog");
        assert_ne!(err.code(), ManagerError::Busy.code());
        assert!(!err.kind().is_retryable());
    }

    #[test]
    fn a_busy_manager_asks_the_caller_to_come_back_rather_than_failing() {
        // Two clients pressing "load" is ordinary. It is a wait, not an error.
        let err = ManagerError::Busy;
        assert_eq!(err.code(), "model_operation_in_progress");
        assert_eq!(err.http_status(), 429);
        assert!(err.kind().is_retryable());
    }

    #[test]
    fn a_stuck_engine_surfaces_rather_than_hanging_the_gateway() {
        let err = ManagerError::DrainTimedOut { seconds: 600 };
        assert_eq!(err.code(), "drain_timed_out");
        assert_eq!(err.http_status(), 503);
        assert!(!err.remedies().is_empty());
    }

    #[test]
    fn a_catalog_error_keeps_its_own_identity_through_the_manager() {
        // The UI branches on these codes; flattening them into one would make
        // "which model?" and "why?" unanswerable.
        let err = ManagerError::from(CatalogError::UnknownModel { id: "x".into() });
        assert_eq!(err.code(), "unknown_model");
        assert_eq!(err.http_status(), 404);

        let err = ManagerError::from(BackendError::NoModelLoaded);
        assert_eq!(err.code(), "no_model_loaded");
    }

    #[test]
    fn a_missing_file_names_the_model_and_the_path() {
        let err = ManagerError::FileMissing {
            id: "qwen3".into(),
            path: PathBuf::from("/models/gone.gguf"),
        };
        let message = err.to_string();
        assert!(message.contains("qwen3"));
        assert!(message.contains("gone.gguf"));
        assert_eq!(err.http_status(), 404);
    }
}
