//! The Jev classifier provider (R9.1a), end to end, against a scripted
//! TypeSafe System One server.
//!
//! The scripted server speaks the documented contract — `GET /v1/models`,
//! `POST /v1/systemone` with a typed Choice question, bearer auth, `401` /
//! `429` / `529` errors — and answers by keyword in the request it is sent,
//! so CI is deterministic and never calls the real API. Each logical route
//! has its own scripted Lightweight node, so which node answered says which
//! route the request resolved to.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use lightweight_api::capabilities::{CapabilitiesBody, CapabilityModel};
use lightweight_router::RouterState;
use lightweight_router::config::RouterFile;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Short on purpose: the secrets gate refuses a committed bearer literal of
/// 16 or more characters.
const KEY: &str = "jev-test-key";

fn ensure_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
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
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { stopping.cancelled().await })
            .await;
    });
    (base, stop)
}

// --- a scripted TypeSafe System One server ------------------------------------------

#[derive(Clone, Default)]
struct Typesafe {
    systemone: Arc<AtomicU32>,
    models_calls: Arc<AtomicU32>,
    /// Abandoned classifications never get here.
    answered: Arc<AtomicU32>,
    seen: Arc<Mutex<Vec<(String, HeaderMap, Value)>>>,
    /// What `GET /v1/models` answers: a status and the names it lists.
    models: Arc<Mutex<(u16, Vec<&'static str>)>>,
}

struct Jev {
    base: String,
    _stop: CancellationToken,
    script: Typesafe,
}

impl Jev {
    async fn start() -> Self {
        let script = Typesafe {
            models: Arc::new(Mutex::new((200, vec!["jev-latest", "jev-preview"]))),
            ..Typesafe::default()
        };
        let app = axum::Router::new()
            .route("/v1/models", get(typesafe_models))
            .route("/v1/systemone", post(typesafe_systemone))
            .with_state(script.clone());
        let (base, stop) = serve(app).await;
        Self {
            base,
            _stop: stop,
            script,
        }
    }

    fn calls(&self) -> u32 {
        self.script.systemone.load(Ordering::SeqCst)
    }

    fn last(&self) -> (String, HeaderMap, Value) {
        self.script
            .seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a call")
    }
}

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(&format!("Bearer {KEY}"))
}

async fn typesafe_models(
    axum::extract::State(script): axum::extract::State<Typesafe>,
    headers: HeaderMap,
) -> Response {
    script.models_calls.fetch_add(1, Ordering::SeqCst);
    if !authorized(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"detail": "Missing or invalid API key"})),
        )
            .into_response();
    }
    let (status, names) = script.models.lock().unwrap().clone();
    if status != 200 {
        return StatusCode::from_u16(status).unwrap().into_response();
    }
    let models: Vec<Value> = names
        .iter()
        .map(|name| json!({"name": name, "description": "a model", "release_date": "2026-09-01"}))
        .collect();
    axum::Json(json!({"models": models})).into_response()
}

