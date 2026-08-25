//! What every request handler shares.

use std::sync::Arc;

use hermes_inference::InferenceBackend;
use tokio_util::sync::CancellationToken;

use crate::auth::AuthPolicy;
use crate::catalog::Catalog;
use crate::metrics::{Metrics, MetricsSnapshot, ModelSnapshot};
use crate::scheduler::{Band, Scheduler, SchedulerConfig, SlotPermit, Ticket};
use crate::system::Probed;
use hermes_core::Actionable as _;

/// How the gateway behaves.
#[derive(Clone, Debug)]
pub struct GatewayConfig {
    pub auth: AuthPolicy,
    /// Requests that may run at once.
    ///
    /// One, because the engine serves one and because a second concurrent
    /// generation on a four-core CPU makes both slower than running them in
    /// turn. The number is a *parameter* rather than a constant so that
    /// continuous batching later is configuration and not a rewrite: raise it,
    /// pass `--parallel N` to the engine, and the KV formula already carries
    /// the factor.
    pub max_concurrent_requests: u32,
    /// How long a request may wait for its turn before the client is told to
    /// come back.
    ///
    /// Prefill and decode on this hardware are slow enough that a queued
    /// request can wait minutes; the alternative to waiting is a 503 that
    /// makes the client retry into the same queue.
    pub queue_timeout: std::time::Duration,
    /// How often a queued streamed request is told where it stands.
    ///
    /// The same fifteen seconds as the prefill keep-alive, and for the same
    /// reason: it is well inside the shortest idle timeout worth worrying
    /// about, and it costs a comment frame. A parameter rather than a constant
    /// because a test that has to wait fifteen seconds to observe one notice is
    /// a test that gets deleted.
    pub queue_notice_interval: std::time::Duration,
    /// Who goes next when more than one request is waiting.
    pub scheduler: SchedulerConfig,
    /// Where this gateway is allowed to read and write.
    ///
    /// Optional for the same reason the manager is: a gateway can be built
    /// without one, and every existing test plus the contract suite's mock
    /// gateway does exactly that. When it is absent, the endpoints that would
    /// describe the filesystem say so rather than reporting a zero that would
    /// read as a full disk.
    pub paths: Option<hermes_system_info::DataPaths>,
    /// The addresses this gateway is actually serving on.
    ///
    /// Recorded rather than re-derived. The listeners are bound before the
    /// state is built — deliberately, so a bad address costs milliseconds
    /// instead of a model load — and asking the sockets again later would mean
    /// a second answer to "where are we serving?" that could disagree with the
    /// first. Empty when nobody has said, which is every in-process test.
    pub bound_addresses: Vec<std::net::SocketAddr>,
    /// Where the control panel's built files are, when this deployment has
    /// them.
    ///
    /// `None` means no panel is served and every unmatched path is a 404,
    /// which is what every existing deployment and every test does today.
    pub web_root: Option<std::path::PathBuf>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            auth: AuthPolicy::Disabled,
            max_concurrent_requests: 1,
            queue_timeout: std::time::Duration::from_secs(600),
            queue_notice_interval: crate::stream::KEEP_ALIVE_INTERVAL,
            scheduler: SchedulerConfig::default(),
            paths: None,
            bound_addresses: Vec::new(),
            web_root: None,
        }
    }
}

/// Shared handler state.
pub struct GatewayState {
    pub backend: Arc<dyn InferenceBackend>,
    pub catalog: Arc<Catalog>,
    pub config: GatewayConfig,
    /// Admission control: one permit per concurrent request, and the order in
    /// which waiting requests get one.
    scheduler: Arc<Scheduler>,
    /// What has happened so far, in numbers.
    ///
    /// Shared rather than owned: a request's guard keeps a handle so that it
    /// can report from its own `Drop`, which outlives the handler.
    metrics: Arc<Metrics>,
    /// The catalog and the operations that change it.
    ///
    /// Optional because a gateway can be built without one — the contract
    /// suite and every existing test do exactly that, and a mock backend has
    /// no models to manage. When it is absent the control API reports that
    /// rather than pretending to have an empty catalog.
    manager: Option<Arc<crate::manager::ModelManager>>,
    /// Long operations, watched rather than waited for.
    jobs: Arc<crate::jobs::Jobs>,
    /// Cancelled when the gateway shuts down.
    ///
    /// The root of the cancellation tree: shutdown cancels every job, and each
    /// job's own token is a child, so nothing can outlive the process it
    /// belongs to.
    shutdown: CancellationToken,
    /// How this gateway reads the machine's memory.
    ///
    /// Held rather than constructed where it is needed. `MemoryProbe` has been
    /// a trait since M1 precisely so a test can supply fixed numbers, and
    /// `FixedMemoryProbe` ships for it; the gateway was the only place that
    /// ignored both and built a `SystemMemoryProbe` inline. That made every
    /// admission verdict over HTTP depend on how much RAM the machine running
    /// the test happened to have free.
    memory: Arc<dyn hermes_system_info::MemoryProbe + Send + Sync>,
}

