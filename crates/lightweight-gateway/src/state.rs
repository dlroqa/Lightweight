//! What every request handler shares.

use std::sync::Arc;

use lightweight_inference::InferenceBackend;
use tokio_util::sync::CancellationToken;

use crate::auth::AuthPolicy;
use crate::catalog::Catalog;
use crate::metrics::{Metrics, MetricsSnapshot, ModelSnapshot};
use crate::scheduler::{Band, PeerKey, Scheduler, SchedulerConfig, SlotPermit, Ticket};
use crate::system::Probed;
use lightweight_core::Actionable as _;

/// How the gateway behaves.
#[derive(Clone, Debug)]
pub struct GatewayConfig {
    pub auth: AuthPolicy,
    /// Whether a fronting proxy's forwarded client IP (`CF-Connecting-IP`) is
    /// trusted. On only when the gateway was started with `--behind-proxy`; see
    /// [`crate::TrustForwarded`] and [`crate::scheduler::PeerKey::resolve`].
    pub trust_forwarded: bool,
    /// Whether this gateway enforces named API keys, and so reloads them from
    /// the store when they change at runtime. True for an exposed or proxied
    /// bind; false for a plain loopback bind, which serves its local clients
    /// without a key and must keep doing so.
    pub manages_keys: bool,
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
    pub paths: Option<lightweight_system_info::DataPaths>,
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
            trust_forwarded: false,
            manages_keys: false,
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
    /// The live authentication policy.
    ///
    /// Seeded from `config.auth` at startup and swapped by [`refresh_keys`] when
    /// a key is created, re-limited or revoked at runtime, so what a request is
    /// checked against is always what is currently stored — `config.auth` is the
    /// snapshot the process began with and must not be read on the request path.
    /// An `Arc` inside the lock so a reader clones a handle under a moment's lock
    /// and then checks credentials without holding it.
    ///
    /// [`refresh_keys`]: GatewayState::refresh_keys
    auth: std::sync::RwLock<Arc<AuthPolicy>>,
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
    memory: Arc<dyn lightweight_system_info::MemoryProbe + Send + Sync>,
    /// Per-key request limits, tracked live. Empty and idle until a named key
    /// with a ceiling is used; loopback and anonymous callers never touch it.
    limiter: Arc<crate::limits::RateLimiter>,
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
        let auth = std::sync::RwLock::new(Arc::new(config.auth.clone()));
        Self {
            backend,
            catalog,
            auth,
            config,
            scheduler,
            metrics: Arc::new(Metrics::new()),
            manager: None,
            jobs: Arc::new(crate::jobs::Jobs::new()),
            shutdown: CancellationToken::new(),
            memory: Arc::new(lightweight_system_info::SystemMemoryProbe),
            limiter: Arc::new(crate::limits::RateLimiter::new()),
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
    ) -> Result<lightweight_system_info::MemorySnapshot, lightweight_system_info::MemoryError> {
        self.memory.snapshot()
    }

    /// The probe itself, for a caller that must read on a blocking thread.
    ///
    /// `Arc` rather than `&dyn`: the model detail reads the header and the
    /// machine together inside one `spawn_blocking`, and a borrow cannot cross
    /// that boundary.
    pub fn memory_probe(&self) -> Arc<dyn lightweight_system_info::MemoryProbe + Send + Sync> {
        Arc::clone(&self.memory)
    }

    /// Measure this gateway against a machine of the caller's choosing.
    ///
    /// Additive, in the manner of [`GatewayState::with_manager`]: every
    /// existing caller keeps the real probe without saying so.
    #[must_use]
    pub fn with_memory_probe(
        mut self,
        probe: Arc<dyn lightweight_system_info::MemoryProbe + Send + Sync>,
    ) -> Self {
        self.memory = probe;
        self
    }

    /// Where this gateway keeps conversations, when it has a data directory.
    ///
    /// Built on demand rather than held: a store is a path and nothing else, so
    /// there is no state to keep in sync and no second place for the directory
    /// to be recorded.
    pub fn conversations(&self) -> Option<lightweight_store::ConversationStore> {
        self.config
            .paths
            .as_ref()
            .map(|paths| lightweight_store::ConversationStore::new(paths.conversations_dir()))
    }

    /// Where this gateway keeps benchmark runs, when it has a data directory.
    ///
    /// `paths.benchmarks_dir()` was chosen in M0 and created at every startup
    /// since; this is the first thing that reads or writes it.
    pub fn benchmarks(&self) -> Option<lightweight_bench::BenchmarkStore> {
        self.config
            .paths
            .as_ref()
            .map(|paths| lightweight_bench::BenchmarkStore::new(paths.benchmarks_dir()))
    }

    /// This gateway's engine, in the form a benchmark record and a calibration
    /// are keyed by.
    ///
    /// One definition, because a benchmark taken through this gateway and a fit
    /// looked up by it have to agree about what "the same engine" means. The
    /// build is stated by the backend rather than guessed here, which is what
    /// makes a run taken by `hermes bench` comparable with one taken through
    /// the API.
    pub fn engine_fingerprint(&self) -> lightweight_bench::EngineFingerprint {
        lightweight_bench::engine_fingerprint(self.backend.as_ref())
    }

