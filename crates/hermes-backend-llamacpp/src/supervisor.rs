//! Running the engine as a supervised child process.
//!
//! This module is most of the reason the engine is a separate process at all.
//! Spec section 27 requires that the application never crash because a model
//! failed to load, an architecture is unsupported, or a CPU instruction is
//! missing. Inside a linked library those are a `GGML_ASSERT`, a SIGILL and an
//! OOM kill — all of which take the whole process down, `catch_unwind`
//! included. Across a process boundary each one is an ordinary exit status
//! that can be observed, classified and reported while the gateway and the UI
//! stay up.
//!
//! Three properties this is built to guarantee:
//!
//! 1. **No orphans.** The engine holds a model in memory — hundreds of
//!    megabytes to several gigabytes. An orphan left behind by a crashed
//!    parent would be invisible and expensive. On Linux the child asks the
//!    kernel to kill it when its parent dies; everywhere it also runs in its
//!    own process group so a signal reaches the whole tree.
//! 2. **Not reachable by anything else.** The child binds loopback on an
//!    ephemeral port and requires a random per-run API key. It is an internal
//!    implementation detail, not a second public endpoint.
//! 3. **Diagnosable failures.** stderr is captured continuously, and the last
//!    lines travel with the error, because that is where the engine says why
//!    it stopped.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hermes_core::RuntimeParams;
use hermes_inference::BackendError;
use hermes_observability::targets;
use hermes_system_info::{CpuInfo, MemoryProbe, SystemMemoryProbe};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

/// stderr lines kept for crash reports.
///
/// Enough to carry llama.cpp's startup banner and the lines around a failure,
/// bounded so a chatty engine cannot grow it without limit.
const STDERR_TAIL_LINES: usize = 64;

/// How often readiness is polled while the engine loads.
const READY_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// KV cache element types the pinned engine accepts.
///
/// Read from `llama-server --help` at build b10590: "allowed values: f32, f16,
/// bf16, q8_0, q4_0, q4_1, iq4_nl, q5_0, q5_1". Not every ggml type is valid
/// here, so checking against this list turns a bad choice into an error naming
/// the alternatives rather than an opaque engine exit at startup.
pub const ALLOWED_KV_CACHE_TYPES: &[&str] = &[
    "f32", "f16", "bf16", "q8_0", "q4_0", "q4_1", "iq4_nl", "q5_0", "q5_1",
];

/// How the engine process ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitClassification {
    /// Exited zero. Normal after a shutdown request.
    Clean,
    /// Exited non-zero, usually a startup failure it has explained on stderr.
    Failed { code: i32 },
    /// Killed by a signal.
    Signalled { signal: i32 },
    /// Still running.
    Running,
}

/// SIGILL. The engine executed an instruction this CPU does not implement.
const SIGILL: i32 = 4;
/// SIGKILL. Almost always the OOM killer, since nothing else sends it here.
const SIGKILL: i32 = 9;
/// SIGSEGV.
const SIGSEGV: i32 = 11;

impl ExitClassification {
    /// Turn an exit into the most specific error available.
    ///
    /// The signal is what makes this worth doing. SIGILL means the engine's own
    /// runtime dispatch chose a CPU variant this machine cannot run, which
    /// section 10 says must never be a bare crash. SIGKILL with little free
    /// memory means the OOM killer took it, which means our RAM estimate
    /// admitted a load that did not fit — worth saying plainly rather than
    /// reporting as a generic failure.
    pub fn into_error(self, tail: Vec<String>) -> BackendError {
        match self {
            Self::Clean | Self::Running => BackendError::EngineUnavailable,
            Self::Failed { code } => BackendError::EngineCrashed {
                detail: format!("exited with status {code}"),
                exit_code: Some(code),
                signal: None,
                tail,
            },
            Self::Signalled { signal: SIGILL } => BackendError::UnsupportedCpuInstruction {
                detected: CpuInfo::detect()
                    .features
                    .iter()
                    .map(|feature| feature.label())
                    .collect::<Vec<_>>()
                    .join(", "),
            },
            Self::Signalled { signal: SIGKILL } => {
                // Distinguish an OOM kill from a deliberate one by asking how
                // much memory was left. A probe failure is not evidence of an
                // OOM, so it falls through to the generic case.
                let starved = SystemMemoryProbe
                    .snapshot()
                    .is_ok_and(|snapshot| snapshot.available < hermes_core::Bytes::from_mib(256));
                if starved {
                    BackendError::EngineOom { tail }
                } else {
                    BackendError::EngineCrashed {
                        detail: "killed (signal 9)".to_owned(),
                        exit_code: None,
                        signal: Some(SIGKILL),
                        tail,
                    }
                }
            }
            Self::Signalled { signal } => BackendError::EngineCrashed {
                detail: match signal {
                    SIGSEGV => "crashed with a segmentation fault".to_owned(),
                    other => format!("killed by signal {other}"),
                },
                exit_code: None,
                signal: Some(signal),
                tail,
            },
        }
    }
}

