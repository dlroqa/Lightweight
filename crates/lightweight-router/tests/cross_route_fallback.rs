//! Explicit cross-route fallback (R9.3.1), end to end, over real sockets.
//!
//! Every node is scripted, and each test builds its own small topology, so
//! which node was contacted — and which was never contacted — says exactly
//! which logical routes a request was attempted on. A node's behaviour can be
//! switched at run time (healthy, every request refused 503, every request
//! answered 500) and keyed by the prompt (`FAIL500`, `BREAK`, `OVERFLOW`,
//! `TOOLCALL`). The classifier answers `PICK <route> <confidence>`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use lightweight_api::capabilities::{CapabilitiesBody, CapabilityModel};
use lightweight_router::RouterState;
use lightweight_router::config::RouterFile;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// A node that never answers: nothing listens on the discard port.
const DOWN: &str = "http://127.0.0.1:9";

fn ensure_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client")
}

// --- scripted nodes -------------------------------------------------------------

const HEALTHY: u8 = 0;
const BUSY: u8 = 1;
const BROKEN: u8 = 2;

#[derive(Clone)]
struct Script {
    serving: String,
    classifier: bool,
    tools: bool,
    mode: Arc<AtomicU8>,
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
        Self::with(serving, false, true).await
    }

    async fn without_tools(serving: &str) -> Self {
        Self::with(serving, false, false).await
    }

    async fn classifier() -> Self {
        Self::with("ClassifierAlias", true, true).await
    }

    async fn with(serving: &str, classifier: bool, tools: bool) -> Self {
        let script = Script {
            serving: serving.to_owned(),
            classifier,
            tools,
            mode: Arc::default(),
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

    fn set(&self, mode: u8) {
        self.script.mode.store(mode, Ordering::SeqCst);
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
            context_length: 8192,
        }),
        4,
    );
    body.features.tools = script.tools;
    body.features.tool_choice = script.tools;
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

