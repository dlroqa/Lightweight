//! Content-aware `Auto` classification (R9.1), end to end, over real sockets.
//!
//! The classifier here is a scripted node behind a `RouterClassifier` route:
//! it answers by keyword, deterministically, so CI never depends on what a
//! real model happens to say. Each other route has its own scripted node, so
//! which node answered says which route the request resolved to.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use lightweight_api::capabilities::{CapabilitiesBody, CapabilityModel};
use lightweight_core::SseDecoder;
use lightweight_router::RouterState;
use lightweight_router::config::RouterFile;
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

// --- scripted nodes -------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Answers a generation as a gateway would, echoing its `model`.
    Model,
    /// Answers as a classifier, by keyword in the last user message.
    Classifier,
    /// Refuses every generation with a 503.
    Down,
}

#[derive(Clone)]
struct Script {
    serving: String,
    role: Role,
    tools: bool,
    hits: Arc<AtomicU32>,
    /// Classifications answered to the end: a handler the router abandoned
    /// never gets here.
    answered: Arc<AtomicU32>,
    seen: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}

struct Node {
    base: String,
    stop: CancellationToken,
    script: Script,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Node {
    async fn start(serving: &str, role: Role) -> Self {
        Self::with(serving, role, true).await
    }

    async fn with(serving: &str, role: Role, tools: bool) -> Self {
        let script = Script {
            serving: serving.to_owned(),
            role,
            tools,
            hits: Arc::default(),
            answered: Arc::default(),
            seen: Arc::default(),
        };
        let app = axum::Router::new()
            .route("/v1/capabilities", get(capabilities))
            .route("/v1/chat/completions", post(generate))
            .route("/v1/completions", post(generate))
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
        Self { base, stop, script }
    }

    fn hits(&self) -> u32 {
        self.script.hits.load(Ordering::SeqCst)
    }

    fn seen(&self) -> Vec<(HeaderMap, Value)> {
        self.script.seen.lock().unwrap().clone()
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

/// What the scripted classifier says about a request, by keyword.
fn classify(text: &str) -> (String, u64) {
    let pick = |route: &str, confidence: f64| {
        json!({"route": route, "confidence": confidence}).to_string()
    };
    if text.contains("SLOW") {
        return (pick("Coder", 0.99), 2_000);
    }
    let answer = if text.contains("Rust") {
        format!("Here you go: {}", pick("Coder", 0.92))
    } else if text.contains("GPU") {
        pick("research", 0.88)
    } else if text.contains("UNSURE") {
        pick("Coder", 0.40)
    } else if text.contains("GARBAGE") {
        "I would say Coder, probably.".to_owned()
    } else if text.contains("INVENT") {
        pick("node-a", 0.99)
    } else if text.contains("CONFIGURED") {
        // A real route, but not a candidate.
        pick("ToolAgent", 0.99)
    } else {
        pick("General", 0.95)
    };
    (answer, 0)
}

async fn generate(
    axum::extract::State(script): axum::extract::State<Script>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    script.hits.fetch_add(1, Ordering::SeqCst);
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    script.seen.lock().unwrap().push((headers, body.clone()));
    let model = body["model"].clone();
    match script.role {
        Role::Down => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({"error": {"message": "busy", "type": "server_error",
                                            "code": "overloaded"}})),
            )
                .into_response();
        }
        Role::Classifier => {
            let text = body["messages"]
                .as_array()
                .and_then(|messages| messages.last())
                .and_then(|message| message["content"].as_str())
                .unwrap_or("")
                .to_owned();
            let (content, delay_ms) = classify(&text);
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            script.answered.fetch_add(1, Ordering::SeqCst);
            return axum::Json(json!({
                "id": "k1", "object": "chat.completion", "model": model,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": content},
                             "finish_reason": "stop"}],
            }))
            .into_response();
        }
        Role::Model => {}
    }
    if body["stream"] == true {
        let mut text = String::new();
        for frame in [
            json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                   "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                   "choices": [{"index": 0, "delta": {"content": "hi"}}]}),
        ] {
            text.push_str(&format!("data: {frame}\n\n"));
        }
        text.push_str("data: [DONE]\n\n");
        return ([("content-type", "text/event-stream")], text).into_response();
    }
    axum::Json(json!({
        "id": "c1", "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"},
                     "finish_reason": "stop"}],
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
        config["listen"] = json!(["127.0.0.1:0"]);
        config["health"] =
            json!({"interval_secs": 3600, "timeout_secs": 2, "failure_threshold": 1});
        let file: RouterFile = serde_json::from_value(config).expect("config shape");
        let config = lightweight_router::validate(file, &|name| {
            (name == "CLASSIFIER_NODE_KEY").then(|| "classifier-key".to_owned())
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
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Self { base, state, stop }
    }

    async fn post(&self, body: Value, session: Option<&str>) -> reqwest::Response {
        let mut request = client()
            .post(format!("{}/v1/chat/completions", self.base))
            .header("X-Request-Id", "client-1")
            .json(&body);
        if let Some(session) = session {
            request = request.header("X-Lightweight-Session", session);
        }
        request.send().await.expect("request")
    }

    async fn send(&self, body: Value) -> (u16, Value) {
        let response = self.post(body, None).await;
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
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

    /// The most recent trace of a client request (not a classification).
    async fn last_trace(&self) -> Value {
        self.get("/api/router/v1/traces?limit=20").await["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|trace| trace["route"] != "RouterClassifier")
            .cloned()
            .unwrap_or(Value::Null)
    }
}

// --- configurations ---------------------------------------------------------------

struct Fleet {
    general: Node,
    coder: Node,
    research: Node,
    tool_agent: Node,
    classifier: Node,
}

impl Fleet {
    async fn start() -> Self {
        Self::with_classifier(Role::Classifier).await
    }

    async fn with_classifier(role: Role) -> Self {
        Self {
            general: Node::start("GeneralAlias", Role::Model).await,
            coder: Node::start("CoderAlias", Role::Model).await,
            research: Node::start("ResearchAlias", Role::Model).await,
            tool_agent: Node::start("ToolAlias", Role::Model).await,
            classifier: Node::start("ClassifierAlias", role).await,
        }
    }

    fn topology(&self) -> Value {
        json!({
            "nodes": [
                {"id": "general", "url": self.general.base},
                {"id": "coder", "url": self.coder.base},
                {"id": "research", "url": self.research.base},
                {"id": "tools", "url": self.tool_agent.base},
                {"id": "classifier", "url": self.classifier.base, "api_key_env": "CLASSIFIER_NODE_KEY"}
            ],
            "routes": [
                {"name": "General", "description": "Everyday conversation and small talk",
                 "deployments": [{"node": "general", "model": "GeneralAlias"}]},
                {"name": "Coder", "description": "Software engineering, debugging, code generation",
                 "deployments": [{"node": "coder", "model": "CoderAlias"}]},
                {"name": "Research", "description": "Current events, web research, comparisons",
                 "deployments": [{"node": "research", "model": "ResearchAlias"}]},
                {"name": "ToolAgent", "deployments": [{"node": "tools", "model": "ToolAlias"}]},
                {"name": "RouterClassifier", "deployments": [{"node": "classifier", "model": "ClassifierAlias"}]}
            ]
        })
    }

    /// Forced tool use is deterministic; everything else is classified.
    fn config(&self) -> Value {
        self.config_with(json!({}))
    }

    fn config_with(&self, classifier: Value) -> Value {
        let mut section = json!({"route": "RouterClassifier",
                                 "routes": ["General", "Coder", "Research"],
                                 "timeout_ms": 5_000});
        if let (Some(section), Value::Object(extra)) = (section.as_object_mut(), classifier) {
            section.extend(extra);
        }
        let mut config = self.topology();
        config["auto_route"] = json!({
            "enabled": true,
            "fallback_route": "General",
            "classifier": section,
            "rules": [
                {"name": "forced-tools", "when": {"tool_choice": "required"}, "route": "ToolAgent"},
                {"name": "semantic", "when": {}, "classify": true}
            ]
        });
        config
    }

    fn hits(&self) -> [u32; 4] {
        [
            self.general.hits(),
            self.coder.hits(),
            self.research.hits(),
            self.tool_agent.hits(),
        ]
    }
}

fn tools() -> Value {
    json!([
        {"type": "function", "function": {"name": "web_search", "parameters": {"type": "object"}}},
        {"type": "function", "function": {"name": "read_file", "parameters": {"type": "object"}}}
    ])
}

/// What Lightagent sends on every turn: its whole tool set, whatever was
/// asked.
fn lightagent(text: &str) -> Value {
    json!({"model": "Auto", "tools": tools(), "messages": [
        {"role": "system", "content": "You are Lightagent. SYSTEM-SECRET"},
        {"role": "user", "content": "an earlier turn HISTORY-SECRET"},
        {"role": "assistant", "content": "an earlier answer"},
        {"role": "user", "content": text}
    ]})
}

fn reached(before: [u32; 4], after: [u32; 4]) -> Vec<usize> {
    (0..4).filter(|&i| after[i] != before[i]).collect()
}

const ALIASES: [&str; 5] = [
    "GeneralAlias",
    "CoderAlias",
    "ResearchAlias",
    "ToolAlias",
    "ClassifierAlias",
];

fn no_alias_in(text: &str) {
    for alias in ALIASES {
        assert!(!text.contains(alias), "{alias} leaked: {text}");
    }
}

// --- meaning, not structure ---------------------------------------------------------

#[tokio::test]
async fn requests_that_all_carry_tools_resolve_by_what_they_ask() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config()).await;

    for (text, route, node) in [
        ("Hello, how are you?", "General", 0),
        ("write a Rust async TCP server", "Coder", 1),
        ("compare today's current GPU announcements", "Research", 2),
    ] {
        let before = fleet.hits();
        let (status, body) = router.send(lightagent(text)).await;
        assert_eq!(status, 200, "{text}: {body}");
        assert_eq!(body["model"], route, "{text}");
        assert_eq!(reached(before, fleet.hits()), [node], "{text}");
        no_alias_in(&body.to_string());
        let trace = router.last_trace().await;
        assert_eq!(trace["requested_route"], "Auto");
        assert_eq!(trace["auto_rule"], "semantic");
        assert_eq!(trace["route"], route);
        assert_eq!(trace["classifier"]["outcome"], "chosen");
        assert_eq!(trace["classifier"]["chosen_route"], route);
    }
    assert_eq!(fleet.classifier.hits(), 3);

    // Forced tool use is decided by its rule, before any classification.
    let mut forced = lightagent("Hello, how are you?");
    forced["tool_choice"] = json!("required");
    let before = fleet.hits();
    let (status, body) = router.send(forced).await;
    assert_eq!(status, 200);
    assert_eq!(body["model"], "ToolAgent");
    assert_eq!(reached(before, fleet.hits()), [3]);
    assert_eq!(fleet.classifier.hits(), 3, "not classified");
    assert!(router.last_trace().await.get("classifier").is_none());
}