/// What to launch, and how.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// The `llama-server` executable.
    pub server_path: PathBuf,
    /// Directory holding the engine's shared objects, so its runtime CPU
    /// dispatch can find the variant it wants.
    pub install_dir: PathBuf,
    pub model_path: PathBuf,
    pub params: RuntimeParams,
    /// Inference threads. Defaults to the physical core count.
    pub threads: u32,
    /// How long to wait for the engine to report ready. Generous by default:
    /// a multi-gigabyte model on a cold page cache and a slow disk takes a
    /// while, and a timeout that fires during a normal load is worse than one
    /// that never fires.
    pub start_timeout: Duration,
}

/// A running engine process.
#[derive(Debug)]
pub struct Engine {
    child: Child,
    port: u16,
    api_key: String,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// The tasks forwarding the child's output. Held so that a crash report can
    /// wait for them to finish before reading the tail.
    pumps: Vec<tokio::task::JoinHandle<()>>,
    pid: Option<u32>,
    started_at: Instant,
}

impl Engine {
    /// The engine's private base URL.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The per-run key the engine requires.
    ///
    /// Not a secret in any lasting sense — it is regenerated every launch and
    /// never leaves this process. Its job is to stop anything else on the
    /// machine that happens to find the port from driving our engine.
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub const fn port(&self) -> u16 {
        self.port
    }

    pub const fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// The last lines the engine wrote to stderr, after waiting for the log
    /// readers to catch up.
    ///
    /// Use this whenever the engine has died. The readers run on their own
    /// tasks and finish when the child's pipes reach end of file, so reading
    /// the tail the instant an exit is noticed can return nothing at all —
    /// which is exactly the case where stderr is the only explanation there is.
    /// A fast startup failure did report an empty tail before this existed.
    pub async fn drained_stderr_tail(&mut self) -> Vec<String> {
        for pump in std::mem::take(&mut self.pumps) {
            // Bounded: a pump only finishes at EOF, and a child that has closed
            // its pipes but not exited must not hold up an error report.
            let _ = tokio::time::timeout(Duration::from_millis(500), pump).await;
        }
        self.stderr_tail()
    }

    /// The last lines captured so far, without waiting.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail
            .lock()
            .map(|tail| tail.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether the process has ended, and how.
    pub fn poll_exit(&mut self) -> ExitClassification {
        match self.child.try_wait() {
            Ok(Some(status)) => classify(status),
            Ok(None) => ExitClassification::Running,
            // The child is unwaitable, which means it is gone in a way we
            // cannot describe.
            Err(_) => ExitClassification::Failed { code: -1 },
        }
    }

    /// Stop the engine, escalating if it does not go quietly.
    ///
    /// Asks politely, waits briefly, then kills. The wait matters: a clean exit
    /// lets the engine release its memory mapping in an orderly way, and on a
    /// machine this tight that is worth two seconds.
    pub async fn shutdown(mut self) -> ExitClassification {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // Signal the whole process group, so nothing the engine spawned is
            // left behind either.
            unix::terminate_group(pid);
        }

        let deadline = Duration::from_secs(2);
        match tokio::time::timeout(deadline, self.child.wait()).await {
            Ok(Ok(status)) => classify(status),
            _ => {
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
                ExitClassification::Signalled { signal: SIGKILL }
            }
        }
    }
}

fn classify(status: std::process::ExitStatus) -> ExitClassification {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return ExitClassification::Signalled { signal };
        }
    }
    match status.code() {
        Some(0) => ExitClassification::Clean,
        Some(code) => ExitClassification::Failed { code },
        None => ExitClassification::Failed { code: -1 },
    }
}

