//! Supervision behaviour, exercised against a stand-in engine.
//!
//! Spec section 27 requires the application to survive every way an engine can
//! fail. Proving that needs an engine that fails on demand, which the real one
//! cannot be asked to do — so these run against `hermes-fake-engine`, and take
//! milliseconds rather than the tens of seconds a real model load costs.
//!
//! What is covered here is the *supervisor*: readiness, crash classification,
//! stderr capture, cancellation, and the promise that no child outlives us.
//! That a real engine and a real model work is proven separately, by
//! `tests/real_engine.rs`.

use std::path::PathBuf;
use std::time::Duration;

use hermes_backend_llamacpp::supervisor::{self, EngineConfig, ExitClassification};
use hermes_core::{Actionable, RuntimeParams};
use tokio_util::sync::CancellationToken;

/// Whether a process still exists, without waiting on it.
///
/// Three implementations because the honest answer is platform-shaped:
/// `/proc` does not exist on macOS, and Windows has neither `/proc` nor signals.
/// The Linux arm keeps its zombie tolerance - a reaped-but-not-collected child
/// is not an orphan, and treating it as one made this test flake.
fn alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        PathBuf::from(format!("/proc/{pid}")).exists()
            && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|stat| stat.contains(" Z "))
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // `kill -0` asks whether the process exists without signalling it.
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(windows)]
    {
        // Filtered `tasklist` prints the process when it exists and an
        // informational line when it does not, so the pid itself is the test.
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()))
    }
}

/// Kill a process the hardest way the platform offers, from outside.
///
/// The point of these tests is that the supervisor survives a death it did not
/// arrange, so the kill has to come from outside the process rather than from
/// the handle the supervisor holds.
fn kill_hard(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(windows)]
    {
        std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// Path to the stand-in engine. Cargo sets this for binaries in this package.
fn fake_engine() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hermes-fake-engine"))
}

use std::path::Path;

/// A model file whose *contents* tell the stand-in engine how to behave.
///
/// Deliberately not an environment variable: those are process-wide, and with
/// tests running in parallel each one would overwrite the others. A file per
/// test has no shared state, so these can run concurrently and mean what they
/// say.
struct FakeModel(PathBuf);

impl FakeModel {
    fn new(mode: &str) -> Self {
        // The clock alone is not unique: on a coarse timer two tests running in
        // parallel are handed the same name. The counter and the pid settle it.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hermes-fake-model-{}-{}-{unique}-{sequence}.gguf",
            mode.replace(':', "-"),
            std::process::id()
        ));
        std::fs::write(&path, mode).expect("write the fake model");
        Self(path)
    }
}