#[tokio::test]
async fn an_explicit_route_and_a_deterministic_rule_never_classify() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config()).await;

    let mut direct = lightagent("compare today's current GPU announcements");
    direct["model"] = json!("Coder");
    let (status, body) = router.send(direct).await;
    assert_eq!((status, body["model"].as_str()), (200, Some("Coder")));
    let (_, body) = router
        .send(json!({"messages": [{"role": "user", "content": "GPU"}],
                                       "model": "research"}))
        .await;
    assert_eq!(body["model"], "Research");
    assert_eq!(fleet.classifier.hits(), 0);
    let trace = router.last_trace().await;
    assert_eq!(trace["requested_route"], "Research");
    assert!(trace.get("classifier").is_none());

    // A client may call the classifier route by name: an ordinary route.
    let (status, _) = router
        .send(json!({"model": "RouterClassifier", "messages": [{"role": "user", "content": "hi"}]}))
        .await;
    assert_eq!(status, 200);
    assert_eq!(fleet.classifier.hits(), 1);
}

#[tokio::test]
async fn without_a_classifying_rule_auto_is_exactly_r8() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let mut config = fleet.config();
    // R8's own structural rule: every Lightagent turn is a tool request.
    config["auto_route"]["rules"] =
        json!([{"name": "tools", "when": {"requires_tools": true}, "route": "Coder"}]);
    let router = Router::start(config).await;
    let (_, body) = router.send(lightagent("Hello, how are you?")).await;
    assert_eq!(body["model"], "Coder");
    let (_, body) = router
        .send(json!({"model": "Auto", "messages": [{"role": "user", "content": "write Rust"}]}))
        .await;
    assert_eq!(body["model"], "General", "the plain fallback, unclassified");
    assert_eq!(
        fleet.classifier.hits(),
        0,
        "an inert classifier is never called"
    );
    let admin = router.get("/api/router/v1/auto").await;
    assert_eq!(admin["classifier"]["invoked_by"], json!([]));
}

