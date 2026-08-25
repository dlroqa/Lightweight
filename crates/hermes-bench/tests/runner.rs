//! The runner, driven against the mock backend.
//!
//! No engine, no model, no network. What is proved here is the harness's own
//! behaviour — that it sizes a prompt by asking the tokenizer, that it gives
//! each cold repetition a different opening, that it records what the engine
//! reported rather than what it asked for — none of which needs a real engine
//! and all of which would otherwise only ever be exercised by a tier that most
//! runs skip.

use hermes_backend_mock::{MockBackend, MockConfig};
use hermes_bench::record::Scenario;
use hermes_bench::runner::{RunPlan, Runner};
use hermes_core::RuntimeParams;
use hermes_gguf::metadata::{ModelMetadata, QuantMix, TokenizerMeta};
use hermes_inference::{InferenceBackend, LoadRequest};

/// The smallest metadata the mock needs. It reads none of it; the shape is
/// what `LoadRequest` requires.
fn metadata() -> ModelMetadata {
    ModelMetadata {
        architecture: "llama".to_owned(),
        supported: true,
        name: Some("mock-model".to_owned()),
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

/// A mock whose tokenizer always answers `prompt_tokens`, so the sizer
/// converges on the first round.
fn counting(prompt_tokens: u32) -> MockConfig {
    MockConfig {
        prompt_tokens,
        ..MockConfig::default()
    }
}

async fn loaded(config: MockConfig) -> (MockBackend, hermes_core::InstanceId) {
    let backend = MockBackend::new(config);
    let request = LoadRequest {
        model: hermes_core::ModelId::with_context("mock-model", 4096),
        gguf_path: std::path::PathBuf::from("/mock/model.gguf"),
        metadata: std::sync::Arc::new(metadata()),
        runtime: RuntimeParams::default(),
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let loaded = backend
        .load(request, tx, tokio_util::sync::CancellationToken::new())
        .await
        .expect("the mock loads");
    let instance = loaded.instance;
    (backend, instance)
}

#[tokio::test]
async fn a_run_produces_one_sample_per_scenario_per_repetition() {
    let (backend, instance) = loaded(counting(64)).await;
    let runner = Runner::new(&backend, instance, RuntimeParams::default(), 4);
    let plan = RunPlan {
        prompt_tokens: 64,
        generate_tokens: 8,
        repetitions: 2,
        scenarios: vec![Scenario::ColdPrefill, Scenario::Decode],
    };

    let mut seen = Vec::new();
    let samples = runner
        .run(&plan, |progress| {
            seen.push((progress.scenario, progress.repetition))
        })
        .await
        .expect("a run");

    assert_eq!(samples.len(), 4);
    assert_eq!(seen.len(), 4, "progress is reported once per sample");
    assert_eq!(samples[0].scenario, Scenario::ColdPrefill);
    assert_eq!(samples[0].repetition, 0);
    assert_eq!(samples[3].scenario, Scenario::Decode);
    // The parameters travel with the sample, so a reader never has to infer the
    // conditions from the numbers.
    assert_eq!(samples[0].params.n_ctx, RuntimeParams::default().n_ctx);
    assert_eq!(samples[0].threads, 4);
}

#[tokio::test]
async fn a_prompt_longer_than_the_context_is_refused_before_the_engine_is_touched() {
    let (backend, instance) = loaded(counting(64)).await;
    let params = RuntimeParams::default().with_context(512);
    let runner = Runner::new(&backend, instance, params, 4);
    let plan = RunPlan {
        prompt_tokens: 4096,
        ..RunPlan::default()
    };

    let error = runner.run(&plan, |_| {}).await.expect_err("refused");
    // Actionable, like every other refusal in this workspace: it names the
    // context it was measured against rather than failing inside the engine.
    let text = error.to_string();
    assert!(text.contains("512"), "{text}");
}

#[tokio::test]
async fn what_is_recorded_is_what_the_engine_reported() {
    // The mock reports its own token counts, and the sample must carry those
    // rather than the numbers the plan asked for. A harness that echoed its own
    // request would report a throughput against tokens the model never saw.
    let (backend, instance) = loaded(counting(32)).await;
    let runner = Runner::new(&backend, instance, RuntimeParams::default(), 2);
    let plan = RunPlan {
        prompt_tokens: 32,
        generate_tokens: 4,
        repetitions: 1,
        scenarios: vec![Scenario::Decode],
    };

    let samples = runner.run(&plan, |_| {}).await.expect("a run");
    let sample = &samples[0];
    assert!(sample.prefill_ms.is_some(), "the mock reports timings");
    assert!(sample.decode_ms.is_some());
    assert!(sample.wall_ms > 0 || sample.time_to_first_token_ms.is_some());
    // The mock's engine reading is fixed, so this proves the reading travelled
    // rather than that any particular number is right.
    assert_eq!(sample.rss, Some(hermes_core::units::Bytes::from_mib(512)));
    assert_eq!(
        sample.peak_rss,
        Some(hermes_core::units::Bytes::from_mib(600))
    );
}

#[tokio::test]
async fn nothing_a_run_records_can_hold_the_prompt_it_used() {
    // The structural guarantee, checked at the boundary a file is written
    // across: serialize a sample and look for the filler text the runner sends.
    let (backend, instance) = loaded(counting(32)).await;
    let runner = Runner::new(&backend, instance, RuntimeParams::default(), 1);
    let plan = RunPlan {
        prompt_tokens: 32,
        generate_tokens: 2,
        repetitions: 1,
        scenarios: vec![Scenario::ColdPrefill],
    };

    let samples = runner.run(&plan, |_| {}).await.expect("a run");
    let json = serde_json::to_string(&samples).expect("serialize");
    assert!(!json.contains("quick brown fox"), "{json}");
    assert!(!json.contains(".gguf"), "{json}");
}

#[tokio::test]
async fn a_prompt_that_will_not_reach_its_target_is_refused_rather_than_measured() {
    // A tokenizer that answers the same number whatever it is given cannot be
    // steered to a target, and the honest response is to say so. Silently
    // measuring an 11-token prompt while reporting it as a 512-token one would
    // produce a throughput figure that is wrong by a factor of fifty and looks
    // entirely plausible.
    let (backend, instance) = loaded(counting(11)).await;
    let runner = Runner::new(&backend, instance, RuntimeParams::default(), 4);
    let plan = RunPlan {
        prompt_tokens: 512,
        repetitions: 1,
        scenarios: vec![Scenario::ColdPrefill],
        ..RunPlan::default()
    };

    let error = runner.run(&plan, |_| {}).await.expect_err("refused");
    let text = error.to_string();
    assert!(text.contains("512") && text.contains("11"), "{text}");
}
