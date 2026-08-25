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
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use hermes_core::{InstanceId, ModelId, RuntimeParams, units::Bytes};
use hermes_gguf::architecture;
use hermes_inference::generation::GenerationRequest;
use hermes_inference::{
    BackendCapabilities, BackendError, BackendHealth, BackendId, DeviceKind, EngineCounters,
    GenerationStream, InferenceBackend, LoadProgress, LoadRequest, LoadedModel, ResourceSnapshot,
};
// Unconditional again, and only because both arms of `read_process_usage` now
// build one: while the reading was `/proc`-only this import was unused off
// Linux, which `-D warnings` turned into a failed build on two runners.
use hermes_inference::CpuTicks;
use hermes_observability::targets;
use hermes_system_info::CpuInfo;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::acquire::RuntimeInstaller;
use crate::supervisor::{self, ALLOWED_KV_CACHE_TYPES, Engine, EngineConfig, ExitClassification};
use crate::upstream::UpstreamClient;

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
    /// Built once at load rather than per request: it holds a connection pool,
    /// and a new client per token-generating request would open a new TCP
    /// connection for every turn.
    client: UpstreamClient,
    effective: RuntimeParams,
    /// The engine's own `/props`, read once when it became ready.
    ///
    /// Captured rather than fetched per call. A running engine's properties
    /// cannot change - the geometry is fixed at startup and a new geometry is
    /// a new instance - so a live read would put a 30-second upstream timeout
    /// on the endpoint clients use to size a prompt, in exchange for an answer
    /// that is the same every time.
    ///
    /// `None` when the read failed, which is "we could not ask", never "it has
    /// none".
    props: Option<serde_json::Value>,
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
    /// Slots the resident engine was given, and 1 when nothing is resident.
    ///
    /// Separate from `running` because `capabilities()` is synchronous and
    /// `running` is behind an async mutex, which cannot be taken there. An
    /// atomic is the whole of the state it needs: one number, written on a
    /// successful load and cleared when the engine goes away.
    slots: AtomicU32,
}