// --- the classifier only recommends, and only configured candidates ----------------

#[tokio::test]
async fn every_classifier_failure_falls_back_and_auto_still_answers() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config_with(json!({"timeout_ms": 400}))).await;

    for (text, outcome, chosen) in [
        ("UNSURE about this", "low_confidence", Some("Coder")),
        ("GARBAGE please", "invalid", None),
        ("INVENT a route", "invalid", None),
        ("CONFIGURED but not a candidate", "invalid", None),
        ("SLOW one", "timeout", None),
    ] {
        let before = fleet.hits();
        let started = Instant::now();
        let (status, body) = router.send(lightagent(text)).await;
        assert_eq!(status, 200, "{text}: {body}");
        assert_eq!(
            body["model"], "General",
            "{text}: the classifier's fallback"
        );
        assert_eq!(reached(before, fleet.hits()), [0], "{text}");
        let trace = router.last_trace().await;
        assert_eq!(trace["route"], "General");
        assert_eq!(trace["classifier"]["outcome"], outcome, "{text}");
        assert_eq!(
            trace["classifier"]["chosen_route"].as_str(),
            chosen,
            "{text}"
        );
        if outcome == "timeout" {
            assert!(
                started.elapsed() < Duration::from_millis(1_500),
                "the timeout bounds the wait: {:?}",
                started.elapsed()
            );
        }
    }
    // Neither the node name nor the non-candidate route was ever used.
    assert_eq!(fleet.tool_agent.hits(), 0);
    let metrics = client()
        .get(format!("{}/metrics", router.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for (outcome, count) in [("low_confidence", 1), ("invalid", 3), ("timeout", 1)] {
        assert!(
            metrics.contains(&format!(
                "router_classifier_requests_total{{outcome=\"{outcome}\"}} {count}"
            )),
            "{outcome}: {metrics}"
        );
    }
    assert!(
        !metrics.contains("router_classifier_route_total{route="),
        "nothing was taken"
    );
}

#[tokio::test]
async fn a_timeout_cancels_the_classification_and_the_fallback_answers() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config_with(json!({"timeout_ms": 300}))).await;

    // The classifier would take 2 s; the bound is 300 ms.
    let started = Instant::now();
    let (status, body) = router.send(lightagent("SLOW write a Rust server")).await;
    let waited = started.elapsed();
    assert_eq!(status, 200, "a timeout is never the client's error: {body}");
    assert_eq!(body["model"], "General", "the deterministic fallback");
    assert!(
        waited >= Duration::from_millis(300) && waited < Duration::from_millis(1_500),
        "bounded by the timeout: {waited:?}"
    );
    assert_eq!(fleet.coder.hits(), 0, "the slow verdict was never taken");
    let trace = router.last_trace().await;
    assert_eq!(trace["classifier"]["outcome"], "timeout");
    let classified = trace["classifier"]["duration_ms"].as_f64().unwrap();
    assert!((300.0..1_500.0).contains(&classified), "{trace}");
    assert!(trace["routing_ms"].as_f64().unwrap() < 200.0, "{trace}");

    // Cancelled, not merely ignored: the classifier node was reached, its
    // handler was dropped when the router let go, and it never answered.
    assert_eq!(fleet.classifier.hits(), 1);
    tokio::time::sleep(Duration::from_millis(2_300)).await;
    assert_eq!(
        fleet.classifier.script.answered.load(Ordering::SeqCst),
        0,
        "the abandoned classification was not left running to completion"
    );
    let traces = router.get("/api/router/v1/traces?limit=10").await;
    let nested = traces["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["route"] == "RouterClassifier")
        .cloned()
        .unwrap();
    assert_eq!(nested["outcome"], "cancelled", "{nested}");
}

