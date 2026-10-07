//! The pre-commit request budget (R9.3.2), end to end, over real sockets.
//!
//! Every node is scripted, so how long each one takes — and whether it was
//! contacted at all — is known exactly. Times are wall-clock: the budget is the
//! minimum the configuration allows (1 s) or a few seconds, and every timing
//! assertion is placed so that a deadline *reset* anywhere (a fresh budget for a
//! deployment, a fallback route, or after classification) lands well outside
//! it, while scheduler jitter on a slow CI runner stays well inside it.
//!
//! Where a step must happen "just after the deadline passed" the router's
//! test-only `phase_delays` place it there exactly, rather than racing a timer.
//! The deterministic deadline/result race itself is proven under paused time in
//! `budget.rs`, where no socket can make the clock jump.
//!
//! Test names carry the design's test ids (B1–B45, section 39 of
//! `docs/R9_3_2_SHARED_REQUEST_BUDGET.md`).

use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use lightweight_api::capabilities::{CapabilitiesBody, CapabilityModel};
use lightweight_router::RouterState;
use lightweight_router::config::RouterFile;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Nothing listens on the discard port: refused at once.
const DOWN: &str = "http://127.0.0.1:9";

/// The shortest budget a configuration may set.
const MIN_BUDGET: u64 = 1_000;

/// How far past its deadline a cut request may land on a loaded CI runner.
/// Every "a reset would have taken longer" case is at least this much longer.
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

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

// --- scripted nodes -------------------------------------------------------------

/// Answer at once (after `delay`).
const HEALTHY: u8 = 0;
/// `503 server_busy`, after `delay`.
const BUSY: u8 = 1;
/// `500`, after `delay`.
const BROKEN: u8 = 2;
/// Accept the request and never answer it: a queue that never moves, or a
/// non-streamed generation that never finishes.
const HANG: u8 = 3;
/// `400 context_length_exceeded`.
const OVERFLOW: u8 = 4;
/// A non-streamed answer whose head is sent at once and whose body follows
/// `tail` later.
const SLOW_BODY: u8 = 5;

#[derive(Clone)]
struct Script {
    serving: String,
    classifier: bool,
    context: u32,
    mode: Arc<AtomicU8>,
    delay_ms: Arc<AtomicU64>,
    tail_ms: Arc<AtomicU64>,
    hits: Arc<AtomicU32>,
    request_ids: Arc<Mutex<Vec<String>>>,
}

struct Node {
    base: String,
    _stop: CancellationToken,
    script: Script,
}

impl Node {
    async fn start(serving: &str) -> Self {
        Self::with(serving, false, 8192).await
    }

    async fn with_context(serving: &str, context: u32) -> Self {
        Self::with(serving, false, context).await
    }

    async fn classifier() -> Self {
        Self::with("ClassifierAlias", true, 8192).await
    }

    async fn with(serving: &str, classifier: bool, context: u32) -> Self {
        let script = Script {
            serving: serving.to_owned(),
            classifier,
            context,
            mode: Arc::default(),
            delay_ms: Arc::default(),
            tail_ms: Arc::default(),
            hits: Arc::default(),
            request_ids: Arc::default(),
        };
        let app = axum::Router::new()
            .route("/v1/capabilities", get(capabilities))
            .route("/v1/chat/completions", post(generate))
            .with_state(script.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let stop = CancellationToken::new();
        let stopping = stop.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move { stopping.cancelled().await })
                .await;
        });
        Self {
            base,
            _stop: stop,
            script,
        }
    }

    fn hits(&self) -> u32 {
        self.script.hits.load(Ordering::SeqCst)
    }

    fn set(&self, mode: u8) -> &Self {
        self.script.mode.store(mode, Ordering::SeqCst);
        self
    }

    fn delay(&self, ms: u64) -> &Self {
        self.script.delay_ms.store(ms, Ordering::SeqCst);
        self
    }

    fn tail(&self, ms: u64) -> &Self {
        self.script.tail_ms.store(ms, Ordering::SeqCst);
        self
    }

    fn request_ids(&self) -> Vec<String> {
        self.script.request_ids.lock().unwrap().clone()
    }
}

async fn capabilities(axum::extract::State(script): axum::extract::State<Script>) -> Response {
    let mut body = CapabilitiesBody::new(
        "0.5.0",
        Some(CapabilityModel {
            id: script.serving.clone(),
            context_length: script.context,
        }),
        4,
    );
    body.features.tools = true;
    body.features.tool_choice = true;
    axum::Json(body).into_response()
}

fn error(status: u16, code: &str) -> Response {
    (
        StatusCode::from_u16(status).unwrap(),
        axum::Json(json!({"error": {"message": code, "type": "server_error", "code": code}})),
    )
        .into_response()
}

fn pick(text: &str) -> (String, f64) {
    let words: Vec<&str> = text.split_whitespace().collect();
    words
        .windows(3)
        .find(|w| w[0] == "PICK")
        .and_then(|w| Some((w[1].to_owned(), w[2].parse().ok()?)))
        .unwrap_or(("General".to_owned(), 0.95))
}

fn chunk(model: &Value, delta: &Value, finish: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
               "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    )
}

async fn generate(
    axum::extract::State(script): axum::extract::State<Script>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    script.hits.fetch_add(1, Ordering::SeqCst);
    if let Some(id) = headers.get("x-request-id").and_then(|v| v.to_str().ok()) {
        script.request_ids.lock().unwrap().push(id.to_owned());
    }
    let mode = script.mode.load(Ordering::SeqCst);
    if mode == HANG {
        tokio::time::sleep(Duration::from_secs(3_600)).await;
    }
    let delay = script.delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(ms(delay)).await;
    }
    match mode {
        BUSY => return error(503, "server_busy"),
        BROKEN => return error(500, "internal_error"),
        OVERFLOW => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": {"message": "too long",
                    "type": "invalid_request_error", "code": "context_length_exceeded"}})),
            )
                .into_response();
        }
        _ => {}
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let model = body["model"].clone();
    let text = body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_str())
        .unwrap_or("")
        .to_owned();
    if script.classifier {
        let (route, confidence) = pick(&text);
        let content = json!({"route": route, "confidence": confidence}).to_string();
        return axum::Json(json!({
            "id": "k1", "object": "chat.completion", "model": model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content},
                         "finish_reason": "stop"}],
        }))
        .into_response();
    }
    let tail = ms(script.tail_ms.load(Ordering::SeqCst));
    if body["stream"] == true {
        let head = chunk(&model, &json!({"role": "assistant", "content": "hi"}), None);
        if text.contains("BREAK") {
            return ([("content-type", "text/event-stream")], head).into_response();
        }
        // The head now; then four more pieces spread over `tail`; then the end.
        let steps = 4_u32;
        let frames = futures_util::stream::unfold(0_u32, move |step| {
            let model = model.clone();
            async move {
                match step {
                    0 => Some((Ok::<_, std::io::Error>(head_bytes(&model)), 1)),
                    n if n <= steps => {
                        tokio::time::sleep(tail / steps).await;
                        Some((
                            Ok(Bytes::from(chunk(
                                &model,
                                &json!({"content": " more"}),
                                None,
                            ))),
                            n + 1,
                        ))
                    }
                    n if n == steps + 1 => Some((
                        Ok(Bytes::from(format!(
                            "{}data: [DONE]\n\n",
                            chunk(&model, &json!({}), Some("stop"))
                        ))),
                        n + 1,
                    )),
                    _ => None,
                }
            }
        });
        return (
            [("content-type", "text/event-stream")],
            Body::from_stream(frames),
        )
            .into_response();
    }
    let answer = json!({
        "id": "c1", "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "message": {"role": "assistant",
                     "content": format!("answer from {}", script.serving)},
                     "finish_reason": "stop"}],
    })
    .to_string();
    if mode == SLOW_BODY {
        // The head now, the body `tail` later: committed long before it ends.
        let body = futures_util::stream::once(async move {
            tokio::time::sleep(tail).await;
            Ok::<_, std::io::Error>(Bytes::from(answer))
        });
        return (
            [("content-type", "application/json")],
            Body::from_stream(body),
        )
            .into_response();
    }
    ([("content-type", "application/json")], answer).into_response()
}

fn head_bytes(model: &Value) -> Bytes {
    Bytes::from(chunk(
        model,
        &json!({"role": "assistant", "content": "hi"}),
        None,
    ))
}

// --- the router -------------------------------------------------------------------

struct Router {
    base: String,
    state: Arc<RouterState>,
    stop: CancellationToken,
}

impl Drop for Router {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Router {
    async fn start(config: Value, budget: Option<u64>) -> Self {
        Self::start_with(config, budget, &[]).await
    }