/// Launch the engine and wait until it is ready to serve.
pub async fn start(
    config: &EngineConfig,
    cancel: &CancellationToken,
) -> Result<Engine, BackendError> {
    if !config.server_path.is_file() {
        return Err(BackendError::RuntimeMissing {
            path: config.server_path.clone(),
        });
    }
    if !config.model_path.is_file() {
        return Err(BackendError::ModelNotFound {
            path: config.model_path.clone(),
        });
    }

    let port = reserve_port()?;
    let api_key = random_api_key()?;

    let mut command = Command::new(&config.server_path);
    command
        .args(build_args(config, port))
        // Through the environment, never the command line. `/proc/<pid>/cmdline`
        // is world-readable on an ordinary Linux system, so a key in argv is a
        // key any local user can read and then use to drive the engine
        // directly, around every check this gateway makes. `/proc/<pid>/environ`
        // is readable only by the owner. Verified available at the pinned
        // build: `llama-server --help` lists `(env: LLAMA_API_KEY)`.
        .env("LLAMA_API_KEY", &api_key)
        .current_dir(&config.install_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A last line of defence if every other mechanism fails: tokio kills
        // the child when the handle drops.
        .kill_on_drop(true);

    // The engine loads its CPU backend variants from beside the executable.
    #[cfg(unix)]
    command.env("LD_LIBRARY_PATH", &config.install_dir);

    #[cfg(unix)]
    unix::configure_child(&mut command);

    let mut child = command.spawn().map_err(|err| {
        BackendError::io(format!("launching {}", config.server_path.display()), err)
    })?;

    let pid = child.id();
    let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));

    let mut pumps = Vec::new();
    if let Some(stderr) = child.stderr.take() {
        pumps.push(spawn_log_pump(stderr, Arc::clone(&stderr_tail)));
    }
    if let Some(stdout) = child.stdout.take() {
        // llama-server logs to stderr, but anything it does put on stdout is
        // still worth having rather than letting the pipe fill and block it.
        pumps.push(spawn_log_pump(stdout, Arc::clone(&stderr_tail)));
    }

    let mut engine = Engine {
        child,
        port,
        api_key,
        stderr_tail,
        pumps,
        pid,
        started_at: Instant::now(),
    };

    tracing::info!(
        target: targets::BACKEND,
        pid = pid.unwrap_or(0),
        port,
        model = %config.model_path.display(),
        n_ctx = config.params.n_ctx,
        threads = config.threads,
        "engine starting"
    );

    match await_ready(&mut engine, config, cancel).await {
        Ok(()) => {
            tracing::info!(
                target: targets::BACKEND,
                pid = pid.unwrap_or(0),
                ready_in_ms = engine.uptime().as_millis(),
                "engine ready"
            );
            Ok(engine)
        }
        Err(err) => {
            // Never leave a half-started engine behind, and wait for the log
            // readers so the report says why it failed.
            let tail = engine.drained_stderr_tail().await;
            let _ = engine.shutdown().await;
            Err(enrich(err, tail))
        }
    }
}

/// Attach captured stderr to an error that does not already carry it.
fn enrich(err: BackendError, captured: Vec<String>) -> BackendError {
    match err {
        BackendError::EngineCrashed {
            detail,
            exit_code,
            signal,
            tail,
        } if tail.is_empty() => BackendError::EngineCrashed {
            detail,
            exit_code,
            signal,
            tail: captured,
        },
        BackendError::EngineOom { tail } if tail.is_empty() => {
            BackendError::EngineOom { tail: captured }
        }
        other => other,
    }
}

/// Poll the engine's health endpoint until it serves, it dies, or time runs out.
async fn await_ready(
    engine: &mut Engine,
    config: &EngineConfig,
    cancel: &CancellationToken,
) -> Result<(), BackendError> {
    // `reqwest` panics rather than erroring when no rustls provider has been
    // installed, so the precondition is established here too. The supervisor
    // can be driven without ever constructing a `RuntimeInstaller`, and did
    // panic on that path before this call existed.
    crate::tls::ensure_provider();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|err| BackendError::RuntimeDownloadFailed {
            reason: err.to_string(),
        })?;
    let health_url = format!("{}/health", engine.base_url());
    let deadline = Instant::now() + config.start_timeout;

    loop {
        if cancel.is_cancelled() {
            return Err(BackendError::Cancelled);
        }

        // Check for death before checking for readiness: a process that has
        // already exited will never answer, and waiting out the full timeout
        // to discover that would hide the reason it died.
        match engine.poll_exit() {
            ExitClassification::Running => {}
            exit => return Err(exit.into_error(engine.drained_stderr_tail().await)),
        }

        if let Ok(response) = client.get(&health_url).send().await
            && response.status().is_success()
        {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(BackendError::StartTimeout {
                seconds: config.start_timeout.as_secs(),
            });
        }
        tokio::time::sleep(READY_POLL_INTERVAL).await;
    }
}