async fn generate(
    axum::extract::State(script): axum::extract::State<Script>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    script.hits.fetch_add(1, Ordering::SeqCst);
    if let Some(id) = headers.get("x-request-id").and_then(|v| v.to_str().ok()) {
        script.request_ids.lock().unwrap().push(id.to_owned());
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
    match script.mode.load(Ordering::SeqCst) {
        BUSY => return error(503, "overloaded"),
        BROKEN => return error(500, "internal_error"),
        _ => {}
    }
    if text.contains("FAIL500") {
        return error(500, "internal_error");
    }
    if text.contains("OVERFLOW") {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(
                json!({"error": {"message": "too long", "type": "invalid_request_error",
                                        "code": "context_length_exceeded"}}),
            ),
        )
            .into_response();
    }
    if body["stream"] == true {
        let head = format!(
            "data: {}\n\n",
            json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                   "choices": [{"index": 0, "delta": {"role": "assistant", "content": "hi"}}]})
        );
        if text.contains("BREAK") {
            return ([("content-type", "text/event-stream")], head).into_response();
        }
        let done = json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                          "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
        return (
            [("content-type", "text/event-stream")],
            format!("{head}data: {done}\n\ndata: [DONE]\n\n"),
        )
            .into_response();
    }
    let message = if text.contains("TOOLCALL") {
        json!({"role": "assistant", "content": null, "tool_calls": [{"id": "call_1",
               "type": "function", "function": {"name": "f", "arguments": "{}"}}]})
    } else {
        json!({"role": "assistant", "content": format!("answer from {}", script.serving)})
    };
    axum::Json(json!({
        "id": "c1", "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
    }))
    .into_response()
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
    async fn start(mut config: Value) -> Self {
        ensure_provider();
        config["listen"] = json!(["127.0.0.1:0"]);
        config["health"] =
            json!({"interval_secs": 3600, "timeout_secs": 2, "failure_threshold": 1});
        config["request"] = json!({"connect_timeout_secs": 2});
        let file: RouterFile = serde_json::from_value(config).expect("config shape");
        let config = lightweight_router::validate(file, &|_| None).expect("valid config");
        let bound = lightweight_router::bind(&config).await.expect("bind");
        let base = format!("http://{}", bound.addresses()[0]);
        let state = bound.state();
        let stop = CancellationToken::new();
        tokio::spawn(bound.serve(stop.clone()));
        for _ in 0..100 {
            if client().get(format!("{base}/health")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
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

    async fn chat(&self, model: &str, text: &str) -> (u16, Value) {
        self.chat_with(json!({"model": model, "messages": [{"role": "user", "content": text}]}))
            .await
    }

    async fn chat_with(&self, body: Value) -> (u16, Value) {
        let response = self.post(body, &[]).await;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
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

    async fn last_trace(&self) -> Value {
        self.get("/api/router/v1/traces?limit=20").await["data"]
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

    /// Every `router_requests_total` series, summed: one per client request.
    /// (The router's own classification request is counted under its
    /// classifier route, as it always has been, and is left out here.)
    async fn requests_total(&self) -> u64 {
        self.metrics()
            .await
            .lines()
            .filter(|line| line.starts_with("router_requests_total{"))
            .filter(|line| !line.contains("route=\"RouterClassifier\""))
            .filter_map(|line| line.rsplit(' ').next()?.parse::<u64>().ok())
            .sum()
    }
}

// --- topologies ---------------------------------------------------------------------

/// `routes`: name → node bases (each its own deployment, in priority order).
/// `auto`: the `auto_route` section; a classifier node is added when the
/// section has one.
fn config(routes: &[(&str, Vec<(&str, &str)>)], classifier: Option<&Node>, auto: Value) -> Value {
    let mut nodes = Vec::new();
    let mut route_rows = Vec::new();
    for (route, deployments) in routes {
        let mut rows = Vec::new();
        for (index, (base, model)) in deployments.iter().enumerate() {
            let id = format!("{}-{index}", route.to_lowercase());
            nodes.push(json!({"id": id, "url": base}));
            rows.push(json!({"node": id, "model": model}));
        }
        route_rows.push(json!({"name": route, "deployments": rows}));
    }
    if let Some(classifier) = classifier {
        nodes.push(json!({"id": "classifier", "url": classifier.base}));
        route_rows.push(json!({"name": "RouterClassifier",
            "deployments": [{"node": "classifier", "model": "ClassifierAlias"}]}));
    }
    json!({"nodes": nodes, "routes": route_rows, "auto_route": auto})
}

/// `Auto` that classifies everything among General, Coder, Research and
/// Reasoning, with `fallback` as its cross-route lists.
fn classifying(fallback: Value) -> Value {
    json!({
        "enabled": true,
        "fallback_route": "General",
        "classifier": {"routes": ["General", "Coder", "Research", "Reasoning"],
                       "lightweight": {"route": "RouterClassifier", "timeout_ms": 5_000}},
        "rules": [
            {"name": "forced-tools", "when": {"tool_choice": "required"}, "route": "ToolAgent"},
            {"name": "semantic", "when": {}, "classify": true}
        ],
        "cross_route_fallback": fallback,
    })
}

fn tools() -> Value {
    json!([{"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}])
}

/// The routes every classifying test needs, all healthy.
struct Fleet {
    general: Node,
    coder: Node,
    research: Node,
    reasoning: Node,
    tool_agent: Node,
    classifier: Node,
}

impl Fleet {
    async fn start() -> Self {
        Self {
            general: Node::start("GeneralAlias").await,
            coder: Node::start("CoderAlias").await,
            research: Node::start("ResearchAlias").await,
            reasoning: Node::start("ReasoningAlias").await,
            tool_agent: Node::start("ToolAlias").await,
            classifier: Node::classifier().await,
        }
    }

    /// The topology, with `down` routes pointed at a node that never answers.
    fn config(&self, down: &[&str], auto: Value) -> Value {
        let base = |route: &str, node: &Node| {
            if down.contains(&route) {
                DOWN.to_owned()
            } else {
                node.base.clone()
            }
        };
        let routes = [
            ("General", base("General", &self.general), "GeneralAlias"),
            ("Coder", base("Coder", &self.coder), "CoderAlias"),
            (
                "Research",
                base("Research", &self.research),
                "ResearchAlias",
            ),
            (
                "Reasoning",
                base("Reasoning", &self.reasoning),
                "ReasoningAlias",
            ),
            (
                "ToolAgent",
                base("ToolAgent", &self.tool_agent),
                "ToolAlias",
            ),
        ];
        let owned: Vec<(String, String, &str)> = routes
            .iter()
            .map(|(r, b, m)| ((*r).to_owned(), b.clone(), *m))
            .collect();
        let refs: Vec<(&str, Vec<(&str, &str)>)> = owned
            .iter()
            .map(|(r, b, m)| (r.as_str(), vec![(b.as_str(), *m)]))
            .collect();
        config(&refs, Some(&self.classifier), auto)
    }
}

fn attempts(trace: &Value) -> Vec<(String, String, String)> {
    trace["cross_route_fallback"]["attempts"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    (
                        row["route"].as_str().unwrap_or("").to_owned(),
                        row["outcome"].as_str().unwrap_or("").to_owned(),
                        row["reason"].as_str().unwrap_or("").to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn attempt(route: &str, outcome: &str, reason: &str) -> (String, String, String) {
    (route.to_owned(), outcome.to_owned(), reason.to_owned())
}

// --- configuration absent: unchanged ---------------------------------------------------

#[tokio::test]
async fn without_a_section_a_failed_route_returns_its_own_error_as_before() {
    let fleet = Fleet::start().await;
    let mut auto = classifying(json!({}));
    auto.as_object_mut().unwrap().remove("cross_route_fallback");
    let router = Router::start(fleet.config(&["Coder"], auto)).await;
    let (status, body) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["code"], "route_unavailable");
    assert!(body["error"]["message"].as_str().unwrap().contains("Coder"));
    let trace = router.last_trace().await;
    assert!(trace.get("cross_route_fallback").is_none(), "{trace}");
    assert_eq!(fleet.general.hits(), 0);
    let admin = router.get("/api/router/v1/auto").await;
    assert_eq!(admin["cross_route_fallback"]["configured"], false);
    assert!(
        !router
            .metrics()
            .await
            .contains("router_cross_route_fallback_total{")
    );
}

// --- the three triggers ------------------------------------------------------------------

#[tokio::test]
async fn an_unavailable_route_falls_back_and_the_fallback_serves() {
    let fleet = Fleet::start().await;
    let router =
        Router::start(fleet.config(&["Coder"], classifying(json!({"Coder": ["General"]})))).await;
    let (status, body) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_eq!(
        (status, body["model"].as_str()),
        (200, Some("General")),
        "{body}"
    );
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "answer from GeneralAlias"
    );

    let trace = router.last_trace().await;
    assert_eq!(trace["requested_route"], "Auto");
    assert_eq!(trace["route"], "General");
    let block = &trace["cross_route_fallback"];
    assert_eq!(block["initial_route"], "Coder");
    assert_eq!(block["final_route"], "General");
    assert_eq!(block["exhausted"], false);
    assert_eq!(
        attempts(&trace),
        [
            attempt("Coder", "failed", "route_unavailable"),
            attempt("General", "committed", "")
        ]
    );
    assert_eq!(
        router
            .state
            .metrics
            .cross_route_fallbacks("Coder", "General", "route_unavailable"),
        1
    );
    assert_eq!(
        router.requests_total().await,
        1,
        "one client request, counted once"
    );
}

#[tokio::test]
async fn route_exhausted_only_after_every_deployment_refused() {
    let general = Node::start("GeneralAlias").await;
    let coder_a = Node::start("CoderA").await;
    let coder_b = Node::start("CoderB").await;
    let classifier = Node::classifier().await;
    coder_a.set(BUSY);
    coder_b.set(BUSY);
    let router = Router::start(config(
        &[
            ("General", vec![(general.base.as_str(), "GeneralAlias")]),
            (
                "Coder",
                vec![
                    (coder_a.base.as_str(), "CoderA"),
                    (coder_b.base.as_str(), "CoderB"),
                ],
            ),
        ],
        Some(&classifier),
        json!({"enabled": true, "fallback_route": "General",
               "classifier": {"routes": ["General", "Coder"],
                              "lightweight": {"route": "RouterClassifier", "timeout_ms": 5_000}},
               "rules": [{"name": "semantic", "when": {}, "classify": true}],
               "cross_route_fallback": {"Coder": ["General"]}}),
    ))
    .await;
    let (status, body) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_eq!(
        (status, body["model"].as_str()),
        (200, Some("General")),
        "{body}"
    );
    assert_eq!(
        (coder_a.hits(), coder_b.hits()),
        (1, 1),
        "every Coder deployment was tried first"
    );
    let trace = router.last_trace().await;
    assert_eq!(
        attempts(&trace)[0],
        attempt("Coder", "failed", "route_exhausted")
    );
    // Both Coder attempts are in the trace, attributed to Coder.
    let routes: Vec<&str> = trace["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["route"].as_str().unwrap())
        .collect();
    assert_eq!(routes, ["Coder", "Coder", "General"]);
    assert_eq!(
        router
            .state
            .metrics
            .cross_route_fallbacks("Coder", "General", "route_exhausted"),
        1
    );
    assert!(
        router
            .metrics()
            .await
            .contains("router_cross_route_fallback_total{from_route=\"Coder\",to_route=\"General\",reason=\"route_exhausted\"} 1")
    );
}

#[tokio::test]
async fn one_deployment_failing_never_leaves_the_route() {
    let general = Node::start("GeneralAlias").await;
    let coder_a = Node::start("CoderA").await;
    let coder_b = Node::start("CoderB").await;
    let classifier = Node::classifier().await;
    let auto = json!({"enabled": true, "fallback_route": "General",
        "classifier": {"routes": ["General", "Coder"],
                       "lightweight": {"route": "RouterClassifier", "timeout_ms": 5_000}},
        "rules": [{"name": "semantic", "when": {}, "classify": true}],
        "cross_route_fallback": {"Coder": ["General"]}});
    // A refuses 503, B answers: Coder serves.
    coder_a.set(BUSY);
    let router = Router::start(config(
        &[
            ("General", vec![(general.base.as_str(), "GeneralAlias")]),
            (
                "Coder",
                vec![
                    (coder_a.base.as_str(), "CoderA"),
                    (coder_b.base.as_str(), "CoderB"),
                ],
            ),
        ],
        Some(&classifier),
        auto.clone(),
    ))
    .await;
    let (status, body) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_eq!(
        (status, body["model"].as_str()),
        (200, Some("Coder")),
        "{body}"
    );
    assert_eq!(coder_b.hits(), 1);
    assert_eq!(general.hits(), 0, "no cross-route fallback");
    assert!(
        router
            .last_trace()
            .await
            .get("cross_route_fallback")
            .is_none()
    );
    assert_eq!(
        router
            .state
            .metrics
            .cross_route_fallbacks("Coder", "General", "route_exhausted"),
        0
    );

    // A down at planning, B healthy: Coder serves too.
    let router = Router::start(config(
        &[
            ("General", vec![(general.base.as_str(), "GeneralAlias")]),
            (
                "Coder",
                vec![(DOWN, "CoderA"), (coder_b.base.as_str(), "CoderB")],
            ),
        ],
        Some(&classifier),
        auto,
    ))
    .await;
    let (_, body) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_eq!(body["model"], "Coder");
    assert_eq!(general.hits(), 0);
}

#[tokio::test]
async fn a_capability_mismatch_falls_back_to_a_route_that_can_serve() {
    let general = Node::start("GeneralAlias").await;
    let coder = Node::without_tools("CoderAlias").await;
    let reasoning = Node::without_tools("ReasoningAlias").await;
    let classifier = Node::classifier().await;
    let router = Router::start(config(
        &[
            ("General", vec![(general.base.as_str(), "GeneralAlias")]),
            ("Coder", vec![(coder.base.as_str(), "CoderAlias")]),
            (
                "Reasoning",
                vec![(reasoning.base.as_str(), "ReasoningAlias")],
            ),
        ],
        Some(&classifier),
        json!({"enabled": true, "fallback_route": "General",
               "classifier": {"routes": ["General", "Coder", "Reasoning"],
                              "lightweight": {"route": "RouterClassifier", "timeout_ms": 5_000}},
               "rules": [{"name": "semantic", "when": {}, "classify": true}],
               "cross_route_fallback": {"Coder": ["Reasoning", "General"]}}),
    ))
    .await;
    // Tools are declared: neither Coder nor Reasoning can take them; R5 runs
    // on each, unweakened, and General serves.
    let (status, body) = router
        .chat_with(json!({"model": "Auto", "tools": tools(),
                          "messages": [{"role": "user", "content": "PICK Coder 0.9"}]}))
        .await;
    assert_eq!(
        (status, body["model"].as_str()),
        (200, Some("General")),
        "{body}"
    );
    assert_eq!((coder.hits(), reasoning.hits()), (0, 0));
    let trace = router.last_trace().await;
    assert_eq!(
        attempts(&trace),
        [
            attempt("Coder", "failed", "route_capability_mismatch"),
            attempt("Reasoning", "failed", "route_capability_mismatch"),
            attempt("General", "committed", "")
        ]
    );
}

// --- every Auto path is eligible -----------------------------------------------------------

#[tokio::test]
async fn a_deterministic_rule_route_is_eligible_and_requirements_are_never_weakened() {
    let general = Node::without_tools("GeneralAlias").await;
    let research = Node::start("ResearchAlias").await;
    let classifier = Node::classifier().await;
    let routes = [
        ("General", vec![(general.base.as_str(), "GeneralAlias")]),
        ("Research", vec![(research.base.as_str(), "ResearchAlias")]),
        ("ToolAgent", vec![(DOWN, "ToolAlias")]),
    ];
    let auto = |fallback: Value| {
        json!({"enabled": true, "fallback_route": "General",
               "classifier": {"routes": ["General", "Research"],
                              "lightweight": {"route": "RouterClassifier", "timeout_ms": 5_000}},
               "rules": [
                   {"name": "forced-tools", "when": {"tool_choice": "required"}, "route": "ToolAgent"},
                   {"name": "semantic", "when": {}, "classify": true}
               ],
               "cross_route_fallback": fallback})
    };
    let forced = json!({"model": "Auto", "tool_choice": "required", "tools": tools(),
                        "messages": [{"role": "user", "content": "go"}]});

    // ToolAgent → General: General cannot do tools, so it is a mismatch, and
    // the client gets General's own error. Nothing was stripped to fit.
    let router = Router::start(config(
        &routes,
        Some(&classifier),
        auto(json!({"ToolAgent": ["General"]})),
    ))
    .await;
    let (status, body) = router.chat_with(forced.clone()).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "route_capability_mismatch");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("General")
    );
    assert_eq!(general.hits(), 0);
    let trace = router.last_trace().await;
    assert_eq!(trace["auto_rule"], "forced-tools");
    assert_eq!(trace["cross_route_fallback"]["exhausted"], true);
    assert_eq!(
        attempts(&trace),
        [
            attempt("ToolAgent", "failed", "route_unavailable"),
            attempt("General", "failed", "route_capability_mismatch")
        ]
    );
    assert_eq!(
        router
            .state
            .metrics
            .cross_route_exhausted("ToolAgent", "route_capability_mismatch"),
        1
    );

    // ToolAgent → Research, which can: the forced tool request is served.
    let router = Router::start(config(
        &routes,
        Some(&classifier),
        auto(json!({"ToolAgent": ["Research"]})),
    ))
    .await;
    let (status, body) = router.chat_with(forced).await;
    assert_eq!(
        (status, body["model"].as_str()),
        (200, Some("Research")),
        "{body}"
    );
    assert_eq!(
        classifier.hits(),
        0,
        "a deterministic rule never classifies"
    );
}

#[tokio::test]
async fn a_classified_route_falls_back_without_classifying_again() {
    let fleet = Fleet::start().await;
    let router =
        Router::start(fleet.config(&["Coder"], classifying(json!({"Coder": ["General"]})))).await;
    let (_, body) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_eq!(body["model"], "General");
    assert_eq!(fleet.classifier.hits(), 1, "the classifier was asked once");
    let trace = router.last_trace().await;
    assert_eq!(trace["classifier"]["chosen_route"], "Coder");
    assert_eq!(trace["classifier"]["outcome"], "chosen");
}

#[tokio::test]
async fn a_scored_route_falls_back_without_scoring_again() {
    let fleet = Fleet::start().await;
    let mut auto = classifying(json!({"Coder": ["Research"]}));
    auto["adaptive_scoring"] = json!({"enabled": true, "weights": {"prior": 0.1},
                                      "priors": {"General": 1.0}});
    let router = Router::start(fleet.config(&["Coder"], auto)).await;
    let (_, body) = router.chat("Auto", "PICK Coder 0.95").await;
    assert_eq!(body["model"], "Research", "the list's order, not a score");
    assert_eq!(fleet.classifier.hits(), 1);
    let metrics = &router.state.metrics;
    assert_eq!(metrics.scoring_decisions("Coder", false), 1, "scored once");
    assert_eq!(metrics.scoring_decisions("Research", false), 0);
    assert_eq!(metrics.scoring_decisions("General", true), 0);
    let trace = router.last_trace().await;
    assert_eq!(
        trace["scoring"]["winner"], "Coder",
        "the initial decision only"
    );
    assert_eq!(fleet.general.hits(), 0);
}

#[tokio::test]
async fn the_auto_fallback_route_is_eligible() {
    let fleet = Fleet::start().await;
    let auto = json!({"enabled": true, "fallback_route": "Coder",
                      "rules": [{"name": "tools", "when": {"requires_tools": true}, "route": "ToolAgent"}],
                      "cross_route_fallback": {"Coder": ["General"]}});
    let router = Router::start(fleet.config(&["Coder"], auto)).await;
    let (status, body) = router.chat("Auto", "hello").await;
    assert_eq!(
        (status, body["model"].as_str()),
        (200, Some("General")),
        "{body}"
    );
    let trace = router.last_trace().await;
    assert_eq!(trace["auto_fallback"], true);
    assert_eq!(trace["cross_route_fallback"]["initial_route"], "Coder");
}

// --- what never falls back -------------------------------------------------------------------

#[tokio::test]
async fn an_explicit_route_never_falls_back() {
    let fleet = Fleet::start().await;
    let router =
        Router::start(fleet.config(&["Coder"], classifying(json!({"Coder": ["General"]})))).await;
    for name in ["Coder", "coder", " CODER "] {
        let (status, body) = router.chat(name, "hello").await;
        assert_eq!(status, 503, "{name}: {body}");
        assert_eq!(body["error"]["code"], "route_unavailable");
    }
    assert_eq!(fleet.general.hits(), 0, "General was never attempted");
    assert!(
        router
            .last_trace()
            .await
            .get("cross_route_fallback")
            .is_none()
    );
    assert_eq!(
        router
            .state
            .metrics
            .cross_route_fallbacks("Coder", "General", "route_unavailable"),
        0
    );
}

#[tokio::test]
async fn a_500_is_the_answer_and_never_falls_back() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config(&[], classifying(json!({"Coder": ["General"]})))).await;
    let (status, body) = router.chat("Auto", "PICK Coder 0.9 FAIL500").await;
    assert_eq!(status, 500, "{body}");
    assert_eq!(fleet.coder.hits(), 1);
    assert_eq!(fleet.general.hits(), 0);
    assert!(
        router
            .last_trace()
            .await
            .get("cross_route_fallback")
            .is_none()
    );

    // A whole route answering 500 is still not a trigger.
    fleet.coder.set(BROKEN);
    let (status, _) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_eq!(status, 500);
    assert_eq!(fleet.general.hits(), 0);
}

#[tokio::test]
async fn a_context_overflow_never_falls_back() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config(&[], classifying(json!({"Coder": ["General"]})))).await;
    let (status, body) = router.chat("Auto", "PICK Coder 0.9 OVERFLOW").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "context_length_exceeded");
    assert_eq!(fleet.general.hits(), 0);
    assert!(
        router
            .last_trace()
            .await
            .get("cross_route_fallback")
            .is_none()
    );
}

#[tokio::test]
async fn a_committed_stream_that_breaks_never_falls_back() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config(&[], classifying(json!({"Coder": ["General"]})))).await;
    let response = router
        .post(
            json!({"model": "Auto", "stream": true,
                   "messages": [{"role": "user", "content": "PICK Coder 0.9 BREAK"}]}),
            &[],
        )
        .await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("\"model\":\"Coder\""), "{text}");
    assert!(
        !text.contains("General"),
        "nothing from another route: {text}"
    );
    let trace = router.last_trace().await;
    assert_eq!(trace["outcome"], "interrupted");
    assert!(trace.get("cross_route_fallback").is_none());
    assert_eq!(fleet.general.hits(), 0);
}

