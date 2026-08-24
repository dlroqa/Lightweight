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
use hermes_system_info::FixedMemoryProbe;
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
            GatewayConfig::default(),
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
                let _ = axum::serve(listener, app).await;
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