/// Answers by keyword in `state.request` (or, with no text, by trait).
async fn typesafe_systemone(
    axum::extract::State(script): axum::extract::State<Typesafe>,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    script.systemone.fetch_add(1, Ordering::SeqCst);
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    script
        .seen
        .lock()
        .unwrap()
        .push((uri.path().to_owned(), headers.clone(), body.clone()));
    if !authorized(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"detail": "Missing or invalid API key: PROVIDER-BODY"})),
        )
            .into_response();
    }
    let text = body["state"]["request"].as_str().unwrap_or("").to_owned();
    let status = |code: u16| {
        (
            StatusCode::from_u16(code).unwrap(),
            axum::Json(json!({"detail": "PROVIDER-BODY"})),
        )
            .into_response()
    };
    if text.contains("FORBIDDEN") {
        return status(403);
    }
    if text.contains("RATE") {
        return status(429);
    }
    if text.contains("OVERLOADED") {
        return status(529);
    }
    if text.contains("BOOM") {
        return status(500);
    }
    if text.contains("INVALID-REQUEST") {
        return status(422);
    }
    if text.contains("MALFORMED") {
        return ([("content-type", "application/json")], "{\"answers\": ").into_response();
    }
    if text.contains("SLOW") {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let (choice, confidence) = if text.contains("UNKNOWN") {
        ("Marketing", 0.9)
    } else if text.contains("AUTO-CHOICE") {
        ("Auto", 0.9)
    } else if text.contains("NON-CANDIDATE") {
        ("ToolAgent", 0.95)
    } else if text.contains("UNSURE") {
        ("Coder", 0.3)
    } else if text.contains("Rust") || text.contains("SLOW") {
        ("Coder", 0.93)
    } else if text.contains("GPU") || text.contains("benchmarks") {
        ("Research", 0.88)
    } else if text.contains("prove") {
        ("Reasoning", 0.8)
    } else {
        ("General", 0.91)
    };
    script.answered.fetch_add(1, Ordering::SeqCst);
    axum::Json(json!({
        "model": "jev-1.13.0",
        "answers": {"route": {"type": "choice", "choice": choice, "confidence": confidence,
                               "probabilities": {choice: confidence}}},
        "usage": {"input_tokens": 300, "output_tokens": 20}
    }))
    .into_response()
}

// --- scripted Lightweight nodes ---------------------------------------------------------

#[derive(Clone)]
struct Model {
    serving: String,
    hits: Arc<AtomicU32>,
}

struct Node {
    base: String,
    _stop: CancellationToken,
    script: Model,
}

impl Node {
    async fn start(serving: &str) -> Self {
        let script = Model {
            serving: serving.to_owned(),
            hits: Arc::default(),
        };
        let app = axum::Router::new()
            .route("/v1/capabilities", get(capabilities))
            .route("/v1/chat/completions", post(generate))
            .with_state(script.clone());
        let (base, stop) = serve(app).await;
        Self {
            base,
            _stop: stop,
            script,
        }
    }

    fn hits(&self) -> u32 {
        self.script.hits.load(Ordering::SeqCst)
    }
}

async fn capabilities(axum::extract::State(script): axum::extract::State<Model>) -> Response {
    let mut body = CapabilitiesBody::new(
        "0.5.0",
        Some(CapabilityModel {
            id: script.serving.clone(),
            context_length: 8192,
        }),
        4,
    );
    body.features.tools = true;
    body.features.tool_choice = true;
    axum::Json(body).into_response()
}

/// Answers a generation; as a Lightweight classifier it always says General.
async fn generate(
    axum::extract::State(script): axum::extract::State<Model>,
    body: axum::body::Bytes,
) -> Response {
    script.hits.fetch_add(1, Ordering::SeqCst);
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let content = if script.serving == "ClassifierAlias" {
        json!({"route": "General", "confidence": 0.99}).to_string()
    } else {
        "hi".to_owned()
    };
    axum::Json(json!({
        "id": "c1", "object": "chat.completion", "model": body["model"],
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content},
                     "finish_reason": "stop"}],
    }))
    .into_response()
}

// --- the router -----------------------------------------------------------------------

