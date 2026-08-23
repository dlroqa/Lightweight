//! [`InferenceBackend`] implemented over a supervised llama.cpp process.
//!
//! Everything the caller cannot do for itself happens here: acquiring the
//! engine, admitting or refusing a load, launching, and cleaning up. Everything
//! the caller *should* do for itself — deciding whether a model fits in memory,
//! choosing a context length, picking sampling parameters — stays above the
//! trait, so that a future backend inherits those policies rather than
//! reimplementing them.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hermes_core::{InstanceId, ModelId, RuntimeParams, units::Bytes};
use hermes_gguf::architecture;
use hermes_inference::{
    BackendCapabilities, BackendError, BackendHealth, BackendId, DeviceKind, InferenceBackend,
    LoadProgress, LoadRequest, LoadedModel, ResourceSnapshot,
};
use hermes_observability::targets;
use hermes_system_info::CpuInfo;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::acquire::RuntimeInstaller;
use crate::supervisor::{self, ALLOWED_KV_CACHE_TYPES, Engine, EngineConfig, ExitClassification};

/// Identifies this backend.
pub const BACKEND_ID: BackendId = BackendId("llamacpp-process");

/// Default ceiling on how long a model may take to become ready.
///
/// Deliberately generous. On the development machine — four slow cores, a
/// spinning-rust-speed disk and no AVX — a 3B model read cold takes tens of
/// seconds, and a timeout that fires during a normal load is a worse failure
/// than one that never fires.
const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(300);

/// A model currently resident in a running engine.
#[derive(Debug)]
struct Running {
    instance: InstanceId,
    model: ModelId,
    engine: Engine,
    effective: RuntimeParams,
    loaded_at: SystemTime,
}

/// Runs models in a supervised llama.cpp child process.
#[derive(Debug)]
pub struct ProcessBackend {
    installer: RuntimeInstaller,
    cpu: CpuInfo,
    start_timeout: Duration,
    /// At most one resident model. Section 22 says to serialize first and add
    /// batching later; the trait's shape does not change when that happens.
    running: Arc<Mutex<Option<Running>>>,
}

impl ProcessBackend {
    /// `runtime_root` is where engine builds are installed.
    pub fn new(runtime_root: impl Into<PathBuf>) -> Result<Self, BackendError> {
        Ok(Self {
            installer: RuntimeInstaller::new(runtime_root)?,
            cpu: CpuInfo::detect(),
            start_timeout: DEFAULT_START_TIMEOUT,
            running: Arc::new(Mutex::new(None)),
        })
    }

    pub fn with_start_timeout(mut self, timeout: Duration) -> Self {
        self.start_timeout = timeout;
        self
    }

    /// The private base URL of the running engine, if there is one.
    ///
    /// Used by the generation layer. Not part of [`InferenceBackend`]: a caller
    /// that could reach the engine's URL could talk to llama.cpp directly, and
    /// the boundary would stop being one.
    pub async fn engine_endpoint(&self) -> Option<(String, String)> {
        let running = self.running.lock().await;
        running
            .as_ref()
            .map(|state| (state.engine.base_url(), state.engine.api_key().to_owned()))
    }