    /// Everything needed to look a calibration up, owned.
    ///
    /// Owned because one of its two callers runs inside `spawn_blocking` -
    /// reading a header and probing memory both block - and cannot hold a
    /// borrow of the state across it.
    pub fn calibration_lookup(&self) -> CalibrationLookup {
        CalibrationLookup {
            path: self
                .config
                .paths
                .as_ref()
                .map(lightweight_system_info::DataPaths::calibration_file),
            engine: self.engine_fingerprint(),
        }
    }

    /// The estimator to use for one load, and what the calibration file had to
    /// say about it.
    ///
    /// Calibrated only when this machine, this engine build and these exact
    /// settings have a fit that passes every rule in `lightweight_bench::apply`;
    /// otherwise the headless defaults this gateway has always used. Never an
    /// error: a damaged calibration file may not cost somebody their model.
    pub fn estimator_for(
        &self,
        metadata: &lightweight_gguf::ModelMetadata,
        params: lightweight_core::RuntimeParams,
    ) -> (lightweight_memory::Estimator, lightweight_bench::Outcome) {
        self.calibration_lookup().estimator_for(metadata, params)
    }

    /// Where this gateway keeps settings, when it has a data directory.
    pub fn settings_store(&self) -> Option<lightweight_store::SettingsStore> {
        self.config
            .paths
            .as_ref()
            .map(|paths| lightweight_store::SettingsStore::new(paths.settings_file()))
    }

    pub fn api_config_store(&self) -> Option<lightweight_store::ApiConfigStore> {
        self.config
            .paths
            .as_ref()
            .map(|paths| lightweight_store::ApiConfigStore::new(paths.api_config_file()))
    }

    pub fn api_keys_store(&self) -> Option<lightweight_store::ApiKeyStore> {
        self.config
            .paths
            .as_ref()
            .map(|paths| lightweight_store::ApiKeyStore::new(paths.api_keys_file()))
    }