// --- the list: once, in order, bounded ----------------------------------------------------------

#[tokio::test]
async fn the_initial_routes_list_is_followed_and_never_a_fallbacks_own() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config(
        &["Coder", "General"],
        classifying(json!({"Coder": ["General", "Reasoning"], "General": ["Research"]})),
    ))
    .await;
    let (status, body) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_eq!(
        (status, body["model"].as_str()),
        (200, Some("Reasoning")),
        "{body}"
    );
    assert_eq!(
        fleet.research.hits(),
        0,
        "General's own list was never consulted"
    );
    assert_eq!(
        attempts(&router.last_trace().await),
        [
            attempt("Coder", "failed", "route_unavailable"),
            attempt("General", "failed", "route_unavailable"),
            attempt("Reasoning", "committed", "")
        ]
    );
}

#[tokio::test]
async fn at_most_four_routes_are_attempted_and_the_last_ones_error_is_returned() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config(
        &["Coder", "General", "Research", "Reasoning"],
        classifying(json!({"Coder": ["General", "Research", "Reasoning"]})),
    ))
    .await;
    let (status, body) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["code"], "route_unavailable");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Reasoning"),
        "the final attempted route's own error: {body}"
    );
    let trace = router.last_trace().await;
    assert_eq!(trace["route"], "Reasoning");
    assert_eq!(trace["cross_route_fallback"]["exhausted"], true);
    assert_eq!(attempts(&trace).len(), 4);
    assert_eq!(fleet.tool_agent.hits(), 0, "no fifth route");
    assert_eq!(
        router
            .state
            .metrics
            .cross_route_exhausted("Coder", "route_unavailable"),
        1
    );
    assert_eq!(router.requests_total().await, 1);
    let metrics = router.metrics().await;
    assert!(metrics.contains(
        "router_cross_route_fallback_exhausted_total{route=\"Coder\",reason=\"route_unavailable\"} 1"
    ));
    assert!(
        metrics.contains("router_requests_total{route=\"Reasoning\",outcome=\"unavailable\"} 1")
    );
    assert!(!metrics.contains("router_requests_total{route=\"Coder\""));
    assert!(
        metrics.contains(
            "# HELP router_requests_total Client requests the router answered, each counted \
             once, by outcome and the final logical route"
        ),
        "the HELP text states the final-route semantic"
    );
}