/// Build the engine's command line.
///
/// Every value is passed explicitly, including ones whose default happens to
/// match what we want. Two reasons: the RAM estimate is computed for exactly
/// these parameters and must not be invalidated by an engine default changing
/// under it, and `--parallel` in particular defaults to `-1` (auto), which
/// would size the KV cache for a slot count we did not choose.
///
/// One thing deliberately absent: the API key. It travels in the environment,
/// because a command line is readable by every user on the machine.
pub fn build_args(config: &EngineConfig, port: u16) -> Vec<String> {
    let params = &config.params;
    vec![
        "--model".into(),
        config.model_path.display().to_string(),
        // Loopback only. Section 23: the engine is never network-reachable, and
        // this one is not even the public surface - the gateway is.
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "--ctx-size".into(),
        params.n_ctx.to_string(),
        "--batch-size".into(),
        params.n_batch.to_string(),
        "--ubatch-size".into(),
        params.n_ubatch.to_string(),
        // Explicit, not left at auto.
        "--parallel".into(),
        params.n_parallel.max(1).to_string(),
        "--threads".into(),
        config.threads.max(1).to_string(),
        // Stated rather than inherited, so the KV arithmetic in the estimator
        // describes what is actually allocated.
        "--cache-type-k".into(),
        params.cache_type_k.name().to_owned(),
        "--cache-type-v".into(),
        params.cache_type_v.name().to_owned(),
        // Chat templates come from the model's own metadata.
        "--jinja".into(),
        // Prometheus metrics on the engine's private port, for the metrics
        // provider to scrape.
        "--metrics".into(),
    ]
}

/// Reserve a free loopback port.
///
/// The listener is bound and then dropped, so there is a brief window in which
/// something else could take the port. The alternative - a fixed port - fails
/// whenever a second instance runs or the port is already in use, which is far
/// more common. A launch that loses the race fails at bind and is retried.
fn reserve_port() -> Result<u16, BackendError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|err| BackendError::io("reserving a port for the engine", err))?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|err| BackendError::io("reading the reserved port", err))
}

/// A random per-run key for the engine's private port.
fn random_api_key() -> Result<String, BackendError> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|err| {
        BackendError::io(
            "generating an engine API key",
            std::io::Error::other(err.to_string()),
        )
    })?;
    Ok(hex::encode(bytes))
}

/// Forward a child's output into the log and into the crash tail.
fn spawn_log_pump<R>(reader: R, tail: Arc<Mutex<VecDeque<String>>>) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            // The engine's own output is noisy and is not ours, so it is logged
            // at debug. It never contains prompt text: prompts travel over the
            // HTTP API, not the command line.
            tracing::debug!(target: targets::BACKEND, "{line}");
            if let Ok(mut tail) = tail.lock() {
                if tail.len() == STDERR_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        }
    })
}

#[cfg(unix)]
mod unix {
    // `pre_exec` is an inherent method on tokio's Command, so the std
    // `CommandExt` trait is not needed here.
    use tokio::process::Command;