    async fn start_with(
        mut config: Value,
        budget: Option<u64>,
        env: &'static [(&'static str, &'static str)],
    ) -> Self {
        ensure_provider();
        config["listen"] = json!(["127.0.0.1:0"]);
        config["health"] =
            json!({"interval_secs": 3600, "timeout_secs": 2, "failure_threshold": 1});
        let mut request = json!({"connect_timeout_secs": 5});
        if let Some(budget) = budget {
            request["pre_commit_budget_ms"] = json!(budget);
        }
        if config.get("request").is_none() {
            config["request"] = request;
        }
        let file: RouterFile = serde_json::from_value(config).expect("config shape");
        let config = lightweight_router::validate(file, &|name| {
            env.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
        .expect("valid config");
        let bound = lightweight_router::bind(&config).await.expect("bind");
        let base = format!("http://{}", bound.addresses()[0]);
        let state = bound.state();
        let stop = CancellationToken::new();
        tokio::spawn(bound.serve(stop.clone()));
        for _ in 0..100 {
            if client().get(format!("{base}/health")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(ms(20)).await;
        }
        Self { base, state, stop }
    }

    async fn post(&self, body: Value, headers: &[(&str, &str)]) -> reqwest::Response {
        let mut request = client()
            .post(format!("{}/v1/chat/completions", self.base))
            .json(&body);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.send().await.expect("request")
    }

    /// `(status, body, how long the client waited)`.
    async fn chat(&self, model: &str, text: &str) -> (u16, Value, Duration) {
        self.chat_with(json!({"model": model, "messages": [{"role": "user", "content": text}]}))
            .await
    }

    async fn chat_with(&self, body: Value) -> (u16, Value, Duration) {
        let started = Instant::now();
        let response = self.post(body, &[]).await;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
            started.elapsed(),
        )
    }

    async fn get(&self, path: &str) -> Value {
        client()
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .expect("request")
            .json()
            .await
            .expect("json")
    }

    /// The newest trace of a client request (not the router's own
    /// classification request).
    async fn last_trace(&self) -> Value {
        self.get("/api/router/v1/traces?limit=50").await["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|trace| trace["route"] != "RouterClassifier")
            .cloned()
            .unwrap_or(Value::Null)
    }

    async fn metrics(&self) -> String {
        client()
            .get(format!("{}/metrics", self.base))
            .send()
            .await
            .expect("request")
            .text()
            .await
            .expect("text")
    }

    /// Every `router_requests_total` series of client requests, summed.
    async fn requests_total(&self) -> u64 {
        self.metrics()
            .await
            .lines()
            .filter(|line| line.starts_with("router_requests_total{"))
            .filter(|line| !line.contains("route=\"RouterClassifier\""))
            .filter_map(|line| line.rsplit(' ').next()?.parse::<u64>().ok())
            .sum()
    }

    async fn series(&self, series: &str) -> Option<u64> {
        value_of(&self.metrics().await, series)
    }

    fn exhausted(&self, stage: lightweight_router::budget::Stage) -> u64 {
        self.state.metrics.budget_exhausted(stage)
    }

    fn after_attempt(&self, ms: u64) {
        self.state
            .phase_delays
            .after_attempt_ms
            .store(ms, Ordering::SeqCst);
    }

    fn before_planning(&self, ms: u64) {
        self.state
            .phase_delays
            .before_planning_ms
            .store(ms, Ordering::SeqCst);
    }
}

/// The value of one exact series line, e.g. `name{a="b"}`.
fn value_of(text: &str, series: &str) -> Option<u64> {
    text.lines()
        .find(|line| line.starts_with(series) && line[series.len()..].starts_with(' '))
        .and_then(|line| line.rsplit(' ').next()?.parse().ok())
}

use lightweight_router::budget::Stage;

// --- topologies ---------------------------------------------------------------------

/// `routes`: name → its deployments' nodes, in priority order. Each node is
/// its own deployment, named `<route>-<index>`, serving the node's alias.
fn topology(routes: &[(&str, &[&Node])], classifier: &[&Node], auto: Option<Value>) -> Value {
    let mut nodes = Vec::new();
    let mut route_rows = Vec::new();
    let mut add = |route: &str, deployments: &[&Node]| {
        let mut rows = Vec::new();
        for (index, node) in deployments.iter().enumerate() {
            let id = format!("{}-{index}", route.to_lowercase());
            nodes.push(json!({"id": id, "url": node.base}));
            rows.push(json!({"node": id, "model": node.script.serving}));
        }
        route_rows.push(json!({"name": route, "deployments": rows}));
    };
    for (route, deployments) in routes {
        add(route, deployments);
    }
    if !classifier.is_empty() {
        add("RouterClassifier", classifier);
    }
    let mut config = json!({"nodes": nodes, "routes": route_rows});
    if let Some(auto) = auto {
        config["auto_route"] = auto;
    }
    config
}

/// `Auto` that classifies everything among General, Coder and Reasoning
/// (falling back to General), with `fallback` as its cross-route lists.
fn classifying(fallback: Value, timeout_ms: u64) -> Value {
    json!({
        "enabled": true,
        "fallback_route": "General",
        "classifier": {"routes": ["General", "Coder", "Reasoning"],
                       "lightweight": {"route": "RouterClassifier", "timeout_ms": timeout_ms}},
        "rules": [{"name": "semantic", "when": {}, "classify": true}],
        "cross_route_fallback": fallback,
    })
}

/// `Auto` sending everything to `route`, deterministically: no rule
/// matches, so its fallback route takes every request.
fn ruled(route: &str, fallback: Value) -> Value {
    json!({
        "enabled": true,
        "fallback_route": route,
        "rules": [],
        "cross_route_fallback": fallback,
    })
}

/// Adaptive scoring on, so route history records observations.
fn scored(mut auto: Value) -> Value {
    auto["adaptive_scoring"] = json!({"enabled": true, "weights": {"prior": 0.1},
                                      "priors": {"General": 1.0}});
    auto
}

/// The routes most tests need, each one healthy node.
struct Fleet {
    general: Node,
    coder: Node,
    reasoning: Node,
    classifier: Node,
}

impl Fleet {
    async fn start() -> Self {
        Self {
            general: Node::start("GeneralAlias").await,
            coder: Node::start("CoderAlias").await,
            reasoning: Node::start("ReasoningAlias").await,
            classifier: Node::classifier().await,
        }
    }

    fn config(&self, auto: Option<Value>) -> Value {
        topology(
            &[
                ("General", &[&self.general]),
                ("Coder", &[&self.coder]),
                ("Reasoning", &[&self.reasoning]),
            ],
            &[&self.classifier],
            auto,
        )
    }
}

fn assert_cut_near(elapsed: Duration, deadline: Duration) {
    assert!(
        elapsed >= deadline - ms(50) && elapsed < deadline + SLACK,
        "cut at {elapsed:?}, expected about {deadline:?}"
    );
}

fn assert_budget_504(status: u16, body: &Value, budget_ms: u64) {
    assert_eq!(status, 504, "{body}");
    assert_eq!(body["error"]["code"], "request_budget_exhausted", "{body}");
    assert_eq!(body["error"]["type"], "server_error", "{body}");
    assert_eq!(
        body["error"]["message"],
        format!(
            "The router's pre-commit request budget of {budget_ms} ms ran out before a response started."
        ),
    );
}

fn histories(metrics: &str) -> Vec<String> {
    let mut rows: Vec<String> = metrics
        .lines()
        .filter(|line| line.starts_with("router_route_history_observations_total{"))
        .map(str::to_owned)
        .collect();
    rows.sort();
    rows
}

// --- B1 / B41: configuration absent, unchanged ----------------------------------------

/// B1 (#1, #59): without the key, nothing changes — no budget block in a
/// trace, no budget series in the metrics, no new timeout (a slow node is
/// waited for, as before), and the admin view says it is not configured.
#[tokio::test]
async fn b01_without_a_budget_nothing_changes() {
    let fleet = Fleet::start().await;
    fleet.coder.delay(1_300);
    let router = Router::start(fleet.config(Some(classifying(json!({}), 5_000))), None).await;

    let (status, body, elapsed) = router.chat("Coder", "slow but fine").await;
    assert_eq!(
        (status, body["model"].as_str()),
        (200, Some("Coder")),
        "{body}"
    );
    assert!(elapsed >= ms(1_300), "no new timeout: {elapsed:?}");
    let trace = router.last_trace().await;
    assert!(trace.get("request_budget").is_none(), "{trace}");
    assert_eq!(trace["outcome"], "ok");

    fleet.coder.set(BUSY).delay(0);
    let (status, body, _) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_eq!(status, 503, "the route's own refusal, as before: {body}");
    assert!(router.last_trace().await.get("request_budget").is_none());

    let metrics = router.metrics().await;
    assert!(
        !metrics
            .lines()
            .any(|line| line.contains("router_request_budget")
                || line.contains("outcome=\"request_budget_exhausted\"")),
        "{metrics}"
    );
    assert_eq!(
        router.get("/api/router/v1/request-budget").await,
        json!({"object": "router.request_budget", "configured": false,
               "pre_commit_budget_ms": null, "scope": "all_client_requests",
               "governs": "pre_commit", "exhaustions_total": 0,
               "exhaustions_by_stage": {"classifier": 0, "route_planning": 0,
                                        "same_route_attempt": 0, "cross_route_fallback": 0}})
    );
}

// --- B2: success within the budget ----------------------------------------------------

/// B2 (#57, #58): a fast answer within its budget: 200, the budget's state at
/// commit in the trace, one remaining-at-commit observation, no exhaustion.
#[tokio::test]
async fn b02_a_fast_success_records_what_was_left_at_commit() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config(None), Some(30_000)).await;
    let (status, body, _) = router.chat("Coder", "hello").await;
    assert_eq!(
        (status, body["model"].as_str()),
        (200, Some("Coder")),
        "{body}"
    );

    let block = &router.last_trace().await["request_budget"];
    assert_eq!(block["configured_ms"], 30_000);
    assert_eq!(block["exhausted"], false);
    let remaining = block["remaining_at_commit_ms"].as_u64().unwrap();
    let elapsed = block["elapsed_before_commit_ms"].as_u64().unwrap();
    assert!(remaining > 0 && remaining <= 30_000, "{block}");
    assert!(
        elapsed + remaining <= 30_000 && elapsed + remaining >= 29_990,
        "{block}"
    );
    assert!(
        block.get("stage").is_none() && block.get("elapsed_ms").is_none(),
        "{block}"
    );

    assert_eq!(router.state.metrics.budget_remaining_observations(), 1);
    assert_eq!(router.state.metrics.budget_exhausted_total(), 0);
    let metrics = router.metrics().await;
    assert_eq!(
        value_of(
            &metrics,
            "router_request_budget_remaining_at_commit_seconds_count"
        ),
        Some(1)
    );
    assert_eq!(
        value_of(&metrics, "router_request_budget_configured_seconds"),
        Some(30)
    );
    for stage in Stage::ALL {
        assert_eq!(
            value_of(
                &metrics,
                &format!(
                    "router_request_budget_exhausted_total{{stage=\"{}\"}}",
                    stage.as_str()
                )
            ),
            Some(0)
        );
    }
    assert!(!metrics.contains("outcome=\"request_budget_exhausted\""));
}

// --- B3–B6: one deadline, never reset ---------------------------------------------------

/// B3 (#7, #8, #62, M1–M3, M24): one deadline across classification, two
/// deployments of the initial route, and a fallback route. Every stage takes
/// part of the budget, and the last one is cut at the *original* deadline —
/// a fresh budget at any stage would have ended the request at least a
/// second later.
#[tokio::test]
async fn b03_one_absolute_deadline_across_classifier_deployments_and_fallback() {
    let fleet = Fleet::start().await;
    let coder_b = Node::start("CoderBackup").await;
    fleet.classifier.delay(1_000);
    fleet.coder.set(BUSY).delay(800);
    coder_b.set(BUSY).delay(800);
    fleet.general.set(HANG);
    let config = topology(
        &[
            ("General", &[&fleet.general]),
            ("Coder", &[&fleet.coder, &coder_b]),
            ("Reasoning", &[&fleet.reasoning]),
        ],
        &[&fleet.classifier],
        Some(classifying(json!({"Coder": ["General"]}), 10_000)),
    );
    let router = Router::start(config, Some(4_000)).await;

    let (status, body, elapsed) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_budget_504(status, &body, 4_000);
    assert_cut_near(elapsed, ms(4_000));
    assert_eq!(
        (fleet.coder.hits(), coder_b.hits(), fleet.general.hits()),
        (1, 1, 1)
    );
    let trace = router.last_trace().await;
    assert_eq!(trace["route"], "General");
    assert_eq!(trace["request_budget"]["stage"], "cross_route_fallback");
    assert!(trace["request_budget"]["elapsed_ms"].as_u64().unwrap() >= 4_000);
    assert_eq!(router.exhausted(Stage::CrossRouteFallback), 1);
}

/// B4 (#10, #26, M1): the fallback route gets what the initial route left,
/// not a fresh budget.
#[tokio::test]
async fn b04_a_fallback_route_gets_only_what_is_left() {
    let fleet = Fleet::start().await;
    fleet.coder.set(BUSY).delay(1_200);
    fleet.general.set(HANG);
    let router = Router::start(
        fleet.config(Some(ruled("Coder", json!({"Coder": ["General"]})))),
        Some(2_000),
    )
    .await;
    let (status, body, elapsed) = router.chat("Auto", "anything").await;
    assert_budget_504(status, &body, 2_000);
    assert_cut_near(elapsed, ms(2_000));
    assert_eq!(fleet.general.hits(), 1, "General was attempted");
    assert_eq!(
        router
            .state
            .metrics
            .cross_route_fallbacks("Coder", "General", "route_exhausted"),
        1,
        "an attempted fallback is a counted transition"
    );
}

/// #27: Reasoning gets only what Coder and General left.
#[tokio::test]
async fn b04_reasoning_gets_only_what_coder_and_general_left() {
    let fleet = Fleet::start().await;
    fleet.coder.set(BUSY).delay(1_000);
    fleet.general.set(BUSY).delay(1_000);
    fleet.reasoning.set(HANG);
    let router = Router::start(
        fleet.config(Some(ruled(
            "Coder",
            json!({"Coder": ["General", "Reasoning"]}),
        ))),
        Some(3_000),
    )
    .await;
    let (status, body, elapsed) = router.chat("Auto", "anything").await;
    assert_budget_504(status, &body, 3_000);
    assert_cut_near(elapsed, ms(3_000));
    assert_eq!(
        (
            fleet.coder.hits(),
            fleet.general.hits(),
            fleet.reasoning.hits()
        ),
        (1, 1, 1)
    );
    let trace = router.last_trace().await;
    assert_eq!(trace["route"], "Reasoning");
    let hops: Vec<(&str, &str, &str)> = trace["cross_route_fallback"]["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["route"].as_str().unwrap(),
                row["outcome"].as_str().unwrap(),
                row["reason"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        hops,
        [
            ("Coder", "failed", "route_exhausted"),
            ("General", "failed", "route_exhausted"),
            ("Reasoning", "failed", "request_budget_exhausted"),
        ]
    );
    assert_eq!(
        trace["cross_route_fallback"]["exhausted"], false,
        "time ran out, not the list"
    );
    assert_eq!(trace["cross_route_fallback"]["final_route"], "Reasoning");
}

/// B5 (#9, #25, M2): the second deployment of a route gets what the first
/// left.
#[tokio::test]
async fn b05_a_second_deployment_gets_only_what_is_left() {
    let a = Node::start("CoderAlias").await;
    let b = Node::start("CoderBackup").await;
    a.set(BUSY).delay(1_200);
    b.set(HANG);
    let router = Router::start(topology(&[("Coder", &[&a, &b])], &[], None), Some(2_000)).await;
    let (status, body, elapsed) = router.chat("Coder", "anything").await;
    assert_budget_504(status, &body, 2_000);
    assert_cut_near(elapsed, ms(2_000));
    assert_eq!((a.hits(), b.hits()), (1, 1));

    let trace = router.last_trace().await;
    let budget = &trace["request_budget"];
    assert_eq!(budget["stage"], "same_route_attempt");
    assert!(
        budget.get("next_unattempted_route").is_none(),
        "a cut attempt: {budget}"
    );
    let outcomes: Vec<&str> = trace["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes, ["failed", "request_budget_exhausted"]);
    assert_eq!(
        router
            .series("router_failovers_total{route=\"Coder\"}")
            .await,
        Some(1)
    );
    assert_eq!(router.exhausted(Stage::SameRouteAttempt), 1);
}

/// B6 (#11, M4, M24): classification spends the budget; routing gets the
/// rest. `routing_ms` still leaves the classifier's time out.
#[tokio::test]
async fn b06_classification_consumes_the_budget() {
    let fleet = Fleet::start().await;
    fleet.classifier.delay(1_200);
    fleet.coder.set(HANG);
    let router = Router::start(
        fleet.config(Some(classifying(json!({}), 10_000))),
        Some(2_000),
    )
    .await;
    let (status, body, elapsed) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_budget_504(status, &body, 2_000);
    assert_cut_near(elapsed, ms(2_000));
    let trace = router.last_trace().await;
    assert_eq!(trace["route"], "Coder");
    assert_eq!(trace["classifier"]["outcome"], "chosen");
    assert!(trace["classifier"]["duration_ms"].as_f64().unwrap() >= 1_200.0);
    assert!(
        trace["routing_ms"].as_f64().unwrap() < 500.0,
        "routing_ms still excludes classification: {trace}"
    );
    assert_eq!(trace["request_budget"]["stage"], "same_route_attempt");
}

// --- B7 / B8 / B43: the classifier's own timeout and the request's deadline ------------

/// B7 (#12): the provider's timeout fires first with budget left: R9.1's
/// behaviour is unchanged — outcome `timeout`, the classifier's fallback
/// route answers.
#[tokio::test]
async fn b07_a_provider_timeout_with_budget_left_takes_the_classifier_fallback() {
    let fleet = Fleet::start().await;
    fleet.classifier.delay(2_000);
    let router = Router::start(
        fleet.config(Some(classifying(json!({}), 300))),
        Some(10_000),
    )
    .await;
    let (status, body, _) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_eq!(
        (status, body["model"].as_str()),
        (200, Some("General")),
        "{body}"
    );
    let trace = router.last_trace().await;
    assert_eq!(trace["classifier"]["outcome"], "timeout");
    assert_eq!(trace["request_budget"]["exhausted"], false);
    let metrics = router.metrics().await;
    assert_eq!(
        value_of(
            &metrics,
            "router_classifier_requests_total{provider=\"lightweight\",outcome=\"timeout\"}"
        ),
        Some(1)
    );
    assert_eq!(router.state.metrics.budget_exhausted_total(), 0);
}

/// B8 (#13, #51, #54, M8, M9): the request's deadline comes first while
/// classifying: 504 at the deadline, stage `classifier`, no R9.1 fallback
/// route, nothing else attempted; counted under `Auto`, never the verdict,
/// the classifier's fallback or `_unknown`; a classification outcome of its
/// own, not `timeout`, and not a provider failure.
#[tokio::test]
async fn b08_the_request_deadline_first_during_classification_ends_the_request() {
    let fleet = Fleet::start().await;
    fleet.classifier.set(HANG);
    let router = Router::start(
        fleet.config(Some(classifying(json!({"General": ["Coder"]}), 10_000))),
        Some(MIN_BUDGET),
    )
    .await;
    let (status, body, elapsed) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_budget_504(status, &body, MIN_BUDGET);
    assert_cut_near(elapsed, ms(MIN_BUDGET));
    assert_eq!(
        (
            fleet.general.hits(),
            fleet.coder.hits(),
            fleet.reasoning.hits()
        ),
        (0, 0, 0),
        "no classifier fallback route, no route at all"
    );
    assert_eq!(fleet.classifier.hits(), 1, "asked once, never again");

    let trace = router.last_trace().await;
    assert_eq!(trace["route"], "Auto");
    assert_eq!(trace["requested_route"], "Auto");
    assert_eq!(trace["classifier"]["outcome"], "request_budget_exhausted");
    assert_eq!(trace["attempts"], json!([]));
    assert_eq!(trace["outcome"], "request_budget_exhausted");
    assert_eq!(trace["status"], 504);
    let budget = &trace["request_budget"];
    assert_eq!(budget["stage"], "classifier");
    assert_eq!(budget["exhausted"], true);
    assert!(budget.get("next_unattempted_route").is_none(), "{budget}");

    let metrics = router.metrics().await;
    assert_eq!(
        value_of(
            &metrics,
            "router_requests_total{route=\"Auto\",outcome=\"request_budget_exhausted\"}"
        ),
        Some(1)
    );
    for label in ["Coder", "General", "_unknown"] {
        assert!(
            !metrics.contains(&format!(
                "router_requests_total{{route=\"{label}\",outcome=\"request_budget_exhausted\"}}"
            )),
            "{label}"
        );
    }
    assert_eq!(router.requests_total().await, 1);
    assert_eq!(
        value_of(
            &metrics,
            "router_classifier_requests_total{provider=\"lightweight\",outcome=\"request_budget_exhausted\"}"
        ),
        Some(1)
    );
    assert!(!metrics.contains("outcome=\"timeout\""), "{metrics}");
    assert_eq!(router.exhausted(Stage::Classifier), 1);
    assert_eq!(router.state.metrics.budget_exhausted_total(), 1);
    let status = &router.get("/api/router/v1/auto").await["classifier"]["status"];
    assert!(
        status["last_failure_kind"].is_null(),
        "not a provider failure: {status}"
    );
    assert!(!metrics.contains("router_cross_route_fallback_total{"));
}

/// B9 (#14): a budget already spent when classification would start: the
/// classifier is never asked.
#[tokio::test]
async fn b09_a_spent_budget_never_asks_the_classifier() {
    let fleet = Fleet::start().await;
    let router = Router::start(
        fleet.config(Some(classifying(json!({}), 10_000))),
        Some(MIN_BUDGET),
    )
    .await;
    router.before_planning(MIN_BUDGET + 100);
    let (status, body, _) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_budget_504(status, &body, MIN_BUDGET);
    assert_eq!(fleet.classifier.hits(), 0);
    assert_eq!(fleet.general.hits() + fleet.coder.hits(), 0);
    let trace = router.last_trace().await;
    assert_eq!(trace["classifier"]["outcome"], "request_budget_exhausted");
    assert_eq!(trace["request_budget"]["stage"], "classifier");
    assert_eq!(router.exhausted(Stage::Classifier), 1);
}

// --- B9–B12: no new work once the budget is spent ----------------------------------------

/// B9 (#15, #20, #52, #53, M5, M22): spent before the first deployment
/// attempt of an explicit route: nothing is sent, no lease is taken, and the
/// request is counted under the route the client named.
#[tokio::test]
async fn b09_spent_before_the_first_attempt_nothing_is_sent() {
    let coder = Node::start("CoderAlias").await;
    let router = Router::start(
        topology(&[("Coder", &[&coder])], &[], None),
        Some(MIN_BUDGET),
    )
    .await;
    router.before_planning(MIN_BUDGET + 100);
    let (status, body, _) = router.chat("Coder", "anything").await;
    assert_budget_504(status, &body, MIN_BUDGET);
    assert_eq!(coder.hits(), 0);
    let trace = router.last_trace().await;
    assert_eq!(trace["route"], "Coder");
    assert_eq!(trace["attempts"], json!([]));
    let budget = &trace["request_budget"];
    assert_eq!(budget["stage"], "route_planning");
    assert_eq!(budget["next_unattempted_route"], "Coder");
    assert_eq!(
        router
            .series("router_requests_total{route=\"Coder\",outcome=\"request_budget_exhausted\"}")
            .await,
        Some(1)
    );
    assert_eq!(router.requests_total().await, 1);
    assert_eq!(
        router
            .series("router_deployment_active_requests{deployment=\"coder-0/CoderAlias\"}")
            .await,
        Some(0)
    );
    assert_eq!(router.exhausted(Stage::RoutePlanning), 1);
}

/// B9/B30 (#51, M22): the same for `Auto`: counted under `Auto`, never the
/// route it resolved to and never attempted; route history observes nothing.
#[tokio::test]
async fn b09_auto_spent_before_any_route_is_counted_under_auto() {
    let fleet = Fleet::start().await;
    // A classifier is configured (scoring needs one), but the matching rule
    // is deterministic: nothing is classified.
    let mut auto = scored(classifying(json!({}), 10_000));
    auto["rules"] = json!([]);
    let router = Router::start(fleet.config(Some(auto)), Some(MIN_BUDGET)).await;
    router.before_planning(MIN_BUDGET + 100);
    let (status, body, _) = router.chat("Auto", "anything").await;
    assert_budget_504(status, &body, MIN_BUDGET);
    assert_eq!(fleet.general.hits(), 0);
    let trace = router.last_trace().await;
    assert_eq!(trace["route"], "Auto");
    assert_eq!(trace["request_budget"]["stage"], "route_planning");
    assert_eq!(trace["request_budget"]["next_unattempted_route"], "General");
    let metrics = router.metrics().await;
    assert_eq!(
        value_of(
            &metrics,
            "router_requests_total{route=\"Auto\",outcome=\"request_budget_exhausted\"}"
        ),
        Some(1)
    );
    assert!(!metrics.contains("route=\"General\",outcome=\"request_budget_exhausted\""));
    assert_eq!(
        histories(&metrics),
        Vec::<String>::new(),
        "a refused route is not observed"
    );
}

/// B10 (#16, #55, M5): the first deployment failed just as the budget ran
/// out: the second is not started, and no failover is counted for it.
#[tokio::test]
async fn b10_a_same_route_retry_is_not_started_once_spent() {
    let a = Node::start("CoderAlias").await;
    let b = Node::start("CoderBackup").await;
    a.set(BUSY);
    let router = Router::start(
        topology(&[("Coder", &[&a, &b])], &[], None),
        Some(MIN_BUDGET),
    )
    .await;
    router.after_attempt(MIN_BUDGET + 100);
    let (status, body, _) = router.chat("Coder", "anything").await;
    assert_budget_504(status, &body, MIN_BUDGET);
    assert_eq!((a.hits(), b.hits()), (1, 0));
    let trace = router.last_trace().await;
    let budget = &trace["request_budget"];
    assert_eq!(budget["stage"], "same_route_attempt");
    assert!(
        budget.get("next_unattempted_route").is_none(),
        "only a deployment: {budget}"
    );
    assert_eq!(trace["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(
        router
            .series("router_failovers_total{route=\"Coder\"}")
            .await,
        None
    );
    assert_eq!(router.exhausted(Stage::SameRouteAttempt), 1);
}

/// B11/B12 (#17–#21, #56, M6, M7): the initial route failed for a fallback
/// reason just as the budget ran out: the next route is not attempted, no
/// transition is counted to it, the list is not "exhausted", and the request
/// is counted under the route that failed. That route keeps the history
/// observation its own failure earned; the refused one gets none.
#[tokio::test]
async fn b11_a_fallback_route_is_not_started_once_spent() {
    let fleet = Fleet::start().await;
    fleet.coder.set(BUSY);
    let router = Router::start(
        fleet.config(Some(scored(classifying(
            json!({"Coder": ["General", "Reasoning"]}),
            10_000,
        )))),
        Some(MIN_BUDGET),
    )
    .await;
    router.after_attempt(MIN_BUDGET + 100);
    let (status, body, _) = router.chat("Auto", "PICK Coder 0.95").await;
    assert_budget_504(status, &body, MIN_BUDGET);
    assert_eq!(
        (
            fleet.coder.hits(),
            fleet.general.hits(),
            fleet.reasoning.hits()
        ),
        (1, 0, 0)
    );

    let trace = router.last_trace().await;
    assert_eq!(trace["route"], "Coder");
    let budget = &trace["request_budget"];
    assert_eq!(budget["stage"], "cross_route_fallback");
    assert_eq!(budget["next_unattempted_route"], "General");
    let block = &trace["cross_route_fallback"];
    assert_eq!(block["exhausted"], false);
    assert_eq!(block["final_route"], "Coder");
    assert_eq!(
        block["attempts"],
        json!([{"route": "Coder", "outcome": "failed", "reason": "route_exhausted"}]),
        "General never appears as attempted"
    );

    let metrics = router.metrics().await;
    assert_eq!(
        router
            .state
            .metrics
            .cross_route_fallbacks("Coder", "General", "route_exhausted"),
        0
    );
    assert!(!metrics.contains("router_cross_route_fallback_total{"));
    assert!(!metrics.contains("router_cross_route_fallback_exhausted_total{"));
    assert_eq!(
        value_of(
            &metrics,
            "router_requests_total{route=\"Coder\",outcome=\"request_budget_exhausted\"}"
        ),
        Some(1)
    );
    assert_eq!(router.requests_total().await, 1);
    assert_eq!(
        histories(&metrics),
        ["router_route_history_observations_total{route=\"Coder\",outcome=\"unavailable\"} 1"],
        "Coder keeps what it earned; General was never attempted"
    );
    assert_eq!(router.exhausted(Stage::CrossRouteFallback), 1);
}

// --- B13–B15: caps on waits that had none, or a longer one --------------------------------

/// B14/B16 (#23, #28): an explicit route whose node accepts and never
/// answers is cut at the deadline.
#[tokio::test]
async fn b14_an_unanswered_response_head_is_cut_at_the_deadline() {
    let coder = Node::start("CoderAlias").await;
    coder.set(HANG);
    let router = Router::start(
        topology(&[("Coder", &[&coder])], &[], None),
        Some(MIN_BUDGET),
    )
    .await;
    let (status, body, elapsed) = router.chat("Coder", "anything").await;
    assert_budget_504(status, &body, MIN_BUDGET);
    assert_cut_near(elapsed, ms(MIN_BUDGET));
    let trace = router.last_trace().await;
    assert_eq!(trace["outcome"], "request_budget_exhausted");
    assert_eq!(trace["status"], 504);
}

/// B16 (#28) for `Auto`: the same enforcement whoever chose the route.
#[tokio::test]
async fn b16_auto_and_explicit_routes_get_the_same_budget() {
    let fleet = Fleet::start().await;
    fleet.general.set(HANG);
    let router = Router::start(
        fleet.config(Some(ruled("General", json!({})))),
        Some(MIN_BUDGET),
    )
    .await;
    for model in ["Auto", "General"] {
        let (status, body, elapsed) = router.chat(model, "anything").await;
        assert_budget_504(status, &body, MIN_BUDGET);
        assert_cut_near(elapsed, ms(MIN_BUDGET));
    }
}

/// B17 (#29, M27): an explicit route never gains cross-route fallback, with
/// or without the budget firing.
#[tokio::test]
async fn b17_an_explicit_route_never_falls_back_under_a_budget() {
    let fleet = Fleet::start().await;
    fleet.coder.set(BUSY);
    let router = Router::start(
        fleet.config(Some(ruled("Coder", json!({"Coder": ["General"]})))),
        Some(MIN_BUDGET),
    )
    .await;
    let (status, body, _) = router.chat("Coder", "anything").await;
    assert_eq!(status, 503, "Coder's own error: {body}");
    fleet.coder.set(HANG);
    let (status, body, _) = router.chat("Coder", "anything").await;
    assert_budget_504(status, &body, MIN_BUDGET);
    assert_eq!(fleet.general.hits(), 0);
    let trace = router.last_trace().await;
    assert!(trace.get("cross_route_fallback").is_none(), "{trace}");
    assert!(
        !router
            .metrics()
            .await
            .contains("router_cross_route_fallback_total{")
    );
}

/// B18 (#30): a `500` is the answer, under a budget too: relayed, never
/// rewritten, never a fallback.
#[tokio::test]
async fn b18_a_500_is_still_the_answer() {
    let fleet = Fleet::start().await;
    fleet.coder.set(BROKEN);
    let router = Router::start(
        fleet.config(Some(ruled("Coder", json!({"Coder": ["General"]})))),
        Some(10_000),
    )
    .await;
    let (status, body, _) = router.chat("Auto", "anything").await;
    assert_eq!(status, 500, "{body}");
    assert_eq!(body["error"]["code"], "internal_error");
    assert_eq!(fleet.general.hits(), 0);
    let trace = router.last_trace().await;
    assert_eq!(trace["request_budget"]["exhausted"], false);
    assert!(
        trace["request_budget"]["remaining_at_commit_ms"]
            .as_u64()
            .is_some()
    );
}

/// B19 (#31): context overflow is unchanged — same-route failover to a larger
/// context, and no cross-route fallback.
#[tokio::test]
async fn b19_context_overflow_is_unchanged() {
    let small = Node::with_context("CoderSmall", 2048).await;
    let large = Node::with_context("CoderLarge", 8192).await;
    let general = Node::start("GeneralAlias").await;
    small.set(OVERFLOW);
    let router = Router::start(
        topology(
            &[("General", &[&general]), ("Coder", &[&small, &large])],
            &[],
            Some(ruled("Coder", json!({"Coder": ["General"]}))),
        ),
        Some(10_000),
    )
    .await;
    let (status, body, _) = router.chat("Auto", "long prompt").await;
    assert_eq!(
        (status, body["model"].as_str()),
        (200, Some("Coder")),
        "{body}"
    );
    assert_eq!((small.hits(), large.hits()), (1, 1));

    large.set(OVERFLOW);
    let (status, body, _) = router.chat("Auto", "long prompt").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "context_length_exceeded");
    assert_eq!(general.hits(), 0, "a context overflow never falls back");
}

// --- B20–B22: the commit boundary -------------------------------------------------------------

/// B20/B40 (#63, M10, M23): a non-streamed head committed before the
/// deadline: its body, arriving long after, is relayed whole; nothing else is
/// asked and nothing is spliced.
#[tokio::test]
async fn b20_a_committed_answer_outlives_the_budget() {
    let fleet = Fleet::start().await;
    fleet.coder.set(SLOW_BODY).tail(1_800);
    let router = Router::start(
        fleet.config(Some(ruled("Coder", json!({"Coder": ["General"]})))),
        Some(MIN_BUDGET),
    )
    .await;
    let (status, body, elapsed) = router.chat("Auto", "anything").await;
    assert_eq!(
        (status, body["model"].as_str()),
        (200, Some("Coder")),
        "{body}"
    );
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "answer from CoderAlias"
    );
    assert!(elapsed >= ms(1_800), "{elapsed:?}");
    assert_eq!((fleet.coder.hits(), fleet.general.hits()), (1, 0));
    let trace = router.last_trace().await;
    assert_eq!(trace["outcome"], "ok");
    assert_eq!(trace["request_budget"]["exhausted"], false);
    assert!(
        trace["request_budget"]["remaining_at_commit_ms"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(router.state.metrics.budget_exhausted_total(), 0);
}

/// B21 (#37): a streamed head commits at once; the stream that follows runs
/// well past the deadline and is relayed to its end.
#[tokio::test]
async fn b21_a_committed_stream_is_never_cut() {
    let fleet = Fleet::start().await;
    fleet.coder.tail(2_000);
    let router = Router::start(fleet.config(None), Some(MIN_BUDGET)).await;
    let started = Instant::now();
    let response = router
        .post(
            json!({"model": "Coder", "stream": true,
                   "messages": [{"role": "user", "content": "stream please"}]}),
            &[],
        )
        .await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(started.elapsed() >= ms(2_000));
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    assert_eq!(text.matches(" more").count(), 4, "{text}");
    let trace = router.last_trace().await;
    assert_eq!(trace["outcome"], "ok");
    assert_eq!(trace["request_budget"]["exhausted"], false);
    assert_eq!(fleet.coder.hits(), 1);
}

/// #38: a committed stream that breaks still never falls back, budget or not.
#[tokio::test]
async fn b21_a_broken_committed_stream_never_falls_back() {
    let fleet = Fleet::start().await;
    let router = Router::start(
        fleet.config(Some(ruled("Coder", json!({"Coder": ["General"]})))),
        Some(MIN_BUDGET),
    )
    .await;
    let response = router
        .post(
            json!({"model": "Auto", "stream": true,
                   "messages": [{"role": "user", "content": "BREAK"}]}),
            &[],
        )
        .await;
    assert_eq!(response.status(), 200);
    let _ = response.text().await;
    tokio::time::sleep(ms(MIN_BUDGET + 200)).await;
    assert_eq!(fleet.general.hits(), 0);
    let trace = router.last_trace().await;
    assert_eq!(trace["outcome"], "interrupted");
    assert_eq!(trace["request_budget"]["exhausted"], false);
}

/// B22 (#39, #40): a non-streamed generation longer than the budget — its head
/// comes only when it is done — is cut at the deadline and answered promptly.
#[tokio::test]
async fn b22_a_long_non_streamed_generation_is_cut_promptly() {
    let coder = Node::start("CoderAlias").await;
    coder.delay(10_000);
    let router = Router::start(
        topology(&[("Coder", &[&coder])], &[], None),
        Some(MIN_BUDGET),
    )
    .await;
    let (status, body, elapsed) = router
        .chat_with(json!({"model": "Coder", "stream": false,
                          "messages": [{"role": "user", "content": "write a novel"}]}))
        .await;
    assert_budget_504(status, &body, MIN_BUDGET);
    assert_cut_near(elapsed, ms(MIN_BUDGET));
}

// --- B23/B24: causal, not clock-based --------------------------------------------------------

/// B23 (#32, M17): a route error that completed before the deadline is the
/// answer, though the deadline passed while it was being answered. The trace
/// says how much was left, and that the budget did not end it.
#[tokio::test]
async fn b23_a_route_error_that_completed_stays_the_answer() {
    let fleet = Fleet::start().await;
    fleet.coder.set(BUSY);
    // `Auto` with an empty fallback list: Coder's own error is final.
    let router = Router::start(
        fleet.config(Some(ruled("Coder", json!({})))),
        Some(MIN_BUDGET),
    )
    .await;
    router.after_attempt(MIN_BUDGET + 100);
    let (status, body, _) = router.chat("Auto", "anything").await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["code"], "server_busy");

    let trace = router.last_trace().await;
    assert_ne!(trace["outcome"], "request_budget_exhausted");
    let budget = &trace["request_budget"];
    assert_eq!(budget["exhausted"], false);
    assert_eq!(budget["remaining_ms"], 0);
    assert!(budget.get("stage").is_none(), "{budget}");
    assert_eq!(router.state.metrics.budget_exhausted_total(), 0);
    assert!(
        !router
            .metrics()
            .await
            .contains("outcome=\"request_budget_exhausted\"")
    );

    // The same for a route with nothing available: `route_unavailable`.
    let down = Router::start(
        json!({"nodes": [{"id": "c", "url": DOWN}],
               "routes": [{"name": "Coder", "deployments": [{"node": "c", "model": "CoderAlias"}]}]}),
        Some(MIN_BUDGET),
    )
    .await;
    let (status, body, _) = down.chat("Coder", "anything").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (503, Some("route_unavailable"))
    );
}

// --- B26–B30: identity and counting ------------------------------------------------------------

/// B26 (#43, M11): a client that leaves is `cancelled`, never budget
/// exhaustion — the deadline lives in the request's own future, which went
/// with it.
#[tokio::test]
async fn b26_client_cancellation_is_not_budget_exhaustion() {
    let coder = Node::start("CoderAlias").await;
    coder.set(HANG);
    let router = Router::start(
        topology(&[("Coder", &[&coder])], &[], None),
        Some(MIN_BUDGET),
    )
    .await;
    let impatient = reqwest::Client::builder().timeout(ms(300)).build().unwrap();
    let result = impatient
        .post(format!("{}/v1/chat/completions", router.base))
        .json(&json!({"model": "Coder", "messages": [{"role": "user", "content": "x"}]}))
        .send()
        .await;
    assert!(result.is_err(), "the client gave up first");
    // Past the deadline: nothing about the departed request may move.
    tokio::time::sleep(ms(MIN_BUDGET + 500)).await;
    let trace = router.last_trace().await;
    assert_eq!(trace["outcome"], "cancelled", "{trace}");
    assert_eq!(trace["request_budget"]["exhausted"], false);
    assert_eq!(router.state.metrics.budget_exhausted_total(), 0);
    assert!(
        !router
            .metrics()
            .await
            .contains("request_budget_exhausted\"")
    );
}

/// B27 (#42, M19): one request id on every attempt, every route, the 504 and
/// the trace; the nested classification request carries `<id>-classify`.
#[tokio::test]
async fn b27_one_request_id_everywhere() {
    let fleet = Fleet::start().await;
    let coder_b = Node::start("CoderBackup").await;
    fleet.coder.set(BUSY);
    coder_b.set(BUSY);
    fleet.general.set(HANG);
    let config = topology(
        &[
            ("General", &[&fleet.general]),
            ("Coder", &[&fleet.coder, &coder_b]),
            ("Reasoning", &[&fleet.reasoning]),
        ],
        &[&fleet.classifier],
        Some(classifying(json!({"Coder": ["General"]}), 10_000)),
    );
    let router = Router::start(config, Some(MIN_BUDGET)).await;
    let response = router
        .post(
            json!({"model": "Auto", "messages": [{"role": "user", "content": "PICK Coder 0.9"}]}),
            &[("x-request-id", "budget-one-id")],
        )
        .await;
    assert_eq!(response.status(), 504);
    assert_eq!(
        response.headers()["x-request-id"].to_str().unwrap(),
        "budget-one-id"
    );
    for node in [&fleet.coder, &coder_b, &fleet.general] {
        assert_eq!(node.request_ids(), ["budget-one-id"]);
    }
    assert_eq!(fleet.classifier.request_ids(), ["budget-one-id-classify"]);
    assert_eq!(router.last_trace().await["request_id"], "budget-one-id");
}

/// B28/B29 (#49, #50, M20, M21, M28): `Auto → Coder → General`, cut while
/// General is uncommitted: counted exactly once, under General, as its own
/// outcome — not Coder, not Auto, never `server_error`.
#[tokio::test]
async fn b29_counted_once_under_the_terminal_route() {
    let fleet = Fleet::start().await;
    let coder_b = Node::start("CoderBackup").await;
    fleet.coder.set(BUSY);
    coder_b.set(BUSY);
    fleet.general.set(HANG);
    let config = topology(
        &[
            ("General", &[&fleet.general]),
            ("Coder", &[&fleet.coder, &coder_b]),
        ],
        &[],
        Some(ruled("Coder", json!({"Coder": ["General"]}))),
    );
    let router = Router::start(config, Some(MIN_BUDGET)).await;
    let (status, body, _) = router.chat("Auto", "anything").await;
    assert_budget_504(status, &body, MIN_BUDGET);
    let metrics = router.metrics().await;
    let rows: Vec<&str> = metrics
        .lines()
        .filter(|line| line.starts_with("router_requests_total{"))
        .collect();
    assert_eq!(
        rows,
        ["router_requests_total{route=\"General\",outcome=\"request_budget_exhausted\"} 1"]
    );
    assert!(!metrics.contains("outcome=\"server_error\""));
    assert_eq!(router.exhausted(Stage::CrossRouteFallback), 1);
    assert_eq!(router.state.metrics.budget_exhausted_total(), 1);
}

// --- B31–B34: neutral to R9.2, to fallback order, to placement --------------------------------

/// B31 (#47, #48, M14): route history records what routes did, never what
/// the clock did: Coder's completed failure keeps its normal observation;
/// General, cut while waiting, is `neutral` — never `server_error`.
#[tokio::test]
async fn b31_budget_expiry_is_neutral_in_route_history() {
    let fleet = Fleet::start().await;
    fleet.coder.set(BUSY);
    fleet.general.set(HANG);
    let router = Router::start(
        fleet.config(Some(scored(classifying(
            json!({"Coder": ["General"]}),
            10_000,
        )))),
        Some(MIN_BUDGET),
    )
    .await;
    let (status, _, _) = router.chat("Auto", "PICK Coder 0.95").await;
    assert_eq!(status, 504);
    assert_eq!(
        histories(&router.metrics().await),
        [
            "router_route_history_observations_total{route=\"Coder\",outcome=\"unavailable\"} 1",
            "router_route_history_observations_total{route=\"General\",outcome=\"neutral\"} 1",
        ]
    );
}

/// B32/B33 (#46, M12, M13): the same request decides the same way — the same
/// scoring trace, the same fallback order — under no budget, a long one, or
/// the shortest one.
#[tokio::test]
async fn b32_scoring_and_fallback_order_ignore_the_budget() {
    let mut decisions = Vec::new();
    for budget in [None, Some(3_600_000), Some(MIN_BUDGET)] {
        let fleet = Fleet::start().await;
        fleet.coder.set(BUSY);
        fleet.general.set(BUSY);
        let router = Router::start(
            fleet.config(Some(scored(classifying(
                json!({"Coder": ["General", "Reasoning"]}),
                10_000,
            )))),
            budget,
        )
        .await;
        let (status, body, _) = router.chat("Auto", "PICK Coder 0.95").await;
        assert_eq!(
            (status, body["model"].as_str()),
            (200, Some("Reasoning")),
            "{body}"
        );
        let trace = router.last_trace().await;
        let mut scoring = trace["scoring"].clone();
        // The history snapshot holds decayed sample counts read at a moment;
        // everything that decides is compared.
        scoring.as_object_mut().unwrap().remove("history");
        decisions.push((scoring, trace["cross_route_fallback"]["attempts"].clone()));
    }
    assert_eq!(decisions[0], decisions[1]);
    assert_eq!(decisions[0], decisions[2]);
}

/// B34 (#45): with a budget, a route with nothing loaded is answered at once
/// with its own error; the request never waits for, or triggers, placement.
#[tokio::test]
async fn b34_placement_is_unchanged() {
    let router = Router::start(
        json!({"nodes": [{"id": "c", "url": DOWN}],
               "routes": [{"name": "Coder", "deployments": [{"node": "c", "model": "CoderAlias"}]}]}),
        Some(30_000),
    )
    .await;
    let (status, body, elapsed) = router.chat("Coder", "anything").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (503, Some("route_unavailable"))
    );
    assert!(elapsed < ms(2_000), "never waits: {elapsed:?}");
    assert!(
        router.state.placement.loading().is_empty(),
        "no load started"
    );
}

// --- B42: the nested classification request inherits ----------------------------------------

/// B42 (#62, M3): the nested classification request runs on its parent's
/// deadline: spent by the time it would start its attempt, it starts none —
/// and as a nested request it records no budget metric of its own.
#[tokio::test]
async fn b42_the_nested_classifier_request_inherits_the_deadline() {
    let fleet = Fleet::start().await;
    let router = Router::start(
        fleet.config(Some(classifying(json!({}), 10_000))),
        Some(MIN_BUDGET),
    )
    .await;
    // Each request — the client's, then the nested one — pauses this long
    // before planning: the client's budget is spent just as the nested
    // request reaches its first attempt. A fresh deadline for the nested
    // request would have let it ask the classifier.
    router.before_planning(MIN_BUDGET / 2 + 50);
    let (status, body, _) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_budget_504(status, &body, MIN_BUDGET);
    assert_eq!(
        fleet.classifier.hits(),
        0,
        "no attempt after the inherited deadline"
    );
    let trace = router.last_trace().await;
    assert_eq!(trace["classifier"]["outcome"], "request_budget_exhausted");
    assert_eq!(trace["request_budget"]["stage"], "classifier");
    assert_eq!(router.exhausted(Stage::Classifier), 1);
    assert_eq!(
        router.state.metrics.budget_exhausted_total(),
        1,
        "the nested request counted nothing of its own"
    );
}

/// B42/B27 (#62, M3): the nested classification request runs on its
/// parent's very deadline. It starts 300 ms after its parent here; a deadline
/// of its own would come 300 ms later, the parent would cut it first, and its
/// trace would read `cancelled`. Inherited, its own timer ends it at the same
/// instant, and its budget's elapsed time is measured from the client's start.
#[tokio::test]
async fn b42_the_nested_request_ends_on_the_parents_deadline() {
    let fleet = Fleet::start().await;
    fleet.classifier.set(HANG);
    let router = Router::start(
        fleet.config(Some(classifying(json!({}), 10_000))),
        Some(MIN_BUDGET),
    )
    .await;
    router.before_planning(300);
    let response = router
        .post(
            json!({"model": "Auto", "messages": [{"role": "user", "content": "PICK Coder 0.9"}]}),
            &[("x-request-id", "nested-deadline")],
        )
        .await;
    assert_eq!(response.status(), 504);
    let traces = router.get("/api/router/v1/traces?limit=50").await;
    let nested = traces["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|trace| trace["request_id"] == "nested-deadline-classify")
        .cloned()
        .expect("the nested request's trace");
    assert_eq!(nested["outcome"], "request_budget_exhausted", "{nested}");
    let budget = &nested["request_budget"];
    assert_eq!(budget["configured_ms"], MIN_BUDGET);
    assert!(
        budget["elapsed_ms"].as_u64().unwrap() >= MIN_BUDGET,
        "measured from the client's start: {budget}"
    );
    assert_eq!(router.state.metrics.budget_exhausted_total(), 1);
    assert_eq!(router.exhausted(Stage::Classifier), 1);
}

/// B42, the nested request's same-route failover: its first classifier
/// deployment failed just as the budget ran out, so its second is never asked.
#[tokio::test]
async fn b42_the_nested_request_starts_no_deployment_after_expiry() {
    let fleet = Fleet::start().await;
    let k2 = Node::classifier().await;
    fleet.classifier.set(BUSY);
    let config = topology(
        &[
            ("General", &[&fleet.general]),
            ("Coder", &[&fleet.coder]),
            ("Reasoning", &[&fleet.reasoning]),
        ],
        &[&fleet.classifier, &k2],
        Some(classifying(json!({}), 10_000)),
    );
    let router = Router::start(config, Some(MIN_BUDGET)).await;
    router.after_attempt(MIN_BUDGET + 100);
    let (status, body, _) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_budget_504(status, &body, MIN_BUDGET);
    assert_eq!((fleet.classifier.hits(), k2.hits()), (1, 0));
    assert_eq!(fleet.general.hits() + fleet.coder.hits(), 0);
    assert_eq!(router.exhausted(Stage::Classifier), 1);
    assert_eq!(router.state.metrics.budget_exhausted_total(), 1);
}

// --- B45: node health ----------------------------------------------------------------------------

/// B45 (#44, M26): a budget cut is not the node's failure: it stays healthy,
/// and the next request goes straight back to it.
#[tokio::test]
async fn b45_a_budget_cut_never_marks_a_node_unhealthy() {
    let coder = Node::start("CoderAlias").await;
    coder.set(HANG);
    let router = Router::start(
        topology(&[("Coder", &[&coder])], &[], None),
        Some(MIN_BUDGET),
    )
    .await;
    let (status, _, _) = router.chat("Coder", "anything").await;
    assert_eq!(status, 504);
    assert_eq!(
        router.series("router_node_health{node=\"coder-0\"}").await,
        Some(1)
    );
    coder.set(HEALTHY);
    let (status, body, _) = router.chat("Coder", "anything").await;
    assert_eq!(
        (status, body["model"].as_str()),
        (200, Some("Coder")),
        "{body}"
    );
    assert_eq!(coder.hits(), 2);
}

// --- admin ------------------------------------------------------------------------------------------

/// #60, #61: the admin view reports the configured budget and counts, behind
/// the router's own auth — and holds nothing about any one request.
#[tokio::test]
async fn the_admin_view_is_authorized_and_safe() {
    let coder = Node::start("CoderAlias").await;
    coder.set(HANG);
    let mut config = topology(&[("Coder", &[&coder])], &[], None);
    config["api_key_env"] = json!("ROUTER_KEY");
    let router = Router::start_with(config, Some(MIN_BUDGET), &[("ROUTER_KEY", "rk-test")]).await;

    let refused = client()
        .get(format!("{}/api/router/v1/request-budget", router.base))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401);

    let cut = client()
        .post(format!("{}/v1/chat/completions", router.base))
        .header("authorization", "Bearer rk-test")
        .json(
            &json!({"model": "Coder", "messages": [{"role": "user", "content": "secret prompt"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(cut.status(), 504);

    let view: Value = client()
        .get(format!("{}/api/router/v1/request-budget", router.base))
        .header("authorization", "Bearer rk-test")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        view,
        json!({"object": "router.request_budget", "configured": true,
               "pre_commit_budget_ms": 1000, "scope": "all_client_requests",
               "governs": "pre_commit", "exhaustions_total": 1,
               "exhaustions_by_stage": {"classifier": 0, "route_planning": 0,
                                        "same_route_attempt": 1, "cross_route_fallback": 0}})
    );
    let text = view.to_string();
    for absent in ["secret prompt", "coder-0", "127.0.0.1", "rk-test"] {
        assert!(!text.contains(absent), "{absent}");
    }
    let post = client()
        .post(format!("{}/api/router/v1/request-budget", router.base))
        .header("authorization", "Bearer rk-test")
        .json(&json!({"pre_commit_budget_ms": 5000}))
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 405, "no write endpoint");
}

/// The budget's metric labels are its four stages and nothing else.
#[tokio::test]
async fn budget_metric_labels_are_low_cardinality() {
    let coder = Node::start("CoderAlias").await;
    coder.set(HANG);
    let router = Router::start(
        topology(&[("Coder", &[&coder])], &[], None),
        Some(MIN_BUDGET),
    )
    .await;
    let _ = router
        .post(
            json!({"model": "Coder", "messages": [{"role": "user", "content": "x"}]}),
            &[("x-request-id", "label-check-id")],
        )
        .await;
    let metrics = router.metrics().await;
    let budget_lines: Vec<&str> = metrics
        .lines()
        .filter(|line| line.starts_with("router_request_budget"))
        .collect();
    assert!(!budget_lines.is_empty());
    for line in &budget_lines {
        for absent in ["label-check-id", "coder-0", "127.0.0.1", "route="] {
            assert!(!line.contains(absent), "{line}");
        }
    }
    let stages: Vec<&str> = budget_lines
        .iter()
        .filter(|line| line.starts_with("router_request_budget_exhausted_total{"))
        .copied()
        .collect();
    assert_eq!(stages.len(), 4, "{stages:?}");
}

// --- B13: a connect that never completes (Linux) --------------------------------------------------

/// A node whose listen queue can be frozen: until then it answers capability
/// probes and closes each connection (so nothing is pooled); after, it
/// accepts nothing and its full accept queue makes every new connect hang.
///
/// Linux only: there a SYN to a listener whose accept queue is full is
/// dropped, so the connect hangs exactly as one to an unreachable host does.
/// Other systems answer it with a reset, which is an ordinary refusal and
/// tests nothing new.
#[cfg(target_os = "linux")]
mod frozen {
    use std::net::SocketAddr;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpSocket, TcpStream};
    use tokio_util::sync::CancellationToken;

    pub struct Frozen {
        pub addr: SocketAddr,
        accepting: CancellationToken,
        fillers: Vec<tokio::task::JoinHandle<Option<TcpStream>>>,
    }

    impl Frozen {
        pub async fn start(alias: &str) -> Self {
            let socket = TcpSocket::new_v4().unwrap();
            socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let listener = socket.listen(1).unwrap();
            let addr = listener.local_addr().unwrap();
            let accepting = CancellationToken::new();
            let stop = accepting.clone();
            let body =
                serde_json::to_string(&lightweight_api::capabilities::CapabilitiesBody::new(
                    "0.5.0",
                    Some(lightweight_api::capabilities::CapabilityModel {
                        id: alias.to_owned(),
                        context_length: 8192,
                    }),
                    4,
                ))
                .unwrap();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        () = stop.cancelled() => break,
                        accepted = listener.accept() => {
                            let Ok((mut stream, _)) = accepted else { continue };
                            let body = body.clone();
                            tokio::spawn(async move {
                                let mut buf = vec![0_u8; 8192];
                                let _ = stream.read(&mut buf).await;
                                let head = format!(
                                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                                     content-length: {}\r\nconnection: close\r\n\r\n",
                                    body.len()
                                );
                                let _ = stream.write_all(head.as_bytes()).await;
                                let _ = stream.write_all(body.as_bytes()).await;
                                let _ = stream.shutdown().await;
                            });
                        }
                    }
                }
                // Keep the socket open, accepting nothing, for the test's life.
                let _listener = listener;
                std::future::pending::<()>().await;
            });
            Self {
                addr,
                accepting,
                fillers: Vec::new(),
            }
        }