impl std::fmt::Debug for GatewayState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let queue = self.scheduler.snapshot();
        f.debug_struct("GatewayState")
            .field("backend", &self.backend.id())
            .field("config", &self.config)
            .field("running", &queue.running)
            .field("waiting", &queue.waiting)
            .finish()
    }
}

impl GatewayState {
    pub fn new(
        backend: Arc<dyn InferenceBackend>,
        catalog: Arc<Catalog>,
        config: GatewayConfig,
    ) -> Self {
        let scheduler = Scheduler::new(config.max_concurrent_requests, config.scheduler);
        Self {
            backend,
            catalog,
            config,
            scheduler,
            metrics: Arc::new(Metrics::new()),
            manager: None,
            jobs: Arc::new(crate::jobs::Jobs::new()),
            shutdown: CancellationToken::new(),
            memory: Arc::new(hermes_system_info::SystemMemoryProbe),
        }
    }

    /// Attach a model manager, enabling the control API.
    ///
    /// Additive rather than a new constructor: every existing caller builds a
    /// state without one and keeps working unchanged.
    #[must_use]
    pub fn with_manager(mut self, manager: Arc<crate::manager::ModelManager>) -> Self {
        self.manager = Some(manager);
        self
    }

    /// The model manager, when this gateway has one.
    pub fn manager(&self) -> Option<&Arc<crate::manager::ModelManager>> {
        self.manager.as_ref()
    }

    /// Read this gateway's memory through the probe it was given.
    ///
    /// Every admission verdict spends the number this returns, so it is the one
    /// seam a test needs in order to assert on a verdict at all.
    pub fn memory_snapshot(
        &self,
    ) -> Result<hermes_system_info::MemorySnapshot, hermes_system_info::MemoryError> {
        self.memory.snapshot()
    }

    /// The probe itself, for a caller that must read on a blocking thread.
    ///
    /// `Arc` rather than `&dyn`: the model detail reads the header and the
    /// machine together inside one `spawn_blocking`, and a borrow cannot cross
    /// that boundary.
    pub fn memory_probe(&self) -> Arc<dyn hermes_system_info::MemoryProbe + Send + Sync> {
        Arc::clone(&self.memory)
    }

    /// Measure this gateway against a machine of the caller's choosing.
    ///
    /// Additive, in the manner of [`GatewayState::with_manager`]: every
    /// existing caller keeps the real probe without saying so.
    #[must_use]
    pub fn with_memory_probe(
        mut self,
        probe: Arc<dyn hermes_system_info::MemoryProbe + Send + Sync>,
    ) -> Self {
        self.memory = probe;
        self
    }

    /// Where this gateway keeps conversations, when it has a data directory.
    ///
    /// Built on demand rather than held: a store is a path and nothing else, so
    /// there is no state to keep in sync and no second place for the directory
    /// to be recorded.
    pub fn conversations(&self) -> Option<hermes_store::ConversationStore> {
        self.config
            .paths
            .as_ref()
            .map(|paths| hermes_store::ConversationStore::new(paths.conversations_dir()))
    }

    /// Where this gateway keeps benchmark runs, when it has a data directory.
    ///
    /// `paths.benchmarks_dir()` was chosen in M0 and created at every startup
    /// since; this is the first thing that reads or writes it.
    pub fn benchmarks(&self) -> Option<hermes_bench::BenchmarkStore> {
        self.config
            .paths
            .as_ref()
            .map(|paths| hermes_bench::BenchmarkStore::new(paths.benchmarks_dir()))
    }