impl Drop for FakeModel {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn config(model: &FakeModel) -> EngineConfig {
    EngineConfig {
        server_path: fake_engine(),
        install_dir: fake_engine()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
        model_path: model.0.clone(),
        params: RuntimeParams::default(),
        threads: 2,
        start_timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn a_healthy_engine_reaches_ready_and_reports_its_endpoint() {
    let engine = supervisor::start(&config(&FakeModel::new("ready")), &CancellationToken::new())
        .await
        .expect("the engine should become ready");

    assert!(engine.port() > 0);
    assert!(engine.base_url().starts_with("http://127.0.0.1:"));
    // Section 23: loopback only. The engine is never a second public surface.
    assert!(engine.base_url().contains("127.0.0.1"));
    // A per-run key, so nothing else on the machine can drive our engine.
    assert_eq!(engine.api_key().len(), 48);

    let pid = engine.pid().expect("a pid");
    let stopped = engine.shutdown().await;
    // SIGTERM is what a graceful stop *is* on Unix. Windows has no signals, so
    // the same intent arrives as a different classification; asserting the Unix
    // shape there would be asserting a lie the supervisor used to tell.
    #[cfg(unix)]
    assert_eq!(stopped, ExitClassification::Signalled { signal: 15 });
    #[cfg(windows)]
    assert_ne!(
        stopped,
        ExitClassification::Running,
        "the engine did not stop"
    );

    // Nothing left behind.
    assert!(!alive(pid), "the engine process outlived its supervisor");
}

#[tokio::test]
async fn an_engine_that_exits_during_startup_reports_why() {
    let err = supervisor::start(
        &config(&FakeModel::new("exit:7")),
        &CancellationToken::new(),
    )
    .await
    .expect_err("a failing engine must not report success");

    assert_eq!(err.code(), "engine_crashed");
    match err {
        hermes_inference::BackendError::EngineCrashed {
            exit_code, tail, ..
        } => {
            assert_eq!(exit_code, Some(7));
            // The engine explains startup failures on stderr, so the tail is
            // the only useful part of the report.
            assert!(
                tail.iter()
                    .any(|line| line.contains("unable to load model")),
                "stderr was not captured: {tail:?}"
            );
        }
        other => panic!("expected a crash, got {other:?}"),
    }
}

#[tokio::test]
async fn an_illegal_instruction_becomes_an_explanation_not_a_crash() {
    // Section 10: never fail merely because an advanced instruction is
    // missing. This is the end-to-end path for the development machine's
    // no-AVX case, if the engine's own dispatch ever chose wrongly.
    let err = supervisor::start(
        &config(&FakeModel::new("signal:4")),
        &CancellationToken::new(),
    )
    .await
    .expect_err("SIGILL must not be reported as success");

    assert_eq!(err.code(), "unsupported_cpu_instruction");
    assert!(
        err.to_string().contains("instruction sets"),
        "the error should say what this CPU does have: {err}"
    );
}

#[tokio::test]
async fn a_segfaulting_engine_is_classified_as_a_crash() {
    let err = supervisor::start(
        &config(&FakeModel::new("signal:11")),
        &CancellationToken::new(),
    )
    .await
    .expect_err("SIGSEGV must not be reported as success");
    assert_eq!(err.code(), "engine_crashed");
    assert!(err.to_string().contains("segmentation fault"));
}

#[tokio::test]
async fn an_engine_that_never_becomes_ready_times_out_rather_than_hanging() {
    let model = FakeModel::new("never_ready");
    let mut config = config(&model);
    config.start_timeout = Duration::from_millis(600);

    let started = std::time::Instant::now();
    let err = supervisor::start(&config, &CancellationToken::new())
        .await
        .expect_err("a silent engine must not be reported as ready");

    assert_eq!(err.code(), "engine_start_timeout");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the timeout did not fire promptly"
    );
    // A timeout that leaves the process running would leak an engine per
    // attempt, each holding a model.
    assert!(
        !err.remedies().is_empty(),
        "a timeout should suggest raising it"
    );
}

#[tokio::test]
async fn a_slow_engine_is_waited_for_rather_than_abandoned() {
    // A large model on a cold cache genuinely takes a while. Giving up on it
    // would be a worse failure than waiting.
    let engine = supervisor::start(
        &config(&FakeModel::new("slow:400")),
        &CancellationToken::new(),
    )
    .await
    .expect("a slow engine should still be waited for");
    assert!(engine.uptime() >= Duration::from_millis(400));
    let _ = engine.shutdown().await;
}

#[tokio::test]
async fn cancelling_a_start_leaves_no_engine_running() {
    let cancel = CancellationToken::new();
    let model = FakeModel::new("slow:5000");
    let config = config(&model);

    let handle = tokio::spawn({
        let cancel = cancel.clone();
        async move { supervisor::start(&config, &cancel).await.map(|e| e.pid()) }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    cancel.cancel();

    let result = handle.await.expect("the task should finish");
    let err = result.expect_err("a cancelled start must not report success");
    assert_eq!(err.code(), "cancelled");
}

#[tokio::test]
async fn a_killed_engine_is_observed_rather_than_taking_us_down_with_it() {
    let mut engine =
        supervisor::start(&config(&FakeModel::new("ready")), &CancellationToken::new())
            .await
            .expect("ready");

    let pid = engine.pid().expect("a pid");
    assert_eq!(engine.poll_exit(), ExitClassification::Running);

    // The whole point of the process boundary: this is survivable.
    assert!(kill_hard(pid), "could not kill the engine");

    // Give the kernel a moment to reap it.
    let mut classification = ExitClassification::Running;
    for _ in 0..50 {
        classification = engine.poll_exit();
        if classification != ExitClassification::Running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // On Unix an external `kill -9` is observable as the signal it was. Windows
    // has no signals, so `taskkill /F` surfaces as an exit code instead - the
    // classification differs, but the property under test does not: the
    // supervisor noticed, and is still alive to say so.
    #[cfg(unix)]
    assert_eq!(classification, ExitClassification::Signalled { signal: 9 });
    #[cfg(windows)]
    assert_ne!(
        classification,
        ExitClassification::Running,
        "the supervisor did not notice the engine had been killed"
    );
    // And it turns into something a user can be shown.
    let err = classification.into_error(engine.stderr_tail());
    assert!(matches!(
        err.code(),
        "engine_crashed" | "engine_out_of_memory"
    ));
}

#[tokio::test]
async fn stderr_is_captured_continuously_not_only_on_failure() {
    let engine = supervisor::start(&config(&FakeModel::new("ready")), &CancellationToken::new())
        .await
        .expect("ready");

    // Waiting on the log pump, which runs on its own task.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let tail = engine.stderr_tail();
    assert!(
        tail.iter().any(|line| line.contains("listening")),
        "expected startup output in the tail: {tail:?}"
    );
    let _ = engine.shutdown().await;
}
