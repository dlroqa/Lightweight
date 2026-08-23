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

use futures_util::StreamExt as _;
use hermes_backend_llamacpp::backend::ProcessBackend;
use hermes_core::{Actionable, GgmlType, ModelId, RuntimeParams};
use hermes_gguf::{GgufFile, ModelMetadata};
use hermes_inference::generation::{
    ChatMessage, FinishReason, GenerationEvent, GenerationRequest, SamplingParams,
};
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

#[tokio::test]
async fn the_real_engine_streams_a_completion_and_reports_its_tokens() {
    // The translation from llama.cpp's SSE to our events is unit-tested against
    // a captured transcript. This is the other half: that the transcript is
    // still what a live engine produces at the pinned build.
    let Some(model) = model_path() else {
        eprintln!("skipping: set {MODEL_ENV} to a .gguf file to run this");
        return;
    };
    let profile = Profile::new("generate");
    let backend = ProcessBackend::new(profile.runtime_dir()).expect("backend");

    let params = RuntimeParams::default().with_context(2048);
    let (tx, _rx) = mpsc::channel(64);
    let loaded = backend
        .load(request(&model, params), tx, CancellationToken::new())
        .await
        .expect("load");

    let generation = GenerationRequest {
        messages: vec![
            ChatMessage::system("Answer in one short sentence."),
            ChatMessage::user("Name a colour."),
        ],
        max_tokens: Some(24),
        sampling: SamplingParams {
            // Pinned so the test is about the contract rather than about what
            // a 135M model feels like saying today.
            temperature: Some(0.0),
            seed: Some(7),
            ..SamplingParams::default()
        },
    };

    // Counted with the model's own chat template applied, which is the whole
    // reason this goes to the engine rather than being estimated here.
    let prompt_tokens = backend
        .count_prompt_tokens(loaded.instance, &generation)
        .await
        .expect("prompt token count");
    assert!(
        prompt_tokens > 0 && prompt_tokens < params.n_ctx,
        "implausible prompt token count: {prompt_tokens}"
    );

    let mut events = backend
        .generate(loaded.instance, generation, CancellationToken::new())
        .await
        .expect("generation");

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut started = false;
    let mut finished = None;
    let mut timings = None;
    while let Some(event) = events.next().await {
        match event.expect("no error mid-stream") {
            GenerationEvent::Started { .. } => started = true,
            GenerationEvent::ContentDelta { text } => content.push_str(&text),
            GenerationEvent::ReasoningDelta { text } => reasoning.push_str(&text),
            GenerationEvent::Timings(measured) => timings = Some(measured),
            GenerationEvent::Finished {
                finish_reason,
                usage,
            } => finished = Some((finish_reason, usage)),
            _ => {}
        }
    }

    assert!(started, "no start event arrived");
    // Content *or* reasoning. Insisting on content assumed a model that answers
    // immediately, and a thinking model given a small budget spends all of it
    // inside its reasoning - which is output, not silence. Qwen3 failed this
    // assertion while behaving perfectly; SmolLM2 still satisfies it through
    // the content branch.
    assert!(
        !content.is_empty() || !reasoning.is_empty(),
        "the engine produced neither content nor reasoning"
    );
    let (finish_reason, usage) = finished.expect("the stream ended without a finish event");
    assert!(matches!(
        finish_reason,
        FinishReason::Stop | FinishReason::Length
    ));
    // The counts must agree with the pre-flight count, or the gateway's
    // context arithmetic is being done against a different prompt.
    assert_eq!(usage.prompt_tokens, prompt_tokens);
    assert!(usage.completion_tokens > 0);
    assert_eq!(
        usage.total_tokens,
        usage.prompt_tokens + usage.completion_tokens
    );
    let timings = timings.expect("the engine reports timings on its usage chunk");
    assert!(timings.predicted_n > 0);

    backend.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn dropping_the_stream_stops_the_engine_decoding() {
    // The property every cancellation path depends on: the pinned build's
    // response reader cancels its task when the request goes away
    // (server-queue.h:218). If that ever stopped being true, a disconnected
    // client would leave the engine generating to nobody.
    let Some(model) = model_path() else {
        eprintln!("skipping: set {MODEL_ENV} to a .gguf file to run this");
        return;
    };
    let profile = Profile::new("cancel");
    let backend = ProcessBackend::new(profile.runtime_dir()).expect("backend");

    let params = RuntimeParams::default().with_context(2048);
    let (tx, _rx) = mpsc::channel(64);
    let loaded = backend
        .load(request(&model, params), tx, CancellationToken::new())
        .await
        .expect("load");
    let pid = backend
        .resource_usage()
        .await
        .expect("usage")
        .expect("an engine is running");
    assert!(pid.rss.get() > 0);

    let cancel = CancellationToken::new();
    let mut events = backend
        .generate(
            loaded.instance,
            GenerationRequest {
                messages: vec![ChatMessage::user("Write a long story about a robot.")],
                max_tokens: Some(500),
                sampling: SamplingParams::default(),
            },
            cancel.clone(),
        )
        .await
        .expect("generation");

    // Read a little, then walk away exactly as a disconnecting client does.
    let first = tokio::time::timeout(Duration::from_secs(120), events.next())
        .await
        .expect("the engine should produce something within two minutes");
    assert!(first.is_some());
    cancel.cancel();
    drop(events);

    // The engine must go idle. Measured from its own CPU time rather than by
    // waiting a fixed period and hoping.
    let cpu_time = |pid: u32| -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let fields: Vec<&str> = stat.rsplit(')').next()?.split_whitespace().collect();
        // utime and stime are fields 14 and 15 of `stat`, which are 11 and 12
        // after the comm field has been split off.
        Some(fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?)
    };
    let Some(engine_pid) = engine_pid(&backend).await else {
        backend.shutdown().await.expect("shutdown");
        return;
    };

    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = cpu_time(engine_pid).unwrap_or(0);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = cpu_time(engine_pid).unwrap_or(0);
    assert!(
        after.saturating_sub(before) <= 2,
        "the engine kept burning CPU after the client disconnected: {} ticks",
        after - before
    );

    backend.shutdown().await.expect("shutdown");
}

/// The engine's pid, read through the backend's own resource reporting.
async fn engine_pid(backend: &ProcessBackend) -> Option<u32> {
    // `resource_usage` proves an engine is running; the pid itself comes from
    // the only place that has it.
    backend.resource_usage().await.ok().flatten()?;
    std::fs::read_dir("/proc")
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .find(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|cmdline| cmdline.contains("llama-server"))
        })
}