    /// Where this gateway keeps settings, when it has a data directory.
    pub fn settings_store(&self) -> Option<hermes_store::SettingsStore> {
        self.config
            .paths
            .as_ref()
            .map(|paths| hermes_store::SettingsStore::new(paths.settings_file()))
    }

    pub fn jobs(&self) -> &Arc<crate::jobs::Jobs> {
        &self.jobs
    }

    /// The counters, for the handlers that record into them.
    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    pub fn scheduler(&self) -> &Arc<Scheduler> {
        &self.scheduler
    }

    /// Everything measured so far, read at one instant.
    pub async fn metrics_snapshot(&self) -> MetricsSnapshot {
        let model = self.catalog.resident().await.map(|model| ModelSnapshot {
            id: model.id.to_string(),
            n_ctx: model.n_ctx,
        });
        let limits = self.scheduler.band_limits();
        let (memory, cpu) = self.engine_usage().await;
        self.metrics.snapshot(
            self.scheduler.snapshot(),
            model,
            crate::metrics::BandSnapshot {
                interactive_prompt_tokens: limits.prompt_tokens,
                interactive_output_tokens: limits.output_tokens,
            },
            memory,
            cpu,
            self.engine_counters().await,
        )
    }

    /// What the engine says about its own work, or why that is not knowable.
    ///
    /// A second per-pull reading, and unlike the two above it costs a loopback
    /// request rather than a `/proc` read. That is affordable exactly once per
    /// scrape and is why there is no sampler behind it.
    async fn engine_counters(&self) -> Probed<hermes_inference::EngineCounters> {
        match self.backend.engine_counters().await {
            Ok(Some(reading)) => Probed::Read { reading },
            Ok(None) if !self.backend.health().await.is_ready() => unavailable(
                "no_engine_running",
                "no engine is running, so it is counting nothing",
            ),
            Ok(None) => unavailable(
                "engine_counters_unsupported",
                "this engine publishes no counters of its own",
            ),
            Err(err) => unavailable(err.code(), &err.to_string()),
        }
    }

    /// What the engine process is holding and spending, or why that is not
    /// knowable.
    ///
    /// Read on demand, once per pull. There is no sampler and no retained
    /// reading: `rss` is a level, so a single read is a complete answer, and
    /// the argument against inventing rates from one sample is
    /// `hermes_system_info::load`'s rather than a new one.
    ///
    /// The backend trait returns `Ok(None)` both for "nothing is running" and
    /// for "this platform has no probe". Those need different words on screen,
    /// so they are told apart here by asking the engine whether it is up -
    /// which is cheaper than widening the trait for one caller.
    async fn engine_usage(
        &self,
    ) -> (
        Probed<crate::metrics::EngineMemory>,
        Probed<crate::metrics::EngineCpu>,
    ) {
        match self.backend.resource_usage().await {
            Ok(Some(usage)) => (
                Probed::Read {
                    reading: crate::metrics::EngineMemory {
                        rss: usage.rss,
                        peak_rss: usage.peak_rss,
                        anon_rss: usage.anon_rss,
                    },
                },
                // The memory fields and the processor-time fields come from two
                // different files, so one can be readable while the other is
                // not. Reporting ticks of zero for an engine whose `stat` could
                // not be read would be indistinguishable from an engine that
                // has genuinely done nothing yet.
                match usage.cpu_ticks {
                    Some(ticks) => Probed::Read {
                        reading: crate::metrics::EngineCpu {
                            user_ticks: ticks.user,
                            system_ticks: ticks.system,
                        },
                    },
                    None => Probed::Unavailable {
                        code: "engine_cpu_probe_unsupported",
                        message: "this platform does not publish a process's processor time"
                            .to_owned(),
                    },
                },
            ),
            Ok(None) if !self.backend.health().await.is_ready() => {
                const CODE: &str = "no_engine_running";
                const MESSAGE: &str = "no engine is running, so it is holding nothing";
                (unavailable(CODE, MESSAGE), unavailable(CODE, MESSAGE))
            }
            Ok(None) => (
                Probed::Unavailable {
                    code: "engine_memory_probe_unsupported",
                    message: "this platform does not publish a process's resident set".to_owned(),
                },
                Probed::Unavailable {
                    code: "engine_cpu_probe_unsupported",
                    message: "this platform does not publish a process's processor time".to_owned(),
                },
            ),
            Err(err) => {
                let message = err.to_string();
                (
                    unavailable(err.code(), &message),
                    unavailable(err.code(), &message),
                )
            }
        }
    }