    /// The authentication policy to check this request against.
    ///
    /// The live one, not the startup snapshot in `config.auth`: a key created,
    /// re-limited or revoked at runtime is reflected here on the next request.
    /// A read clones the `Arc` under a brief lock and releases it, so the
    /// constant-time credential comparison runs without the lock held.
    pub fn auth_policy(&self) -> Arc<AuthPolicy> {
        Arc::clone(
            &self
                .auth
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Rebuild the named-key set from the store so a runtime change takes hold.
    ///
    /// Called after a successful create, limit change or revocation. A no-op on
    /// a gateway that does not manage keys — a plain loopback bind serves local
    /// clients without one and must keep doing so — and on a gateway with no
    /// data directory, which has no store to read. The static key is preserved;
    /// only the store-backed named set is replaced. See [`AuthPolicy::reloaded`].
    ///
    /// The read is a small JSON file and this runs only on an administrative
    /// request, never the generation path, so it is done inline rather than
    /// behind a blocking task.
    pub fn refresh_keys(&self) {
        if !self.config.manages_keys {
            return;
        }
        let Some(store) = self.api_keys_store() else {
            return;
        };
        let Ok(named) = store.list() else {
            // A store that could not be read leaves the current policy in place:
            // the live keys stay valid rather than every request suddenly
            // failing because one reload hit a transient error.
            return;
        };
        let refreshed = Arc::new(self.auth_policy().reloaded(named));
        *self
            .auth
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = refreshed;
    }

    pub fn jobs(&self) -> &Arc<crate::jobs::Jobs> {
        &self.jobs
    }

    /// The counters, for the handlers that record into them.
    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    pub fn limiter(&self) -> &Arc<crate::limits::RateLimiter> {
        &self.limiter
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
    async fn engine_counters(&self) -> Probed<lightweight_inference::EngineCounters> {
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
    /// `lightweight_system_info::load`'s rather than a new one.
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
    pub async fn acquire_slot(&self, band: Band, peer: PeerKey) -> Option<SlotPermit> {
        self.scheduler
            .admit(band, peer, self.config.queue_timeout)
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
    pub fn enqueue(&self, band: Band, peer: PeerKey) -> Ticket {
        self.scheduler.enqueue(band, peer)
    }

    /// A slot if one is free, and a place in the queue if not — decided at one
    /// instant, so a slot released between the two cannot go idle.
    pub fn admit_or_enqueue(&self, band: Band, peer: PeerKey) -> Result<SlotPermit, Ticket> {
        self.scheduler.admit_or_enqueue(band, peer)
    }

    /// Wait for a ticket's turn, up to the queue timeout.
    ///
    /// `None` is a wait that ran out, which the caller turns into a 503. The
    /// ticket is told so before it is dropped, or its departure would be
    /// counted as a client that walked away.
    pub async fn wait_for_slot(&self, mut ticket: Ticket) -> Option<SlotPermit> {
        match tokio::time::timeout(self.config.queue_timeout, ticket.granted()).await {
            Ok(permit) => permit,
            Err(_) => {
                ticket.timed_out();
                None
            }
        }
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

/// Where this gateway's fitted coefficients live, and which engine they must
/// describe.
///
/// A value rather than two arguments, because the pair is meaningless split up:
/// a fit found by path and applied to a different engine build is exactly the
/// confident wrong number `lightweight_bench::apply` refuses.
#[derive(Clone, Debug)]
pub struct CalibrationLookup {
    path: Option<std::path::PathBuf>,
    engine: lightweight_bench::EngineFingerprint,
}

impl CalibrationLookup {
    /// The estimator for one load, calibrated when this machine has earned it.
    ///
    /// A gateway with no data directory has nowhere to have written a fit, so
    /// it reports the same `NoFit` an empty file would: the shipped defaults
    /// stand, which is what every estimate did before M10.
    pub fn estimator_for(
        &self,
        metadata: &lightweight_gguf::ModelMetadata,
        params: lightweight_core::RuntimeParams,
    ) -> (lightweight_memory::Estimator, lightweight_bench::Outcome) {
        let base = lightweight_memory::ComputeModel::headless();
        let Some(path) = self.path.as_ref() else {
            return (
                lightweight_memory::Estimator::new(base),
                lightweight_bench::Outcome::Rejected(lightweight_bench::Untrusted::NoFit),
            );
        };
        lightweight_bench::estimator_for(path, &self.engine, metadata, params, base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightweight_backend_mock::MockBackend;

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
        let first = state
            .acquire_slot(Band::Bulk, PeerKey::default())
            .await
            .expect("first permit");
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
        let _held = state
            .acquire_slot(Band::Bulk, PeerKey::default())
            .await
            .expect("first permit");
        assert!(
            state
                .acquire_slot(Band::Bulk, PeerKey::default())
                .await
                .is_none(),
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
            permits.push(
                state
                    .acquire_slot(Band::Bulk, PeerKey::default())
                    .await
                    .expect("permit"),
            );
        }
        assert_eq!(state.scheduler().snapshot().running, 4);
    }

    #[test]
    fn refreshing_keys_tracks_the_store_so_the_limit_enforced_is_the_limit_set() {
        // The bug this closes: auth was a startup snapshot, so a key created,
        // re-limited or revoked on a running gateway had no effect until
        // restart. After each store change the gateway rebuilds its named set,
        // so what a request is checked against is always what is stored now.
        use lightweight_store::RateLimit;

        let root = std::env::temp_dir().join(format!(
            "hermes-state-refresh-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let paths = lightweight_system_info::DataPaths::rooted_at(&root);
        paths.create_all().expect("create data dirs");

        // Starts Required via a static key (so it is enabled), managing keys,
        // with no named keys yet.
        let state = state(GatewayConfig {
            auth: AuthPolicy::with_static_key("static".to_owned()),
            manages_keys: true,
            paths: Some(paths.clone()),
            ..GatewayConfig::default()
        });

        let store = state.api_keys_store().expect("a store");
        let (record, full) = store
            .create(
                "alice",
                RateLimit {
                    per_minute: Some(2),
                    per_day: None,
                },
            )
            .expect("create");
        let bearer = format!("Bearer {full}");

        // Before a refresh the live policy is the startup snapshot: it has never
        // heard of this key.
        assert!(
            state.auth_policy().identify(Some(&bearer)).is_err(),
            "a key created after startup must not be honoured until the reload"
        );

        // After the reload the key authenticates and carries the limit it was
        // created with.
        state.refresh_keys();
        let caller = state
            .auth_policy()
            .identify(Some(&bearer))
            .expect("the reloaded key is valid");
        let identified = caller.expect("a named key identifies its record");
        assert_eq!(identified.id, record.id);
        assert_eq!(identified.limit.per_minute, Some(2));

        // A limit changed at runtime is the limit enforced from the next reload.
        store
            .set_limit(
                &record.id,
                RateLimit {
                    per_minute: Some(9),
                    per_day: None,
                },
            )
            .expect("set_limit");
        state.refresh_keys();
        let updated = state
            .auth_policy()
            .identify(Some(&bearer))
            .expect("still valid")
            .expect("still a named key");
        assert_eq!(
            updated.limit.per_minute,
            Some(9),
            "the new ceiling must be live"
        );

        // A revoked key stops authenticating at once, not at the next restart.
        store.revoke(&record.id).expect("revoke");
        state.refresh_keys();
        assert!(
            state.auth_policy().identify(Some(&bearer)).is_err(),
            "a revoked key must be rejected after the reload"
        );
        // The static key it was started with is untouched by any of this.
        assert!(state.auth_policy().check(Some("Bearer static")).is_ok());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_gateway_that_does_not_manage_keys_ignores_a_refresh() {
        // A plain loopback gateway serves local clients with no key; a stray
        // refresh must not start demanding one.
        let state = state(GatewayConfig::default());
        state.refresh_keys();
        assert!(!state.auth_policy().is_enabled());
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