    /// Reject a load we already know the engine cannot serve.
    ///
    /// All three checks happen before anything is launched, so the failure is a
    /// structured error naming the alternatives rather than an engine that
    /// starts and then exits with a message on stderr.
    fn admit(&self, request: &LoadRequest) -> Result<(), BackendError> {
        let metadata = &request.metadata;

        if !metadata.supported {
            return Err(BackendError::UnsupportedArchitecture {
                found: metadata.architecture.clone(),
                supported: architecture::nearest(&metadata.architecture, 8)
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            });
        }

        if let Some(model_max) = metadata.context_length {
            let model_max = u32::try_from(model_max).unwrap_or(u32::MAX);
            if request.runtime.n_ctx > model_max {
                return Err(BackendError::InvalidContextLength {
                    requested: request.runtime.n_ctx,
                    max: model_max,
                });
            }
        }

        for cache_type in [request.runtime.cache_type_k, request.runtime.cache_type_v] {
            if !ALLOWED_KV_CACHE_TYPES.contains(&cache_type.name()) {
                return Err(BackendError::UnsupportedKvCacheType {
                    requested: cache_type.name().to_owned(),
                    supported: ALLOWED_KV_CACHE_TYPES
                        .iter()
                        .map(|name| (*name).to_owned())
                        .collect(),
                });
            }
        }

        if !request.gguf_path.is_file() {
            return Err(BackendError::ModelNotFound {
                path: request.gguf_path.clone(),
            });
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl InferenceBackend for ProcessBackend {
    fn id(&self) -> BackendId {
        BACKEND_ID
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            device: DeviceKind::Cpu,
            streaming: true,
            tool_calls: true,
            reasoning_content: true,
            // One at a time for now. Section 22: serialize first, and design so
            // continuous batching is an internal change later.
            max_concurrent_requests: 1,
            kv_cache_types: ALLOWED_KV_CACHE_TYPES
                .iter()
                .filter_map(|name| hermes_core::GgmlType::from_name(name))
                .collect(),
        }
    }

    /// # Progress reporting
    ///
    /// Every update is sent with `try_send`, never `send().await`. Progress is
    /// advisory, and a caller that stops draining it — a closed UI, a test that
    /// holds a receiver it never reads — must not be able to stall a model
    /// load. It could: a cold start fills the channel from the download loop,
    /// and the next blocking send then waits forever. Dropping an update the
    /// nobody is reading is always the right trade.
    async fn load(
        &self,
        request: LoadRequest,
        progress: mpsc::Sender<LoadProgress>,
        cancel: CancellationToken,
    ) -> Result<LoadedModel, BackendError> {
        self.admit(&request)?;

        let server_path = self.installer.ensure(&progress, &cancel).await?;
        if cancel.is_cancelled() {
            return Err(BackendError::Cancelled);
        }

        // Replace whatever is resident. Done before launching rather than
        // after, because two engines holding two models at once is exactly the
        // memory spike admission control exists to prevent.
        {
            let mut running = self.running.lock().await;
            if let Some(previous) = running.take() {
                tracing::info!(
                    target: targets::MODEL,
                    model = %previous.model,
                    "unloading to make room"
                );
                let _ = previous.engine.shutdown().await;
            }
        }

        let _ = progress.try_send(LoadProgress::StartingEngine);

        let config = EngineConfig {
            server_path,
            install_dir: self
                .installer
                .server_path()?
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            model_path: request.gguf_path.clone(),
            params: request.runtime,
            threads: request
                .runtime
                .threads
                .unwrap_or_else(|| self.cpu.default_threads()),
            start_timeout: self.start_timeout,
        };

        let _ = progress.try_send(LoadProgress::LoadingWeights);
        let engine = supervisor::start(&config, &cancel).await?;

        let instance = InstanceId::new();
        let loaded_at = SystemTime::now();
        // The engine may clamp what it was asked for, so record the thread
        // count actually used rather than the request.
        let effective = RuntimeParams {
            threads: Some(config.threads),
            ..request.runtime
        };

        tracing::info!(
            target: targets::MODEL,
            model = %request.model,
            instance = %instance,
            n_ctx = effective.n_ctx,
            threads = config.threads,
            variant = self.cpu.expected_ggml_variant(),
            "model loaded"
        );

        *self.running.lock().await = Some(Running {
            instance,
            model: request.model.clone(),
            engine,
            effective,
            loaded_at,
        });

        let _ = progress.try_send(LoadProgress::Ready);

        Ok(LoadedModel {
            model: request.model,
            backend: BACKEND_ID,
            instance,
            effective,
            loaded_at,
        })
    }

    async fn unload(&self, instance: InstanceId) -> Result<(), BackendError> {
        let mut running = self.running.lock().await;
        // Idempotent on purpose: a caller recovering from a crash should not
        // have to know whether the instance is already gone.
        let Some(state) = running.as_ref() else {
            return Ok(());
        };
        if state.instance != instance {
            return Ok(());
        }
        if let Some(state) = running.take() {
            tracing::info!(target: targets::MODEL, model = %state.model, "unloading");
            let _ = state.engine.shutdown().await;
        }
        Ok(())
    }

    async fn health(&self) -> BackendHealth {
        let mut running = self.running.lock().await;
        let Some(state) = running.as_mut() else {
            return BackendHealth::Stopped;
        };
        match state.engine.poll_exit() {
            ExitClassification::Running => BackendHealth::Ready,
            exit => {
                let tail = state.engine.drained_stderr_tail().await;
                let detail = exit.into_error(tail).to_string();
                // A dead engine is not a resident model. Clearing it here means
                // the next load starts cleanly instead of inheriting a corpse.
                *running = None;
                BackendHealth::Failed { detail }
            }
        }
    }

    async fn resource_usage(&self) -> Result<Option<ResourceSnapshot>, BackendError> {
        let running = self.running.lock().await;
        let Some(state) = running.as_ref() else {
            return Ok(None);
        };
        let Some(pid) = state.engine.pid() else {
            return Ok(None);
        };
        Ok(read_process_memory(pid))
    }

    async fn shutdown(&self) -> Result<(), BackendError> {
        let mut running = self.running.lock().await;
        if let Some(state) = running.take() {
            let _ = state.engine.shutdown().await;
        }
        Ok(())
    }
}

impl ProcessBackend {
    /// The resident model, if any.
    pub async fn resident(&self) -> Option<(ModelId, InstanceId, RuntimeParams, SystemTime)> {
        let running = self.running.lock().await;
        running.as_ref().map(|state| {
            (
                state.model.clone(),
                state.instance,
                state.effective,
                state.loaded_at,
            )
        })
    }
}

/// Read a process's memory use from the operating system.
///
/// This is a real dividend of the process boundary: the engine's memory is
/// measurable on its own rather than tangled up with the gateway's, which is
/// what lets the RAM estimator be calibrated against observed peak usage rather
/// than staying an estimate forever.
#[cfg(target_os = "linux")]
fn read_process_memory(pid: u32) -> Option<ResourceSnapshot> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let field = |name: &str| -> Option<Bytes> {
        status.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name)
                .then(|| value.split_whitespace().next()?.parse::<u64>().ok())
                .flatten()
                .map(Bytes::from_kib)
        })
    };
    Some(ResourceSnapshot {
        rss: field("VmRSS")?,
        // The high-water mark is the number that matters for calibration: the
        // peak is what would have triggered an OOM kill, not the current value.
        peak_rss: field("VmHWM").unwrap_or_default(),
        cpu_percent: None,
    })
}

#[cfg(not(target_os = "linux"))]
fn read_process_memory(_pid: u32) -> Option<ResourceSnapshot> {
    // Only implemented for Linux so far. Reporting `None` is honest; inventing
    // a number here would corrupt the estimator calibration that consumes it.
    None
}

use std::path::Path;