    /// Put the child in its own process group and ask the kernel to kill it if
    /// we die.
    ///
    /// The engine holds a whole model in memory. An orphan would be invisible
    /// and would keep gigabytes reserved until someone noticed, so this is not
    /// a nicety.
    pub(super) fn configure_child(command: &mut Command) {
        // SAFETY: `pre_exec` runs in the forked child between `fork` and
        // `exec`, where only async-signal-safe operations are permitted. Both
        // calls made here are plain syscalls on that list, take no locks and
        // allocate nothing:
        //   * `prctl(PR_SET_PDEATHSIG, SIGKILL)` asks the kernel to SIGKILL
        //     this process when its parent dies.
        //   * `setsid` detaches it into a new session and process group, so a
        //     later signal reaches the engine and anything it spawned rather
        //     than our own group.
        // Return values are deliberately ignored: both are best-effort
        // hardening, and neither failing is a reason to abandon a launch that
        // would otherwise work. `kill_on_drop` remains as a backstop.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(|| {
                #[cfg(target_os = "linux")]
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                libc::setsid();
                Ok(())
            });
        }
    }

    /// Send SIGTERM to the child's whole process group.
    pub(super) fn terminate_group(pid: u32) {
        let Ok(pid) = i32::try_from(pid) else {
            return;
        };
        // SAFETY: `kill` with a negative pid signals the process group led by
        // that pid. The child was placed in its own group by `setsid` above, so
        // this cannot reach our own process or anything unrelated. A failure
        // means the group is already gone, which is the desired end state.
        #[allow(unsafe_code)]
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `code()` is an `Actionable` method, used only by these assertions.
    use hermes_core::Actionable;
    use hermes_core::GgmlType;

    fn config() -> EngineConfig {
        EngineConfig {
            server_path: PathBuf::from("/engine/llama-server"),
            install_dir: PathBuf::from("/engine"),
            model_path: PathBuf::from("/models/m.gguf"),
            params: RuntimeParams {
                n_ctx: 8192,
                n_batch: 2048,
                n_ubatch: 512,
                n_parallel: 1,
                cache_type_k: GgmlType::F16,
                cache_type_v: GgmlType::F16,
                threads: Some(4),
            },
            threads: 4,
            start_timeout: Duration::from_secs(60),
        }
    }

    fn args() -> Vec<String> {
        build_args(&config(), 45_871)
    }

    /// The value following `flag`, if present.
    fn value_of(args: &[String], flag: &str) -> Option<String> {
        let index = args.iter().position(|arg| arg == flag)?;
        args.get(index.saturating_add(1)).cloned()
    }

    #[test]
    fn the_engine_is_bound_to_loopback_only() {
        // Section 23: the engine is an internal detail and must never be
        // network-reachable. It is not the public surface; the gateway is.
        assert_eq!(value_of(&args(), "--host").as_deref(), Some("127.0.0.1"));
    }

    #[test]
    fn the_engine_requires_a_key_that_changes_every_run() {
        let first = random_api_key().expect("entropy");
        let second = random_api_key().expect("entropy");
        assert_ne!(first, second, "the key must not be reused between runs");
        assert_eq!(first.len(), 48, "24 bytes of entropy, hex encoded");
    }

    #[test]
    fn the_engines_key_never_appears_on_its_command_line() {
        // `/proc/<pid>/cmdline` is world-readable, so a key in argv is a key
        // any local user can read - and then use to drive the engine directly,
        // around the admission control, the context policy and the auth this
        // gateway applies. It travels in the environment instead, which is
        // readable only by the owner.
        let args = args();
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--api-key" || arg == "--api-key-file"),
            "the key is being passed on the command line: {args:?}"
        );
    }

    #[test]
    fn memory_shaping_parameters_are_passed_explicitly() {
        // The RAM estimate is computed for exactly these values. Letting the
        // engine choose any of them would make the estimate describe something
        // other than what is running.
        let args = args();
        assert_eq!(value_of(&args, "--ctx-size").as_deref(), Some("8192"));
        assert_eq!(value_of(&args, "--cache-type-k").as_deref(), Some("f16"));
        assert_eq!(value_of(&args, "--cache-type-v").as_deref(), Some("f16"));
        assert_eq!(value_of(&args, "--ubatch-size").as_deref(), Some("512"));
    }

    #[test]
    fn parallel_slots_are_stated_because_the_engine_defaults_to_auto() {
        // `llama-server --parallel` defaults to -1, which sizes the KV cache
        // for a slot count we did not choose. Verified from `--help` at b10590.
        assert_eq!(value_of(&args(), "--parallel").as_deref(), Some("1"));
    }

    #[test]
    fn thread_count_is_stated_and_never_zero() {
        assert_eq!(value_of(&args(), "--threads").as_deref(), Some("4"));

        let mut zeroed = config();
        zeroed.threads = 0;
        // Zero threads would be rejected by the engine; clamp rather than pass
        // through a value we know is invalid.
        assert_eq!(
            value_of(&build_args(&zeroed, 1), "--threads").as_deref(),
            Some("1")
        );
    }

    #[test]
    fn quantized_kv_cache_types_reach_the_engine_by_name() {
        let mut quantized = config();
        quantized.params.cache_type_k = GgmlType::Q8_0;
        quantized.params.cache_type_v = GgmlType::Q8_0;
        let args = build_args(&quantized, 1);
        assert_eq!(value_of(&args, "--cache-type-k").as_deref(), Some("q8_0"));
    }

    #[test]
    fn chat_templating_is_enabled_so_conversations_can_be_formatted() {
        assert!(args().iter().any(|arg| arg == "--jinja"));
    }

    #[test]
    fn every_kv_type_the_engine_allows_is_a_type_we_can_size() {
        // The estimator computes KV bytes from ggml block geometry. A type the
        // engine accepts but we cannot size would produce an estimate of zero.
        for name in ALLOWED_KV_CACHE_TYPES {
            let ty = GgmlType::from_name(name)
                .unwrap_or_else(|| panic!("{name} is not a ggml type we know"));
            assert!(
                ty.bytes_for_elements(1024).is_some(),
                "{name} cannot be sized"
            );
        }
    }

    // ---- exit classification ----

    #[test]
    fn an_illegal_instruction_names_the_cpu_features_present() {
        // Section 10: never fail merely because an advanced instruction is
        // missing. SIGILL must explain itself, not surface as a bare crash.
        let err = ExitClassification::Signalled { signal: SIGILL }.into_error(vec![]);
        assert_eq!(err.code(), "unsupported_cpu_instruction");
        assert!(
            err.to_string().contains("instruction sets"),
            "the message should list what this CPU does have: {err}"
        );
    }

    #[test]
    fn a_segfault_is_reported_as_a_crash_with_its_signal() {
        let err = ExitClassification::Signalled { signal: SIGSEGV }.into_error(vec![]);
        assert_eq!(err.code(), "engine_crashed");
        assert!(err.to_string().contains("segmentation fault"));
    }

    #[test]
    fn a_nonzero_exit_carries_the_status_and_the_stderr_tail() {
        // The engine explains startup failures on stderr, so the tail is
        // usually the only useful part of the report.
        let tail = vec!["error: unable to load model".to_owned()];
        let err = ExitClassification::Failed { code: 1 }.into_error(tail.clone());
        match err {
            BackendError::EngineCrashed {
                exit_code, tail: t, ..
            } => {
                assert_eq!(exit_code, Some(1));
                assert_eq!(t, tail);
            }
            other => panic!("expected a crash, got {other:?}"),
        }
    }

    #[test]
    fn a_clean_exit_is_not_reported_as_a_crash() {
        let err = ExitClassification::Clean.into_error(vec![]);
        assert_eq!(err.code(), "engine_unavailable");
    }

    #[test]
    fn a_crash_is_transient_but_an_oom_is_not() {
        // Restarting after an OOM kill would just repeat it.
        assert!(
            ExitClassification::Failed { code: 1 }
                .into_error(vec![])
                .is_transient()
        );
        assert!(!BackendError::EngineOom { tail: vec![] }.is_transient());
    }

    // ---- port reservation ----

    #[test]
    fn reserved_ports_are_free_and_distinct() {
        let first = reserve_port().expect("a port");
        let second = reserve_port().expect("a port");
        assert!(first > 0 && second > 0);
        // The OS hands out different ephemeral ports, so two launches do not
        // collide with each other.
        assert_ne!(first, second);
        // And the port really is bindable.
        assert!(std::net::TcpListener::bind(("127.0.0.1", first)).is_ok());
    }

    // ---- pre-flight checks ----

    #[tokio::test]
    async fn a_missing_engine_binary_is_reported_before_launching() {
        let mut missing = config();
        missing.server_path = PathBuf::from("/nonexistent/llama-server");
        let err = start(&missing, &CancellationToken::new())
            .await
            .expect_err("should refuse");
        assert_eq!(err.code(), "runtime_missing");
    }

    #[tokio::test]
    async fn a_missing_model_is_reported_before_launching() {
        // Launching an engine that will immediately fail wastes seconds and
        // buries the reason in its stderr.
        let mut missing = config();
        missing.server_path = std::env::current_exe().unwrap_or_default();
        missing.model_path = PathBuf::from("/nonexistent/model.gguf");
        let err = start(&missing, &CancellationToken::new())
            .await
            .expect_err("should refuse");
        assert_eq!(err.code(), "model_not_found");
    }
}
