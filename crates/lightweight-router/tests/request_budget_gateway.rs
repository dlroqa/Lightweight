//! The pre-commit request budget (R9.3.2) against a real Lightweight gateway.
//!
//! The scripted nodes in `request_budget.rs` prove what the router does. These
//! tests ask what only a real node can answer, over the mock engine:
//!
//! * **B15 / I12** — a non-streamed request waiting in the node's own queue
//!   (the gateway allows 600 s) is cut by the router at the remaining budget.
//!   The router sees only a long wait for a response head; no node
//!   cooperation is needed.
//! * **B38 / B39** — a non-streamed generation longer than the budget: the
//!   client gets its 504 at the deadline, and the test measures whether the
//!   node's generation stops when the router drops the connection. The
//!   measured result is printed, asserted, and recorded in `docs/ROUTER.md`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use lightweight_backend_mock::{MockBackend, MockConfig, Script};
use lightweight_core::ModelId;
use lightweight_gateway::catalog::{Catalog, ResidentModel};
use lightweight_gateway::{GatewayConfig, GatewayState};
use lightweight_inference::InferenceBackend;
use lightweight_router::config::RouterFile;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const N_CTX: u32 = 4096;
const BUDGET_MS: u64 = 1_000;
const SLACK: Duration = Duration::from_millis(650);

fn ensure_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("client")
}

async fn serve(app: axum::Router) -> (String, CancellationToken) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let stop = CancellationToken::new();
    let stopping = stop.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, lightweight_gateway::service(app))
            .with_graceful_shutdown(async move { stopping.cancelled().await })
            .await;
    });
    (base, stop)
}

/// A real gateway over the mock engine, serving one model as `alias`, whose
/// every generation is `fragments` pieces `interval` apart. One generation
/// at a time, and the gateway's own 600 s queue, as shipped.
async fn real_node(
    alias: &str,
    fragments: usize,
    interval: Duration,
) -> (String, CancellationToken, Arc<MockBackend>) {
    let backend = Arc::new(MockBackend::new(MockConfig {
        script: Script::Content(vec!["tok ".to_owned(); fragments]),
        token_interval: interval,
        ..MockConfig::default()
    }));
    let model = ModelId::with_context("qwen2.5-coder-7b-instruct-q4_k_m", N_CTX);
    let loaded = backend.make_resident(model.clone(), N_CTX).await;
    let catalog = Arc::new(Catalog::with_resident(ResidentModel {
        id: model,
        alias: Some(alias.to_owned()),
        instance: loaded.instance,
        n_ctx: N_CTX,
        architecture: "mock".into(),
        param_count: Some(7_000_000_000),
        quantization: Some("Q4_K_M".into()),
        model_max_context_length: Some(32_768),
        ram_verdict: Some("safe".into()),
        backend: Some("mock".into()),
        model_path: "/models/qwen2.5-coder-7b-instruct-q4_k_m.gguf".into(),
        effective: lightweight_core::RuntimeParams::default(),
    }));
    let config = GatewayConfig::default();
    assert_eq!(config.max_concurrent_requests, 1);
    assert_eq!(config.queue_timeout, Duration::from_secs(600));
    let state = Arc::new(GatewayState::new(
        Arc::clone(&backend) as Arc<dyn InferenceBackend>,
        catalog,
        config,
    ));
    let (base, stop) = serve(lightweight_gateway::app(state)).await;
    (base, stop, backend)
}

struct Router {
    base: String,
    stop: CancellationToken,
}

