//! What the gateway spends, and what it says it spent.
//!
//! Every assertion here rests on a *given* machine rather than the one running
//! the tests. `MemoryProbe` has been a trait since M1 so that a verdict can be
//! asserted at all; until M7.2 the gateway was the only place that ignored it
//! and read the real `/proc/meminfo`, which made "was this load refused?" a
//! question about how much RAM the CI box happened to have free.

use std::sync::Arc;

use hermes_backend_mock::{MockBackend, MockConfig};
use hermes_catalog::CatalogStore;
use hermes_catalog::install::Installer;
use hermes_core::units::Bytes;
use hermes_gateway::manager::{ModelManager, RuntimeDefaults};
use hermes_gateway::{GatewayConfig, GatewayState};
use hermes_gguf::fixture::{GgufBuilder, TempDir};
use hermes_system_info::{FailingMemoryProbe, FixedMemoryProbe};
use serde_json::Value;

fn ensure_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A machine with `available_mib` free out of 64 GiB.
fn machine(available_mib: u64) -> Arc<FixedMemoryProbe> {
    Arc::new(FixedMemoryProbe::with_available(
        Bytes::from_gib(64),
        Bytes::from_mib(available_mib),
    ))
}

/// A gateway holding one fixture model, measured against a machine we choose.
async fn gateway(
    tag: &str,
    available_mib: u64,
    backend: MockBackend,
) -> (TempDir, Arc<GatewayState>, String) {
    let dir = TempDir::new(tag);
    let model_path = dir.write("fixture.gguf", &GgufBuilder::small_model("llama").build());

    let manager = Arc::new(ModelManager::new(
        CatalogStore::open(dir.path().join("catalog.json")).expect("catalog"),
        Installer::new(dir.path().join("models"), dir.path().join("downloads")).expect("installer"),
        RuntimeDefaults::default(),
    ));
    let installed = manager
        .register_at_startup(model_path)
        .await
        .expect("register the fixture");

    let state = Arc::new(
        GatewayState::new(
            Arc::new(backend),
            hermes_gateway::catalog::shared(None),
            GatewayConfig {
                // A data directory of its own, so a test can store a setting
                // and then watch a load obey it.
                paths: Some(hermes_system_info::DataPaths::rooted_at(dir.path())),
                ..GatewayConfig::default()
            },
        )
        .with_manager(Arc::clone(&manager))
        .with_memory_probe(machine(available_mib)),
    );
    let id = installed.id.clone();
    (dir, state, id)
}