#[tokio::test]
async fn the_final_routes_own_error_is_returned_whatever_it_is() {
    let research = Node::start("ResearchAlias").await;
    let general = Node::without_tools("GeneralAlias").await;
    let classifier = Node::classifier().await;
    let router = Router::start(config(
        &[
            ("General", vec![(general.base.as_str(), "GeneralAlias")]),
            ("Research", vec![(DOWN, "ResearchAlias")]),
        ],
        Some(&classifier),
        json!({"enabled": true, "fallback_route": "General",
               "classifier": {"routes": ["General", "Research"],
                              "lightweight": {"route": "RouterClassifier", "timeout_ms": 5_000}},
               "rules": [{"name": "semantic", "when": {}, "classify": true}],
               "cross_route_fallback": {"Research": ["General"]}}),
    ))
    .await;
    let (status, body) = router
        .chat_with(json!({"model": "Auto", "tools": tools(),
                          "messages": [{"role": "user", "content": "PICK Research 0.9"}]}))
        .await;
    assert_eq!(status, 400, "General's own mismatch: {body}");
    assert_eq!(body["error"]["code"], "route_capability_mismatch");
    let trace = router.last_trace().await;
    assert_eq!(trace["requested_route"], "Auto");
    let block = &trace["cross_route_fallback"];
    assert_eq!(
        (
            &block["initial_route"],
            &block["final_route"],
            &block["exhausted"]
        ),
        (&json!("Research"), &json!("General"), &json!(true))
    );
    assert_eq!(
        attempts(&trace),
        [
            attempt("Research", "failed", "route_unavailable"),
            attempt("General", "failed", "route_capability_mismatch")
        ]
    );
    assert_eq!(research.hits(), 0);
}

