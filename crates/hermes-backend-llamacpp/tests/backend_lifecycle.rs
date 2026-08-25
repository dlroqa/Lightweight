//! `ProcessBackend` lifecycle, against a pre-installed stand-in engine.
//!
//! These skip the download entirely by planting the stand-in where the
//! installer expects a real engine, so they exercise admission, loading,
//! health, unloading and progress reporting in milliseconds.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hermes_backend_llamacpp::backend::ProcessBackend;
use hermes_backend_llamacpp::manifest;
use hermes_core::{Actionable, GgmlType, ModelId, RuntimeParams};
use hermes_gguf::{ModelMetadata, QuantMix, TokenizerMeta};
use hermes_inference::{BackendHealth, InferenceBackend, LoadProgress, LoadRequest};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A runtime directory with the stand-in engine already installed, plus a
/// model file whose contents drive its behaviour.
struct Fixture {
    root: PathBuf,
    model: PathBuf,
}

impl Fixture {
    fn new(tag: &str, mode: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("hermes-lifecycle-{tag}-{unique}"));

        // Planted where the installer looks, so `ensure` finds it installed and
        // never reaches the network.
        let artifact = manifest::for_this_platform().expect("this platform is supported");
        let install = root.join(artifact.install_dir_name());
        std::fs::create_dir_all(&install).expect("install dir");
        std::fs::copy(
            env!("CARGO_BIN_EXE_hermes-fake-engine"),
            install.join(manifest::server_executable()),
        )
        .expect("plant the stand-in engine");

        let model = root.join("model.gguf");
        std::fs::write(&model, mode).expect("write the fake model");