struct Router {
    base: String,
    _state: Arc<RouterState>,
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
            (name == "TYPESAFE_API_KEY").then(|| KEY.to_owned())
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
        Self {
            base,
            _state: state,
            stop,
        }
    }

    async fn send(&self, body: Value) -> (u16, Value) {
        let response = client()
            .post(format!("{}/v1/chat/completions", self.base))
            .json(&body)
            .send()
            .await
            .expect("request");
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> Value {
        client()
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn text(&self, path: &str) -> String {
        client()
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }

    async fn check(&self) -> (u16, Value) {
        let response = client()
            .post(format!("{}/api/router/v1/classifier/check", self.base))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn last_trace(&self) -> Value {
        self.get("/api/router/v1/traces?limit=1").await["data"][0].clone()
    }
}

struct Fleet {
    general: Node,
    coder: Node,
    research: Node,
    reasoning: Node,
    tool_agent: Node,
    classifier: Node,
    jev: Jev,
}

impl Fleet {
    async fn start() -> Self {
        Self {
            general: Node::start("GeneralAlias").await,
            coder: Node::start("CoderAlias").await,
            research: Node::start("ResearchAlias").await,
            reasoning: Node::start("ReasoningAlias").await,
            tool_agent: Node::start("ToolAlias").await,
            classifier: Node::start("ClassifierAlias").await,
            jev: Jev::start().await,
        }
    }

    /// Forced tool use is deterministic; everything else is classified, by
    /// the provider named.
    fn config(&self, provider: &str, jev: Value) -> Value {
        let mut jev_block = json!({"base_url": format!("{}///", self.jev.base),
                                   "model": "jev-latest", "timeout_ms": 1_000});
        if let (Some(block), Value::Object(extra)) = (jev_block.as_object_mut(), jev) {
            block.extend(extra);
        }
        json!({
            "nodes": [
                {"id": "general", "url": self.general.base},
                {"id": "coder", "url": self.coder.base},
                {"id": "research", "url": self.research.base},
                {"id": "reasoning", "url": self.reasoning.base},
                {"id": "tools", "url": self.tool_agent.base},
                {"id": "classifier", "url": self.classifier.base}
            ],
            "routes": [
                {"name": "General", "description": "General conversation and requests that do not need a specialist",
                 "deployments": [{"node": "general", "model": "GeneralAlias"}]},
                {"name": "Coder", "description": "Programming, debugging, software design and code generation",
                 "deployments": [{"node": "coder", "model": "CoderAlias"}]},
                {"name": "Research", "description": "Current information, research synthesis and retrieval",
                 "deployments": [{"node": "research", "model": "ResearchAlias"}]},
                {"name": "Reasoning", "description": "Complex analytical and multi-step reasoning",
                 "deployments": [{"node": "reasoning", "model": "ReasoningAlias"}]},
                {"name": "ToolAgent", "deployments": [{"node": "tools", "model": "ToolAlias"}]},
                {"name": "RouterClassifier", "deployments": [{"node": "classifier", "model": "ClassifierAlias"}]}
            ],
            "auto_route": {
                "enabled": true,
                "fallback_route": "General",
                "classifier": {
                    "provider": provider,
                    "routes": ["General", "Coder", "Research", "Reasoning"],
                    "lightweight": {"route": "RouterClassifier", "timeout_ms": 5_000},
                    "jev": jev_block
                },
                "rules": [
                    {"name": "forced-tools", "when": {"tool_choice": "required"}, "route": "ToolAgent"},
                    {"name": "semantic", "when": {}, "classify": true}
                ]
            }
        })
    }

    fn hits(&self) -> [u32; 5] {
        [
            self.general.hits(),
            self.coder.hits(),
            self.research.hits(),
            self.reasoning.hits(),
            self.tool_agent.hits(),
        ]
    }
}

fn reached(before: [u32; 5], after: [u32; 5]) -> Vec<usize> {
    (0..5).filter(|&i| after[i] != before[i]).collect()
}

fn tools() -> Value {
    json!([
        {"type": "function", "function": {"name": "web_search", "parameters": {"type": "object"}}},
        {"type": "function", "function": {"name": "read_file", "parameters": {"type": "object"}}}
    ])
}

/// What Lightagent sends on every turn: its whole tool set.
fn lightagent(text: &str) -> Value {
    json!({"model": "Auto", "tools": tools(), "messages": [
        {"role": "system", "content": "You are Lightagent. SYSTEM-SECRET"},
        {"role": "user", "content": "an earlier turn HISTORY-SECRET"},
        {"role": "assistant", "content": "an earlier answer"},
        {"role": "user", "content": text}
    ]})
}

// --- choosing by meaning, through Jev ----------------------------------------------------

#[tokio::test]
async fn lightagent_like_requests_resolve_through_jev_and_the_route_pipeline_does_the_rest() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config("jev", json!({}))).await;

    for (text, route, node) in [
        ("Hello, how are you?", "General", 0),
        ("write a Rust async TCP server", "Coder", 1),
        ("compare today's current GPU benchmarks", "Research", 2),
        (
            "prove that the square root of 2 is irrational",
            "Reasoning",
            3,
        ),
    ] {
        let before = fleet.hits();
        let (status, body) = router.send(lightagent(text)).await;
        assert_eq!(status, 200, "{text}: {body}");
        assert_eq!(
            body["model"], route,
            "{text}: the response names the chosen route"
        );
        assert_eq!(reached(before, fleet.hits()), [node], "{text}");
        let trace = router.last_trace().await;
        assert_eq!(trace["requested_route"], "Auto");
        assert_eq!(trace["auto_rule"], "semantic");
        assert_eq!(trace["route"], route);
        let classifier = &trace["classifier"];
        assert_eq!(classifier["provider"], "jev");
        assert_eq!(
            classifier["model"], "jev-1.13.0",
            "the versioned id that answered"
        );
        assert_eq!(classifier["outcome"], "chosen");
        assert_eq!(classifier["chosen_route"], route);
        assert!(classifier.get("route").is_none(), "Jev is not a route");
        assert!(trace["routing_ms"].as_f64().unwrap() < 200.0);
    }
    assert_eq!(fleet.jev.calls(), 4);
    assert_eq!(
        fleet.classifier.hits(),
        0,
        "the standby Lightweight classifier is never asked"
    );

    // A forced tool call is its rule's, and an explicit route is the client's:
    // neither is classified.
    let mut forced = lightagent("Hello, how are you?");
    forced["tool_choice"] = json!("required");
    let (_, body) = router.send(forced).await;
    assert_eq!(body["model"], "ToolAgent");
    let mut direct = lightagent("compare today's current GPU benchmarks");
    direct["model"] = json!("Coder");
    let (_, body) = router.send(direct).await;
    assert_eq!(body["model"], "Coder");
    assert_eq!(fleet.jev.calls(), 4);
}