// --- identity, request id, counters, trace -----------------------------------------------------

#[tokio::test]
async fn the_response_names_the_route_that_served_it() {
    let fleet = Fleet::start().await;
    let router =
        Router::start(fleet.config(&["Coder"], classifying(json!({"Coder": ["General"]})))).await;
    // A whole body.
    let (_, body) = router.chat("Auto", "PICK Coder 0.9").await;
    assert_eq!(body["model"], "General");
    // A tool call.
    let (_, body) = router.chat("Auto", "PICK Coder 0.9 TOOLCALL").await;
    assert_eq!(body["model"], "General");
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["id"],
        "call_1"
    );
    // A stream: every frame.
    let response = router
        .post(
            json!({"model": "Auto", "stream": true,
                   "messages": [{"role": "user", "content": "PICK Coder 0.9"}]}),
            &[],
        )
        .await;
    let text = response.text().await.unwrap();
    let frames: Vec<Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    assert!(!frames.is_empty(), "{text}");
    assert!(
        frames.iter().all(|frame| frame["model"] == "General"),
        "{text}"
    );
}

#[tokio::test]
async fn one_request_id_across_every_route_attempt() {
    let general = Node::start("GeneralAlias").await;
    let coder = Node::start("CoderAlias").await;
    let classifier = Node::classifier().await;
    coder.set(BUSY);
    let router = Router::start(config(
        &[
            ("General", vec![(general.base.as_str(), "GeneralAlias")]),
            ("Coder", vec![(coder.base.as_str(), "CoderAlias")]),
        ],
        Some(&classifier),
        json!({"enabled": true, "fallback_route": "General",
               "classifier": {"routes": ["General", "Coder"],
                              "lightweight": {"route": "RouterClassifier", "timeout_ms": 5_000}},
               "rules": [{"name": "semantic", "when": {}, "classify": true}],
               "cross_route_fallback": {"Coder": ["General"]}}),
    ))
    .await;
    let response = router
        .post(
            json!({"model": "Auto", "messages": [{"role": "user", "content": "PICK Coder 0.9"}]}),
            &[("X-Request-Id", "client-req-7")],
        )
        .await;
    assert_eq!(
        response.headers()["x-request-id"].to_str().unwrap(),
        "client-req-7"
    );
    assert_eq!(response.status(), 200);
    assert_eq!(coder.request_ids(), ["client-req-7"]);
    assert_eq!(general.request_ids(), ["client-req-7"]);
    assert_eq!(router.last_trace().await["request_id"], "client-req-7");
}