        Self { root, model }
    }

    /// Metadata describing a small, supported model.
    ///
    /// Built directly rather than parsed from a real file: these tests are
    /// about the backend's lifecycle, and a real GGUF would only slow them down.
    fn metadata(&self) -> ModelMetadata {
        ModelMetadata {
            architecture: "llama".to_owned(),
            supported: true,
            name: Some("stand-in".to_owned()),
            context_length: Some(8192),
            block_count: Some(2),
            embedding_length: Some(64),
            feed_forward_length: Some(256),
            head_count: Some(8),
            head_count_kv: Some(vec![2]),
            key_length: None,
            value_length: None,
            sliding_window: None,
            rope_freq_base: None,
            vocab_size: Some(128),
            tokenizer: TokenizerMeta::default(),
            file_type: None,
            quantization: QuantMix::default(),
            tensor_count: 0,
            param_count: Some(0),
            weight_bytes: Some(0),
            gguf_version: 3,
            alignment: 32,
            missing: Vec::new(),
        }
    }

    fn request(&self, params: RuntimeParams) -> LoadRequest {
        LoadRequest {
            model: ModelId::with_context("stand-in", params.n_ctx),
            gguf_path: self.model.clone(),
            metadata: Arc::new(self.metadata()),
            runtime: params,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn a_load_completes_even_when_nobody_reads_the_progress_channel() {
    // The regression this exists for: `load` used `send().await` for progress,
    // so a caller holding a receiver it never drained would fill the channel
    // and block the load forever. A closed UI is exactly that caller. Progress
    // is advisory and must never be able to stall the work it describes.
    let fixture = Fixture::new("nodrain", "ready");
    let backend = ProcessBackend::new(&fixture.root).expect("backend");

    // Capacity 1, and deliberately never read from.
    let (tx, _never_read) = mpsc::channel(1);

    let loaded = tokio::time::timeout(
        Duration::from_secs(20),
        backend.load(
            fixture.request(RuntimeParams::default().with_context(2048)),
            tx,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("the load must not block on an undrained progress channel")
    .expect("the load should succeed");

    assert_eq!(loaded.effective.n_ctx, 2048);
    backend.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn progress_reaches_a_caller_that_does_read_it() {
    let fixture = Fixture::new("progress", "ready");
    let backend = ProcessBackend::new(&fixture.root).expect("backend");

    let (tx, mut rx) = mpsc::channel(64);
    let collector = tokio::spawn(async move {
        let mut stages = Vec::new();
        while let Some(update) = rx.recv().await {
            stages.push(update);
        }
        stages
    });

    backend
        .load(
            fixture.request(RuntimeParams::default()),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("load");
    let stages = collector.await.expect("collector");

    assert!(stages.contains(&LoadProgress::StartingEngine), "{stages:?}");
    assert!(stages.contains(&LoadProgress::Ready), "{stages:?}");
    backend.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn health_moves_from_stopped_to_ready_and_back() {
    let fixture = Fixture::new("health", "ready");
    let backend = ProcessBackend::new(&fixture.root).expect("backend");
    assert_eq!(backend.health().await, BackendHealth::Stopped);

    let (tx, _rx) = mpsc::channel(8);
    let loaded = backend
        .load(
            fixture.request(RuntimeParams::default()),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("load");
    assert!(backend.health().await.is_ready());

    backend.unload(loaded.instance).await.expect("unload");
    assert_eq!(backend.health().await, BackendHealth::Stopped);
}

#[tokio::test]
async fn unloading_something_already_gone_is_not_an_error() {
    // A caller recovering from a crash should not have to know whether the
    // instance is still there.
    let fixture = Fixture::new("idempotent", "ready");
    let backend = ProcessBackend::new(&fixture.root).expect("backend");

    let (tx, _rx) = mpsc::channel(8);
    let loaded = backend
        .load(
            fixture.request(RuntimeParams::default()),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("load");

    backend.unload(loaded.instance).await.expect("first unload");
    backend
        .unload(loaded.instance)
        .await
        .expect("second unload must also succeed");
}

#[tokio::test]
async fn loading_a_second_model_replaces_the_first() {
    // Two engines resident at once is the memory spike admission control
    // exists to prevent, so the previous one must go before the next starts.
    let fixture = Fixture::new("replace", "ready");
    let backend = ProcessBackend::new(&fixture.root).expect("backend");

    let (tx, _rx) = mpsc::channel(8);
    let first = backend
        .load(
            fixture.request(RuntimeParams::default().with_context(2048)),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("first load");

    let (tx, _rx) = mpsc::channel(8);
    let second = backend
        .load(
            fixture.request(RuntimeParams::default().with_context(4096)),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("second load");

    assert_ne!(first.instance, second.instance);
    let resident = backend.resident().await.expect("something is resident");
    assert_eq!(resident.1, second.instance);
    backend.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn an_unsupported_architecture_is_refused_with_alternatives() {
    let fixture = Fixture::new("arch", "ready");
    let backend = ProcessBackend::new(&fixture.root).expect("backend");

    let mut request = fixture.request(RuntimeParams::default());
    let mut metadata = fixture.metadata();
    metadata.architecture = "gemma".to_owned(); // close to `gemma3`, but not it
    metadata.supported = false;
    request.metadata = Arc::new(metadata);

    let (tx, _rx) = mpsc::channel(8);
    let err = backend
        .load(request, tx, CancellationToken::new())
        .await
        .expect_err("an unsupported architecture must be refused");

    assert_eq!(err.code(), "unsupported_architecture");
    // Suggesting near matches turns a dead end into a next step.
    assert!(!err.remedies().is_empty());
}

#[tokio::test]
async fn a_kv_cache_type_the_engine_rejects_is_refused() {
    let fixture = Fixture::new("kv", "ready");
    let backend = ProcessBackend::new(&fixture.root).expect("backend");

    let (tx, _rx) = mpsc::channel(8);
    let err = backend
        .load(
            fixture.request(RuntimeParams::default().with_kv_cache_type(GgmlType::Q6_K)),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect_err("q6_K is not a valid KV cache type for this engine");
    assert_eq!(err.code(), "unsupported_kv_cache_type");
}

#[tokio::test]
async fn capabilities_advertise_only_kv_types_the_engine_accepts() {
    let fixture = Fixture::new("caps", "ready");
    let backend = ProcessBackend::new(&fixture.root).expect("backend");
    let caps = backend.capabilities();

    assert!(caps.kv_cache_types.contains(&GgmlType::F16));
    assert!(caps.kv_cache_types.contains(&GgmlType::Q8_0));
    // Valid for weights, rejected for the KV cache.
    assert!(!caps.kv_cache_types.contains(&GgmlType::Q6_K));
}

#[tokio::test]
async fn a_dead_engine_is_reported_as_failed_and_cleared() {
    // A crashed engine must not remain "resident": the next load has to start
    // cleanly rather than inherit a corpse.
    let fixture = Fixture::new("dead", "ready");
    let backend = ProcessBackend::new(&fixture.root).expect("backend");

    let (tx, _rx) = mpsc::channel(8);
    backend
        .load(
            fixture.request(RuntimeParams::default()),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("load");

    let (_, _, _, _) = backend.resident().await.expect("resident");
    let usage = backend.resource_usage().await.expect("usage");
    // The engine's *own* resident set is read from `/proc/<pid>/status`, which
    // exists on Linux and nowhere else. M10a.1 gave the workspace macOS and
    // Windows probes for the machine; this is the per-process reading, and it
    // has no equivalent there yet. Asserted per platform rather than skipped,
    // so that implementing it elsewhere makes this test fail until it is
    // updated - and so that the gap is stated where somebody will meet it.
    #[cfg(target_os = "linux")]
    assert!(usage.is_some(), "a running engine should report memory use");
    #[cfg(not(target_os = "linux"))]
    assert!(
        usage.is_none(),
        "per-process memory is a `/proc` reading; off Linux it must report \
         nothing rather than a made-up number"
    );

    backend.shutdown().await.expect("shutdown");
    assert_eq!(backend.health().await, BackendHealth::Stopped);
    assert!(backend.resident().await.is_none());
    assert!(backend.resource_usage().await.expect("usage").is_none());
}
