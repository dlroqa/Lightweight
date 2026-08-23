//! The inference backend contract.
//!
//! Everything above this line — the gateway, the scheduler, the model manager,
//! the UI — talks to an engine only through [`InferenceBackend`]. Spec sections
//! 28 and 37 ask for exactly that, so that llama.cpp can eventually be replaced
//! by a proprietary Hermes runtime without changes above, and section 29 asks
//! that a future CUDA or Metal backend fits the same shape while only the CPU
//! one is enabled today.
//!
//! Two things the trait deliberately does not expose: the engine's process or
//! handle, and its wire format. A caller that could reach either would couple
//! itself to llama.cpp, and the boundary would stop being a boundary.
//!
//! This milestone defines the **lifecycle** half — acquire, load, observe,
//! unload. Generation is added next, on top of the same instance handle.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use hermes_core::{GgmlType, InstanceId, ModelId, RuntimeParams, units::Bytes};
use hermes_gguf::ModelMetadata;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::BackendError;

/// Names a backend implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BackendId(pub &'static str);

impl std::fmt::Display for BackendId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// Where inference runs.
///
/// Only [`DeviceKind::Cpu`] is enabled. The others exist so that the type does
/// not have to change when section 29's future backends arrive — they are
/// reserved, not implemented.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Cpu,
    Cuda,
    Metal,
    Rocm,
}

/// What a backend can do.
///
/// Callers branch on this rather than on [`BackendId`]. Matching on the
/// identity of the engine is how an abstraction quietly stops being one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackendCapabilities {
    pub device: DeviceKind,
    pub streaming: bool,
    pub tool_calls: bool,
    pub reasoning_content: bool,
    /// Requests that can run at once. One today; the scheduler is written so
    /// that raising it is an internal change.
    pub max_concurrent_requests: u32,
    /// KV cache element types the engine accepts.
    ///
    /// Not every ggml type is valid here: the pinned llama.cpp build accepts
    /// nine of them and rejects the rest at startup. Advertising the real set
    /// lets a bad choice be refused with a list of alternatives instead of
    /// failing as an opaque engine exit.
    pub kv_cache_types: Vec<GgmlType>,
}

/// A request to make a model resident.
#[derive(Clone, Debug)]
pub struct LoadRequest {
    pub model: ModelId,
    pub gguf_path: PathBuf,
    /// Already parsed by us.
    ///
    /// Passed in rather than re-read by the backend so that policy decisions —
    /// which architectures are allowed, what context is permitted, whether the
    /// model fits in memory — stay above the boundary and identical for every
    /// backend.
    pub metadata: Arc<ModelMetadata>,
    pub runtime: RuntimeParams,
}

/// A resident model.
#[derive(Clone, Debug)]
pub struct LoadedModel {
    pub model: ModelId,
    pub backend: BackendId,
    pub instance: InstanceId,
    /// What the engine **actually** did.
    ///
    /// May differ from what was asked: an engine can clamp a context or round a
    /// batch size. The UI shows these, never the requested values, so that what
    /// is displayed is what is running.
    pub effective: RuntimeParams,
    pub loaded_at: SystemTime,
}

/// Progress while a model is being made resident.
///
/// Loading a 3B model from a cold page cache on a slow disk takes tens of
/// seconds. A spinner for that long reads as a hang, so the stages are reported
/// as they happen.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum LoadProgress {
    /// Fetching the engine itself, the first time only.
    AcquiringRuntime {
        downloaded: u64,
        total: Option<u64>,
    },
    VerifyingRuntime,
    StartingEngine,
    /// The engine is reading the model.
    LoadingWeights,
    Ready,
}

/// Whether the engine is usable right now.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BackendHealth {
    /// No engine process is running. Normal when no model is loaded.
    Stopped,
    Starting,
    Ready,
    /// Running but not accepting work, for example still loading weights.
    Busy {
        detail: String,
    },
    Failed {
        detail: String,
    },
}

impl BackendHealth {
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// What the engine is consuming.
///
/// Read from the operating system rather than estimated. This is a real benefit
/// of the process boundary: the engine's memory is measurable on its own
/// instead of being tangled up with ours, which is also how the RAM estimator
/// gets calibrated against observed peak usage.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub rss: Bytes,
    /// High-water mark since the process started.
    pub peak_rss: Bytes,
    /// CPU use since the previous sample, as a percentage of one core.
    pub cpu_percent: Option<f64>,
}

/// An inference engine.
#[async_trait::async_trait]
pub trait InferenceBackend: Send + Sync + 'static {
    fn id(&self) -> BackendId;

    fn capabilities(&self) -> BackendCapabilities;

    /// Make a model resident, reporting progress as it goes.
    ///
    /// Cancelling must leave nothing behind: no half-started engine, no orphan
    /// process holding a gigabyte of weights.
    async fn load(
        &self,
        request: LoadRequest,
        progress: mpsc::Sender<LoadProgress>,
        cancel: CancellationToken,
    ) -> Result<LoadedModel, BackendError>;

    /// Release a resident model.
    ///
    /// Idempotent: unloading an instance that is already gone succeeds, so a
    /// caller recovering from a crash does not have to distinguish the cases.
    async fn unload(&self, instance: InstanceId) -> Result<(), BackendError>;

    async fn health(&self) -> BackendHealth;

    /// What the engine is consuming, or `None` when nothing is running.
    async fn resource_usage(&self) -> Result<Option<ResourceSnapshot>, BackendError>;

    /// Stop everything and leave no child processes behind.
    async fn shutdown(&self) -> Result<(), BackendError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_cpu_device_is_enabled_in_this_release() {
        // Section 29: keep the abstraction ready for GPU backends, but ship
        // only the CPU one.
        let caps = BackendCapabilities {
            device: DeviceKind::Cpu,
            streaming: true,
            tool_calls: true,
            reasoning_content: true,
            max_concurrent_requests: 1,
            kv_cache_types: vec![GgmlType::F16],
        };
        assert_eq!(caps.device, DeviceKind::Cpu);
    }

    #[test]
    fn health_states_serialize_with_their_detail() {
        let json = serde_json::to_value(BackendHealth::Failed {
            detail: "engine exited".into(),
        })
        .expect("serialize");
        assert_eq!(json["state"], "failed");
        assert_eq!(json["detail"], "engine exited");
        assert!(!BackendHealth::Starting.is_ready());
        assert!(BackendHealth::Ready.is_ready());
    }

    #[test]
    fn load_progress_reports_a_download_before_a_start() {
        // The first launch has to fetch the engine, which is the slowest step
        // and the one most likely to be mistaken for a hang.
        let json = serde_json::to_value(LoadProgress::AcquiringRuntime {
            downloaded: 1024,
            total: Some(16_369_239),
        })
        .expect("serialize");
        assert_eq!(json["stage"], "acquiring_runtime");
        assert_eq!(json["downloaded"], 1024);
    }
}