#[tokio::test]
async fn the_trace_and_admin_view_carry_routes_and_reasons_only() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config(
        &["Coder"],
        classifying(json!({"Coder": ["General"], "Research": ["General"]})),
    ))
    .await;
    let response = router
        .post(
            json!({"model": "Auto", "messages": [{"role": "user", "content": "PICK Coder 0.9 secret-prompt"}]}),
            &[("X-Lightweight-Session", "session-secret")],
        )
        .await;
    assert_eq!(response.status(), 200);
    let trace = router.last_trace().await;
    let block = trace["cross_route_fallback"].to_string();
    for absent in ["secret", "127.0.0.1", "Alias", "deployment", "session"] {
        assert!(!block.contains(absent), "{absent}: {block}");
    }
    assert!(!trace.to_string().contains("secret-prompt"));

    let admin = router.get("/api/router/v1/auto").await["cross_route_fallback"].clone();
    assert_eq!(admin["configured"], true);
    assert_eq!(
        admin["chains"],
        json!({"Coder": ["General"], "Research": ["General"]})
    );
    assert_eq!(admin["max_routes"], 3);
    assert_eq!(admin["applies_to"], "auto");
    assert_eq!(
        admin["triggers"],
        json!([
            "route_unavailable",
            "route_exhausted",
            "route_capability_mismatch"
        ])
    );
    assert_eq!(admin["counts"]["Coder"]["General"]["route_unavailable"], 1);
    assert_eq!(admin["exhausted"], json!({}));
}