#[tokio::test]
async fn jev_is_sent_one_typed_choice_and_never_a_secret_history_or_node() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config("jev", json!({"max_input_chars": 40}))).await;
    let long = format!("write a Rust async TCP server {}", "x".repeat(200));
    router.send(lightagent(&long)).await;

    let (path, headers, body) = fleet.jev.last();
    assert_eq!(
        path, "/v1/systemone",
        "a base_url's trailing slashes never double one"
    );
    assert_eq!(
        headers.get("authorization").unwrap(),
        &format!("Bearer {KEY}")
    );
    assert_eq!(headers.get("content-type").unwrap(), "application/json");
    assert!(
        headers.get("x-request-id").is_none(),
        "no router id leaves for an external service"
    );
    assert_eq!(body["model"], "jev-latest");
    let question = &body["questions"]["route"];
    assert_eq!(question["type"], "choice");
    let options: Vec<&str> = question["criteria"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(options.len(), 4);
    for option in ["General", "Coder", "Research", "Reasoning"] {
        assert!(options.contains(&option), "{option}: {options:?}");
    }
    assert_eq!(
        question["criteria"]["Coder"],
        "Programming, debugging, software design and code generation"
    );
    assert_eq!(
        body["state"]["request"].as_str().unwrap().chars().count(),
        40
    );
    assert_eq!(body["state"]["request_truncated"], true);
    assert_eq!(body["state"]["tools_declared"], true);
    let text = body.to_string();
    for absent in [
        "SYSTEM-SECRET",
        "HISTORY-SECRET",
        "web_search",
        "ToolAgent",
        "RouterClassifier",
        "GeneralAlias",
        "127.0.0.1",
        KEY,
    ] {
        assert!(!text.contains(absent), "{absent} reached Jev: {text}");
    }
    assert_eq!(
        router.last_trace().await["classifier"]["input_truncated"],
        true
    );
}