#[tokio::test]
async fn an_unavailable_classifier_falls_back_to_its_own_fallback_route() {
    ensure_provider();
    let fleet = Fleet::with_classifier(Role::Down).await;
    let router = Router::start(fleet.config_with(json!({"fallback_route": "Research"}))).await;
    let (status, body) = router
        .send(lightagent("write a Rust async TCP server"))
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        body["model"], "Research",
        "the classifier's fallback, not Auto's"
    );
    let trace = router.last_trace().await;
    assert_eq!(trace["classifier"]["outcome"], "unavailable");
    assert_eq!(trace["auto_rule"], "semantic");
}

#[tokio::test]
async fn the_classifier_sees_candidates_and_the_last_message_only() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config_with(json!({"max_input_chars": 40}))).await;
    let long = format!("write a Rust async TCP server {}", "x".repeat(200));
    router.send(lightagent(&long)).await;

    let (headers, sent) = fleet.classifier.seen().pop().unwrap();
    assert_eq!(
        sent["model"], "ClassifierAlias",
        "an ordinary routed request to its route"
    );
    assert_eq!(
        headers.get("x-request-id").unwrap(),
        "client-1-classify",
        "findable from the client's id"
    );
    assert_eq!(
        headers.get("authorization").unwrap(),
        "Bearer classifier-key",
        "the classifier node's own key, as for any request"
    );
    assert!(headers.get("x-lightweight-session").is_none());
    let text = sent.to_string();
    for present in [
        "General",
        "Coder",
        "Research",
        "Software engineering, debugging, code generation",
        "write a Rust async TCP ",
        "Tools declared: yes",
        "(truncated)",
    ] {
        assert!(text.contains(present), "{present}: {text}");
    }
    for absent in [
        "ToolAgent",
        "SYSTEM-SECRET",
        "HISTORY-SECRET",
        "web_search",
        "xxxxxxxxxxxxxxxxxxxx",
        "GeneralAlias",
        "CoderAlias",
    ] {
        assert!(
            !text.contains(absent),
            "{absent} reached the classifier: {text}"
        );
    }
    assert!(sent.get("tools").is_none());
    assert_eq!(
        router.last_trace().await["classifier"]["input_truncated"],
        true
    );
}