// --- affinity, placement, history ------------------------------------------------------------

#[tokio::test]
async fn the_fallback_route_uses_its_own_affinity() {
    let general_a = Node::start("GeneralA").await;
    let general_b = Node::start("GeneralB").await;
    let coder = Node::start("CoderAlias").await;
    let classifier = Node::classifier().await;
    let mut cfg = config(
        &[
            (
                "General",
                vec![
                    (general_a.base.as_str(), "GeneralA"),
                    (general_b.base.as_str(), "GeneralB"),
                ],
            ),
            ("Coder", vec![(coder.base.as_str(), "CoderAlias")]),
        ],
        Some(&classifier),
        json!({"enabled": true, "fallback_route": "General",
               "classifier": {"routes": ["General", "Coder"],
                              "lightweight": {"route": "RouterClassifier", "timeout_ms": 5_000}},
               "rules": [{"name": "semantic", "when": {}, "classify": true}],
               "cross_route_fallback": {"Coder": ["General"]}}),
    );
    cfg["session_affinity"] = json!({"enabled": true});
    let router = Router::start(cfg).await;
    let session = [("X-Lightweight-Session", "s-1")];
    let explicit =
        |route: &str| json!({"model": route, "messages": [{"role": "user", "content": "hello"}]});
    // Coder's affinity: Coder's only deployment.
    assert_eq!(router.post(explicit("Coder"), &session).await.status(), 200);
    // General's affinity: B (A refused, so the session settled on B).
    general_a.set(BUSY);
    assert_eq!(
        router.post(explicit("General"), &session).await.status(),
        200
    );
    general_a.set(HEALTHY);
    let before_a = general_a.hits();

    // Coder now refuses; Auto → Coder → General, for the same session.
    coder.set(BUSY);
    let response = router
        .post(
            json!({"model": "Auto", "messages": [{"role": "user", "content": "PICK Coder 0.9"}]}),
            &session,
        )
        .await;
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "General");
    assert_eq!(
        general_a.hits(),
        before_a,
        "General's own affinity (B) went first, not priority (A)"
    );
    let trace = router.last_trace().await;
    assert_eq!(trace["session"]["affinity"], "hit");
    assert_eq!(trace["final_deployment"], "general-1/GeneralB");

    let sessions = router.get("/api/router/v1/sessions").await;
    let entries: BTreeMap<String, String> = sessions["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["route"].as_str().unwrap().to_owned(),
                e["deployment"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        entries,
        BTreeMap::from([
            ("Coder".to_owned(), "coder-0/CoderAlias".to_owned()),
            ("General".to_owned(), "general-1/GeneralB".to_owned())
        ]),
        "Coder's affinity untouched, never reused for General"
    );
}