#[tokio::test]
async fn with_user_text_off_jev_is_sent_only_traits_and_routes() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config("jev", json!({"include_user_text": false}))).await;
    let (status, _) = router
        .send(lightagent("write a Rust async TCP server"))
        .await;
    assert_eq!(status, 200);
    let (_, _, body) = fleet.jev.last();
    assert!(body["state"].get("request").is_none());
    assert!(!body.to_string().contains("Rust async"));
    assert_eq!(body["state"]["tools_declared"], true);
    assert!(
        body["questions"]["route"]["criteria"]
            .get("Coder")
            .is_some()
    );
}

// --- every failure falls back, and only candidates are taken -----------------------------------

#[tokio::test]
async fn every_jev_failure_falls_back_to_the_deterministic_route() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config("jev", json!({"timeout_ms": 400}))).await;

    for (text, outcome, chosen) in [
        ("FORBIDDEN please", "auth_error", None),
        ("RATE limited", "rate_limited", None),
        ("OVERLOADED now", "rate_limited", None),
        ("BOOM", "provider_error", None),
        ("INVALID-REQUEST", "provider_error", None),
        ("MALFORMED answer", "invalid", None),
        ("UNKNOWN option", "invalid", None),
        ("AUTO-CHOICE", "invalid", None),
        ("NON-CANDIDATE route", "invalid", None),
        ("UNSURE Rust", "low_confidence", Some("Coder")),
        ("SLOW Rust", "timeout", None),
    ] {
        let before = fleet.hits();
        let started = Instant::now();
        let (status, body) = router.send(lightagent(text)).await;
        assert_eq!(
            status, 200,
            "{text}: Auto never fails because the classifier did: {body}"
        );
        assert_eq!(body["model"], "General", "{text}");
        assert_eq!(reached(before, fleet.hits()), [0], "{text}");
        let trace = router.last_trace().await;
        assert_eq!(trace["classifier"]["provider"], "jev");
        assert_eq!(trace["classifier"]["outcome"], outcome, "{text}");
        assert_eq!(
            trace["classifier"]["chosen_route"].as_str(),
            chosen,
            "{text}"
        );
        if outcome == "timeout" {
            assert!(
                started.elapsed() < Duration::from_millis(1_500),
                "{:?}",
                started.elapsed()
            );
        }
        let shown = trace.to_string();
        assert!(
            !shown.contains("PROVIDER-BODY"),
            "a provider's error body is never kept"
        );
    }
    // The node name, Auto and the non-candidate were never routed to.
    assert_eq!(fleet.tool_agent.hits(), 0);

    let metrics = router.text("/metrics").await;
    for (outcome, count) in [
        ("auth_error", 1),
        ("rate_limited", 2),
        ("provider_error", 2),
        ("invalid", 4),
        ("low_confidence", 1),
        ("timeout", 1),
    ] {
        assert!(
            metrics.contains(&format!(
                "router_classifier_requests_total{{provider=\"jev\",outcome=\"{outcome}\"}} {count}"
            )),
            "{outcome}: {metrics}"
        );
    }
    assert!(metrics.contains(
        "router_classifier_duration_seconds_count{provider=\"jev\",outcome=\"timeout\"} 1"
    ));
    for absent in ["PROVIDER-BODY", KEY, "Rust", "TYPESAFE_API_KEY"] {
        assert!(!metrics.contains(absent), "{absent} in metrics");
    }

    // The abandoned slow classification was cancelled, not merely ignored.
    tokio::time::sleep(Duration::from_millis(2_300)).await;
    let answered = fleet.jev.script.answered.load(Ordering::SeqCst);
    assert_eq!(
        answered, 4,
        "UNKNOWN, AUTO-CHOICE, NON-CANDIDATE, UNSURE — and never SLOW"
    );

    let status = router.get("/api/router/v1/auto").await["classifier"]["status"].clone();
    assert_eq!(status["last_failure_kind"], "timeout");
    assert!(
        status["last_success_at"].is_u64(),
        "UNSURE was an answer: {status}"
    );
}