// --- after the route is chosen, nothing changes --------------------------------------

#[tokio::test]
async fn the_chosen_route_streams_under_its_own_name_with_its_own_filtering_and_affinity() {
    ensure_provider();
    let general = Node::start("GeneralAlias", Role::Model).await;
    let coder_no_tools = Node::with("CoderPlain", Role::Model, false).await;
    let coder_a = Node::start("CoderA", Role::Model).await;
    let coder_b = Node::start("CoderB", Role::Model).await;
    let research = Node::start("ResearchAlias", Role::Model).await;
    let classifier = Node::start("ClassifierAlias", Role::Classifier).await;
    let config = json!({
        "session_affinity": {"enabled": true},
        "nodes": [
            {"id": "general", "url": general.base}, {"id": "plain", "url": coder_no_tools.base},
            {"id": "ca", "url": coder_a.base}, {"id": "cb", "url": coder_b.base},
            {"id": "research", "url": research.base}, {"id": "classifier", "url": classifier.base}
        ],
        "routes": [
            {"name": "General", "deployments": [{"node": "general", "model": "GeneralAlias"}]},
            {"name": "Coder", "strategy": "round_robin", "deployments": [
                {"node": "plain", "model": "CoderPlain"},
                {"node": "ca", "model": "CoderA"}, {"node": "cb", "model": "CoderB"}]},
            {"name": "Research", "deployments": [{"node": "research", "model": "ResearchAlias"}]},
            {"name": "RouterClassifier", "deployments": [{"node": "classifier", "model": "ClassifierAlias"}]}
        ],
        "auto_route": {"enabled": true, "fallback_route": "General",
            "classifier": {"route": "RouterClassifier", "routes": ["General", "Coder", "Research"],
                           "timeout_ms": 5_000},
            "rules": [{"name": "semantic", "when": {}, "classify": true}]}
    });
    let router = Router::start(config).await;

    // Streamed: every chunk is the chosen route; nothing physical leaks.
    let mut body = lightagent("write a Rust async TCP server");
    body["stream"] = json!(true);
    let response = router.post(body, Some("session-1")).await;
    assert_eq!(response.status(), 200);
    let bytes = response.bytes().await.unwrap();
    for alias in ["CoderPlain", "CoderA", "CoderB", "ClassifierAlias"] {
        assert!(!String::from_utf8_lossy(&bytes).contains(alias));
    }
    let mut decoder = SseDecoder::new();
    decoder.feed(&bytes).unwrap();
    for event in decoder.drain().iter().filter(|event| !event.is_done()) {
        let chunk: Value = serde_json::from_str(&event.data).unwrap();
        assert_eq!(chunk["model"], "Coder");
    }

    // Capability filtering inside Coder: the tool-less deployment is passed
    // over for a tool request, as for any request to Coder.
    assert_eq!(coder_no_tools.hits(), 0);
    let trace = router.last_trace().await;
    assert_eq!(trace["unfit"][0]["deployment"], "plain/CoderPlain");
    let settled = trace["final_deployment"].as_str().unwrap().to_owned();

    // Affinity is Coder's, keyed by the resolved route: the session sticks.
    for _ in 0..3 {
        router
            .post(lightagent("more Rust please"), Some("session-1"))
            .await;
        assert_eq!(
            router.last_trace().await["final_deployment"],
            settled.as_str()
        );
    }
    // The same session asking something else resolves elsewhere, with its own
    // affinity; Coder's is untouched.
    router
        .post(lightagent("Hello there"), Some("session-1"))
        .await;
    assert_eq!(router.last_trace().await["route"], "General");
    let sessions = router.get("/api/router/v1/sessions").await;
    let mut routes: Vec<String> = sessions["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["route"].as_str().unwrap().to_owned())
        .collect();
    routes.sort();
    assert_eq!(
        routes,
        ["Coder", "General"],
        "never Auto, never the classifier"
    );
    // Round-robin on Coder moved only on Coder's requests.
    assert_eq!(router.state.metrics.decisions("Coder", "round_robin"), 1);
}

// --- observability ------------------------------------------------------------------

#[tokio::test]
async fn classification_time_is_its_own_and_the_admin_view_shows_the_classifier() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config_with(json!({"timeout_ms": 400}))).await;
    router.send(lightagent("SLOW one")).await;
    let trace = router.last_trace().await;
    let classified = trace["classifier"]["duration_ms"].as_f64().unwrap();
    let routing = trace["routing_ms"].as_f64().unwrap();
    assert!(classified >= 390.0, "{trace}");
    assert!(routing < 200.0, "routing_ms keeps its R6 meaning: {trace}");
    // The classification itself is a request with its own trace.
    let traces = router.get("/api/router/v1/traces?limit=10").await;
    assert!(
        traces["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["route"] == "RouterClassifier" && t["request_id"] == "client-1-classify")
    );

    router
        .send(lightagent("write a Rust async TCP server"))
        .await;
    let metrics = client()
        .get(format!("{}/metrics", router.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("router_classifier_requests_total{outcome=\"chosen\"} 1"));
    assert!(metrics.contains("router_classifier_route_total{route=\"Coder\"} 1"));
    assert!(metrics.contains("router_classifier_duration_seconds_count{outcome=\"timeout\"} 1"));
    assert!(
        metrics.contains("router_auto_route_decisions_total{rule=\"semantic\",route=\"Coder\"} 1")
    );
    assert!(!metrics.contains("Rust"), "no prompt in a metric");

    let admin = router.get("/api/router/v1/auto").await;
    let classifier = &admin["classifier"];
    assert_eq!(classifier["route"], "RouterClassifier");
    assert_eq!(classifier["fallback_route"], "General");
    assert_eq!(classifier["min_confidence"], 0.65);
    assert_eq!(classifier["timeout_ms"], 400);
    assert_eq!(classifier["max_input_chars"], 2000);
    assert_eq!(classifier["invoked_by"], json!(["semantic"]));
    assert_eq!(
        classifier["candidates"][1],
        json!({"route": "Coder", "description": "Software engineering, debugging, code generation"})
    );
    assert_eq!(classifier["outcomes"]["chosen"], 1);
    assert_eq!(admin["rules"][1]["classify"], true);
    let shown = admin.to_string();
    assert!(!shown.contains("classifier-key") && !shown.contains("CLASSIFIER_NODE_KEY"));
    no_alias_in(&shown);

    // Discovery: the candidates are Auto's routes; the classifier is not.
    let capabilities = router.get("/v1/capabilities").await;
    assert_eq!(
        capabilities["auto"]["routes"],
        json!(["ToolAgent", "General", "Coder", "Research"])
    );
}