#[tokio::test]
async fn fallback_takes_no_placement_action() {
    let fleet = Fleet::start().await;
    let router =
        Router::start(fleet.config(&["Coder"], classifying(json!({"Coder": ["General"]})))).await;
    let before = router.get("/api/router/v1/placement").await;
    for _ in 0..3 {
        assert_eq!(router.chat("Auto", "PICK Coder 0.9").await.0, 200);
    }
    assert_eq!(router.get("/api/router/v1/placement").await, before);
    assert_eq!(router.state.metrics.reconcile_passes(), 0);
    assert!(
        !router
            .metrics()
            .await
            .contains("router_placement_actions_total{")
    );
}

#[tokio::test]
async fn each_route_attempt_is_observed_in_history_which_never_steers() {
    let fleet = Fleet::start().await;
    let mut auto = classifying(json!({"Coder": ["General"]}));
    auto["adaptive_scoring"] = json!({"enabled": true});
    let router = Router::start(fleet.config(&["Coder"], auto)).await;
    assert_eq!(router.chat("Auto", "PICK Coder 0.9").await.0, 200);
    let admin = router.get("/api/router/v1/auto").await;
    let scoring = &admin["adaptive_scoring"];
    assert_eq!(scoring["history_affects_scoring"], false);
    let row = |route: &str| {
        scoring["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["route"] == route)
            .cloned()
            .unwrap()
    };
    assert_eq!(row("Coder")["unavailable"], 1, "the route left");
    assert!(
        row("General")["successes"].as_f64().unwrap() > 0.99,
        "the route that served"
    );
    let metrics = router.metrics().await;
    assert!(metrics.contains(
        "router_route_history_observations_total{route=\"Coder\",outcome=\"unavailable\"} 1"
    ));
    assert!(metrics.contains(
        "router_route_history_observations_total{route=\"General\",outcome=\"success\"} 1"
    ));
}