        /// Stop accepting and fill the accept queue.
        pub async fn freeze(&mut self) {
            self.accepting.cancel();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            for _ in 0..6 {
                let addr = self.addr;
                self.fillers.push(tokio::spawn(
                    async move { TcpStream::connect(addr).await.ok() },
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }

    impl Drop for Frozen {
        fn drop(&mut self) {
            for filler in &self.fillers {
                filler.abort();
            }
        }
    }
}

/// B13 (#21, #44, M15, M26): only 1 s of budget against a 5 s connect
/// timeout: the hanging connect is cut at the deadline — not after 5 s — and
/// the node's health is untouched.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn b13_a_hanging_connect_is_cut_at_the_deadline_not_the_connect_timeout() {
    let mut node = frozen::Frozen::start("CoderAlias").await;
    let config = json!({
        "nodes": [{"id": "coder-0", "url": format!("http://{}", node.addr)}],
        "routes": [{"name": "Coder", "deployments": [{"node": "coder-0", "model": "CoderAlias"}]}],
        "request": {"connect_timeout_secs": 5, "pre_commit_budget_ms": MIN_BUDGET},
    });
    let router = Router::start(config, None).await;
    assert_eq!(
        router.series("router_node_health{node=\"coder-0\"}").await,
        Some(1)
    );
    node.freeze().await;
    let (status, body, elapsed) = router.chat("Coder", "anything").await;
    assert_budget_504(status, &body, MIN_BUDGET);
    assert_cut_near(elapsed, ms(MIN_BUDGET));
    assert_eq!(
        router.series("router_node_health{node=\"coder-0\"}").await,
        Some(1),
        "a budget cut is not the node's failure"
    );
}

/// #22: an operation's own limit still fires first when it is the shorter:
/// a 1 s connect timeout under a 10 s budget fails the connect at 1 s, as
/// today — a node failure, counted against its health — not a budget 504.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn b13_a_shorter_connect_timeout_still_fires_first() {
    let mut node = frozen::Frozen::start("CoderAlias").await;
    let config = json!({
        "nodes": [{"id": "coder-0", "url": format!("http://{}", node.addr)}],
        "routes": [{"name": "Coder", "deployments": [{"node": "coder-0", "model": "CoderAlias"}]}],
        "request": {"connect_timeout_secs": 1, "pre_commit_budget_ms": 10_000},
    });
    let router = Router::start(config, None).await;
    node.freeze().await;
    let (status, body, elapsed) = router.chat("Coder", "anything").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (503, Some("route_unavailable")),
        "{body}"
    );
    assert!(elapsed >= ms(900) && elapsed < ms(5_000), "{elapsed:?}");
    assert_eq!(
        router.series("router_node_health{node=\"coder-0\"}").await,
        Some(0)
    );
    assert_eq!(router.state.metrics.budget_exhausted_total(), 0);
}