impl ProcessBackend {
    /// `runtime_root` is where engine builds are installed.
    pub fn new(runtime_root: impl Into<PathBuf>) -> Result<Self, BackendError> {
        Ok(Self {
            installer: RuntimeInstaller::new(runtime_root)?,
            cpu: CpuInfo::detect(),
            start_timeout: DEFAULT_START_TIMEOUT,
            running: Arc::new(Mutex::new(None)),
            slots: AtomicU32::new(1),
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

        // Locking weights means locking all of them, and the kernel enforces a
        // per-process allowance that is commonly far smaller than a model. The
        // engine's own response to exceeding it is a warning on stderr and a
        // model that pages anyway — which looks like success and performs like
        // failure. Checked here instead, where it can be a refusal that names
        // the number.
        if request
            .runtime
            .load_mode
            .is_some_and(hermes_core::LoadMode::locks_weights)
            && let Some(limit) = crate::supervisor::locked_memory_limit()
            && let Some(weights) = request.metadata.weight_bytes
            && weights > limit
        {
            return Err(BackendError::LockedMemoryTooSmall {
                required: Bytes(weights),
                limit: Bytes(limit),
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
            // What the resident engine was actually given, not a ceiling: a
            // number nobody can act on is not a capability. One when nothing
            // is loaded, which is what an engine with no slots can serve.
            max_concurrent_requests: self.slots.load(Ordering::Relaxed),
            build: Some(crate::manifest::PINNED_BUILD.to_owned()),
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
        let requested = RuntimeParams {
            threads: Some(config.threads),
            ..request.runtime
        };

        let client = UpstreamClient::new(engine.base_url(), engine.api_key())?;
        // What the engine says it is running, not what it was asked to run.
        let props = client.props().await.ok();
        let effective = reconciled(requested, props.as_ref());

        tracing::info!(
            target: targets::MODEL,
            model = %request.model,
            instance = %instance,
            n_ctx = effective.n_ctx,
            threads = config.threads,
            variant = self.cpu.expected_ggml_variant(),
            "model loaded"
        );

        self.slots
            .store(effective.n_parallel.max(1), Ordering::Relaxed);

        *self.running.lock().await = Some(Running {
            instance,
            model: request.model.clone(),
            engine,
            client,
            effective,
            props,
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
            // Back to what an engine with no model can serve, so the reported
            // capability never describes a process that is no longer running.
            self.slots.store(1, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn generate(
        &self,
        instance: InstanceId,
        request: GenerationRequest,
        cancel: CancellationToken,
    ) -> Result<GenerationStream, BackendError> {
        // The client is cloned out from under the lock before the request is
        // sent. Holding it across the whole generation would serialize every
        // other operation on the backend behind a completion that can run for
        // minutes - including the health check that would notice the engine
        // dying mid-stream.
        let client = self.client_for(instance).await?;
        client.generate(request, cancel).await
    }

    async fn count_prompt_tokens(
        &self,
        instance: InstanceId,
        request: &GenerationRequest,
    ) -> Result<u32, BackendError> {
        let client = self.client_for(instance).await?;
        client.count_prompt_tokens(request).await
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
        Ok(read_process_usage(pid))
    }

    async fn engine_counters(&self) -> Result<Option<EngineCounters>, BackendError> {
        let Some((base_url, api_key)) = self.engine_endpoint().await else {
            return Ok(None);
        };
        // A scrape failure is not a generation failure: the counters are
        // reported when they can be read and reported as unavailable when they
        // cannot, and neither outcome touches anything a client is waiting on.
        let client = UpstreamClient::new(base_url, api_key)?;
        client.engine_counters().await.map(Some)
    }

    /// Served from the copy taken when the engine became ready.
    ///
    /// See `Running::props`: a running engine's geometry cannot change, so a
    /// live read would put a thirty-second upstream timeout on the endpoint
    /// clients use to size a prompt in exchange for the same answer every time.
    async fn engine_props(&self) -> Option<serde_json::Value> {
        self.running.lock().await.as_ref()?.props.clone()
    }

    async fn shutdown(&self) -> Result<(), BackendError> {
        let mut running = self.running.lock().await;
        if let Some(state) = running.take() {
            let _ = state.engine.shutdown().await;
            self.slots.store(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

impl ProcessBackend {
    /// The upstream client for a resident instance.
    ///
    /// The instance check is the point: a request queued against a model that
    /// has since been unloaded and replaced must fail rather than silently
    /// running against whatever is loaded now.
    async fn client_for(&self, instance: InstanceId) -> Result<UpstreamClient, BackendError> {
        let running = self.running.lock().await;
        let state = running.as_ref().ok_or(BackendError::NoModelLoaded)?;
        if state.instance != instance {
            return Err(BackendError::NoModelLoaded);
        }
        Ok(state.client.clone())
    }

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

/// The parameters as the engine reports them, rather than as it was asked.
///
/// The engine divides `--ctx-size` across its slots, so the arithmetic in
/// `supervisor::engine_context` is a claim about what one client will get. This
/// is where that claim is checked against a number the engine itself published.
/// Computing it a second time from what we asked for would agree by
/// construction and prove nothing.
///
/// Three cases, and the direction to be wrong in is settled by the rule that
/// governs every estimate here - never promise a window that does not exist:
///
/// * **The engine reports less.** Adopt it. One assignment corrects everything
///   downstream at once, because `/props`, `/v1/models`, the overflow check,
///   `clamp_max_tokens` and the band ceilings are all derived from this field.
/// * **The engine reports more.** Ignore it. A larger window is memory nobody
///   budgeted for, and admission control was run against the smaller number.
/// * **Nothing could be read.** Keep what was asked for. The engine has already
///   reported ready and the weights are already resident; losing a working
///   model because a metadata call failed would be the worse answer.
///
/// Refusing the load is deliberately not among them. A refusal is right when a
/// pre-flight can catch the problem before the cost is paid - which is what
/// `admit` does for KV cache types and memory - and wrong once several seconds
/// and several gigabytes have already been spent on something the gateway can
/// simply describe correctly.
fn reconciled(requested: RuntimeParams, props: Option<&serde_json::Value>) -> RuntimeParams {
    let Some(window) = props.and_then(crate::upstream::window_from_props) else {
        tracing::debug!(
            target: targets::MODEL,
            "the engine did not report a context length; recording what it was asked for"
        );
        return requested;
    };
    if window.n_ctx >= requested.n_ctx && window.total_slots >= requested.n_parallel.max(1) {
        return requested;
    }
    tracing::warn!(
        target: targets::MODEL,
        requested_n_ctx = requested.n_ctx,
        engine_n_ctx = window.n_ctx,
        requested_slots = requested.n_parallel.max(1),
        engine_slots = window.total_slots,
        "the engine is serving less than it was asked for; advertising what it has"
    );
    RuntimeParams {
        n_ctx: window.n_ctx.min(requested.n_ctx),
        n_parallel: window.total_slots.min(requested.n_parallel.max(1)),
        ..requested
    }
}

/// Read a process's memory use from the operating system.
///
/// This is a real dividend of the process boundary: the engine's memory is
/// measurable on its own rather than tangled up with the gateway's, which is
/// what lets the RAM estimator be calibrated against observed peak usage rather
/// than staying an estimate forever.
#[cfg(target_os = "linux")]
fn read_process_usage(pid: u32) -> Option<ResourceSnapshot> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let mut usage = parse_process_status(&status)?;
    // Processor time lives in a different file, and its absence is not a
    // reason to withhold the memory reading that did succeed.
    usage.cpu_ticks = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .as_deref()
        .and_then(parse_process_stat);
    Some(usage)
}

/// Pull the memory fields out of a `/proc/<pid>/status` body.
///
/// Split from the file read for the same reason `parse_meminfo` is: a captured
/// sample can then be asserted on exactly, without a live process to read.
#[cfg(target_os = "linux")]
fn parse_process_status(status: &str) -> Option<ResourceSnapshot> {
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
        // `VmHWM` counts every resident page, the mapped weights included.
        peak_kind: hermes_inference::PeakKind::ResidentSet,
        // Absent before Linux 4.5. `None` rather than a default, because a
        // swap spends this number: a zero would be silently correct and an
        // absent reading treated as zero would silently refuse the swap.
        anon_rss: field("RssAnon"),
        // Memory only. Processor time is a second file, read by the caller.
        cpu_ticks: None,
    })
}

/// Pull the processor-time counters out of a `/proc/<pid>/stat` body.
///
/// The file is one line, and the second field is the executable name in
/// parentheses — which may itself contain spaces and parentheses, so the fields
/// are counted from the **last** `)` rather than by splitting the whole line.
/// Splitting naively works for `llama-server` and breaks the day the engine is
/// renamed to something with a space in it, which is exactly the kind of
/// failure that would be read as "the CPU probe is broken on this machine".
///
/// `utime` and `stime` are fields 14 and 15 in `proc(5)`, so they are the
/// twelfth and thirteenth values after the name.
#[cfg(target_os = "linux")]
fn parse_process_stat(stat: &str) -> Option<CpuTicks> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = after_name.split_whitespace().collect();
    Some(CpuTicks {
        user: fields.get(11)?.parse().ok()?,
        system: fields.get(12)?.parse().ok()?,
    })
}

/// The same reading, where there is no `/proc` to read.
///
/// macOS and Windows publish it through one system call each, wrapped in
/// `hermes-system-info` on top of the workspace's only FFI crate - so this is
/// a delegation rather than a second implementation, and the Linux path above
/// keeps the parsers its own tests pin.
///
/// A failure is still `None`, exactly as an unreadable `/proc` entry is: the
/// engine may have exited between the poll and the read, and inventing a
/// number here would corrupt the calibration that consumes it.
#[cfg(not(target_os = "linux"))]
fn read_process_usage(pid: u32) -> Option<ResourceSnapshot> {
    let usage = hermes_system_info::process::usage(pid).ok()?;
    Some(ResourceSnapshot {
        rss: usage.rss,
        peak_rss: usage.peak_rss,
        peak_kind: if usage.peak_is_footprint {
            hermes_inference::PeakKind::Footprint
        } else {
            hermes_inference::PeakKind::ResidentSet
        },
        anon_rss: usage.anon_rss,
        cpu_ticks: Some(CpuTicks {
            user: usage.user_ticks,
            system: usage.system_ticks,
        }),
    })
}

use std::path::Path;

#[cfg(all(test, target_os = "linux"))]
mod status_tests {
    use super::*;

    /// Captured from a running `llama-server` on the development machine.
    const SAMPLE: &str = "\
Name:\tllama-server
State:\tS (sleeping)
Threads:\t4
VmPeak:\t 1843204 kB
VmSize:\t 1712132 kB
VmHWM:\t  804128 kB
VmRSS:\t  803916 kB
RssAnon:\t  198432 kB
RssFile:\t  605484 kB
RssShmem:\t       0 kB
";

    #[test]
    fn a_captured_proc_status_yields_rss_hwm_and_anon() {
        let usage = parse_process_status(SAMPLE).expect("VmRSS is present");
        assert_eq!(usage.rss, Bytes::from_kib(803_916));
        assert_eq!(usage.peak_rss, Bytes::from_kib(804_128));
        assert_eq!(usage.anon_rss, Some(Bytes::from_kib(198_432)));
        // The reason a swap may not credit the whole resident set: three
        // quarters of it here is the mmapped model file, which the kernel
        // already counts as available.
        assert!(
            usage.anon_rss.expect("anon") < usage.rss,
            "an mmapped engine's anonymous set must be the smaller figure"
        );
    }

    #[test]
    fn a_status_without_rssanon_reports_none_not_zero() {
        // Before Linux 4.5 the kernel does not publish it. Zero would be a
        // legitimate reading, so the absence has to be a different answer -
        // otherwise a swap silently declines to credit anything and refuses
        // itself for memory that is about to be free.
        let without = SAMPLE
            .lines()
            .filter(|line| !line.starts_with("RssAnon"))
            .collect::<Vec<_>>()
            .join("\n");
        let usage = parse_process_status(&without).expect("VmRSS is still present");
        assert_eq!(usage.anon_rss, None);
        assert_eq!(usage.rss, Bytes::from_kib(803_916));
    }

    #[test]
    fn a_status_with_no_resident_set_is_not_a_reading_at_all() {
        assert!(parse_process_status("Name:\tllama-server\n").is_none());
    }

    /// Captured from `/proc/<pid>/stat` on the development machine, truncated
    /// after the fields this parser reads.
    const STAT_SAMPLE: &str = "4213 (llama-server) S 4190 4213 4190 0 -1 4194304 \
9821 0 0 0 73142 1885 0 0 20 0 5 0 91544 1798332416 201979";

    #[test]
    fn a_captured_proc_stat_yields_user_and_system_ticks() {
        let ticks = parse_process_stat(STAT_SAMPLE).expect("utime and stime are present");
        assert_eq!(ticks.user, 73_142);
        assert_eq!(ticks.system, 1_885);
        assert_eq!(ticks.total(), 75_027);
    }

    #[test]
    fn an_executable_name_containing_spaces_and_parens_does_not_shift_the_fields() {
        // The second field is the executable name in parentheses and the kernel
        // does not escape it. Counting fields from the start of the line puts
        // `utime` wherever the name's spaces leave it, which is a wrong number
        // rather than a missing one - the worst kind. Counting from the last
        // `)` is what makes this hold.
        let awkward = STAT_SAMPLE.replace("(llama-server)", "(llama server (dl))");
        let ticks = parse_process_stat(&awkward).expect("the fields are still findable");
        assert_eq!(ticks.user, 73_142);
        assert_eq!(ticks.system, 1_885);
    }

    #[test]
    fn a_truncated_stat_line_is_no_reading_rather_than_a_zero() {
        // Zero ticks is a legitimate answer - an engine that has been asked to
        // do nothing yet - so an unreadable file must not be able to produce
        // it, or an idle engine and a broken probe become indistinguishable.
        assert!(parse_process_stat("4213 (llama-server) S 4190 4213").is_none());
        assert!(parse_process_stat("no parenthesis here").is_none());
    }

    #[test]
    fn ticks_between_two_readings_are_the_difference_and_never_negative() {
        let earlier = CpuTicks {
            user: 100,
            system: 10,
        };
        let later = CpuTicks {
            user: 180,
            system: 12,
        };
        assert_eq!(later.since(&earlier), Some(82));
        // Across an engine restart the counters begin again, and a replaced
        // engine did not consume negative processor time.
        assert_eq!(earlier.since(&later), None);
    }
}

#[cfg(test)]
mod reconciliation_tests {
    use super::*;
    use serde_json::json;

    fn asked_for(n_ctx: u32, n_parallel: u32) -> RuntimeParams {
        RuntimeParams {
            n_ctx,
            n_parallel,
            ..RuntimeParams::default()
        }
    }

    fn engine_says(n_ctx: u32, total_slots: u32) -> serde_json::Value {
        json!({
            "default_generation_settings": {"n_ctx": n_ctx},
            "total_slots": total_slots,
        })
    }

    #[test]
    fn an_engine_serving_the_window_it_was_asked_for_changes_nothing() {
        let requested = asked_for(2048, 4);
        let effective = reconciled(requested, Some(&engine_says(2048, 4)));
        assert_eq!(effective, requested);
    }

    #[test]
    fn an_engine_serving_a_smaller_window_is_believed_rather_than_overruled() {
        // The shape of the defect this milestone fixes: ask for 2048 per
        // client, and an engine that divided rather than multiplied hands back
        // 512. What is advertised must be what a client actually has.
        let effective = reconciled(asked_for(2048, 4), Some(&engine_says(512, 4)));
        assert_eq!(effective.n_ctx, 512);
    }

    #[test]
    fn an_engine_claiming_more_than_was_budgeted_for_is_not_believed() {
        // Admission control ran against 2048. A larger window is memory that
        // was never priced, and adopting it would spend it.
        let effective = reconciled(asked_for(2048, 1), Some(&engine_says(8192, 1)));
        assert_eq!(effective.n_ctx, 2048);
    }

    #[test]
    fn fewer_slots_than_were_asked_for_are_recorded_as_the_slots_that_exist() {
        let effective = reconciled(asked_for(2048, 4), Some(&engine_says(2048, 2)));
        assert_eq!(effective.n_parallel, 2);
    }

    #[test]
    fn props_that_could_not_be_read_leave_the_load_standing() {
        // The engine is running and the weights are resident. Losing that over
        // a metadata call is a worse answer than an unverified context.
        let requested = asked_for(4096, 2);
        assert_eq!(reconciled(requested, None), requested);
        assert_eq!(reconciled(requested, Some(&json!({}))), requested);
    }
}
