//! The router, end to end, over real sockets.
//!
//! Two kinds of node stand behind the router here:
//!
//! * **Real Lightweight gateways**, the same `lightweight_gateway::app` that
//!   `hermes serve` runs, over the deterministic mock engine and serving a
//!   model under a node-local alias. These prove the parts that only a real
//!   node can: that the node accepts the alias the router rewrote to, that its
//!   streams survive the relay, that its credential check passes, and that a
//!   client walking away really stops its generation.
//! * **Scripted nodes** that answer the capabilities probe like a gateway and
//!   then do exactly one thing wrong on demand — refuse with a 503, drop the
//!   connection mid-stream — and record what reached them. These prove the
//!   failure rules, which a healthy real node will not produce on cue.
//!
//! Every router here probes only when a test says so (`probe_now`), so a
//! health transition happens at a line of the test rather than at a moment on
//! the clock.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use lightweight_api::capabilities::{CapabilitiesBody, CapabilityModel};
use lightweight_backend_mock::{MockBackend, MockConfig, Script};
use lightweight_core::{ModelId, SseDecoder, SseEvent};
use lightweight_gateway::catalog::{Catalog, ResidentModel};
use lightweight_gateway::{AuthPolicy, GatewayConfig, GatewayState};
use lightweight_inference::InferenceBackend;
use lightweight_router::RouterState;
use lightweight_router::config::RouterFile;
use lightweight_router::domain::{NodeHealth, NodeId};
use lightweight_router::metrics::Outcome;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn ensure_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client")
}

/// A server that can be stopped and started again on the same port, the way
/// a machine is rebooted.
struct Served {
    port: u16,
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Served {
    async fn start(app: axum::Router, port: u16) -> Self {
        // A port just released can take a moment to be bindable again on some
        // platforms; retry briefly rather than flake.
        let mut listener = None;
        for _ in 0..100 {
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(bound) => {
                    listener = Some(bound);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        let listener = listener.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let stop = CancellationToken::new();
        let stopping = stop.clone();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, lightweight_gateway::service(app))
                .with_graceful_shutdown(async move { stopping.cancelled().await })
                .await;
        });
        Self {
            port,
            stop,
            task: Some(task),
        }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Stop accepting and close idle connections: the node is gone.
    async fn shutdown(&mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
    }
}

// --- real gateways ----------------------------------------------------------

const N_CTX: u32 = 4096;

struct RealNode {
    served: Served,
    backend: Arc<MockBackend>,
    state: Arc<GatewayState>,
}

impl RealNode {
    /// A gateway serving one model under the node-local `alias`.
    async fn start(alias: &str, mock: MockConfig, gateway: GatewayConfig) -> Self {
        Self::start_on(0, alias, mock, gateway).await
    }

    async fn start_on(port: u16, alias: &str, mock: MockConfig, gateway: GatewayConfig) -> Self {
        let backend = Arc::new(MockBackend::new(mock));
        // The canonical id is deliberately unlike the alias and unlike any
        // route name: if it ever reaches a client, a test sees it.
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
            Arc::clone(&backend) as Arc<dyn InferenceBackend>,
            catalog,
            gateway,
        ));
        let served = Served::start(lightweight_gateway::app(Arc::clone(&state)), port).await;
        Self {
            served,
            backend,
            state,
        }
    }

    fn base(&self) -> String {
        self.served.base()
    }
}

// --- scripted nodes ---------------------------------------------------------

/// What a scripted node does with a generation request.
#[derive(Clone)]
enum Act {
    /// Answer as a gateway would, echoing back the `model` it was sent.
    Answer,
    /// Refuse with this status and body before any output.
    Refuse(u16, Value),
    /// Start a stream, send one chunk, then drop the connection.
    StreamThenDrop,
    /// Stay in flight until the gate is opened, then answer. A streamed
    /// request gets its first chunk at once and the rest after the gate.
    Hold(Arc<tokio::sync::Semaphore>),
}

/// A gate a held request waits behind.
fn gate() -> Arc<tokio::sync::Semaphore> {
    Arc::new(tokio::sync::Semaphore::new(0))
}

fn open(gate: &tokio::sync::Semaphore) {
    gate.add_permits(10_000);
}

/// What reached a scripted node.
#[derive(Clone, Debug)]
struct Seen {
    headers: HeaderMap,
    body: Value,
}

#[derive(Clone)]
struct NodeScript {
    serving: String,
    act: Act,
    /// What the capabilities probe reports.
    tools: bool,
    context_length: u32,
    /// The slot count the probe advertises.
    limit: u32,
    /// What the probe stops advertising, changeable while the node runs, as a
    /// hot swap would change it.
    withheld: Arc<Mutex<Withheld>>,
    hits: Arc<AtomicU32>,
    seen: Arc<Mutex<Vec<Seen>>>,
    probes: Arc<Mutex<Vec<HeaderMap>>>,
}

struct FakeNode {
    served: Served,
    script: NodeScript,
}

impl FakeNode {
    async fn start(serving: &str, act: Act) -> Self {
        Self::start_with(serving, act, true, 2048).await
    }

    /// A node reporting its own tool support and context, so two deployments
    /// of one route can genuinely differ.
    async fn start_with(serving: &str, act: Act, tools: bool, context_length: u32) -> Self {
        Self::start_full(0, serving, act, tools, context_length, 1).await
    }

    /// A node advertising `limit` concurrent requests, on `port` (0 for any).
    async fn start_limited(port: u16, serving: &str, act: Act, limit: u32) -> Self {
        Self::start_full(port, serving, act, true, 2048, limit).await
    }

    async fn start_full(
        port: u16,
        serving: &str,
        act: Act,
        tools: bool,
        context_length: u32,
        limit: u32,
    ) -> Self {
        let script = NodeScript {
            serving: serving.to_owned(),
            act,
            tools,
            context_length,
            limit,
            withheld: Arc::default(),
            hits: Arc::default(),
            seen: Arc::default(),
            probes: Arc::default(),
        };
        let app = axum::Router::new()
            .route("/v1/capabilities", get(fake_capabilities))
            .route("/v1/chat/completions", post(fake_generate))
            .route("/v1/completions", post(fake_generate))
            .with_state(script.clone());
        Self {
            served: Served::start(app, port).await,
            script,
        }
    }

    fn base(&self) -> String {
        self.served.base()
    }

    fn hits(&self) -> u32 {
        self.script.hits.load(Ordering::SeqCst)
    }

    /// Change what the next probe is told.
    fn withhold(&self, change: impl FnOnce(&mut Withheld)) {
        change(&mut self.script.withheld.lock().unwrap());
    }

    fn seen(&self) -> Vec<Seen> {
        self.script.seen.lock().unwrap().clone()
    }
}

async fn fake_capabilities(
    axum::extract::State(script): axum::extract::State<NodeScript>,
    headers: HeaderMap,
) -> Response {
    script.probes.lock().unwrap().push(headers);
    let mut body = CapabilitiesBody::new(
        "0.4.1",
        Some(CapabilityModel {
            id: script.serving.clone(),
            context_length: script.context_length,
        }),
        script.limit,
    );
    body.features.tools = script.tools;
    let withheld = *script.withheld.lock().unwrap();
    let features = &mut body.features;
    features.chat_completions &= !withheld.chat;
    features.completions &= !withheld.completions;
    features.tools &= !withheld.tools;
    features.tool_choice &= !withheld.tool_choice;
    features.reasoning_content &= !withheld.reasoning;
    if let (Some(context), Some(model)) = (withheld.context, body.state.model.as_mut()) {
        model.context_length = context;
    }
    axum::Json(body).into_response()
}

/// Features a scripted node stops advertising, and a context it reports
/// instead of its own. Nothing withheld by default.
#[derive(Clone, Copy, Debug, Default)]
struct Withheld {
    chat: bool,
    completions: bool,
    tools: bool,
    tool_choice: bool,
    reasoning: bool,
    context: Option<u32>,
}

async fn fake_generate(
    axum::extract::State(script): axum::extract::State<NodeScript>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    script.hits.fetch_add(1, Ordering::SeqCst);
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    script.seen.lock().unwrap().push(Seen {
        headers,
        body: body.clone(),
    });
    let model = body["model"].clone();
    match script.act {
        Act::Hold(ref gate) if body["stream"] == true => {
            let gate = Arc::clone(gate);
            let first = format!(
                "data: {}\n\n",
                json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                       "choices": [{"index": 0, "delta": {"content": "held"}}]})
            );
            let stream = futures_util::stream::unfold(0_u8, move |step| {
                let (gate, first) = (Arc::clone(&gate), first.clone());
                async move {
                    match step {
                        0 => Some((Ok::<_, std::io::Error>(first), 1)),
                        1 => {
                            let _ = gate.acquire().await;
                            Some((Ok("data: [DONE]\n\n".to_owned()), 2))
                        }
                        _ => None,
                    }
                }
            });
            (
                [("content-type", "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response()
        }
        Act::Hold(ref gate) => {
            let _ = gate.acquire().await;
            axum::Json(json!({
                "id": "c1",
                "object": "chat.completion",
                "model": model,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "held"},
                             "finish_reason": "stop"}],
            }))
            .into_response()
        }
        Act::Refuse(status, ref envelope) => (
            StatusCode::from_u16(status).unwrap(),
            axum::Json(envelope.clone()),
        )
            .into_response(),
        Act::Answer if body["stream"] == true => {
            let frames = [
                json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                       "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]}),
                json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                       "choices": [{"index": 0, "delta": {"content": "hi"}}]}),
            ];
            let mut text = String::from(": keep-alive\n\n");
            for frame in frames {
                text.push_str(&format!("data: {frame}\n\n"));
            }
            text.push_str("data: [DONE]\n\n");
            ([("content-type", "text/event-stream")], text).into_response()
        }
        Act::Answer => axum::Json(json!({
            "id": "c1",
            "object": "chat.completion",
            "model": model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"},
                         "finish_reason": "stop"}],
        }))
        .into_response(),
        Act::StreamThenDrop => {
            let first = format!(
                "data: {}\n\n",
                json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                       "choices": [{"index": 0, "delta": {"content": "Here is the"}}]})
            );
            let stream = futures_util::stream::unfold(0_u8, move |step| {
                let first = first.clone();
                async move {
                    match step {
                        0 => Some((Ok::<_, std::io::Error>(first), 1)),
                        1 => {
                            // Let the first chunk reach the router before the
                            // connection dies, as it would from a node that
                            // crashed mid-answer.
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Some((Err(std::io::Error::other("the node died")), 2))
                        }
                        _ => None,
                    }
                }
            });
            (
                [("content-type", "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response()
        }
    }
}

// --- the router -------------------------------------------------------------

struct Router {
    base: String,
    state: Arc<RouterState>,
    _stop: StopOnDrop,
}