    /// A permit to run one request, waited for up to the queue timeout.
    ///
    /// Returns `None` when the wait ran out, which the caller turns into a 503
    /// with a `Retry-After` rather than a request that hangs forever.
    pub async fn acquire_slot(&self, band: Band) -> Option<SlotPermit> {
        self.scheduler
            .admit(band, self.config.queue_timeout)
            .await
            .ok()
    }

    /// A slot if one is free this instant, and nothing if not.
    ///
    /// The uncontended path. A caller that gets `None` here has learned
    /// something worth telling the client — that it is queued — which is why
    /// this is separate from waiting.
    pub fn try_acquire_slot(&self) -> Option<SlotPermit> {
        self.scheduler.try_admit()
    }

    /// Join the queue, keeping the place so it can be reported and released.
    pub fn enqueue(&self, band: Band) -> Ticket {
        self.scheduler.enqueue(band)
    }

    /// A cancellation token for one job, rooted in the gateway's own.
    ///
    /// Child rather than independent: a shutdown must reach an in-flight
    /// generation, or the process waits on an engine nobody is listening to.
    pub fn job_token(&self) -> CancellationToken {
        self.shutdown.child_token()
    }

    /// The token that cancels everything.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Stop accepting work and cancel what is running.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }
}

/// One unavailability, for readings of two different shapes.
///
/// A closure would infer a single concrete `T` from its first use, and these
/// two readings come from the same probe failing once: saying so twice is the
/// honest report, and a generic function is what lets it be said twice.
fn unavailable<T>(code: &'static str, message: &str) -> Probed<T> {
    Probed::Unavailable {
        code,
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_backend_mock::MockBackend;

    fn state(config: GatewayConfig) -> GatewayState {
        GatewayState::new(
            Arc::new(MockBackend::default()),
            crate::catalog::shared(None),
            config,
        )
    }

    #[tokio::test]
    async fn one_request_runs_at_a_time_by_default() {
        let state = state(GatewayConfig::default());
        let first = state.acquire_slot(Band::Bulk).await.expect("first permit");
        assert_eq!(state.scheduler().snapshot().running, 1);
        drop(first);
        assert_eq!(state.scheduler().snapshot().running, 0);
    }

    #[tokio::test]
    async fn a_queued_request_gives_up_rather_than_hanging_forever() {
        let state = state(GatewayConfig {
            queue_timeout: std::time::Duration::from_millis(20),
            ..GatewayConfig::default()
        });
        let _held = state.acquire_slot(Band::Bulk).await.expect("first permit");
        assert!(
            state.acquire_slot(Band::Bulk).await.is_none(),
            "the second request must time out rather than wait forever"
        );
    }

    #[tokio::test]
    async fn a_shutdown_cancels_jobs_that_are_already_running() {
        // Otherwise the process waits on a generation whose client is gone.
        let state = state(GatewayConfig::default());
        let job = state.job_token();
        assert!(!job.is_cancelled());
        state.shutdown();
        assert!(job.is_cancelled());
    }

    #[tokio::test]
    async fn raising_the_concurrency_is_configuration_not_a_rewrite() {
        // The whole point of keeping this a parameter: continuous batching
        // later must not need a new type or a new caller.
        let state = state(GatewayConfig {
            max_concurrent_requests: 4,
            ..GatewayConfig::default()
        });
        let mut permits = Vec::new();
        for _ in 0..4 {
            permits.push(state.acquire_slot(Band::Bulk).await.expect("permit"));
        }
        assert_eq!(state.scheduler().snapshot().running, 4);
    }

    #[tokio::test]
    async fn the_metrics_snapshot_describes_a_gateway_that_has_done_nothing() {
        // A fresh gateway must read as empty rather than as fast: a mean over
        // no samples is not zero.
        let state = state(GatewayConfig::default());
        let snapshot = state.metrics_snapshot().await;
        assert_eq!(snapshot.generations, 0);
        assert_eq!(snapshot.queue.capacity, 1);
        assert!(snapshot.decode_tokens_per_second().is_none());
        assert!(snapshot.time_to_first_token.mean_ms().is_none());
    }
}