impl Drop for Router {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Router {
    async fn start(node: &str, alias: &str) -> Self {
        ensure_provider();
        let file: RouterFile = serde_json::from_value(json!({
            "listen": ["127.0.0.1:0"],
            "health": {"interval_secs": 3600, "timeout_secs": 2, "failure_threshold": 1},
            "request": {"connect_timeout_secs": 5, "pre_commit_budget_ms": BUDGET_MS},
            "nodes": [{"id": "node", "url": node}],
            "routes": [{"name": "Coder", "deployments": [{"node": "node", "model": alias}]}]
        }))
        .expect("config shape");
        let config = lightweight_router::validate(file, &|_| None).expect("valid config");
        let bound = lightweight_router::bind(&config).await.expect("bind");
        let base = format!("http://{}", bound.addresses()[0]);
        let stop = CancellationToken::new();
        tokio::spawn(bound.serve(stop.clone()));
        for _ in 0..100 {
            if client().get(format!("{base}/health")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Self { base, stop }
    }

    async fn chat(&self) -> (u16, Value, Duration) {
        let started = Instant::now();
        let response = client()
            .post(format!("{}/v1/chat/completions", self.base))
            .json(&json!({"model": "Coder", "stream": false,
                          "messages": [{"role": "user", "content": "write at length"}]}))
            .send()
            .await
            .expect("request");
        let status = response.status().as_u16();
        let body = response.json().await.unwrap_or(Value::Null);
        (status, body, started.elapsed())
    }
}

/// Poll `in_flight` until it reaches zero or `limit` passes; how long it took.
async fn until_idle(backend: &MockBackend, limit: Duration) -> Option<Duration> {
    let started = Instant::now();
    while started.elapsed() < limit {
        if backend.generations_in_flight() == 0 {
            return Some(started.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

/// B38/B39 (#39–#41): a 10 s non-streamed generation under a 1 s budget.
///
/// The client gets its 504 at the deadline — never after the generation. Then
/// the measurement: once the router drops its connection, does the real node
/// stop generating? The node runs the generation inside its request handler,
/// and dropping the connection drops that handler; the mock engine's live
/// generation count shows whether the engine's stream went with it.
#[tokio::test]
async fn b38_b39_a_non_streamed_generation_is_cut_and_the_node_stops_generating() {
    let (node, _stop, backend) = real_node("Coder7B", 100, Duration::from_millis(100)).await;
    let router = Router::start(&node, "Coder7B").await;

    let (status, body, elapsed) = router.chat().await;
    assert_eq!(status, 504, "{body}");
    assert_eq!(body["error"]["code"], "request_budget_exhausted");
    assert!(
        elapsed >= Duration::from_millis(BUDGET_MS) - Duration::from_millis(50)
            && elapsed < Duration::from_millis(BUDGET_MS) + SLACK,
        "the 504 came at {elapsed:?}: it must be prompt, not after the 10 s generation"
    );
    assert_eq!(backend.generation_count(), 1, "the generation had started");

    let stopped = until_idle(&backend, Duration::from_secs(12)).await;
    eprintln!(
        "B38 measurement: 504 after {elapsed:?}; the node's generation stopped {stopped:?} \
         after the 504 (a full generation is 10 s)"
    );
    let stopped = stopped.expect("the node's generation never stopped");
    assert!(
        stopped < Duration::from_secs(2),
        "the node kept generating for {stopped:?} after the router dropped the request"
    );
}

/// B15 (#24, M16): one generation holds the node's only slot; a second,
/// non-streamed request waits in the node's queue, which would hold it up to
/// 600 s. The router cuts that wait at its budget.
#[tokio::test]
async fn b15_the_node_queue_never_outlasts_the_budget() {
    let (node, _stop, backend) = real_node("Coder7B", 60, Duration::from_millis(100)).await;
    let router = Router::start(&node, "Coder7B").await;

    // Occupy the slot directly at the node, for about six seconds.
    let busy = tokio::spawn({
        let node = node.clone();
        async move {
            client()
                .post(format!("{node}/v1/chat/completions"))
                .json(&json!({"model": "Coder7B", "stream": false,
                              "messages": [{"role": "user", "content": "occupy"}]}))
                .send()
                .await
                .map(|response| response.status().as_u16())
        }
    });
    for _ in 0..200 {
        if backend.generations_in_flight() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(backend.generations_in_flight(), 1, "the slot is taken");

    let (status, body, elapsed) = router.chat().await;
    assert_eq!(status, 504, "{body}");
    assert_eq!(body["error"]["code"], "request_budget_exhausted");
    assert!(
        elapsed < Duration::from_millis(BUDGET_MS) + SLACK,
        "queued for {elapsed:?}: the node's 600 s allowance must not hold the router"
    );
    assert_eq!(
        backend.generation_count(),
        1,
        "the queued request never reached the engine"
    );
    assert_eq!(
        busy.await.unwrap().ok(),
        Some(200),
        "the occupying request is untouched"
    );
}