struct Server {
    base: String,
    _task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(state: Arc<GatewayState>) -> Self {
        let app = hermes_gateway::app(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        Self {
            base: format!("http://127.0.0.1:{port}"),
            _task: tokio::spawn(async move {
                let _ = axum::serve(listener, hermes_gateway::service(app)).await;
            }),
        }
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let response = reqwest::Client::new()
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .expect("request");
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn text(&self, path: &str) -> String {
        reqwest::Client::new()
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .expect("request")
            .text()
            .await
            .expect("body")
    }

    async fn put(&self, path: &str, body: Value) -> (u16, Value) {
        let response = reqwest::Client::new()
            .put(format!("{}{path}", self.base))
            .json(&body)
            .send()
            .await
            .expect("request");
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let response = reqwest::Client::new()
            .post(format!("{}{path}", self.base))
            .json(&body)
            .send()
            .await
            .expect("request");
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    /// Load a model and wait for the job to settle.
    ///
    /// Through the job rather than by calling the manager directly: a refused
    /// load returns 202 and fails *inside* the job, so a test that stopped at
    /// the status code would call every refusal a success - which is exactly
    /// the mistake the panel made until M7.3.
    async fn load(&self, id: &str, body: Value) -> Value {
        let (status, accepted) = self.post(&format!("/api/v1/models/{id}/load"), body).await;
        assert_eq!(status, 202, "{accepted}");
        let job = accepted["job"].as_u64().expect("a job id");

        for _ in 0..200 {
            let (_, described) = self.get(&format!("/api/v1/jobs/{job}")).await;
            if described["status"]["state"] != "running" {
                return described["status"].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("the load job never settled");
    }
}

#[tokio::test]
async fn the_estimate_is_computed_against_the_machine_we_were_given() {
    ensure_provider();

    // The same model, the same context, two machines. Nothing else differs, so
    // the verdict is the machine's answer and not the fixture's.
    let (_small_dir, small, small_id) = gateway("mem-small", 64, MockBackend::default()).await;
    let (_large_dir, large, large_id) = gateway("mem-large", 32_768, MockBackend::default()).await;

    let small_server = Server::start(small).await;
    let (status, body) = small_server
        .get(&format!("/api/v1/models/{small_id}"))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["estimate"]["verdict"], "insufficient",
        "64 MiB free cannot hold an engine: {}",
        body["estimate"]
    );

    let large_server = Server::start(large).await;
    let (status, body) = large_server
        .get(&format!("/api/v1/models/{large_id}"))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["estimate"]["verdict"], "safe",
        "32 GiB free is not tight for a fixture model: {}",
        body["estimate"]
    );
}

#[tokio::test]
async fn an_estimate_the_gateway_could_not_compute_says_why() {
    ensure_provider();

    // A probe that cannot read is not the same as a model that does not fit,
    // and the panel has to be able to tell them apart. `Probed` is how, and the
    // `read` case flattens so nothing that already reads the estimate changes.
    let (_dir, state, id) = gateway("mem-read", 32_768, MockBackend::default()).await;
    let server = Server::start(state).await;
    let (status, body) = server.get(&format!("/api/v1/models/{id}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["estimate"]["state"], "read");
    assert!(body["estimate"]["total"].as_u64().is_some());
}

#[tokio::test]
async fn a_swap_is_not_refused_for_memory_the_outgoing_model_is_about_to_release() {
    ensure_provider();

    // The engine is stopped only after the new load is admitted, so the
    // outgoing model's memory is still spent when the verdict is taken. What it
    // is about to hand back is credited; what the kernel already counts as
    // available - the mmapped weights - is not.
    let backend = MockBackend::new(MockConfig {
        anon_rss: Some(Bytes::from_mib(2_048)),
        ..MockConfig::default()
    });
    let (_dir, state, id) = gateway("mem-swap", 64, backend).await;
    let server = Server::start(state).await;

    // Nothing resident: the credit does not apply, and this does not fit.
    let refused = server.load(&id, serde_json::json!({})).await;
    assert_eq!(
        refused["state"], "failed",
        "64 MiB free must not admit a load on its own: {refused}"
    );
    assert_eq!(refused["error"]["code"], "insufficient_memory");
    // A refusal that says only "it does not fit" is the one the error taxonomy
    // exists to prevent. The estimator computes what would make it fit, and
    // this is where those reach the user.
    let remedies = refused["error"]["remedies"]
        .as_array()
        .expect("a refusal must say what to do about it");
    assert!(!remedies.is_empty());
    assert!(
        remedies.iter().all(|remedy| remedy["label"].is_string()),
        "{remedies:?}"
    );

    // Force it resident, so there is something to reclaim. The same 64 MiB
    // plus 2 GiB about to be released is a different question, and gets a
    // different answer.
    let forced = server.load(&id, serde_json::json!({ "force": true })).await;
    assert_eq!(forced["state"], "succeeded", "{forced}");

    let swapped = server.load(&id, serde_json::json!({})).await;
    assert_eq!(
        swapped["state"], "succeeded",
        "a swap must be judged against the memory it will have: {swapped}"
    );
}

#[tokio::test]
async fn a_credit_is_never_taken_when_the_probe_could_not_read_it() {
    ensure_provider();

    // `None` is not zero and it is not a licence to guess. A kernel that does
    // not publish `RssAnon` must leave the swap judged exactly as it was before
    // the credit existed - pessimistic, and never optimistic.
    let backend = MockBackend::new(MockConfig {
        anon_rss: None,
        ..MockConfig::default()
    });
    let (_dir, state, id) = gateway("mem-nocredit", 64, backend).await;
    let server = Server::start(state).await;

    let forced = server.load(&id, serde_json::json!({ "force": true })).await;
    assert_eq!(forced["state"], "succeeded", "{forced}");

    let swapped = server.load(&id, serde_json::json!({})).await;
    assert_eq!(
        swapped["state"], "failed",
        "with no reading there is nothing to credit, so the answer must not change: {swapped}"
    );
    assert_eq!(swapped["error"]["code"], "insufficient_memory");
}

/// The remedy `MemoryError` advertises has to actually work.
///
/// Every variant of that error offers exactly one remedy - `ForceLoad` - and a
/// test in `memory.rs` has asserted so since M7. On this path it was a dead
/// end: `state.memory_snapshot()?` ran before `options.force` was ever read, so
/// the gateway told the user to force the load and then refused the forced load
/// for the same reason. The error was honest and the advice was unreachable.
///
/// Checked against the defect it exists for: restore the `?` on the probe and
/// this fails, while the refusal test below still passes.
#[tokio::test]
async fn a_load_can_be_forced_past_a_probe_that_cannot_read() {
    ensure_provider();

    let (_dir, state, id) = gateway("mem-probe-fails", 64, MockBackend::default()).await;
    let state = Arc::new(
        Arc::try_unwrap(state)
            .unwrap_or_else(|_| panic!("the gateway state is not shared yet"))
            .with_memory_probe(Arc::new(FailingMemoryProbe)),
    );
    let server = Server::start(Arc::clone(&state)).await;

    let forced = server.load(&id, serde_json::json!({ "force": true })).await;
    assert_eq!(
        forced["state"], "succeeded",
        "the one remedy the error offers must reach the load: {forced}"
    );

    // And what was skipped is admitted rather than papered over: a load nobody
    // judged must not report a verdict as though somebody had.
    let (_status, resident) = server.get("/api/v1/models/resident").await;
    assert!(
        resident["ram_verdict"].is_null(),
        "an unjudged load must not carry a verdict: {resident}"
    );
}

/// Without `--force`, a probe that cannot read is still a refusal.
///
/// The fix above must not have turned an unreadable machine into a silently
/// unjudged one for every caller - only for the caller who asked.
#[tokio::test]
async fn a_load_is_still_refused_when_the_probe_cannot_read_and_nobody_forced_it() {
    ensure_provider();

    let (_dir, state, id) = gateway("mem-probe-fails-hard", 64, MockBackend::default()).await;
    let state = Arc::new(
        Arc::try_unwrap(state)
            .unwrap_or_else(|_| panic!("the gateway state is not shared yet"))
            .with_memory_probe(Arc::new(FailingMemoryProbe)),
    );
    let server = Server::start(Arc::clone(&state)).await;

    let refused = server.load(&id, serde_json::json!({})).await;
    assert_eq!(refused["state"], "failed", "{refused}");
    assert_eq!(refused["error"]["code"], "memory_probe_failed", "{refused}");
}

#[tokio::test]
async fn the_metrics_report_the_engine_they_are_running() {
    ensure_provider();

    // The one number that makes a `Coarse` estimate checkable by the person
    // reading it: what the engine actually took, beside what we predicted it
    // would take. It had existed on the backend trait since M2 and reached no
    // endpoint until now.
    let (_dir, state, id) = gateway("mem-metrics", 32_768, MockBackend::default()).await;
    let server = Server::start(state).await;

    let (_, before) = server.get("/api/v1/metrics").await;
    assert_eq!(
        before["engine"]["state"], "unavailable",
        "with no engine there is no reading, and zero would be a lie: {}",
        before["engine"]
    );
    assert_eq!(before["engine"]["code"], "no_engine_running");

    let loaded = server.load(&id, serde_json::json!({})).await;
    assert_eq!(loaded["state"], "succeeded", "{loaded}");

    let (_, after) = server.get("/api/v1/metrics").await;
    assert_eq!(after["engine"]["state"], "read", "{}", after["engine"]);
    assert!(after["engine"]["rss"].as_u64().unwrap_or_default() > 0);
    assert!(after["engine"]["peak_rss"].as_u64().unwrap_or_default() > 0);
}

#[tokio::test]
async fn the_prometheus_text_carries_the_engine_only_once_it_is_measured() {
    ensure_provider();

    let (_dir, state, id) = gateway("mem-prom", 32_768, MockBackend::default()).await;
    let server = Server::start(state).await;

    let quiet = server.text("/metrics").await;
    assert!(
        !quiet.contains("hermes_engine_resident_bytes"),
        "an unmeasured gauge must be absent, not zero"
    );

    let loaded = server.load(&id, serde_json::json!({})).await;
    assert_eq!(loaded["state"], "succeeded", "{loaded}");

    let busy = server.text("/metrics").await;
    for gauge in [
        "hermes_engine_resident_bytes",
        "hermes_engine_peak_resident_bytes",
    ] {
        assert!(busy.contains(&format!("# TYPE {gauge} gauge")), "{busy}");
    }
}

#[tokio::test]
async fn the_estimate_on_the_detail_is_for_the_context_a_bare_load_would_pick() {
    ensure_provider();

    // These were two rules that disagreed: the load path read the stored
    // default and never `last_n_ctx`; the detail read `last_n_ctx` and never
    // the stored default. So the estimate on screen could be for a context no
    // button produced. One function decides now, and this is what proves it.
    let (_dir, state, id) = gateway("ctx-agree", 32_768, MockBackend::default()).await;
    let server = Server::start(state).await;

    let (status, saved) = server
        .put(
            "/api/v1/settings",
            serde_json::json!({ "gateway": { "default_n_ctx": 2048 } }),
        )
        .await;
    assert!(status == 200 || status == 204, "{saved}");

    let (_, detail) = server.get(&format!("/api/v1/models/{id}")).await;
    let shown = detail["estimate"]["params"]["n_ctx"]
        .as_u64()
        .expect("a context");
    assert_eq!(detail["context_source"], "setting", "{detail}");

    let loaded = server.load(&id, serde_json::json!({})).await;
    assert_eq!(loaded["state"], "succeeded", "{loaded}");
    let (_, metrics) = server.get("/api/v1/metrics").await;
    assert_eq!(
        metrics["model"]["n_ctx"].as_u64(),
        Some(shown),
        "the detail promised {shown} and the load used {}",
        metrics["model"]["n_ctx"]
    );
}

#[tokio::test]
async fn a_previous_context_does_not_steer_the_next_load() {
    ensure_provider();

    // `last_n_ctx` is history. Honouring it would quietly disable sizing the
    // window to the machine, which is the whole argument for measuring rather
    // than defaulting.
    let (_dir, state, id) = gateway("ctx-history", 32_768, MockBackend::default()).await;
    let server = Server::start(state).await;

    let first = server.load(&id, serde_json::json!({ "ctx": 1024 })).await;
    assert_eq!(first["state"], "succeeded", "{first}");

    let (_, detail) = server.get(&format!("/api/v1/models/{id}")).await;
    assert_eq!(detail["last_n_ctx"], 1024, "the history is recorded");
    assert_eq!(
        detail["context_source"], "fitted",
        "with no request and no setting, the machine decides: {detail}"
    );
    assert_ne!(
        detail["estimate"]["params"]["n_ctx"], 1024,
        "a 32 GiB machine can give this fixture more than it last used"
    );
}

#[tokio::test]
async fn the_estimate_follows_the_options_the_caller_is_weighing() {
    ensure_provider();

    // The panel has to be able to price a choice before making it. Doing the
    // arithmetic client-side would mean a second implementation of ggml block
    // geometry, waiting to disagree with what the engine allocates.
    let (_dir, state, id) = gateway("ctx-weigh", 32_768, MockBackend::default()).await;
    let server = Server::start(state).await;

    let kv_at = |body: &Value| body["estimate"]["kv_cache"].as_u64().expect("kv");

    let (_, small) = server.get(&format!("/api/v1/models/{id}?ctx=1024")).await;
    let (_, large) = server.get(&format!("/api/v1/models/{id}?ctx=2048")).await;
    assert_eq!(
        kv_at(&large),
        kv_at(&small) * 2,
        "the KV cache is linear in context"
    );
    assert_eq!(small["context_source"], "requested");

    let (_, quantized) = server
        .get(&format!("/api/v1/models/{id}?ctx=2048&kv_type=q8_0"))
        .await;
    // The same identity the unit tier pins, now proven through HTTP: q8_0 is
    // 34 bytes per 32 elements against f16's 64, not half.
    assert_eq!(kv_at(&quantized) * 64, kv_at(&large) * 34);
}

#[tokio::test]
async fn a_rejected_kv_type_names_the_endpoint_that_lists_them() {
    ensure_provider();

    // The 400 used to point at /health, which has never listed them. A message
    // naming an endpoint is only worth having if the endpoint answers, so this
    // checks both halves — and would fail again if either drifted.
    let (_dir, state, id) = gateway("kv-list", 32_768, MockBackend::default()).await;
    let server = Server::start(state).await;

    let (status, refused) = server
        .post(
            &format!("/api/v1/models/{id}/load"),
            serde_json::json!({ "kv_type": "q8_o" }),
        )
        .await;
    assert_eq!(status, 400, "{refused}");
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("/api/v1/gateway"),
        "the refusal must name where to look: {message}"
    );

    let (_, report) = server.get("/api/v1/gateway").await;
    let offered = report["engine_capabilities"]["kv_cache_types"]
        .as_array()
        .expect("the endpoint the message names must answer");
    assert!(!offered.is_empty());
    assert!(
        offered.iter().any(|kind| kind == "f16"),
        "the offered list must contain a type a load would accept: {offered:?}"
    );
    assert!(report["defaults"]["kv_type"].is_string());
}

#[tokio::test]
async fn a_larger_physical_batch_is_priced_before_it_is_chosen() {
    ensure_provider();

    // `n_ubatch` is the one runtime knob that changes both throughput and the
    // memory estimate — compute buffers scale with it and with nothing else in
    // `RuntimeParams`. That is why it joins `ctx` and `kv_type` as a query
    // parameter, and why `threads` does not.
    let (_dir, state, id) = gateway("ubatch-price", 32_768, MockBackend::default()).await;
    let server = Server::start(state).await;

    let compute_at = |body: &Value| body["estimate"]["compute"].as_u64().expect("compute");

    let (_, small) = server
        .get(&format!("/api/v1/models/{id}?ctx=2048&ubatch=128"))
        .await;
    let (_, large) = server
        .get(&format!("/api/v1/models/{id}?ctx=2048&ubatch=512"))
        .await;

    assert_eq!(
        compute_at(&large),
        compute_at(&small) * 4,
        "compute buffers are linear in the physical batch"
    );
    // And the KV cache is not: it is a property of the context, and a caller
    // weighing a batch size must not be shown a KV cache that moved.
    assert_eq!(
        large["estimate"]["kv_cache"], small["estimate"]["kv_cache"],
        "the batch size does not change the KV cache"
    );
}

#[tokio::test]
async fn an_absent_ubatch_leaves_the_response_exactly_as_it_was() {
    ensure_provider();

    // The additive rule, at the surface a client sees: adding a query parameter
    // must not change the answer given to a caller that does not use it.
    let (_dir, state, id) = gateway("ubatch-absent", 32_768, MockBackend::default()).await;
    let server = Server::start(state).await;

    let (_, plain) = server.get(&format!("/api/v1/models/{id}?ctx=2048")).await;
    let (_, explicit) = server
        .get(&format!("/api/v1/models/{id}?ctx=2048&ubatch=512"))
        .await;
    assert_eq!(
        plain["estimate"], explicit["estimate"],
        "512 is the default, so naming it changes nothing"
    );
}

#[tokio::test]
async fn a_load_mode_the_engine_does_not_accept_is_refused_by_name() {
    ensure_provider();

    let (_dir, state, id) = gateway("load-mode", 32_768, MockBackend::default()).await;
    let server = Server::start(state).await;

    let (status, refused) = server
        .post(
            &format!("/api/v1/models/{id}/load"),
            serde_json::json!({ "load_mode": "mmap+lock" }),
        )
        .await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["error"]["param"], "load_mode");

    // The endpoint the message names must actually list them.
    let (status, gateway_report) = server.get("/api/v1/gateway").await;
    assert_eq!(status, 200);
    let modes = gateway_report["defaults"]["load_modes"]
        .as_array()
        .expect("the load modes are listed");
    assert!(
        modes.iter().any(|mode| mode == "mmap+mlock"),
        "{gateway_report}"
    );
    assert!(modes.iter().any(|mode| mode == "auto"));
}

#[tokio::test]
async fn a_measurement_of_this_machine_reaches_the_estimate_on_screen() {
    ensure_provider();

    // Every estimate this product has ever produced used the shipped
    // coefficients: `Confidence::Measured` was unreachable, and the panel's
    // "this is an upper bound rather than a prediction" notice was permanent.
    // This is the whole of M10b in one assertion - a fit written for this
    // machine, this engine and these settings changes the number on screen and
    // says so.
    let (dir, state, id) = gateway("mem-calibrated", 32_768, MockBackend::default()).await;
    let server = Server::start(state.clone()).await;

    let (status, before) = server.get(&format!("/api/v1/models/{id}")).await;
    assert_eq!(status, 200, "{before}");
    assert_eq!(
        before["estimate"]["confidence"], "coarse",
        "with no calibration file the shipped coefficients must still be used"
    );
    let coarse_compute = before["estimate"]["compute"].as_u64().expect("compute");
    let coarse_overhead = before["estimate"]["overhead"].as_u64().expect("overhead");

    // The bucket is read back out of the estimate the gateway just produced,
    // rather than assumed here: a fit is keyed by the settings a load would
    // actually use, and guessing them would test the guess. Built through
    // `bucket_for`, which is the same function the load path looks up with.
    let params: hermes_core::RuntimeParams =
        serde_json::from_value(before["estimate"]["params"].clone()).expect("the params");
    let n_ubatch = params.n_ubatch;
    let file = hermes_gguf::GgufFile::open(dir.path().join("fixture.gguf")).expect("the fixture");
    let metadata = hermes_gguf::ModelMetadata::from_file(&file).expect("its metadata");
    let bucket = hermes_bench::apply::bucket_for(&metadata, params);

    // A measurement below the shipped guesses, so that "the fit was used" and
    // "the fit was ignored" cannot look alike - and derived from what the
    // gateway just reported rather than picked, because the fixture is a small
    // model and a number invented here could easily be *above* the guess.
    //
    // Halfway between the exactly-computed logits term, which no fit may go
    // under, and the coarse estimate's own slope.
    let logits_per_ubatch = metadata.vocab_size.expect("a vocabulary") as f64 * 4.0;
    let coarse_slope = coarse_compute as f64 / f64::from(n_ubatch);
    let slope = logits_per_ubatch + (coarse_slope - logits_per_ubatch) / 2.0;
    // The shipped engine baseline is 64 MiB and the headless host overhead is
    // 48 MiB. A fit sets the first and may never touch the second.
    let intercept = Bytes::from_mib(32).get() as f64;
    let points: Vec<hermes_bench::fit::ResidualPoint> = [n_ubatch / 2, n_ubatch, n_ubatch * 2]
        .into_iter()
        .map(|ubatch| hermes_bench::fit::ResidualPoint {
            n_ubatch: ubatch,
            residual_bytes: (slope * f64::from(ubatch) + intercept) as u64,
            peak_rss: Bytes::ZERO,
        })
        .collect();
    let mut calibration = hermes_bench::fit::Calibration::default();
    calibration.insert(hermes_bench::fit::Fit {
        at_unix: 1_700_000_000,
        machine: hermes_bench::MachineFingerprint::detect(),
        engine: hermes_bench::engine_fingerprint(&MockBackend::default()),
        bucket,
        compute_bytes_per_ubatch: Some(slope),
        overhead_bytes: Some(intercept),
        max_residual_bytes: points
            .iter()
            .map(|point| point.residual_bytes)
            .max()
            .unwrap_or_default(),
        points,
    });
    let paths = hermes_system_info::DataPaths::rooted_at(dir.path());
    paths.create_all().expect("the data directories");
    calibration
        .save(&paths.calibration_file())
        .expect("write the calibration");

    let (status, after) = server.get(&format!("/api/v1/models/{id}")).await;
    assert_eq!(status, 200, "{after}");
    assert_eq!(
        after["estimate"]["confidence"], "measured",
        "a fit for this machine, engine and bucket must be spent: {}",
        after["estimate"]
    );
    let measured_compute = after["estimate"]["compute"].as_u64().expect("compute");
    let measured_overhead = after["estimate"]["overhead"].as_u64().expect("overhead");
    assert!(
        measured_compute < coarse_compute,
        "the measurement was below the guess, so the estimate must come down: \
         {measured_compute} vs {coarse_compute}"
    );
    assert!(
        measured_overhead < coarse_overhead,
        "{measured_overhead} vs {coarse_overhead}"
    );

    // The exact half is not a fit's business, and never moves.
    assert_eq!(before["estimate"]["weights"], after["estimate"]["weights"]);
    assert_eq!(
        before["estimate"]["kv_cache"],
        after["estimate"]["kv_cache"]
    );
}

#[tokio::test]
async fn a_damaged_calibration_file_costs_nobody_their_estimate() {
    ensure_provider();

    // A benchmark artefact must never be able to break a load. The file is
    // read on the load path now, so "it will not parse" has to mean "the
    // shipped coefficients stand", not "no estimate".
    let (dir, state, id) = gateway("mem-damaged", 32_768, MockBackend::default()).await;
    let paths = hermes_system_info::DataPaths::rooted_at(dir.path());
    paths.create_all().expect("the data directories");
    std::fs::write(paths.calibration_file(), b"{ this is not json").expect("write");

    let server = Server::start(state).await;
    let (status, body) = server.get(&format!("/api/v1/models/{id}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["estimate"]["state"], "read");
    assert_eq!(body["estimate"]["confidence"], "coarse");
    assert!(body["estimate"]["total"].as_u64().is_some());
}
