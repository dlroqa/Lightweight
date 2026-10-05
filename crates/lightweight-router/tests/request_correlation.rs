//! One request id from the client, through the router, into a real node's log.
//!
//! A test binary of its own, with one test: it installs the process-wide log
//! subscriber, which is the only way to read every log line reliably — a
//! subscriber scoped to one thread races with tests on other threads over
//! `tracing`'s per-callsite cache and silently misses lines.
//!
//! What it proves, against a real Lightweight gateway behind the router:
//!
//! * the router's and the node's lines about one request carry the same id —
//!   after a failover too, so the refused attempt and the answering node agree;
//! * the node echoes the id it was given, and closes each request with one
//!   `request finished` line saying how it ended;
//! * no prompt text reaches either log.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use lightweight_api::capabilities::{CapabilitiesBody, CapabilityModel};
use lightweight_backend_mock::{MockBackend, MockConfig};
use lightweight_core::ModelId;
use lightweight_gateway::catalog::{Catalog, ResidentModel};
use lightweight_gateway::{GatewayConfig, GatewayState};
use lightweight_inference::InferenceBackend;
use lightweight_router::config::RouterFile;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const N_CTX: u32 = 4096;

/// Log records, as the JSON lines the subscriber wrote.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Captured {
    fn records(&self) -> Vec<Value> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
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

/// A real gateway over the mock engine, serving one model as `alias`.
async fn real_node(alias: &str) -> (String, CancellationToken) {
    let backend = Arc::new(MockBackend::new(MockConfig::default()));
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
    let state = Arc::new(GatewayState::new(
        backend as Arc<dyn InferenceBackend>,
        catalog,
        GatewayConfig::default(),
    ));
    serve(lightweight_gateway::app(state)).await
}

/// A node that probes healthy and refuses every generation with a 503, keeping
/// the request id of each.
#[derive(Clone, Default)]
struct Refuser {
    ids: Arc<Mutex<Vec<String>>>,
    hits: Arc<AtomicU32>,
}

async fn refusing_node() -> (String, CancellationToken, Refuser) {
    let refuser = Refuser::default();
    let app = axum::Router::new()
        .route(
            "/v1/capabilities",
            get(async || {
                axum::Json(CapabilitiesBody::new(
                    "0.5.0",
                    Some(CapabilityModel {
                        id: "AliasA".into(),
                        context_length: N_CTX,
                    }),
                    1,
                ))
                .into_response()
            }),
        )
        .route(
            "/v1/chat/completions",
            post(
                async |axum::extract::State(refuser): axum::extract::State<Refuser>,
                       headers: HeaderMap|
                       -> Response {
                    refuser.hits.fetch_add(1, Ordering::SeqCst);
                    refuser.ids.lock().unwrap().push(
                        headers
                            .get("x-request-id")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or("")
                            .to_owned(),
                    );
                    (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        axum::Json(json!({"error": {"message": "busy",
                            "type": "server_error", "code": "server_busy"}})),
                    )
                        .into_response()
                },
            ),
        )
        .with_state(refuser.clone());
    let (base, stop) = serve(app).await;
    (base, stop, refuser)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn router_and_node_log_one_request_id_and_no_prompt() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let captured = Captured::default();
    tracing_subscriber::fmt()
        .json()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::INFO)
        .init();

    // a refuses before answering; b is a real gateway.
    let (a, _stop_a, refuser) = refusing_node().await;
    let (b, _stop_b) = real_node("QwenCoder").await;
    let file: RouterFile = serde_json::from_value(json!({
        "listen": ["127.0.0.1:0"],
        "health": {"interval_secs": 3600, "timeout_secs": 2, "failure_threshold": 2},
        "default_route": "Coder",
        "nodes": [{"id": "a", "url": a}, {"id": "b", "url": b}],
        "routes": [{"name": "Coder", "strategy": "priority", "deployments": [
            {"node": "a", "model": "AliasA"},
            {"node": "b", "model": "QwenCoder"}
        ]}]
    }))
    .expect("config shape");
    let config = lightweight_router::validate(file, &|_| None).expect("valid config");
    let bound = lightweight_router::bind(&config).await.expect("bind");
    let router = format!("http://{}", bound.addresses()[0]);
    let stop = CancellationToken::new();
    tokio::spawn(bound.serve(stop.clone()));
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client.get(format!("{router}/health")).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let marker = "PROMPT-MARKER-never-logged";
    for (id, stream) in [("corr-plain", false), ("corr-stream", true)] {
        let response = client
            .post(format!("{router}/v1/chat/completions"))
            .header("X-Request-Id", id)
            .json(&json!({"model": "Coder", "stream": stream,
                          "messages": [{"role": "user", "content": marker}]}))
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-request-id"], id);
        let _ = response.bytes().await;
        assert_eq!(
            refuser.ids.lock().unwrap().last().map(String::as_str),
            Some(id),
            "the refused attempt carried the same id"
        );
    }

    // The node echoes the id when asked directly, too.
    let direct = client
        .post(format!("{b}/v1/chat/completions"))
        .header("X-Request-Id", "corr-direct")
        .json(&json!({"model": "QwenCoder", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("request");
    assert_eq!(direct.headers()["x-request-id"], "corr-direct");
    let _ = direct.bytes().await;

    let line = |records: &[Value], id: &str, target: &str, message: &str| -> Option<Value> {
        records
            .iter()
            .find(|record| {
                record["target"] == target
                    && record["fields"]["message"] == message
                    && record["fields"]["request_id"] == id
            })
            .cloned()
    };
    let mut records = Vec::new();
    for _ in 0..250 {
        records = captured.records();
        if ["corr-plain", "corr-stream", "corr-direct"]
            .iter()
            .all(|id| {
                line(&records, id, "hermes::inference", "request finished").is_some()
                    && (*id == "corr-direct"
                        || line(&records, id, "hermes::router", "request finished").is_some())
            })
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    for id in ["corr-plain", "corr-stream"] {
        for (target, message) in [
            ("hermes::router", "deployment refused before answering"),
            ("hermes::router", "routed"),
            ("hermes::router", "request finished"),
            ("hermes::inference", "generating"),
            ("hermes::inference", "request finished"),
        ] {
            assert!(
                line(&records, id, target, message).is_some(),
                "{id}: no {target} {message:?} line"
            );
        }
        let node = line(&records, id, "hermes::inference", "request finished").unwrap();
        assert_eq!(node["fields"]["outcome"], "completed", "{node}");
        let router = line(&records, id, "hermes::router", "request finished").unwrap();
        assert_eq!(router["fields"]["deployment"], "b/QwenCoder", "{router}");
        assert_eq!(router["fields"]["attempts"], 2, "{router}");
    }
    assert!(
        line(
            &records,
            "corr-direct",
            "hermes::inference",
            "request finished"
        )
        .is_some(),
        "the node closes a request it was sent directly under the same id"
    );
    assert!(
        !captured.text().contains(marker),
        "a prompt reached the log"
    );
    stop.cancel();
}