#[tokio::test]
async fn a_bad_key_and_an_unreachable_service_fall_back() {
    ensure_provider();
    let fleet = Fleet::start().await;

    // The router's key is not the one the service accepts.
    let mut config = fleet.config("jev", json!({"api_key_env": "WRONG_KEY"}));
    config["listen"] = json!(["127.0.0.1:0"]);
    config["health"] = json!({"interval_secs": 3600, "timeout_secs": 2, "failure_threshold": 1});
    let file: RouterFile = serde_json::from_value(config).unwrap();
    let validated = lightweight_router::validate(file, &|name| {
        (name == "WRONG_KEY").then(|| "not-the-key".to_owned())
    })
    .unwrap();
    let bound = lightweight_router::bind(&validated).await.unwrap();
    let base = format!("http://{}", bound.addresses()[0]);
    let stop = CancellationToken::new();
    tokio::spawn(bound.serve(stop.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let response = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&lightagent("write a Rust async TCP server"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "General");
    let traces: Value = client()
        .get(format!("{base}/api/router/v1/traces?limit=1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(traces["data"][0]["classifier"]["outcome"], "auth_error");
    stop.cancel();

    // Nothing listening where Jev should be. Windows reports a refused
    // loopback connection only after retrying for about two seconds, so the
    // bound is long enough for the refusal, not the timeout, to be what is
    // seen on every platform.
    let unused = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let closed = format!("http://{}", unused.local_addr().unwrap());
    drop(unused);
    let router =
        Router::start(fleet.config("jev", json!({"base_url": closed, "timeout_ms": 5_000}))).await;
    let (status, body) = router
        .send(lightagent("write a Rust async TCP server"))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["model"], "General");
    assert_eq!(
        router.last_trace().await["classifier"]["outcome"],
        "connection_error"
    );
}

// --- configuration you can see, and check -----------------------------------------------------

#[tokio::test]
async fn the_admin_view_and_the_check_show_jev_without_its_key() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config("jev", json!({}))).await;

    // The start-up check ran in the background and is recorded.
    let mut last_check = Value::Null;
    for _ in 0..100 {
        last_check =
            router.get("/api/router/v1/auto").await["classifier"]["status"]["last_check"].clone();
        if !last_check.is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(last_check["status"], "ok", "{last_check}");
    assert_eq!(last_check["model_listed"], true);

    let auto = router.get("/api/router/v1/auto").await;
    let classifier = &auto["classifier"];
    assert_eq!(classifier["provider"], "jev");
    assert_eq!(classifier["model"], "jev-latest");
    assert_eq!(classifier["timeout_ms"], 1000);
    let jev = &classifier["jev"];
    assert_eq!(jev["active"], true);
    assert_eq!(
        jev["base_url"], fleet.jev.base,
        "normalized: no trailing slash"
    );
    assert_eq!(jev["api_key_env"], "TYPESAFE_API_KEY");
    assert_eq!(jev["api_key_configured"], true);
    assert_eq!(jev["include_user_text"], true);
    assert_eq!(
        classifier["lightweight"]["active"], false,
        "the standby block is shown too"
    );
    let shown = auto.to_string();
    assert!(!shown.contains(KEY) && !shown.contains("Bearer"), "{shown}");

    let (status, report) = router.check().await;
    assert_eq!(status, 200);
    assert_eq!(report["provider"], "jev");
    assert_eq!(report["status"], "ok");
    assert_eq!(report["model"], "jev-latest");
    assert_eq!(report["model_listed"], true);
    assert!(!report.to_string().contains(KEY));

    // A model the account does not list, then a key the service refuses.
    fleet.jev.script.models.lock().unwrap().1 = vec!["jev-preview"];
    let (_, report) = router.check().await;
    assert_eq!(report["status"], "model_not_listed");
    assert_eq!(report["model_listed"], false);
    *fleet.jev.script.models.lock().unwrap() = (429, vec![]);
    let (_, report) = router.check().await;
    assert_eq!(report["status"], "rate_limited");
    assert_eq!(report["http_status"], 429);
    assert_eq!(
        router.get("/api/router/v1/auto").await["classifier"]["status"]["last_check"]["status"],
        "rate_limited"
    );
    assert_eq!(fleet.jev.calls(), 0, "checking never classifies");
}

#[tokio::test]
async fn a_check_reports_a_refused_key_and_switching_provider_needs_no_rule_change() {
    ensure_provider();
    let fleet = Fleet::start().await;

    // Lightweight active, Jev configured beside it: Jev is never called.
    let router = Router::start(fleet.config("lightweight", json!({}))).await;
    let (_, body) = router
        .send(lightagent("write a Rust async TCP server"))
        .await;
    assert_eq!(
        body["model"], "General",
        "the scripted Lightweight classifier always says General"
    );
    assert_eq!(fleet.classifier.hits(), 1);
    assert_eq!(fleet.jev.calls(), 0);
    assert_eq!(
        router.last_trace().await["classifier"]["provider"],
        "lightweight"
    );
    let (status, report) = router.check().await;
    assert_eq!(status, 200);
    assert_eq!(report["provider"], "lightweight");
    assert_eq!(report["status"], "ok");
    // The rules as configured, without their running decision counts.
    let configured = |mut rules: Value| {
        for rule in rules.as_array_mut().unwrap() {
            rule.as_object_mut().unwrap().remove("decisions");
        }
        rules
    };
    let rules = configured(router.get("/api/router/v1/auto").await["rules"].clone());
    drop(router);

    // One word changed; the rules are identical.
    let router = Router::start(fleet.config("jev", json!({}))).await;
    assert_eq!(
        configured(router.get("/api/router/v1/auto").await["rules"].clone()),
        rules
    );
    let (_, body) = router
        .send(lightagent("write a Rust async TCP server"))
        .await;
    assert_eq!(body["model"], "Coder");
    assert_eq!(
        fleet.classifier.hits(),
        1,
        "the Lightweight classifier is not asked now"
    );
    assert_eq!(fleet.jev.calls(), 1);
    drop(router);

    // A key the service refuses is reported by the check, sanitized.
    let mut config = fleet.config("jev", json!({"api_key_env": "WRONG_KEY"}));
    config["listen"] = json!(["127.0.0.1:0"]);
    let file: RouterFile = serde_json::from_value(config).unwrap();
    let validated = lightweight_router::validate(file, &|name| {
        (name == "WRONG_KEY").then(|| "not-the-key".to_owned())
    })
    .unwrap();
    let bound = lightweight_router::bind(&validated).await.unwrap();
    let base = format!("http://{}", bound.addresses()[0]);
    let stop = CancellationToken::new();
    tokio::spawn(bound.serve(stop.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let report: Value = client()
        .post(format!("{base}/api/router/v1/classifier/check"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(report["status"], "auth_error");
    assert_eq!(report["http_status"], 401);
    let shown = report.to_string();
    assert!(
        !shown.contains("not-the-key") && !shown.contains("Missing or invalid"),
        "{shown}"
    );
    stop.cancel();
}

#[tokio::test]
async fn without_a_classifier_there_is_nothing_to_check() {
    ensure_provider();
    let node = Node::start("GeneralAlias").await;
    let router = Router::start(json!({
        "nodes": [{"id": "n", "url": node.base}],
        "routes": [{"name": "General", "deployments": [{"node": "n", "model": "GeneralAlias"}]}]
    }))
    .await;
    let (status, body) = router.check().await;
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "classifier_not_configured");
}