struct StopOnDrop(CancellationToken);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Router {
    /// Start a router from `config`, resolving `api_key_env` names from `env`.
    ///
    /// Probes run only when a test calls [`Router::probe`], apart from the one
    /// every router makes before its first request.
    async fn start(mut config: Value, env: &[(&str, &str)]) -> Self {
        config["listen"] = json!(["127.0.0.1:0"]);
        if config.get("health").is_none() {
            config["health"] =
                json!({"interval_secs": 3600, "timeout_secs": 2, "failure_threshold": 2});
        }
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let file: RouterFile = serde_json::from_value(config).expect("config shape");
        let config = lightweight_router::validate(file, &|name| {
            env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
        })
        .expect("valid config");
        let bound = lightweight_router::bind(&config).await.expect("bind");
        let base = format!("http://{}", bound.addresses()[0]);
        let state = bound.state();
        let stop = CancellationToken::new();
        tokio::spawn(bound.serve(stop.clone()));
        let router = Self {
            base,
            state,
            _stop: StopOnDrop(stop),
        };
        // Wait for the first probe to finish and the router to serve.
        for _ in 0..100 {
            if client()
                .get(format!("{}/health", router.base))
                .send()
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        router
    }

    async fn probe(&self) {
        self.state.probe_now().await;
    }

    fn health(&self, node: &str) -> NodeHealth {
        self.state
            .health
            .status(&NodeId::parse(node).unwrap())
            .health
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        client()
            .post(format!("{}{path}", self.base))
            // What Lightagent and Hermes send when no key is configured.
            .header("Authorization", "Bearer no-key-required")
            .json(&body)
            .send()
            .await
            .expect("request")
    }

    async fn chat(&self, model: Option<&str>) -> (u16, Value) {
        let mut body = json!({"messages": [{"role": "user", "content": "hi"}]});
        if let Some(model) = model {
            body["model"] = json!(model);
        }
        let response = self.post("/v1/chat/completions", body).await;
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let response = client()
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .expect("request");
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }
}

fn two_node_config(primary: &str, fallback: &str) -> Value {
    json!({
        "default_route": "Fast",
        "nodes": [
            {"id": "node-a", "url": primary},
            {"id": "node-b", "url": fallback}
        ],
        "routes": [
            {"name": "Coder", "strategy": "priority", "deployments": [
                {"node": "node-a", "model": "QwenCoder"},
                {"node": "node-b", "model": "CoderBackup"}
            ]},
            {"name": "Fast", "deployments": [{"node": "node-b", "model": "CoderBackup"}]}
        ]
    })
}

fn decode(bytes: &[u8]) -> Vec<SseEvent> {
    let mut decoder = SseDecoder::new();
    decoder.feed(bytes).expect("valid SSE");
    decoder.drain()
}

async fn poll<T>(what: &str, mut check: impl AsyncFnMut() -> Option<T>) -> T {
    for _ in 0..250 {
        if let Some(value) = check().await {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

// --- discovery --------------------------------------------------------------

#[tokio::test]
async fn models_lists_routes_and_nothing_behind_them() {
    ensure_provider();
    let a = RealNode::start("QwenCoder", MockConfig::default(), GatewayConfig::default()).await;
    let b = RealNode::start(
        "CoderBackup",
        MockConfig::default(),
        GatewayConfig::default(),
    )
    .await;
    let router = Router::start(two_node_config(&a.base(), &b.base()), &[]).await;

    let (status, body) = router.get("/v1/models").await;
    assert_eq!(status, 200);
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["Coder", "Fast"]);
    for row in body["data"].as_array().unwrap() {
        assert_eq!(row["owned_by"], "lightweight-router");
        assert_eq!(row["object"], "model");
        // The context a client sizes prompts by, where it looks for it.
        assert_eq!(row["context_length"], N_CTX);
    }

    let text = body.to_string();
    for leak in [
        "QwenCoder",
        "CoderBackup",
        "node-a",
        "node-b",
        "127.0.0.1",
        "qwen2.5-coder",
        ".gguf",
    ] {
        assert!(
            !text.contains(leak),
            "{leak} leaked into /v1/models: {text}"
        );
    }
}

#[tokio::test]
async fn a_route_with_no_available_deployment_is_still_listed() {
    ensure_provider();
    let router = Router::start(
        two_node_config("http://127.0.0.1:9", "http://127.0.0.1:9"),
        &[],
    )
    .await;
    let (status, body) = router.get("/v1/models").await;
    assert_eq!(status, 200);
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert!(body["data"][0].get("context_length").is_none());
}

#[tokio::test]
async fn capabilities_speak_the_lightweight_contract_for_routes() {
    ensure_provider();
    let a = RealNode::start("QwenCoder", MockConfig::default(), GatewayConfig::default()).await;
    let router = Router::start(two_node_config(&a.base(), "http://127.0.0.1:9"), &[]).await;

    let (status, body) = router.get("/v1/capabilities").await;
    assert_eq!(status, 200);
    assert_eq!(body["protocol"]["name"], "lightweight-public-inference");
    assert_eq!(body["protocol"]["version"], 1);
    assert_eq!(body["server"]["name"], "Lightweight Router");
    assert_eq!(body["state"]["model_loaded"], true);
    // The default route's only deployment is down, so it is not offered as
    // the model a bare request would reach.
    assert!(body["state"].get("model").is_none());

    let routes = body["routes"].as_array().unwrap();
    assert_eq!(routes[0]["id"], "Coder");
    assert_eq!(routes[0]["available"], true);
    assert_eq!(routes[0]["features"]["streaming"], true);
    assert_eq!(routes[1]["id"], "Fast");
    assert_eq!(routes[1]["available"], false);
    assert_eq!(
        routes[1]["features"]["streaming"], false,
        "a route claims nothing it cannot currently send anywhere"
    );
    assert!(!body.to_string().contains("QwenCoder"));
}

// --- identity rewriting -----------------------------------------------------

#[tokio::test]
async fn a_chat_completion_is_sent_as_the_node_alias_and_answered_as_the_route() {
    ensure_provider();
    let a = RealNode::start("QwenCoder", MockConfig::default(), GatewayConfig::default()).await;
    let b = RealNode::start(
        "CoderBackup",
        MockConfig::default(),
        GatewayConfig::default(),
    )
    .await;
    let router = Router::start(two_node_config(&a.base(), &b.base()), &[]).await;

    let (status, body) = router.chat(Some("Coder")).await;
    assert_eq!(status, 200, "{body}");
    // The node accepted the request, which it would not have under the
    // route's name: it serves `QwenCoder` and refuses anything else.
    assert_eq!(body["model"], "Coder");
    assert_eq!(body["choices"][0]["message"]["content"], "Hello, world");
    assert_eq!(a.backend.generation_count(), 1, "the primary answered");
    assert_eq!(b.backend.generation_count(), 0);
    assert!(!body.to_string().contains("QwenCoder"));
}

#[tokio::test]
async fn the_rewrite_is_visible_on_the_wire() {
    ensure_provider();
    let node = FakeNode::start("QwenCoder", Act::Answer).await;
    let router = Router::start(
        json!({
            "nodes": [{"id": "n", "url": node.base()}],
            "routes": [{"name": "Coder", "deployments": [{"node": "n", "model": "QwenCoder"}]}]
        }),
        &[],
    )
    .await;

    let (status, body) = router.chat(Some("coder")).await;
    assert_eq!(status, 200);
    assert_eq!(body["model"], "Coder", "the route's own spelling");
    let seen = node.seen();
    assert_eq!(seen[0].body["model"], "QwenCoder");
    assert_eq!(
        seen[0].body["messages"][0]["content"], "hi",
        "everything else is forwarded as sent"
    );
}

#[tokio::test]
async fn a_text_completion_is_rewritten_both_ways() {
    ensure_provider();
    let a = RealNode::start("QwenCoder", MockConfig::default(), GatewayConfig::default()).await;
    let router = Router::start(
        json!({
            "nodes": [{"id": "a", "url": a.base()}],
            "routes": [{"name": "Coder", "deployments": [{"node": "a", "model": "QwenCoder"}]}]
        }),
        &[],
    )
    .await;

    let response = router
        .post(
            "/v1/completions",
            json!({"model": "Coder", "prompt": "def add(a, b):"}),
        )
        .await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "Coder");
    assert_eq!(body["object"], "text_completion");

    let response = router
        .post(
            "/v1/completions",
            json!({"model": "Coder", "prompt": "def add(a, b):", "stream": true}),
        )
        .await;
    let events = decode(&response.bytes().await.unwrap());
    assert!(events.last().unwrap().is_done());
    for event in events.iter().filter(|event| !event.is_done()) {
        let chunk: Value = serde_json::from_str(&event.data).unwrap();
        assert_eq!(chunk["model"], "Coder", "{chunk}");
    }
}

// --- streaming --------------------------------------------------------------

#[tokio::test]
async fn a_stream_is_relayed_with_every_chunk_renamed_and_done_kept() {
    ensure_provider();
    let a = RealNode::start("QwenCoder", MockConfig::default(), GatewayConfig::default()).await;
    let router = Router::start(
        json!({
            "nodes": [{"id": "a", "url": a.base()}],
            "routes": [{"name": "Coder", "deployments": [{"node": "a", "model": "QwenCoder"}]}]
        }),
        &[],
    )
    .await;

    let response = router
        .post(
            "/v1/chat/completions",
            json!({"model": "Coder", "stream": true,
                   "stream_options": {"include_usage": true},
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let raw = response.bytes().await.unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("QwenCoder"));

    let events = decode(&raw);
    assert!(events.last().unwrap().is_done(), "[DONE] is preserved");
    let chunks: Vec<Value> = events
        .iter()
        .filter(|event| !event.is_done())
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .collect();
    let content: String = chunks
        .iter()
        .filter_map(|chunk| chunk["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(content, "Hello, world", "in order, nothing lost");
    assert!(chunks.iter().all(|chunk| chunk["model"] == "Coder"));
    assert!(
        chunks.last().unwrap()["usage"].is_object(),
        "the usage chunk survives"
    );
}

#[tokio::test]
async fn a_stream_arrives_while_it_is_still_being_generated_and_a_disconnect_stops_the_node() {
    ensure_provider();
    // A generation that never ends: if the router buffered, the client would
    // never see a byte; if it did not propagate the disconnect, the node would
    // generate forever.
    let a = RealNode::start(
        "QwenCoder",
        MockConfig {
            script: Script::Endless {
                fragment: "tick ".into(),
                interval: Duration::from_millis(20),
            },
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;
    let router = Router::start(
        json!({
            "nodes": [{"id": "a", "url": a.base()}],
            "routes": [{"name": "Coder", "deployments": [{"node": "a", "model": "QwenCoder"}]}]
        }),
        &[],
    )
    .await;

    let mut response = router
        .post(
            "/v1/chat/completions",
            json!({"model": "Coder", "stream": true,
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(response.status(), 200);

    let mut received = Vec::new();
    while !String::from_utf8_lossy(&received).contains("tick") {
        let chunk = tokio::time::timeout(Duration::from_secs(5), response.chunk())
            .await
            .expect("output arrives before the generation ends")
            .expect("readable")
            .expect("not finished");
        received.extend_from_slice(&chunk);
    }
    assert!(String::from_utf8_lossy(&received).contains("\"model\":\"Coder\""));
    assert_eq!(
        router.state.metrics.active(),
        1,
        "the open stream is counted"
    );

    // The client walks away.
    drop(response);

    let snapshot = poll("the node to see the generation cancelled", async || {
        let snapshot = a.state.metrics_snapshot().await;
        (snapshot.finish_reasons.cancelled == 1).then_some(snapshot)
    })
    .await;
    assert_eq!(snapshot.finish_reasons.error, 0);
    assert_eq!(snapshot.queue.running, 0, "the node's slot came back");
    poll("the router to stop counting the stream", async || {
        (router.state.metrics.active() == 0).then_some(())
    })
    .await;
}

// --- default and unknown routes ---------------------------------------------

#[tokio::test]
async fn default_and_an_omitted_model_reach_the_configured_default_route() {
    ensure_provider();
    let a = RealNode::start("QwenCoder", MockConfig::default(), GatewayConfig::default()).await;
    let b = RealNode::start(
        "CoderBackup",
        MockConfig::default(),
        GatewayConfig::default(),
    )
    .await;
    let router = Router::start(two_node_config(&a.base(), &b.base()), &[]).await;

    for model in [Some("default"), Some("DEFAULT"), None] {
        let (status, body) = router.chat(model).await;
        assert_eq!(status, 200, "{model:?}: {body}");
        assert_eq!(
            body["model"], "Fast",
            "{model:?} is answered as the default route"
        );
    }
    assert_eq!(a.backend.generation_count(), 0);
    assert_eq!(
        b.backend.generation_count(),
        3,
        "Fast's deployment answered"
    );
}

#[tokio::test]
async fn without_a_default_route_default_and_an_omitted_model_are_refused() {
    ensure_provider();
    let a = RealNode::start("QwenCoder", MockConfig::default(), GatewayConfig::default()).await;
    let mut config = two_node_config(&a.base(), "http://127.0.0.1:9");
    config.as_object_mut().unwrap().remove("default_route");
    let router = Router::start(config, &[]).await;

    for model in [Some("default"), None] {
        let (status, body) = router.chat(model).await;
        assert_eq!(status, 400, "{model:?}");
        assert_eq!(body["error"]["code"], "no_default_route");
        assert_eq!(body["error"]["param"], "model");
    }
    assert_eq!(a.backend.generation_count(), 0, "nothing was guessed");
}

#[tokio::test]
async fn an_unknown_route_is_model_not_found_and_nothing_is_substituted() {
    ensure_provider();
    let a = RealNode::start("QwenCoder", MockConfig::default(), GatewayConfig::default()).await;
    let b = RealNode::start(
        "CoderBackup",
        MockConfig::default(),
        GatewayConfig::default(),
    )
    .await;
    let router = Router::start(two_node_config(&a.base(), &b.base()), &[]).await;

    let (status, body) = router.chat(Some("Research")).await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "model_not_found");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["param"], "model");
    // A node-local alias is not a route either.
    let (status, _) = router.chat(Some("QwenCoder")).await;
    assert_eq!(status, 404);
    assert_eq!(
        a.backend.generation_count() + b.backend.generation_count(),
        0
    );
    assert_eq!(
        router.state.metrics.requests(
            lightweight_router::metrics::UNKNOWN_ROUTE,
            Outcome::ClientError
        ),
        2
    );
}

#[tokio::test]
async fn a_known_route_with_nothing_available_is_route_unavailable() {
    ensure_provider();
    // Both nodes are offline when the router starts; it starts anyway.
    let router = Router::start(
        two_node_config("http://127.0.0.1:9", "http://127.0.0.1:9"),
        &[],
    )
    .await;
    assert_ne!(router.health("node-a"), NodeHealth::Healthy);

    let response = router
        .post(
            "/v1/chat/completions",
            json!({"model": "Coder", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers()["retry-after"], "3600");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "route_unavailable");
    assert_eq!(
        body["error"]["message"],
        "No healthy deployment is available for route \"Coder\"."
    );

    let (_, health) = router.get("/health").await;
    assert_eq!(health["status"], "unavailable");
}

// --- failover ---------------------------------------------------------------

#[tokio::test]
async fn a_refused_connection_fails_over_at_once_without_waiting_for_a_probe() {
    ensure_provider();
    let mut a = RealNode::start("QwenCoder", MockConfig::default(), GatewayConfig::default()).await;
    let b = RealNode::start(
        "CoderBackup",
        MockConfig::default(),
        GatewayConfig::default(),
    )
    .await;
    let router = Router::start(two_node_config(&a.base(), &b.base()), &[]).await;
    assert_eq!(router.health("node-a"), NodeHealth::Healthy);

    // The primary goes away between probes: the router still believes it is
    // healthy, tries it, and moves on.
    a.served.shutdown().await;
    // The probe interval is an hour: the only thing that can move this
    // request is the attempt itself, so a pass here proves the request did
    // not wait for a health check.
    let started = std::time::Instant::now();
    let (status, body) = router.chat(Some("Coder")).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["model"], "Coder",
        "the public name, whichever node answered"
    );
    assert_eq!(b.backend.generation_count(), 1);
    assert_eq!(router.state.metrics.failovers("Coder"), 1);

    // The failed attempt was recorded against the primary's health, and one
    // failure is below the threshold, so it stays eligible: the snapshot said
    // "eligible", the attempt said "not answering", and the request moved on.
    let primary = router
        .state
        .health
        .status(&NodeId::parse("node-a").unwrap());
    assert_eq!(primary.consecutive_failures, 1);
    assert_eq!(primary.health, NodeHealth::Healthy);
}

#[tokio::test]
async fn a_503_before_any_output_fails_over() {
    ensure_provider();
    let a = FakeNode::start(
        "QwenCoder",
        Act::Refuse(
            503,
            json!({"error": {"message": "busy", "type": "server_error", "code": "server_busy"}}),
        ),
    )
    .await;
    let b = FakeNode::start("CoderBackup", Act::Answer).await;
    let router = Router::start(two_node_config(&a.base(), &b.base()), &[]).await;

    let (status, body) = router.chat(Some("Coder")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["model"], "Coder");
    assert_eq!((a.hits(), b.hits()), (1, 1));
    assert_eq!(b.seen()[0].body["model"], "CoderBackup");
}

#[tokio::test]
async fn when_every_deployment_refuses_the_last_refusal_is_returned() {
    ensure_provider();
    let busy = json!({"error": {"message": "busy", "type": "server_error", "code": "server_busy"}});
    let a = FakeNode::start("QwenCoder", Act::Refuse(503, busy.clone())).await;
    let b = FakeNode::start("CoderBackup", Act::Refuse(503, busy)).await;
    let router = Router::start(two_node_config(&a.base(), &b.base()), &[]).await;

    let (status, body) = router.chat(Some("Coder")).await;
    assert_eq!(status, 503);
    assert_eq!(
        body["error"]["code"], "server_busy",
        "the node's own reason, not a generic one"
    );
    assert_eq!((a.hits(), b.hits()), (1, 1));
}

#[tokio::test]
async fn a_client_error_from_the_node_stands_and_is_not_retried_elsewhere() {
    ensure_provider();
    let overflow = json!({"error": {
        "message": "the request exceeds the available context size (2048 tokens)",
        "type": "invalid_request_error",
        "code": "context_length_exceeded"
    }});
    let a = FakeNode::start("QwenCoder", Act::Refuse(400, overflow.clone())).await;
    let b = FakeNode::start("CoderBackup", Act::Answer).await;
    let router = Router::start(two_node_config(&a.base(), &b.base()), &[]).await;

    let (status, body) = router.chat(Some("Coder")).await;
    assert_eq!(status, 400);
    assert_eq!(body, overflow, "forwarded unchanged, parseable as before");
    assert_eq!(b.hits(), 0);
}

#[tokio::test]
async fn a_node_that_stopped_serving_the_model_is_skipped_without_leaking_its_name() {
    ensure_provider();
    let gone = json!({"error": {
        "message": "the model \"QwenCoder\" is not loaded; this gateway is serving \"Other\"",
        "type": "invalid_request_error", "param": "model", "code": "model_not_found"
    }});
    let a = FakeNode::start("QwenCoder", Act::Refuse(404, gone)).await;
    let b = FakeNode::start("CoderBackup", Act::Answer).await;
    let router = Router::start(two_node_config(&a.base(), &b.base()), &[]).await;

    let (status, body) = router.chat(Some("Coder")).await;
    assert_eq!(status, 200);
    assert_eq!(body["model"], "Coder");
    assert!(!body.to_string().contains("QwenCoder"));
    // The router forgot what the primary was serving, so the next request
    // goes straight to the backup rather than failing the same way.
    router.chat(Some("Coder")).await;
    assert_eq!((a.hits(), b.hits()), (1, 2));
}

#[tokio::test]
async fn a_stream_that_fails_after_output_is_never_finished_by_another_node() {
    ensure_provider();
    let a = FakeNode::start("QwenCoder", Act::StreamThenDrop).await;
    let b = FakeNode::start("CoderBackup", Act::Answer).await;
    let router = Router::start(two_node_config(&a.base(), &b.base()), &[]).await;

    let response = router
        .post(
            "/v1/chat/completions",
            json!({"model": "Coder", "stream": true,
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(response.status(), 200);
    let events = decode(&response.bytes().await.unwrap_or_default());

    let first: Value = serde_json::from_str(&events[0].data).unwrap();
    assert_eq!(first["choices"][0]["delta"]["content"], "Here is the");
    assert_eq!(first["model"], "Coder");
    let last: Value = serde_json::from_str(&events.last().unwrap().data).unwrap();
    assert_eq!(last["error"]["code"], "upstream_stream_interrupted");
    assert!(
        !events.iter().any(SseEvent::is_done),
        "a failed stream must not look complete"
    );
    assert_eq!(b.hits(), 0, "no other node was asked to continue");
    assert_eq!(router.state.metrics.failovers("Coder"), 0);
}

// --- health -----------------------------------------------------------------

#[tokio::test]
async fn health_follows_the_node_down_and_back_and_priority_returns_to_the_primary() {
    ensure_provider();
    let mut a = RealNode::start("QwenCoder", MockConfig::default(), GatewayConfig::default()).await;
    let a_port = a.served.port;
    let b = RealNode::start(
        "CoderBackup",
        MockConfig::default(),
        GatewayConfig::default(),
    )
    .await;
    let router = Router::start(two_node_config(&a.base(), &b.base()), &[]).await;

    // 1. Both healthy: the primary answers.
    assert_eq!(router.chat(Some("Coder")).await.0, 200);
    assert_eq!(a.backend.generation_count(), 1);

    // 2. The primary stops. One failed probe is not enough to call it.
    a.served.shutdown().await;
    router.probe().await;
    assert_eq!(
        router.health("node-a"),
        NodeHealth::Healthy,
        "below the threshold"
    );
    router.probe().await;
    assert_eq!(router.health("node-a"), NodeHealth::Unhealthy);

    // 3. Requests go to the backup without trying the primary.
    let (status, body) = router.chat(Some("Coder")).await;
    assert_eq!(status, 200);
    assert_eq!(body["model"], "Coder");
    assert_eq!(b.backend.generation_count(), 1);
    assert_eq!(
        router.state.metrics.failovers("Coder"),
        0,
        "skipped by health, not by a failed attempt"
    );

    // 4. The primary comes back on the same address; one probe restores it,
    //    and priority puts it first again.
    let a = RealNode::start_on(
        a_port,
        "QwenCoder",
        MockConfig::default(),
        GatewayConfig::default(),
    )
    .await;
    router.probe().await;
    assert_eq!(router.health("node-a"), NodeHealth::Healthy);
    assert_eq!(router.chat(Some("Coder")).await.0, 200);
    assert_eq!(a.backend.generation_count(), 1, "back on the primary");
    assert_eq!(b.backend.generation_count(), 1);
}

#[tokio::test]
async fn something_that_is_not_lightweight_is_never_healthy() {
    ensure_provider();
    // A server answering 200 at the probe path, but not with the contract.
    let impostor = Served::start(
        axum::Router::new().route("/v1/capabilities", get(|| async { "{}" })),
        0,
    )
    .await;
    let router = Router::start(
        json!({
            "nodes": [{"id": "x", "url": impostor.base()}],
            "routes": [{"name": "Coder", "deployments": [{"node": "x", "model": "QwenCoder"}]}],
            "health": {"interval_secs": 3600, "timeout_secs": 2, "failure_threshold": 1}
        }),
        &[],
    )
    .await;
    assert_eq!(router.health("x"), NodeHealth::Unhealthy);
    assert_eq!(router.chat(Some("Coder")).await.0, 503);
}

// --- credentials and headers ------------------------------------------------

#[tokio::test]
async fn the_client_key_stays_at_the_router_and_each_node_gets_its_own() {
    ensure_provider();
    let a = FakeNode::start("QwenCoder", Act::Answer).await;
    let router = Router::start(
        json!({
            "api_key_env": "ROUTER_KEY",
            "nodes": [{"id": "a", "url": a.base(), "api_key_env": "NODE_A_KEY"}],
            "routes": [{"name": "Coder", "deployments": [{"node": "a", "model": "QwenCoder"}]}]
        }),
        &[
            ("ROUTER_KEY", "router-secret"),
            ("NODE_A_KEY", "node-a-secret"),
        ],
    )
    .await;

    // No key, or the node's key, is refused at the router.
    let (status, body) = router.chat(Some("Coder")).await;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["code"], "invalid_api_key");
    let refused = client()
        .post(format!("{}/v1/chat/completions", router.base))
        .header("Authorization", "Bearer node-a-secret")
        .json(&json!({"model": "Coder", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401);

    let response = client()
        .post(format!("{}/v1/chat/completions", router.base))
        .header("Authorization", "Bearer router-secret")
        .header("X-Request-Id", "trace-7f3a")
        .header("X-Internal-Secret", "do-not-forward")
        .header("Cookie", "session=abc")
        .json(&json!({"model": "Coder", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-request-id"], "trace-7f3a");

    let seen = &a.seen()[0];
    assert_eq!(seen.headers["authorization"], "Bearer node-a-secret");
    assert_eq!(
        seen.headers["x-request-id"], "trace-7f3a",
        "the trace survives the hop"
    );
    for header in ["x-internal-secret", "cookie"] {
        assert!(!seen.headers.contains_key(header), "{header} was forwarded");
    }
    let all: String = seen
        .headers
        .iter()
        .map(|(_, value)| value.to_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        !all.contains("router-secret"),
        "the client's key reached a node"
    );

    // Probes carry the node's key too.
    let probes = a.script.probes.lock().unwrap().clone();
    assert_eq!(probes[0]["authorization"], "Bearer node-a-secret");

    // The control API says a node has a key, never what it is.
    let control = client()
        .get(format!("{}/api/router/v1/nodes", router.base))
        .header("Authorization", "Bearer router-secret")
        .send()
        .await
        .unwrap();
    let text = control.text().await.unwrap();
    assert!(text.contains("\"auth\":\"bearer\""), "{text}");
    assert!(!text.contains("node-a-secret") && !text.contains("router-secret"));
}

#[tokio::test]
async fn a_real_node_requiring_a_key_is_reached_with_its_own_key() {
    ensure_provider();
    let a = RealNode::start(
        "QwenCoder",
        MockConfig::default(),
        GatewayConfig {
            auth: AuthPolicy::with_static_key("node-a-secret".into()),
            ..GatewayConfig::default()
        },
    )
    .await;
    let config = json!({
        "nodes": [{"id": "a", "url": a.base(), "api_key_env": "NODE_A_KEY"}],
        "routes": [{"name": "Coder", "deployments": [{"node": "a", "model": "QwenCoder"}]}],
        "health": {"interval_secs": 3600, "timeout_secs": 2, "failure_threshold": 1}
    });

    let right = Router::start(config.clone(), &[("NODE_A_KEY", "node-a-secret")]).await;
    assert_eq!(right.health("a"), NodeHealth::Healthy);
    let (status, body) = right.chat(Some("Coder")).await;
    assert_eq!(status, 200, "{body}");

    let wrong = Router::start(config, &[("NODE_A_KEY", "stale")]).await;
    assert_eq!(wrong.health("a"), NodeHealth::Unhealthy);
    let status = wrong.state.health.status(&NodeId::parse("a").unwrap());
    assert!(status.last_error.unwrap().contains("401"));
}

// --- the control API --------------------------------------------------------

#[tokio::test]
async fn the_control_api_shows_routes_deployments_and_health() {
    ensure_provider();
    let a = RealNode::start("QwenCoder", MockConfig::default(), GatewayConfig::default()).await;
    let router = Router::start(two_node_config(&a.base(), "http://127.0.0.1:9"), &[]).await;

    let (status, routes) = router.get("/api/router/v1/routes").await;
    assert_eq!(status, 200);
    assert_eq!(routes["default_route"], "Fast");
    let coder = &routes["data"][0];
    assert_eq!(coder["name"], "Coder");
    assert_eq!(coder["strategy"], "priority");
    assert_eq!(coder["deployments"][0]["node"], "node-a");
    assert_eq!(coder["deployments"][0]["model"], "QwenCoder");
    assert_eq!(coder["deployments"][0]["priority"], 1);
    assert_eq!(coder["deployments"][0]["available"], true);
    assert_eq!(coder["deployments"][1]["available"], false);
    assert_eq!(
        coder["deployments"][1]["unavailable_reason"],
        "node_unknown"
    );

    let (_, nodes) = router.get("/api/router/v1/nodes").await;
    assert_eq!(nodes["data"][0]["health"], "healthy");
    assert_eq!(nodes["data"][0]["serving"]["id"], "QwenCoder");
    assert!(nodes["data"][0]["last_seen"].is_u64());

    let (_, deployments) = router.get("/api/router/v1/deployments").await;
    let shared = deployments["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "node-b/CoderBackup")
        .unwrap();
    assert_eq!(shared["routes"], json!(["Coder", "Fast"]));

    let (_, detail) = router.get("/api/router/v1/health").await;
    assert_eq!(detail["routes"][0]["available"], true);
    assert_eq!(detail["routes"][1]["available"], false);

    let metrics = client()
        .get(format!("{}/metrics", router.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("router_node_health{node=\"node-a\"} 1"),
        "{metrics}"
    );
}

// --- final review: capability state, identity, errors, recovery -------------

#[tokio::test]
async fn per_deployment_capabilities_survive_and_the_route_reports_the_eligible_set() {
    ensure_provider();
    let a = FakeNode::start_with("QwenCoder", Act::Answer, true, 32_768).await;
    let mut b = FakeNode::start_with("CoderBackup", Act::Answer, false, 8_192).await;
    let router = Router::start(two_node_config(&a.base(), &b.base()), &[]).await;

    // Both eligible: the route promises the intersection, and the smaller
    // context, because either may receive the request.
    let (_, models) = router.get("/v1/models").await;
    assert_eq!(models["data"][0]["context_length"], 8_192);
    let (_, caps) = router.get("/v1/capabilities").await;
    assert_eq!(caps["routes"][0]["features"]["tools"], false);
    assert_eq!(caps["routes"][0]["context_length"], 8_192);

    // Internally, each deployment still has its own figures.
    let (_, deployments) = router.get("/api/router/v1/deployments").await;
    let row = |id: &str| {
        deployments["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap()
            .clone()
    };
    let primary = row("node-a/QwenCoder");
    assert_eq!(primary["observed"]["capabilities"]["tools"], true);
    assert_eq!(primary["observed"]["context_length"], 32_768);
    let backup = row("node-b/CoderBackup");
    assert_eq!(backup["observed"]["capabilities"]["tools"], false);
    assert_eq!(backup["observed"]["context_length"], 8_192);

    // The backup leaves the eligible set: the route's context and features
    // follow, from the same set routing uses.
    b.served.shutdown().await;
    router.probe().await;
    router.probe().await;
    let (_, models) = router.get("/v1/models").await;
    assert_eq!(models["data"][0]["context_length"], 32_768);
    let (_, caps) = router.get("/v1/capabilities").await;
    assert_eq!(caps["routes"][0]["features"]["tools"], true);
    // Fast's only deployment is gone, so it advertises no context at all.
    assert!(models["data"][1].get("context_length").is_none());
    // And the backup's own figures are still on record for a later selector.
    let (_, deployments) = router.get("/api/router/v1/deployments").await;
    let backup = deployments["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "node-b/CoderBackup")
        .unwrap();
    assert_eq!(backup["available"], false);
    assert_eq!(backup["observed"]["context_length"], 8_192);
}

#[tokio::test]
async fn no_physical_identity_reaches_an_ordinary_client() {
    ensure_provider();
    let mut a = RealNode::start("QwenCoder", MockConfig::default(), GatewayConfig::default()).await;
    let mut b = RealNode::start(
        "CoderBackup",
        MockConfig::default(),
        GatewayConfig::default(),
    )
    .await;
    let router = Router::start(two_node_config(&a.base(), &b.base()), &[]).await;
    let forbidden = [
        "node-a".to_owned(),
        "node-b".to_owned(),
        "127.0.0.1".to_owned(),
        "QwenCoder".to_owned(),
        "CoderBackup".to_owned(),
        "qwen2.5-coder".to_owned(),
        ".gguf".to_owned(),
        "/models/".to_owned(),
    ];
    let check = |what: &str, text: &str| {
        for leak in &forbidden {
            assert!(
                !text.contains(leak.as_str()),
                "{what} leaked {leak:?}: {text}"
            );
        }
    };

    for path in ["/v1/models", "/v1/capabilities", "/health"] {
        let text = client()
            .get(format!("{}{path}", router.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        check(path, &text);
    }
    let messages = json!([{"role": "user", "content": "hi"}]);
    for (path, body) in [
        (
            "/v1/chat/completions",
            json!({"model": "Coder", "messages": messages}),
        ),
        (
            "/v1/chat/completions",
            json!({"model": "Coder", "messages": messages, "stream": true,
                   "stream_options": {"include_usage": true}}),
        ),
        ("/v1/chat/completions", json!({"messages": messages})),
        ("/v1/completions", json!({"model": "Coder", "prompt": "x"})),
        (
            "/v1/completions",
            json!({"model": "Coder", "prompt": "x", "stream": true}),
        ),
        // Refusals that talk about the model.
        (
            "/v1/chat/completions",
            json!({"model": "Research", "messages": messages}),
        ),
        (
            "/v1/chat/completions",
            json!({"model": "QwenCoder", "messages": messages}),
        ),
        (
            "/v1/chat/completions",
            json!({"model": 42, "messages": messages}),
        ),
    ] {
        let response = router.post(path, body.clone()).await;
        let status = response.status();
        let text = response.text().await.unwrap();
        // The client's own word is echoed back in a refusal of it; that is
        // not a leak, so the one case naming a node alias is checked for the
        // other identities only.
        if body["model"] == "QwenCoder" {
            assert_eq!(status, 404);
            assert!(!text.contains("CoderBackup") && !text.contains("node-a"));
            continue;
        }
        check(&format!("{path} {body} -> {status}"), &text);
    }

    // And with every deployment down, the unavailable answer names only the route.
    a.served.shutdown().await;
    b.served.shutdown().await;
    router.probe().await;
    router.probe().await;
    let response = router
        .post(
            "/v1/chat/completions",
            json!({"model": "Coder", "messages": messages}),
        )
        .await;
    assert_eq!(response.status(), 503);
    check("route_unavailable", &response.text().await.unwrap());
}

#[tokio::test]
async fn a_lone_deployment_that_lost_its_model_is_route_unavailable_without_the_alias() {
    ensure_provider();
    let gone = json!({"error": {
        "message": "the model \"QwenCoder\" is not loaded; this gateway is serving \"Other\"",
        "type": "invalid_request_error", "param": "model", "code": "model_not_found"
    }});
    let a = FakeNode::start("QwenCoder", Act::Refuse(404, gone)).await;
    let router = Router::start(
        json!({
            "nodes": [{"id": "a", "url": a.base()}],
            "routes": [{"name": "Coder", "deployments": [{"node": "a", "model": "QwenCoder"}]}]
        }),
        &[],
    )
    .await;

    let response = router
        .post(
            "/v1/chat/completions",
            json!({"model": "Coder", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(response.status(), 503);
    let text = response.text().await.unwrap();
    assert!(!text.contains("QwenCoder"), "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["error"]["code"], "route_unavailable");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("\"Coder\"")
    );
}

#[tokio::test]
async fn a_500_is_returned_as_the_node_wrote_it_and_never_retried_elsewhere() {
    ensure_provider();
    let failed = json!({"error": {
        "message": "the engine could not complete the generation",
        "type": "server_error",
        "code": "generation_failed"
    }});
    let a = FakeNode::start("QwenCoder", Act::Refuse(500, failed.clone())).await;
    let b = FakeNode::start("CoderBackup", Act::Answer).await;
    let router = Router::start(two_node_config(&a.base(), &b.base()), &[]).await;

    let (status, body) = router.chat(Some("Coder")).await;
    assert_eq!(status, 500);
    assert_eq!(body, failed, "code and semantics preserved");
    assert_eq!((a.hits(), b.hits()), (1, 0));
    assert_eq!(router.state.metrics.failovers("Coder"), 0);
}

#[tokio::test]
async fn a_node_offline_at_startup_gets_no_traffic_until_a_probe_sees_it() {
    ensure_provider();
    // Reserve a port, then free it, so the router starts against a node that
    // is not there yet.
    let port = {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        held.local_addr().unwrap().port()
    };
    let router = Router::start(
        json!({
            "nodes": [{"id": "late", "url": format!("http://127.0.0.1:{port}")}],
            "routes": [{"name": "Coder", "deployments": [{"node": "late", "model": "QwenCoder"}]}]
        }),
        &[],
    )
    .await;
    // One failed probe at startup, below the threshold of two: unknown, and
    // the router is serving regardless.
    assert_eq!(router.health("late"), NodeHealth::Unknown);
    assert_eq!(router.get("/health").await.0, 200);

    // The node comes up. Unknown is still not eligible, so nothing is sent to
    // it until a probe has actually seen it.
    let node = RealNode::start_on(
        port,
        "QwenCoder",
        MockConfig::default(),
        GatewayConfig::default(),
    )
    .await;
    assert_eq!(router.chat(Some("Coder")).await.0, 503);
    assert_eq!(node.backend.generation_count(), 0);

    // One successful probe, no restart: it serves.
    router.probe().await;
    assert_eq!(router.health("late"), NodeHealth::Healthy);
    let (status, body) = router.chat(Some("Coder")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(node.backend.generation_count(), 1);
}

// --- R4: round-robin and least-busy over real sockets ----------------------

/// One route, `Coder`, over `nodes` under `strategy`. Each node is
/// `(id, url, node-local model)`.
fn policy_config(strategy: &str, nodes: &[(&str, String, &str)]) -> Value {
    json!({
        "default_route": "Coder",
        "nodes": nodes
            .iter()
            .map(|(id, url, _)| json!({"id": id, "url": url}))
            .collect::<Vec<_>>(),
        "routes": [{
            "name": "Coder",
            "strategy": strategy,
            "deployments": nodes
                .iter()
                .map(|(id, _, model)| json!({"node": id, "model": model}))
                .collect::<Vec<_>>(),
        }]
    })
}

/// In-flight counts by deployment id.
fn in_flight(router: &Router) -> std::collections::BTreeMap<String, u64> {
    router
        .state
        .selector
        .load()
        .snapshot()
        .into_iter()
        .map(|(id, active)| (id.as_str().to_owned(), active))
        .collect()
}

/// Wait until every slot the router took has been given back.
async fn all_released(router: &Router, what: &str) {
    poll(what, async || {
        (in_flight(router).values().all(|active| *active == 0)
            && router.state.metrics.active() == 0)
            .then_some(())
    })
    .await;
}

/// Which of `nodes` took the last request, by hit counts before and after.
fn which(nodes: &[&FakeNode], before: &[u32]) -> usize {
    let changed: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter(|(index, node)| node.hits() != before[*index])
        .map(|(index, _)| index)
        .collect();
    assert_eq!(changed.len(), 1, "exactly one node should have answered");
    changed[0]
}

fn hits(nodes: &[&FakeNode]) -> Vec<u32> {
    nodes.iter().map(|node| node.hits()).collect()
}

#[tokio::test]
async fn round_robin_takes_turns_in_configured_order_and_answers_as_the_route() {
    ensure_provider();
    let a = FakeNode::start("AliasA", Act::Answer).await;
    let b = FakeNode::start("AliasB", Act::Answer).await;
    let c = FakeNode::start("AliasC", Act::Answer).await;
    let router = Router::start(
        policy_config(
            "round_robin",
            &[
                ("a", a.base(), "AliasA"),
                ("b", b.base(), "AliasB"),
                ("c", c.base(), "AliasC"),
            ],
        ),
        &[],
    )
    .await;
    let nodes = [&a, &b, &c];

    let mut turns = Vec::new();
    for request in 0..6 {
        let before = hits(&nodes);
        let stream = request % 2 == 1;
        let response = router
            .post(
                "/v1/chat/completions",
                json!({"model": "Coder", "stream": stream,
                       "messages": [{"role": "user", "content": "hi"}]}),
            )
            .await;
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        assert!(text.contains("\"model\":\"Coder\""), "{text}");
        for leak in ["AliasA", "AliasB", "AliasC", "127.0.0.1"] {
            assert!(!text.contains(leak), "{leak} leaked: {text}");
        }
        turns.push(which(&nodes, &before));
    }
    assert_eq!(turns, [0, 1, 2, 0, 1, 2], "A B C A B C");
    assert_eq!(
        router.state.metrics.decisions("Coder", "round_robin"),
        6,
        "every request decided by the rotation"
    );
    all_released(&router, "slots after round-robin requests").await;
}

#[tokio::test]
async fn round_robin_rotates_over_healthy_deployments_and_takes_a_recovered_one_back() {
    ensure_provider();
    let a = FakeNode::start("AliasA", Act::Answer).await;
    let mut b = FakeNode::start("AliasB", Act::Answer).await;
    let b_port = b.served.port;
    let c = FakeNode::start("AliasC", Act::Answer).await;
    let router = Router::start(
        policy_config(
            "round_robin",
            &[
                ("a", a.base(), "AliasA"),
                ("b", b.base(), "AliasB"),
                ("c", c.base(), "AliasC"),
            ],
        ),
        &[],
    )
    .await;

    // B goes down and the probes see it.
    b.served.shutdown().await;
    router.probe().await;
    router.probe().await;
    assert_eq!(router.health("b"), NodeHealth::Unhealthy);

    let mut turns = Vec::new();
    for _ in 0..4 {
        let before = [a.hits(), c.hits()];
        assert_eq!(router.chat(Some("Coder")).await.0, 200);
        turns.push(if a.hits() != before[0] { "a" } else { "c" });
    }
    turns.dedup();
    assert_eq!(turns.len(), 4, "alternating, never two in a row: {turns:?}");

    // B comes back on the same address; one probe and it is in the ring.
    let b = FakeNode::start_limited(b_port, "AliasB", Act::Answer, 1).await;
    router.probe().await;
    for _ in 0..3 {
        assert_eq!(router.chat(Some("Coder")).await.0, 200);
    }
    assert_eq!(
        b.hits(),
        1,
        "one full turn includes the recovered deployment"
    );
}

#[tokio::test]
async fn round_robin_failover_uses_the_rest_of_the_ring_and_advances_once_per_request() {
    ensure_provider();
    let a = FakeNode::start("AliasA", Act::Answer).await;
    let mut b = FakeNode::start("AliasB", Act::Answer).await;
    let c = FakeNode::start("AliasC", Act::Answer).await;
    let router = Router::start(
        policy_config(
            "round_robin",
            &[
                ("a", a.base(), "AliasA"),
                ("b", b.base(), "AliasB"),
                ("c", c.base(), "AliasC"),
            ],
        ),
        &[],
    )
    .await;
    // B dies between probes: still eligible, so its turn comes and fails.
    b.served.shutdown().await;

    let mut statuses = Vec::new();
    for _ in 0..3 {
        statuses.push(router.chat(Some("Coder")).await.0);
    }
    assert_eq!(statuses, [200, 200, 200]);
    // 1 -> A. 2 -> B, refused, failover to C. 3 -> C, the rotation's own turn.
    assert_eq!((a.hits(), c.hits()), (1, 2));
    assert_eq!(
        router.state.selector.cursor(
            &router.state.topology,
            &router.state.topology.routes()[0].name
        ),
        Some(3),
        "three requests, three steps - the failover attempt took none"
    );
    assert_eq!(
        router
            .state
            .metrics
            .decisions("Coder", "round_robin_failover"),
        1
    );
    all_released(&router, "slots after a round-robin failover").await;
}

#[tokio::test]
async fn round_robin_shares_concurrent_requests_exactly() {
    ensure_provider();
    let a = FakeNode::start("AliasA", Act::Answer).await;
    let b = FakeNode::start("AliasB", Act::Answer).await;
    let c = FakeNode::start("AliasC", Act::Answer).await;
    let router = Arc::new(
        Router::start(
            policy_config(
                "round_robin",
                &[
                    ("a", a.base(), "AliasA"),
                    ("b", b.base(), "AliasB"),
                    ("c", c.base(), "AliasC"),
                ],
            ),
            &[],
        )
        .await,
    );
    let requests: Vec<_> = (0..30)
        .map(|_| {
            let router = Arc::clone(&router);
            tokio::spawn(async move { router.chat(Some("Coder")).await.0 })
        })
        .collect();
    for request in requests {
        assert_eq!(request.await.unwrap(), 200);
    }
    // Thirty distinct cursor values over a ring of three, whatever order the
    // requests were planned in.
    assert_eq!((a.hits(), b.hits(), c.hits()), (10, 10, 10));
    all_released(&router, "slots after concurrent round-robin").await;
}

#[tokio::test]
async fn least_busy_puts_concurrent_work_where_the_capacity_is_and_releases_it() {
    ensure_provider();
    let hold = gate();
    let a = FakeNode::start_limited(0, "AliasA", Act::Hold(Arc::clone(&hold)), 4).await;
    let b = FakeNode::start_limited(0, "AliasB", Act::Hold(Arc::clone(&hold)), 1).await;
    let router = Arc::new(
        Router::start(
            policy_config(
                "least_busy",
                &[("a", a.base(), "AliasA"), ("b", b.base(), "AliasB")],
            ),
            &[],
        )
        .await,
    );

    let requests: Vec<_> = (0..5)
        .map(|_| {
            let router = Arc::clone(&router);
            tokio::spawn(async move { router.chat(Some("Coder")).await })
        })
        .collect();
    poll("all five requests in flight", async || {
        (a.hits() + b.hits() == 5).then_some(())
    })
    .await;
    // Over 4 + 1 slots, five requests fill both exactly: 4/4 and 1/1.
    let load = in_flight(&router);
    assert_eq!(load["a/AliasA"], 4, "{load:?}");
    assert_eq!(load["b/AliasB"], 1, "{load:?}");

    // The admin view shows the same numbers, with each node's own limit.
    let (_, deployments) = router.get("/api/router/v1/deployments").await;
    let a_row = deployments["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "a/AliasA")
        .unwrap();
    assert_eq!(a_row["active_requests"], 4);
    assert_eq!(a_row["concurrency_limit"], 4);
    let (_, routes) = router.get("/api/router/v1/routes").await;
    assert_eq!(routes["data"][0]["strategy"], "least_busy");

    open(&hold);
    for request in requests {
        let (status, body) = request.await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["model"], "Coder");
    }
    all_released(&router, "slots after held least-busy requests").await;
}

/// The concurrency limit the admin view shows for one deployment.
async fn concurrency_limit(router: &Router, deployment: &str) -> Value {
    let (_, deployments) = router.get("/api/router/v1/deployments").await;
    deployments["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == deployment)
        .unwrap()["concurrency_limit"]
        .clone()
}

#[tokio::test]
async fn least_busy_follows_a_node_whose_scheduler_was_resized_after_the_next_probe() {
    ensure_provider();
    // A is a real gateway, so the limit the router sees is the one its own
    // `/v1/capabilities` publishes. Its engine holds every request in prefill
    // long enough for the three below to be in flight together.
    let a = RealNode::start(
        "AliasA",
        MockConfig {
            prefill: Duration::from_secs(3),
            ..MockConfig::default()
        },
        GatewayConfig {
            max_concurrent_requests: 4,
            ..GatewayConfig::default()
        },
    )
    .await;
    let hold = gate();
    let b = FakeNode::start_limited(0, "AliasB", Act::Hold(Arc::clone(&hold)), 4).await;
    let router = Arc::new(
        Router::start(
            policy_config(
                "least_busy",
                &[("a", a.base(), "AliasA"), ("b", b.base(), "AliasB")],
            ),
            &[],
        )
        .await,
    );
    assert_eq!(concurrency_limit(&router, "a/AliasA").await, 4);

    // What a hot swap does once the new engine is up: the scheduler takes the
    // slot count that engine was started with.
    a.state.scheduler().set_capacity(2);
    // The router does not guess; until it probes, it still holds the old answer.
    assert_eq!(concurrency_limit(&router, "a/AliasA").await, 4);
    router.probe().await;
    assert_eq!(concurrency_limit(&router, "a/AliasA").await, 2);

    // One at a time, so each choice sees the one before it in flight.
    //   1st: A 0/2 vs B 0/4, a tie       -> A, by configured order
    //   2nd: A 1/2 vs B 0/4              -> B
    //   3rd: A 1/2 = 50% vs B 1/4 = 25%  -> B
    // Had the router kept A's stale 4, the 3rd would be 1/4 against 1/4, a
    // tie, and go to A.
    let mut requests = Vec::new();
    for expected in [(1, 0), (1, 1), (1, 2)] {
        let router_for_request = Arc::clone(&router);
        requests.push(tokio::spawn(async move {
            router_for_request.chat(Some("Coder")).await
        }));
        poll("the request to be in flight", async || {
            let load = in_flight(&router);
            (load["a/AliasA"] + load["b/AliasB"] == expected.0 + expected.1).then_some(())
        })
        .await;
        let load = in_flight(&router);
        assert_eq!(
            (load["a/AliasA"], load["b/AliasB"]),
            expected,
            "least-busy must divide by A's live limit of 2: {load:?}"
        );
    }

    open(&hold);
    for request in requests {
        let (status, body) = request.await.unwrap();
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["model"], "Coder");
    }
    all_released(&router, "slots after a resized node's requests").await;
}

#[tokio::test]
async fn least_busy_failover_moves_the_slot_to_the_deployment_doing_the_work() {
    ensure_provider();
    let hold = gate();
    // A has far more capacity, so least-busy picks it - but it is gone.
    let mut a = FakeNode::start_limited(0, "AliasA", Act::Answer, 8).await;
    let b = FakeNode::start_limited(0, "AliasB", Act::Hold(Arc::clone(&hold)), 1).await;
    let router = Arc::new(
        Router::start(
            policy_config(
                "least_busy",
                &[("a", a.base(), "AliasA"), ("b", b.base(), "AliasB")],
            ),
            &[],
        )
        .await,
    );
    a.served.shutdown().await;

    let request = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.chat(Some("Coder")).await })
    };
    poll("the failover attempt to reach B", async || {
        (b.hits() == 1).then_some(())
    })
    .await;
    let load = in_flight(&router);
    assert_eq!(load["a/AliasA"], 0, "the failed attempt gave its slot back");
    assert_eq!(load["b/AliasB"], 1, "the working attempt holds one");

    open(&hold);
    let (status, body) = request.await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(body["model"], "Coder");
    assert_eq!(
        router
            .state
            .metrics
            .decisions("Coder", "least_busy_failover"),
        1
    );
    all_released(&router, "slots after a least-busy failover").await;
}

#[tokio::test]
async fn no_exit_path_leaks_an_in_flight_slot() {
    ensure_provider();
    let busy = json!({"error": {"message": "busy", "type": "server_error", "code": "server_busy"}});
    let broken =
        json!({"error": {"message": "boom", "type": "server_error", "code": "generation_failed"}});

    // Each case: what the first-choice node does, and whether a second node
    // that answers stands behind it.
    let cases: Vec<(&str, Act, bool)> = vec![
        ("success", Act::Answer, false),
        ("upstream 500", Act::Refuse(500, broken), false),
        ("upstream 503 then failover", Act::Refuse(503, busy), true),
        ("stream failure after output", Act::StreamThenDrop, false),
    ];
    for (case, act, with_backup) in cases {
        let first = FakeNode::start_limited(0, "AliasA", act, 8).await;
        let backup = FakeNode::start_limited(0, "AliasB", Act::Answer, 1).await;
        let mut nodes = vec![("a", first.base(), "AliasA")];
        if with_backup {
            nodes.push(("b", backup.base(), "AliasB"));
        }
        let router = Router::start(policy_config("least_busy", &nodes), &[]).await;
        for stream in [false, true] {
            let response = router
                .post(
                    "/v1/chat/completions",
                    json!({"model": "Coder", "stream": stream,
                           "messages": [{"role": "user", "content": "hi"}]}),
                )
                .await;
            let _ = response.bytes().await;
            all_released(&router, case).await;
        }
    }

    // Connection refused, then failover.
    let mut gone = FakeNode::start_limited(0, "AliasA", Act::Answer, 8).await;
    let backup = FakeNode::start_limited(0, "AliasB", Act::Answer, 1).await;
    let router = Router::start(
        policy_config(
            "least_busy",
            &[("a", gone.base(), "AliasA"), ("b", backup.base(), "AliasB")],
        ),
        &[],
    )
    .await;
    gone.served.shutdown().await;
    assert_eq!(router.chat(Some("Coder")).await.0, 200);
    all_released(&router, "connection refused then failover").await;

    // A client that walks away mid-stream, and one that gives up waiting.
    let hold = gate();
    let slow = FakeNode::start_limited(0, "AliasA", Act::Hold(Arc::clone(&hold)), 4).await;
    let router = Router::start(
        policy_config("least_busy", &[("a", slow.base(), "AliasA")]),
        &[],
    )
    .await;
    let mut response = router
        .post(
            "/v1/chat/completions",
            json!({"model": "Coder", "stream": true,
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    let first = response.chunk().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("held"));
    assert_eq!(in_flight(&router)["a/AliasA"], 1, "counted while streaming");
    drop(response);
    all_released(&router, "client disconnect mid-stream").await;

    let impatient = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let timed_out = impatient
        .post(format!("{}/v1/chat/completions", router.base))
        .json(&json!({"model": "Coder", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await;
    assert!(timed_out.is_err(), "the client gave up");
    all_released(&router, "client timeout while the node was still working").await;
    open(&hold);
}

#[tokio::test]
async fn a_node_reporting_no_capacity_is_only_a_fallback() {
    ensure_provider();
    let a = FakeNode::start_limited(0, "AliasA", Act::Answer, 1).await;
    let zero = FakeNode::start_limited(0, "AliasB", Act::Answer, 0).await;
    let router = Router::start(
        policy_config(
            "least_busy",
            &[("a", a.base(), "AliasA"), ("b", zero.base(), "AliasB")],
        ),
        &[],
    )
    .await;
    for _ in 0..3 {
        assert_eq!(router.chat(Some("Coder")).await.0, 200);
    }
    assert_eq!(
        (a.hits(), zero.hits()),
        (3, 0),
        "zero capacity is never assumed to have room"
    );
    let mut a = a;
    a.served.shutdown().await;
    router.probe().await;
    router.probe().await;
    assert_eq!(router.chat(Some("Coder")).await.0, 200);
    assert_eq!(
        zero.hits(),
        1,
        "but it still serves when it is all there is"
    );
}

#[tokio::test]
async fn every_policy_answers_as_the_route_and_default_uses_the_routes_policy() {
    ensure_provider();
    for strategy in ["priority", "round_robin", "least_busy"] {
        let a = FakeNode::start("QwenCoder", Act::Answer).await;
        let b = FakeNode::start("CoderBackup", Act::Answer).await;
        let router = Router::start(
            policy_config(
                strategy,
                &[
                    ("node-a", a.base(), "QwenCoder"),
                    ("node-b", b.base(), "CoderBackup"),
                ],
            ),
            &[],
        )
        .await;
        for (model, stream) in [
            (json!("Coder"), false),
            (json!("Coder"), true),
            (json!("default"), false),
            (Value::Null, true),
        ] {
            let mut body =
                json!({"stream": stream, "messages": [{"role": "user", "content": "hi"}]});
            if !model.is_null() {
                body["model"] = model.clone();
            }
            let response = router.post("/v1/chat/completions", body).await;
            assert_eq!(response.status(), 200, "{strategy} {model}");
            let text = response.text().await.unwrap();
            assert!(text.contains("\"model\":\"Coder\""), "{strategy}: {text}");
            for leak in ["QwenCoder", "CoderBackup", "node-a", "node-b"] {
                assert!(!text.contains(leak), "{strategy} leaked {leak}: {text}");
            }
        }
        // `default` went through the route's own policy, not priority.
        let (_, models) = router.get("/v1/models").await;
        assert_eq!(models["data"][0]["id"], "Coder");
        let expected = match strategy {
            "round_robin" => "round_robin",
            "least_busy" => "least_busy_tiebreak",
            _ => "primary_healthy",
        };
        assert!(
            router.state.metrics.decisions("Coder", expected) >= 2,
            "{strategy}: default and omitted model used the {expected} path"
        );
        if strategy == "round_robin" {
            assert_eq!((a.hits(), b.hits()), (2, 2));
        }
    }
}

// --- R5: request-aware capability filtering ---------------------------------

/// A chat request for `route` with `extra` merged in.
fn chat_body(route: &str, extra: Value) -> Value {
    let mut body = json!({"model": route, "messages": [{"role": "user", "content": "hi"}]});
    if let (Some(body), Value::Object(extra)) = (body.as_object_mut(), extra) {
        body.extend(extra);
    }
    body
}

fn a_tool() -> Value {
    json!([{"type": "function", "function": {"name": "search", "parameters": {"type": "object"}}}])
}

async fn send(router: &Router, path: &str, body: Value) -> (u16, Value) {
    let response = router.post(path, body).await;
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// Every chunk of a streamed answer, as JSON, with `[DONE]` checked.
async fn streamed(router: &Router, body: Value) -> Vec<Value> {
    let response = router.post("/v1/chat/completions", body).await;
    assert_eq!(response.status(), 200);
    let events = decode(&response.bytes().await.unwrap());
    assert!(events.last().unwrap().is_done());
    events
        .iter()
        .filter(|event| !event.is_done())
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .collect()
}

#[tokio::test]
async fn priority_sends_a_tool_request_straight_to_the_first_deployment_with_tools() {
    ensure_provider();
    let a = FakeNode::start("AliasA", Act::Answer).await;
    a.withhold(|w| w.tools = true);
    let b = FakeNode::start("AliasB", Act::Answer).await;
    let router = Router::start(
        policy_config(
            "priority",
            &[
                ("node-a", a.base(), "AliasA"),
                ("node-b", b.base(), "AliasB"),
            ],
        ),
        &[],
    )
    .await;

    let (status, body) = send(
        &router,
        "/v1/chat/completions",
        chat_body("Coder", json!({"tools": a_tool()})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["model"], "Coder");
    // Never sent to a, not even to be refused: a was never a candidate.
    assert_eq!((a.hits(), b.hits()), (0, 1));
    assert_eq!(b.seen()[0].body["model"], "AliasB");
    assert_eq!(b.seen()[0].body["tools"], a_tool(), "forwarded untouched");
    assert_eq!(router.state.metrics.failovers("Coder"), 0);
    assert_eq!(
        router
            .state
            .metrics
            .capability_filtered("Coder", "tools_unsupported"),
        1
    );

    // Streamed, the route's name is on every chunk and no node's is anywhere.
    let chunks = streamed(
        &router,
        chat_body("Coder", json!({"tools": a_tool(), "stream": true})),
    )
    .await;
    assert!(chunks.iter().all(|chunk| chunk["model"] == "Coder"));
    assert!(
        !chunks
            .iter()
            .any(|chunk| chunk.to_string().contains("Alias"))
    );
    assert_eq!((a.hits(), b.hits()), (0, 2));

    // An ordinary request still goes to the primary.
    let (status, _) = router.chat(Some("Coder")).await;
    assert_eq!(status, 200);
    assert_eq!((a.hits(), b.hits()), (1, 2));
}

#[tokio::test]
async fn round_robin_rotates_tool_requests_over_the_capable_deployments_only() {
    ensure_provider();
    let a = FakeNode::start("AliasA", Act::Answer).await;
    let b = FakeNode::start("AliasB", Act::Answer).await;
    b.withhold(|w| w.tools = true);
    let c = FakeNode::start("AliasC", Act::Answer).await;
    let router = Router::start(
        policy_config(
            "round_robin",
            &[
                ("a", a.base(), "AliasA"),
                ("b", b.base(), "AliasB"),
                ("c", c.base(), "AliasC"),
            ],
        ),
        &[],
    )
    .await;
    let nodes = [&a, &b, &c];

    let mut order = Vec::new();
    for _ in 0..6 {
        let before = hits(&nodes);
        let (status, body) = send(
            &router,
            "/v1/chat/completions",
            chat_body("Coder", json!({"tools": a_tool()})),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["model"], "Coder");
        order.push(["a", "b", "c"][which(&nodes, &before)]);
    }
    assert_eq!(order, ["a", "c", "a", "c", "a", "c"]);
    assert_eq!(b.hits(), 0);

    // Requests without tools rotate over all three again.
    for _ in 0..3 {
        router.chat(Some("Coder")).await;
    }
    assert!(b.hits() > 0, "b serves a request that does not need tools");
}

#[tokio::test]
async fn least_busy_with_tools_never_picks_an_idle_deployment_without_them() {
    ensure_provider();
    let hold = gate();
    let a = FakeNode::start_limited(0, "AliasA", Act::Hold(Arc::clone(&hold)), 4).await;
    let b = FakeNode::start_limited(0, "AliasB", Act::Answer, 4).await;
    b.withhold(|w| w.tools = true);
    let c = FakeNode::start_limited(0, "AliasC", Act::Hold(Arc::clone(&hold)), 4).await;
    let router = Arc::new(
        Router::start(
            policy_config(
                "least_busy",
                &[
                    ("a", a.base(), "AliasA"),
                    ("b", b.base(), "AliasB"),
                    ("c", c.base(), "AliasC"),
                ],
            ),
            &[],
        )
        .await,
    );

    // One at a time over a and c only:
    //   0/4 vs 0/4 -> a;  1/4 vs 0/4 -> c;  1/4 vs 1/4 -> a;  2/4 vs 1/4 -> c.
    // b sits at 0/4 throughout and is never compared.
    let mut requests = Vec::new();
    for (total, expected) in [(1, (1, 0)), (2, (1, 1)), (3, (2, 1)), (4, (2, 2))] {
        let router_for_request = Arc::clone(&router);
        requests.push(tokio::spawn(async move {
            send(
                &router_for_request,
                "/v1/chat/completions",
                chat_body("Coder", json!({"tools": a_tool()})),
            )
            .await
        }));
        poll("the tool request to be in flight", async || {
            (a.hits() + c.hits() == total).then_some(())
        })
        .await;
        let load = in_flight(&router);
        assert_eq!((load["a/AliasA"], load["c/AliasC"]), expected, "{load:?}");
        assert_eq!(load["b/AliasB"], 0);
    }
    assert_eq!(b.hits(), 0);

    open(&hold);
    for request in requests {
        let (status, body) = request.await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["model"], "Coder");
    }
    all_released(&router, "slots after filtered least-busy requests").await;
}

#[tokio::test]
async fn failover_stays_inside_the_capable_set() {
    ensure_provider();
    let a = FakeNode::start("AliasA", Act::Answer).await;
    a.withhold(|w| w.tools = true);
    let b = FakeNode::start(
        "AliasB",
        Act::Refuse(
            503,
            json!({"error": {"message": "busy", "type": "server_error", "code": "server_busy"}}),
        ),
    )
    .await;
    let c = FakeNode::start("AliasC", Act::Answer).await;
    let router = Router::start(
        policy_config(
            "priority",
            &[
                ("a", a.base(), "AliasA"),
                ("b", b.base(), "AliasB"),
                ("c", c.base(), "AliasC"),
            ],
        ),
        &[],
    )
    .await;

    let (status, body) = send(
        &router,
        "/v1/chat/completions",
        chat_body("Coder", json!({"tools": a_tool()})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["model"], "Coder");
    // b refused before answering, so the request moved on - to c, never to a,
    // although a is healthy and first in the route.
    assert_eq!((a.hits(), b.hits(), c.hits()), (0, 1, 1));
    assert_eq!(router.state.metrics.failovers("Coder"), 1);

    // With c gone too, the last refusal stands; a is still never asked.
    let lone = Router::start(
        policy_config(
            "priority",
            &[("a", a.base(), "AliasA"), ("b", b.base(), "AliasB")],
        ),
        &[],
    )
    .await;
    let (status, body) = send(
        &lone,
        "/v1/chat/completions",
        chat_body("Coder", json!({"tools": a_tool()})),
    )
    .await;
    assert_eq!(status, 503);
    assert_eq!(body["error"]["code"], "server_busy");
    assert_eq!(a.hits(), 0);
}

#[tokio::test]
async fn a_long_prompt_narrows_to_the_deployment_whose_context_can_hold_it() {
    ensure_provider();
    let small = FakeNode::start_with("Small", Act::Answer, true, 2_048).await;
    let large = FakeNode::start_with("Large", Act::Answer, true, 32_768).await;
    let router = Router::start(
        policy_config(
            "priority",
            &[("s", small.base(), "Small"), ("l", large.base(), "Large")],
        ),
        &[],
    )
    .await;
    // The route promises only what every deployment can: the smaller window.
    let (_, caps) = router.get("/v1/capabilities").await;
    assert_eq!(caps["state"]["model"]["context_length"], 2_048);

    // ~20 KB of prompt is at least 3 334 tokens: more than 2 048 can hold.
    let long = "word ".repeat(4_000);
    let (status, body) = send(
        &router,
        "/v1/chat/completions",
        json!({"model": "Coder", "messages": [{"role": "user", "content": long}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!((small.hits(), large.hits()), (0, 1));

    // A short prompt with a huge budget still fits the small one: the node
    // clamps max_tokens, it does not refuse it.
    let (status, _) = send(
        &router,
        "/v1/chat/completions",
        chat_body("Coder", json!({"max_tokens": 65_536})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!((small.hits(), large.hits()), (1, 1));

    // Too long for either: refused by the router, naming the route only.
    let huge = "word ".repeat(40_000);
    let (status, body) = send(
        &router,
        "/v1/chat/completions",
        json!({"model": "Coder", "messages": [{"role": "user", "content": huge}]}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "route_capability_mismatch");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("\"Coder\"") && message.contains("context window"),
        "{message}"
    );
    assert!(!message.contains("Small") && !message.contains("Large"));
    assert_eq!((small.hits(), large.hits()), (1, 1));
    assert_eq!(router.state.metrics.capability_mismatches("Coder"), 1);
}

#[tokio::test]
async fn reasoning_is_required_only_of_requests_that_ask_for_it() {
    ensure_provider();
    let plain = FakeNode::start("Plain", Act::Answer).await;
    plain.withhold(|w| w.reasoning = true);
    let thinker = FakeNode::start("Thinker", Act::Answer).await;
    let router = Router::start(
        policy_config(
            "priority",
            &[
                ("p", plain.base(), "Plain"),
                ("t", thinker.base(), "Thinker"),
            ],
        ),
        &[],
    )
    .await;

    router.chat(Some("Coder")).await;
    assert_eq!((plain.hits(), thinker.hits()), (1, 0), "an ordinary chat");

    let (status, body) = send(
        &router,
        "/v1/chat/completions",
        chat_body("Coder", json!({"reasoning_effort": "high"})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["model"], "Coder");
    assert_eq!((plain.hits(), thinker.hits()), (1, 1));

    // "none" turns thinking off, which the plain one can do.
    send(
        &router,
        "/v1/chat/completions",
        chat_body("Coder", json!({"reasoning_effort": "none"})),
    )
    .await;
    assert_eq!((plain.hits(), thinker.hits()), (2, 1));

    // No reasoning anywhere: a mismatch that names the field.
    thinker.withhold(|w| w.reasoning = true);
    router.probe().await;
    let (status, body) = send(
        &router,
        "/v1/chat/completions",
        chat_body("Coder", json!({"reasoning_effort": "high"})),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "route_capability_mismatch");
    assert_eq!(body["error"]["param"], "reasoning_effort");
}

#[tokio::test]
async fn each_endpoint_goes_only_to_deployments_that_offer_it() {
    ensure_provider();
    let chat_only = FakeNode::start("ChatOnly", Act::Answer).await;
    chat_only.withhold(|w| w.completions = true);
    let completion_only = FakeNode::start("TextOnly", Act::Answer).await;
    completion_only.withhold(|w| w.chat = true);
    let router = Router::start(
        policy_config(
            "round_robin",
            &[
                ("c", chat_only.base(), "ChatOnly"),
                ("t", completion_only.base(), "TextOnly"),
            ],
        ),
        &[],
    )
    .await;

    for _ in 0..3 {
        let (status, body) = router.chat(Some("Coder")).await;
        assert_eq!(status, 200);
        assert_eq!(body["model"], "Coder");
    }
    assert_eq!((chat_only.hits(), completion_only.hits()), (3, 0));

    for _ in 0..3 {
        let (status, body) = send(
            &router,
            "/v1/completions",
            json!({"model": "Coder", "prompt": "Once upon"}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["model"], "Coder");
    }
    assert_eq!((chat_only.hits(), completion_only.hits()), (3, 3));
}

#[tokio::test]
async fn unknown_unavailable_and_mismatched_are_three_different_answers() {
    ensure_provider();
    let a = FakeNode::start("AliasA", Act::Answer).await;
    a.withhold(|w| {
        w.tools = true;
        w.tool_choice = true;
    });
    let router = Router::start(
        json!({
            "nodes": [
                {"id": "node-a", "url": a.base()},
                {"id": "node-down", "url": "http://127.0.0.1:9"}
            ],
            "routes": [
                {"name": "Coder", "deployments": [{"node": "node-a", "model": "AliasA"}]},
                {"name": "Gone", "deployments": [{"node": "node-down", "model": "Whatever"}]}
            ]
        }),
        &[],
    )
    .await;
    let tools = |route: &str| chat_body(route, json!({"tools": a_tool()}));

    let (status, body) = send(&router, "/v1/chat/completions", tools("Research")).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (404, Some("model_not_found"))
    );

    let (status, body) = send(&router, "/v1/chat/completions", tools("Gone")).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (503, Some("route_unavailable"))
    );

    let (status, body) = send(&router, "/v1/chat/completions", tools("Coder")).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (400, Some("route_capability_mismatch"))
    );
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["param"], "tools");
    let text = body.to_string();
    assert!(text.contains("tool calling"), "{text}");
    for secret in ["AliasA", "node-a", "127.0.0.1", "qwen", ".gguf"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    // Streamed, the refusal is the same JSON, before any stream starts.
    let (status, body) = send(
        &router,
        "/v1/chat/completions",
        chat_body("Coder", json!({"tools": a_tool(), "stream": true})),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (400, Some("route_capability_mismatch"))
    );

    // A malformed request is the gateway's own 400, from the router, before
    // any node is chosen - the same whichever node would have taken it.
    let (status, body) = send(
        &router,
        "/v1/chat/completions",
        chat_body("Coder", json!({"tool_choice": "required"})),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_tool_choice");
    assert_eq!(body["error"]["param"], "tool_choice");
    assert_eq!(a.hits(), 0);

    // The request that needs nothing special is served as before.
    let (status, body) = router.chat(Some("Coder")).await;
    assert_eq!((status, body["model"].as_str()), (200, Some("Coder")));
}

#[tokio::test]
async fn a_capability_a_node_gains_or_loses_is_seen_on_the_next_probe() {
    ensure_provider();
    let a = FakeNode::start("AliasA", Act::Answer).await;
    a.withhold(|w| w.tools = true);
    let b = FakeNode::start("AliasB", Act::Answer).await;
    let router = Router::start(
        policy_config(
            "priority",
            &[("a", a.base(), "AliasA"), ("b", b.base(), "AliasB")],
        ),
        &[],
    )
    .await;
    let tool_request = || chat_body("Coder", json!({"tools": a_tool()}));

    send(&router, "/v1/chat/completions", tool_request()).await;
    assert_eq!((a.hits(), b.hits()), (0, 1));

    // a hot-swaps to a model with tools. The router does not guess: until it
    // probes, a is still without them.
    a.withhold(|w| w.tools = false);
    send(&router, "/v1/chat/completions", tool_request()).await;
    assert_eq!((a.hits(), b.hits()), (0, 2));
    router.probe().await;
    let (_, deployments) = router.get("/api/router/v1/deployments").await;
    let a_row = deployments["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "a/AliasA")
        .unwrap()
        .clone();
    assert_eq!(a_row["observed"]["capabilities"]["tools"], true);
    send(&router, "/v1/chat/completions", tool_request()).await;
    assert_eq!((a.hits(), b.hits()), (1, 2));

    // And loses a window: a long prompt now goes past it.
    a.withhold(|w| w.context = Some(512));
    router.probe().await;
    let long = "word ".repeat(1_000);
    send(
        &router,
        "/v1/chat/completions",
        json!({"model": "Coder", "messages": [{"role": "user", "content": long}]}),
    )
    .await;
    assert_eq!((a.hits(), b.hits()), (1, 3));
}

#[tokio::test]
async fn real_gateways_are_filtered_by_the_context_they_report() {
    ensure_provider();
    // A real gateway serving at 4 096 tokens, and a node that reports 32 768.
    let real = RealNode::start("QwenCoder", MockConfig::default(), GatewayConfig::default()).await;
    let large = FakeNode::start_with("Large", Act::Answer, true, 32_768).await;
    let router = Router::start(
        policy_config(
            "priority",
            &[
                ("real", real.base(), "QwenCoder"),
                ("big", large.base(), "Large"),
            ],
        ),
        &[],
    )
    .await;

    // A real gateway advertises tools, tool_choice, reasoning and both
    // endpoints: an ordinary or tool request stays on it.
    let (status, body) = send(
        &router,
        "/v1/chat/completions",
        chat_body(
            "Coder",
            json!({"tools": a_tool(), "tool_choice": "auto",
                                  "reasoning_effort": "low"}),
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["model"], "Coder");
    assert_eq!((real.backend.generation_count(), large.hits()), (1, 0));

    // ~30 KB: at least 5 000 tokens, which 4 096 cannot hold.
    let long = "word ".repeat(6_000);
    let (status, body) = send(
        &router,
        "/v1/chat/completions",
        json!({"model": "Coder", "messages": [{"role": "user", "content": long}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["model"], "Coder");
    assert_eq!((real.backend.generation_count(), large.hits()), (1, 1));
}
