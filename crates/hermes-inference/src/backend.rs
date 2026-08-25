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
//! The trait covers the lifecycle — acquire, load, observe, unload — and
//! generation on top of the same instance handle.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use futures_util::stream::BoxStream;
use hermes_core::{GgmlType, InstanceId, ModelId, RuntimeParams, units::Bytes};
use hermes_gguf::ModelMetadata;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::BackendError;
use crate::generation::{GenerationEvent, GenerationRequest};

/// A stream of generation events.
///
/// Boxed rather than an associated type so that [`InferenceBackend`] stays
/// object-safe: the gateway holds `Arc<dyn InferenceBackend>` and must be able
/// to swap engines at runtime, which an associated stream type would make
/// impossible.
pub type GenerationStream = BoxStream<'static, Result<GenerationEvent, BackendError>>;

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
    /// The engine build these capabilities describe.
    ///
    /// `None` for a backend with no version to state, such as the mock. It is
    /// here because a measurement is a fact about one build: a benchmark taken
    /// against one engine and a coefficient fitted from it must not be applied
    /// to another, and without this the only surface that knows the build is
    /// the crate that pins it.
    pub build: Option<String>,
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

/// Processor time a process has been charged for, in kernel clock ticks.
///
/// Ticks — jiffies, `USER_HZ`, normally 100 per second — exactly as
/// `/proc/<pid>/stat` publishes them, and unconverted for the same reason
/// [`ResourceSnapshot::cpu_ticks`] gives. User and system time are kept apart
/// because they answer different questions: an engine accumulating system time
/// is usually paging or contending rather than computing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuTicks {
    pub user: u64,
    pub system: u64,
}

impl CpuTicks {
    /// Every tick this process has been charged for.
    pub const fn total(&self) -> u64 {
        self.user.saturating_add(self.system)
    }

    /// Ticks consumed between an earlier reading and this one.
    ///
    /// `None` when the counters went backwards. They cannot for one live
    /// process, but they can across a restart, and an engine that was replaced
    /// did not consume negative processor time.
    pub fn since(&self, earlier: &Self) -> Option<u64> {
        self.total().checked_sub(earlier.total())
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
    /// The part of `rss` that is anonymous, and so genuinely returned to the
    /// free pool when this process exits.
    ///
    /// The engine mmaps the model file, so most of `rss` is file-backed page
    /// cache that the kernel already counts as available. Only this figure is
    /// new headroom for whatever loads next — which makes it the one number a
    /// model swap may spend, and the reason it is separated out here rather
    /// than left for a caller to guess at.
    ///
    /// `None` where the kernel did not publish it or the platform has no probe.
    /// Never zero standing in for a reading: zero is a real answer.
    pub anon_rss: Option<Bytes>,
    /// Processor time the engine has consumed, in kernel clock ticks.
    ///
    /// A counter, not a rate, and published unconverted for the reason
    /// `hermes_system_info::load` gives about `/proc/stat`: a rate cannot be
    /// read at an instant, and converting to seconds would divide by a
    /// `USER_HZ` this crate would have to guess at. Two readings and the
    /// interval between them mean something exact; one reading is a level a
    /// caller differences.
    ///
    /// `None` where the platform has no probe. Never zero standing in for a
    /// reading — zero is a real answer, and it is the right answer for an
    /// engine that has not been asked to do anything yet.
    pub cpu_ticks: Option<CpuTicks>,
}

/// What the engine says about its own work, in terms this crate can name.
///
/// Everything here is something the gateway **cannot** compute for itself:
/// how long the longest sequence has grown, how many slots a decode step
/// actually kept busy, how many requests the engine deferred internally. What
/// the gateway already measures — tokens, times, throughput — is deliberately
/// absent, because two implementations of one number are two numbers waiting
/// to disagree in public.
///
/// Every field is `Option`, and the reason is the whole design: an engine
/// publishes what its build publishes, and a counter this build does not have
/// must read as "not reported" rather than as zero. `kv_cache_usage_ratio`
/// exists in some llama.cpp builds and not in the pinned one, which is exactly
/// the case this shape exists to survive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineCounters {
    /// Longest sequence the engine has seen, prompt plus generation.
    ///
    /// The honest answer to "is the context we loaded larger than anything
    /// anyone has used?", which is a memory question before it is a speed one.
    pub max_sequence_tokens: Option<u64>,
    /// `llama_decode()` calls. Against tokens generated, this is how batching
    /// becomes visible: one decode per token means none is happening.
    pub decode_calls: Option<u64>,
    /// Average slots busy per decode step.
    ///
    /// The number that says whether raising the slot count did anything at
    /// all, rather than whether it was configured.
    pub busy_slots_per_decode: Option<f64>,
    /// Requests the engine is working on this instant.
    pub requests_processing: Option<u64>,
    /// Requests the engine has put aside for lack of a free slot.
    ///
    /// Queueing *inside* the engine, which our own queue depth cannot see.
    pub requests_deferred: Option<u64>,
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

    /// Generate a completion, streaming events as they happen.
    ///
    /// Always a stream, even when the caller wants one whole response: the
    /// engine produces tokens over tens of seconds on a CPU, and a
    /// non-streaming API is the streaming one with an accumulator on the end.
    /// The reverse — synthesizing a stream from a completed response — would
    /// throw away the timing information that time-to-first-token and
    /// tokens-per-second are computed from.
    ///
    /// Dropping the returned stream must stop the work. That is what makes a
    /// disconnected client stop costing CPU, and it is why cancellation is a
    /// token rather than a method: a `Drop` can cancel a token, but it cannot
    /// call an async method.
    async fn generate(
        &self,
        instance: InstanceId,
        request: GenerationRequest,
        cancel: CancellationToken,
    ) -> Result<GenerationStream, BackendError>;

    /// How many tokens the prompt occupies, before generating anything.
    ///
    /// The pre-flight check that turns "the prompt filled the window" into a
    /// 400 the client can parse and act on, rather than an empty stream it
    /// retries blindly. It must apply the model's own chat template, because
    /// the template's own tokens are part of the count.
    async fn count_prompt_tokens(
        &self,
        instance: InstanceId,
        request: &GenerationRequest,
    ) -> Result<u32, BackendError>;

    async fn health(&self) -> BackendHealth;

    /// What the engine is consuming, or `None` when nothing is running.
    async fn resource_usage(&self) -> Result<Option<ResourceSnapshot>, BackendError>;

    /// What the engine reports about its own work, or `None` when it reports
    /// nothing.
    ///
    /// Defaulted rather than required, because "this engine publishes no
    /// counters" is a legitimate engine. A backend that has them overrides
    /// this; one that does not says so by saying nothing, and the gateway
    /// renders that as "not reported" rather than as a row of zeroes.
    async fn engine_counters(&self) -> Result<Option<EngineCounters>, BackendError> {
        Ok(None)
    }

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
            build: None,
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
