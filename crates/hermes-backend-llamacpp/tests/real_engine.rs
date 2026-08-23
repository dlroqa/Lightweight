//! End-to-end against the genuine engine and a genuine model.
//!
//! Everything in `supervision.rs` runs against a stand-in, which proves the
//! supervisor's logic but says nothing about whether the real llama.cpp build
//! actually starts, finds its CPU backend, and reads a GGUF file. That is what
//! this covers.
//!
//! It needs a model, so it is opt-in: point `HERMES_TEST_MODEL` at a `.gguf`
//! file and it runs; leave it unset and every test here skips, so a clean
//! checkout still passes. It also downloads the pinned engine on first run.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hermes_backend_llamacpp::backend::ProcessBackend;
use hermes_core::{Actionable, GgmlType, ModelId, RuntimeParams};
use hermes_gguf::{GgufFile, ModelMetadata};
use hermes_inference::{BackendHealth, InferenceBackend, LoadRequest};
use hermes_system_info::MemoryProbe as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Points at a real GGUF model, when one is available.
const MODEL_ENV: &str = "HERMES_TEST_MODEL";

fn model_path() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(MODEL_ENV)?);
    path.is_file().then_some(path)
}

/// A scratch data directory that cleans up after itself.
struct Profile(PathBuf);

impl Profile {
    fn new(tag: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("hermes-real-{tag}-{unique}"));
        std::fs::create_dir_all(&path).expect("profile dir");
        Self(path)
    }

    /// Share one engine install across runs so each test is not a fresh 16 MB
    /// download.
    fn runtime_dir(&self) -> PathBuf {
        std::env::temp_dir().join("hermes-shared-engine")
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn metadata(path: &PathBuf) -> ModelMetadata {
    let file = GgufFile::open(path).expect("the model should parse");
    ModelMetadata::from_file(&file).expect("metadata should extract")
}

fn request(path: &PathBuf, params: RuntimeParams) -> LoadRequest {
    LoadRequest {
        model: ModelId::with_context("test-model", params.n_ctx),
        gguf_path: path.clone(),
        metadata: Arc::new(metadata(path)),
        runtime: params,
    }
}

#[tokio::test]
async fn the_real_engine_loads_a_real_model_and_reports_its_memory() {
    let Some(model) = model_path() else {
        eprintln!("skipping: set {MODEL_ENV} to a .gguf file to run this");
        return;
    };
    let profile = Profile::new("load");
    let backend = ProcessBackend::new(profile.runtime_dir()).expect("backend");

    let params = RuntimeParams::default().with_context(2048);
    let (tx, mut rx) = mpsc::channel(64);
    let progress = tokio::spawn(async move {
        let mut stages = Vec::new();
        while let Some(update) = rx.recv().await {
            stages.push(update);
        }
        stages
    });

    let loaded = backend
        .load(request(&model, params), tx, CancellationToken::new())
        .await
        .expect("the real engine should load a real model");

    assert_eq!(loaded.effective.n_ctx, 2048);
    assert!(loaded.effective.threads.unwrap_or(0) >= 1);
    assert!(backend.health().await.is_ready());

    // Progress must actually be reported: a silent thirty-second load reads as
    // a hang.
    let stages = progress.await.expect("reporter");
    assert!(!stages.is_empty(), "no progress was reported");

    // Memory read from the operating system, which is what makes calibrating
    // the RAM estimator possible at all.
    let usage = backend
        .resource_usage()
        .await
        .expect("usage query")
        .expect("an engine is running");
    assert!(
        usage.rss > hermes_core::Bytes::from_mib(64),
        "implausibly small resident set: {}",
        usage.rss
    );
    assert!(usage.peak_rss >= usage.rss);

    backend.shutdown().await.expect("shutdown");
    assert_eq!(backend.health().await, BackendHealth::Stopped);
}

#[tokio::test]
async fn the_ram_estimate_is_an_upper_bound_on_what_the_engine_actually_uses() {
    // The estimate is deliberately conservative - it does not discount a
    // declared sliding window, and its compute term is uncalibrated. Being
    // above the truth is correct; being below it would mean admitting loads
    // that end in an OOM kill.
    let Some(model) = model_path() else {
        eprintln!("skipping: set {MODEL_ENV} to a .gguf file to run this");
        return;
    };
    let profile = Profile::new("estimate");
    let backend = ProcessBackend::new(profile.runtime_dir()).expect("backend");

    let params = RuntimeParams::default().with_context(2048);
    let metadata = metadata(&model);
    let estimate = hermes_memory::Estimator::headless().estimate(
        &metadata,
        params,
        hermes_system_info::SystemMemoryProbe
            .snapshot()
            .expect("memory probe"),
    );

    let (tx, _rx) = mpsc::channel(64);
    backend
        .load(request(&model, params), tx, CancellationToken::new())
        .await
        .expect("load");
    // Let the engine finish touching its weights before sampling the peak.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let usage = backend
        .resource_usage()
        .await
        .expect("usage")
        .expect("running");
    backend.shutdown().await.expect("shutdown");

    // The estimate covers the gateway and UI as well as the engine, so it
    // should exceed the engine's own peak - but not absurdly.
    assert!(
        estimate.total.get() >= usage.peak_rss.get(),
        "the estimate ({}) was below the engine's actual peak ({}); \
         admitting on that basis risks an OOM kill",
        estimate.total,
        usage.peak_rss
    );
    assert!(
        estimate.total.get() < usage.peak_rss.get().saturating_mul(4),
        "the estimate ({}) is more than 4x the peak ({}), which would refuse \
         loads that would have worked",
        estimate.total,
        usage.peak_rss
    );
}

#[tokio::test]
async fn a_context_beyond_the_models_maximum_is_refused_without_launching() {
    let Some(model) = model_path() else {
        eprintln!("skipping: set {MODEL_ENV} to a .gguf file to run this");
        return;
    };
    let profile = Profile::new("ctx");
    let backend = ProcessBackend::new(profile.runtime_dir()).expect("backend");

    let metadata = metadata(&model);
    let beyond = u32::try_from(metadata.context_length.unwrap_or(4096)).unwrap_or(u32::MAX);
    let params = RuntimeParams::default().with_context(beyond.saturating_add(1024));

    let started = std::time::Instant::now();
    let (tx, _rx) = mpsc::channel(4);
    let err = backend
        .load(request(&model, params), tx, CancellationToken::new())
        .await
        .expect_err("a context beyond the model's maximum must be refused");

    assert_eq!(err.code(), "invalid_context_length");
    // Refused from metadata alone, so it costs no engine start.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the refusal should not have required launching anything"
    );
    // And the message carries a limit a client can parse back out.
    assert!(err.to_string().contains("maximum context length of"));
}

#[tokio::test]
async fn a_kv_cache_type_the_engine_rejects_is_refused_before_launching() {
    let Some(model) = model_path() else {
        eprintln!("skipping: set {MODEL_ENV} to a .gguf file to run this");
        return;
    };
    let profile = Profile::new("kv");
    let backend = ProcessBackend::new(profile.runtime_dir()).expect("backend");

    // q6_K is a real ggml type, and a perfectly good one for weights, but
    // llama-server does not accept it for the KV cache. Catching that here
    // turns an opaque engine exit into a list of what is allowed.
    let params = RuntimeParams::default().with_kv_cache_type(GgmlType::Q6_K);
    let (tx, _rx) = mpsc::channel(4);
    let err = backend
        .load(request(&model, params), tx, CancellationToken::new())
        .await
        .expect_err("an unsupported KV cache type must be refused");

    assert_eq!(err.code(), "unsupported_kv_cache_type");
    assert!(
        !err.remedies().is_empty(),
        "the refusal should list alternatives"
    );
}
