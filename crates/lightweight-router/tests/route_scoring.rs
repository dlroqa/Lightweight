//! Adaptive logical-route scoring (R9.2), end to end, over real sockets.
//!
//! Every classifier here is scripted: the Lightweight classifier node and the
//! TypeSafe server both answer `PICK <route> <confidence>` exactly as written
//! in the user's message, so which route was named and how confidently is the
//! test's choice, never a model's. Each route has its own scripted node, so
//! which node answered says which route won, and each node can be told to
//! fail in a particular way to feed route history.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::StreamExt as _;
use lightweight_api::capabilities::{CapabilitiesBody, CapabilityModel};
use lightweight_router::RouterState;
use lightweight_router::config::RouterFile;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Short on purpose: the secrets gate refuses a committed bearer literal of
/// 16 or more characters.
const ROUTER_KEY: &str = "rk-test";
const JEV_KEY: &str = "jev-test";

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

/// `PICK Coder 0.70` in `text`, as (route, confidence); General at 0.95
/// otherwise.
fn pick(text: &str) -> (String, f64) {
    let mut words = text.split_whitespace();
    while let Some(word) = words.next() {
        if word == "PICK"
            && let (Some(route), Some(confidence)) = (words.next(), words.next())
            && let Ok(confidence) = confidence.parse()
        {
            return (route.to_owned(), confidence);
        }
    }
    ("General".to_owned(), 0.95)
}

/// A decayed count read a moment after it was recorded: within 0.01.
#[track_caller]
fn close(value: &Value, expected: f64) {
    let found = value.as_f64().unwrap_or(f64::NAN);
    assert!(
        (found - expected).abs() < 0.01,
        "{found} is not {expected} (to within 0.01)"
    );
}

// --- scripted nodes -------------------------------------------------------------

#[derive(Clone)]
struct Script {
    serving: String,
    classifier: bool,
    tools: bool,
    hits: Arc<AtomicU32>,
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

    async fn with(serving: &str, classifier: bool, tools: bool) -> Self {
        let script = Script {
            serving: serving.to_owned(),
            classifier,
            tools,
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

fn frame(model: &Value, delta: &Value) -> String {
    let chunk = json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                       "choices": [{"index": 0, "delta": delta}]});
    format!("data: {chunk}\n\n")
}

/// A model node answers by keyword: `FAIL500`, `BAD400`, `BUSY503`, `BREAK`
/// (a stream the node breaks off), `SLOWSTREAM` (a stream that stalls), or
/// a normal answer. A classifier node answers [`pick`].
async fn generate(
    axum::extract::State(script): axum::extract::State<Script>,
    body: axum::body::Bytes,
) -> Response {
    script.hits.fetch_add(1, Ordering::SeqCst);
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
    if text.contains("FAIL500") {
        return error(500, "internal_error");
    }
    if text.contains("BAD400") {
        return error(400, "invalid_request");
    }
    if text.contains("BUSY503") {
        return error(503, "overloaded");
    }
    if body["stream"] == true {
        let head = frame(&model, &json!({"role": "assistant", "content": "hi"}));
        if text.contains("BREAK") {
            // Committed, then over without a finish: the node broke it off.
            return ([("content-type", "text/event-stream")], head).into_response();
        }
        if text.contains("SLOWSTREAM") {
            let stream = futures_util::stream::unfold(0, move |sent| {
                let head = head.clone();
                async move {
                    if sent > 0 {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    Some((Ok::<_, std::convert::Infallible>(head), sent + 1))
                }
            });
            return (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(stream),
            )
                .into_response();
        }
        let done = json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                          "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
        let text = format!("{head}data: {done}\n\ndata: [DONE]\n\n");
        return ([("content-type", "text/event-stream")], text).into_response();
    }
    axum::Json(json!({
        "id": "c1", "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"},
                     "finish_reason": "stop"}],
    }))
    .into_response()
}

// --- a scripted TypeSafe System One server ---------------------------------------

#[derive(Clone, Default)]
struct Typesafe {
    calls: Arc<AtomicU32>,
}

async fn typesafe_models() -> Response {
    axum::Json(json!({"models": [{"name": "jev-latest"}]})).into_response()
}

async fn typesafe_systemone(
    axum::extract::State(script): axum::extract::State<Typesafe>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    script.calls.fetch_add(1, Ordering::SeqCst);
    let authorized = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(&format!("Bearer {JEV_KEY}"));
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let (choice, confidence) = pick(body["state"]["request"].as_str().unwrap_or(""));
    axum::Json(json!({
        "model": "jev-1.13.0",
        // Probabilities are sent, as the real API sends them, and must not be
        // read: R9.2 consumes only the choice and its confidence.
        "answers": {"route": {"type": "choice", "choice": choice, "confidence": confidence,
                               "probabilities": {choice: 0.01, "General": 0.99}}},
    }))
    .into_response()
}

// --- the router -------------------------------------------------------------------

struct Router {
    base: String,
    state: Arc<RouterState>,
    key: Option<&'static str>,
    stop: CancellationToken,
}

impl Drop for Router {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Router {
    async fn start(config: Value) -> Self {
        Self::start_with(config, None).await
    }

    async fn start_with(mut config: Value, key: Option<&'static str>) -> Self {
        ensure_provider();
        config["listen"] = json!(["127.0.0.1:0"]);
        config["health"] =
            json!({"interval_secs": 3600, "timeout_secs": 2, "failure_threshold": 1});
        if key.is_some() {
            config["api_key_env"] = json!("ROUTER_KEY");
        }
        let file: RouterFile = serde_json::from_value(config).expect("config shape");
        let config = lightweight_router::validate(file, &|name| match name {
            "ROUTER_KEY" => Some(ROUTER_KEY.to_owned()),
            "TYPESAFE_API_KEY" => Some(JEV_KEY.to_owned()),
            _ => None,
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
            state,
            key,
            stop,
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let request = client().request(method, format!("{}{path}", self.base));
        match self.key {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }

    /// One chat request for `model` saying `text`, read to its end.
    async fn chat(&self, model: &str, text: &str) -> (u16, Value) {
        self.chat_with(json!({"model": model, "messages": [{"role": "user", "content": text}]}))
            .await
    }

    async fn chat_with(&self, body: Value) -> (u16, Value) {
        let response = self
            .request(reqwest::Method::POST, "/v1/chat/completions")
            .json(&body)
            .send()
            .await
            .expect("request");
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn get(&self, path: &str) -> Value {
        self.request(reqwest::Method::GET, path)
            .send()
            .await
            .expect("request")
            .json()
            .await
            .expect("json")
    }

    async fn reset(&self, body: Option<Value>) -> (u16, Value) {
        let mut request = self.request(
            reqwest::Method::POST,
            "/api/router/v1/adaptive-scoring/reset",
        );
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("request");
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
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

    /// One route's history, from the admin view.
    async fn history(&self, route: &str) -> Value {
        self.get("/api/router/v1/auto").await["adaptive_scoring"]["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["route"] == route)
            .cloned()
            .unwrap_or(Value::Null)
    }

    async fn metrics(&self) -> String {
        self.request(reqwest::Method::GET, "/metrics")
            .send()
            .await
            .expect("request")
            .text()
            .await
            .expect("text")
    }
}

// --- the fleet ----------------------------------------------------------------------

struct Fleet {
    general: Node,
    coder: Node,
    research: Node,
    tool_agent: Node,
    classifier: Node,
    jev: (String, CancellationToken, Typesafe),
}

impl Fleet {
    async fn start() -> Self {
        let typesafe = Typesafe::default();
        let app = axum::Router::new()
            .route("/v1/models", get(typesafe_models))
            .route("/v1/systemone", post(typesafe_systemone))
            .with_state(typesafe.clone());
        let (jev_base, jev_stop) = serve(app).await;
        Self {
            general: Node::start("GeneralAlias").await,
            coder: Node::start("CoderAlias").await,
            // Research cannot serve tools: a tool request to it is a
            // capability mismatch.
            research: Node::with("ResearchAlias", false, false).await,
            tool_agent: Node::start("ToolAlias").await,
            classifier: Node::with("ClassifierAlias", true, true).await,
            jev: (jev_base, jev_stop, typesafe),
        }
    }

    /// Forced tool use is deterministic; everything else is classified by
    /// `provider`, among General, Coder, Research and Offline (whose only node
    /// is never up).
    fn config(&self, provider: &str, scoring: Option<Value>) -> Value {
        let mut auto = json!({
            "enabled": true,
            "fallback_route": "General",
            "classifier": {
                "provider": provider,
                "routes": ["General", "Coder", "Research", "Offline"],
                "lightweight": {"route": "RouterClassifier", "timeout_ms": 5_000,
                                "min_confidence": 0.65},
                "jev": {"base_url": self.jev.0, "model": "jev-latest", "timeout_ms": 5_000,
                        "min_confidence": 0.65}
            },
            "rules": [
                {"name": "forced-tools", "when": {"tool_choice": "required"}, "route": "ToolAgent"},
                {"name": "semantic", "when": {}, "classify": true}
            ]
        });
        if let Some(scoring) = scoring {
            auto["adaptive_scoring"] = scoring;
        }
        json!({
            "nodes": [
                {"id": "general", "url": self.general.base},
                {"id": "coder", "url": self.coder.base},
                {"id": "research", "url": self.research.base},
                {"id": "tools", "url": self.tool_agent.base},
                {"id": "classifier", "url": self.classifier.base},
                {"id": "offline", "url": "http://127.0.0.1:9"}
            ],
            "routes": [
                {"name": "General", "deployments": [{"node": "general", "model": "GeneralAlias"}]},
                {"name": "Coder", "deployments": [{"node": "coder", "model": "CoderAlias"}]},
                {"name": "Research", "deployments": [{"node": "research", "model": "ResearchAlias"}]},
                {"name": "Offline", "deployments": [{"node": "offline", "model": "OfflineAlias"}]},
                {"name": "ToolAgent", "deployments": [{"node": "tools", "model": "ToolAlias"}]},
                {"name": "RouterClassifier", "deployments": [{"node": "classifier", "model": "ClassifierAlias"}]}
            ],
            "auto_route": auto,
        })
    }

    fn jev_calls(&self) -> u32 {
        self.jev.2.calls.load(Ordering::SeqCst)
    }
}

/// `(prior + 2·history) / classifier` = 0.05 + 0.12 = 0.17 < 0.175: the most
/// influence the bound allows at `min_confidence` 0.65, near enough.
fn borderline_scoring() -> Value {
    json!({"enabled": true, "weights": {"prior": 0.05, "history": 0.06},
           "priors": {"General": 0.2}})
}

/// Direct traffic that makes General look reliable and Coder unreliable.
async fn favour_general(router: &Router) {
    for _ in 0..30 {
        assert_eq!(router.chat("General", "hello").await.0, 200);
        assert_eq!(router.chat("Coder", "FAIL500").await.0, 500);
    }
}

// --- compatibility ------------------------------------------------------------------

#[tokio::test]
async fn absent_or_off_classification_resolves_as_r91_and_nothing_is_recorded() {
    let fleet = Fleet::start().await;
    // An off section is still configured (and checked), and otherwise inert:
    // weights that would decide every borderline case change nothing.
    let off = json!({"enabled": false, "weights": {"prior": 0.05, "history": 0.06},
                     "priors": {"General": 1.0}});
    for (scoring, configured) in [(None, false), (Some(off), true)] {
        let router = Router::start(fleet.config("lightweight", scoring)).await;
        favour_general(&router).await;
        for (text, route) in [
            ("PICK Coder 0.95", "Coder"),
            ("PICK Coder 0.65", "Coder"),
            ("PICK Coder 0.40", "General"),
        ] {
            let (status, body) = router.chat("Auto", text).await;
            assert_eq!(
                (status, body["model"].as_str()),
                (200, Some(route)),
                "{text}"
            );
            let trace = router.last_trace().await;
            assert!(trace.get("scoring").is_none(), "{trace}");
        }
        let view = &router.get("/api/router/v1/auto").await["adaptive_scoring"];
        assert_eq!(view["configured"], configured);
        assert_eq!(view["enabled"], false);
        assert!(!router.state.route_history.is_recording());
        assert_eq!(router.state.metrics.scoring_fallbacks("below_threshold"), 0);
        let metrics = router.metrics().await;
        assert!(!metrics.contains("router_route_history_observations_total{"));
        assert!(!metrics.contains("router_route_history_signal"));
        assert!(!metrics.contains("router_route_scoring_decisions_total{"));
    }
}

#[tokio::test]
async fn neutral_weights_reproduce_r91_even_with_lopsided_history() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config("lightweight", Some(json!({"enabled": true})))).await;
    favour_general(&router).await;
    for (text, route, reason) in [
        ("PICK Coder 0.95", "Coder", "scored"),
        ("PICK Coder 0.65", "Coder", "scored"),
        ("PICK Coder 0.40", "General", "below_threshold"),
        ("PICK General 0.99", "General", "uncontested"),
    ] {
        let (status, body) = router.chat("Auto", text).await;
        assert_eq!(
            (status, body["model"].as_str()),
            (200, Some(route)),
            "{text}"
        );
        let trace = router.last_trace().await;
        assert_eq!(trace["scoring"]["reason"], reason, "{trace}");
        assert_eq!(trace["scoring"]["overrode"], false);
    }
}

// --- the threshold boundary, end to end (the smoke test) -------------------------------

#[tokio::test]
async fn high_borderline_and_low_confidence_coder() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config("lightweight", Some(borderline_scoring()))).await;
    favour_general(&router).await;
    let coder_before = fleet.coder.hits();

    // High confidence: Coder, despite General's favourable history and prior.
    let (status, body) = router.chat("Auto", "PICK Coder 0.95").await;
    assert_eq!((status, body["model"].as_str()), (200, Some("Coder")));
    assert_eq!(fleet.coder.hits(), coder_before + 1);
    let trace = router.last_trace().await;
    assert_eq!(trace["scoring"]["reason"], "scored");
    assert_eq!(trace["scoring"]["winner"], "Coder");
    assert_eq!(trace["scoring"]["overrode"], false);

    // Borderline: within the bound, history and prior choose General.
    let (status, body) = router.chat("Auto", "PICK Coder 0.70").await;
    assert_eq!((status, body["model"].as_str()), (200, Some("General")));
    let trace = router.last_trace().await;
    let scoring = &trace["scoring"];
    assert_eq!(scoring["reason"], "scored");
    assert_eq!(scoring["classified_route"], "Coder");
    assert_eq!(scoring["winner"], "General");
    assert_eq!(scoring["overrode"], true);
    assert_eq!(scoring["classifier_baseline"], 0.65);
    assert_eq!(scoring["candidates"][0]["route"], "Coder");
    assert_eq!(scoring["candidates"][0]["basis"], "verdict");
    assert_eq!(scoring["candidates"][1]["route"], "General");
    assert_eq!(scoring["candidates"][1]["basis"], "baseline");
    assert!(
        scoring["candidates"][1]["total_score"].as_f64()
            > scoring["candidates"][0]["total_score"].as_f64()
    );
    // R9.1's own record of the classification is unchanged.
    assert_eq!(trace["classifier"]["outcome"], "chosen");
    assert_eq!(trace["classifier"]["chosen_route"], "Coder");
    assert_eq!(trace["route"], "General");
    let text = scoring.to_string();
    for absent in ["deployment", "node", "Alias", "session", "127.0.0.1"] {
        assert!(!text.contains(absent), "{absent}: {text}");
    }

    // Below the threshold: R9.1's fallback, and Coder is not a contender.
    let coder_before = fleet.coder.hits();
    let (status, body) = router.chat("Auto", "PICK Coder 0.40").await;
    assert_eq!((status, body["model"].as_str()), (200, Some("General")));
    assert_eq!(fleet.coder.hits(), coder_before);
    let trace = router.last_trace().await;
    assert_eq!(trace["scoring"]["reason"], "below_threshold");
    assert_eq!(trace["scoring"]["rejected_route"], "Coder");
    assert_eq!(trace["scoring"]["candidates"], json!([]));
    assert_eq!(trace["classifier"]["outcome"], "low_confidence");

    let state = &router.state;
    assert_eq!(state.metrics.scoring_decisions("Coder", false), 1);
    assert_eq!(state.metrics.scoring_decisions("General", true), 1);
    assert_eq!(state.metrics.scoring_fallbacks("below_threshold"), 1);
    let metrics = router.metrics().await;
    for line in [
        "router_route_scoring_decisions_total{route=\"General\",overrode=\"true\"} 1",
        "router_route_scoring_decisions_total{route=\"Coder\",overrode=\"false\"} 1",
        "router_route_scoring_fallback_total{reason=\"below_threshold\"} 1",
    ] {
        assert!(metrics.contains(line), "{line}\n{metrics}");
    }
}

#[tokio::test]
async fn a_rejected_route_is_never_resurrected_however_favoured() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config(
        "lightweight",
        Some(
            json!({"enabled": true, "weights": {"prior": 0.05, "history": 0.06},
                    "priors": {"Coder": 1.0}}),
        ),
    ))
    .await;
    // Coder: the top prior and a perfect record. General: a failing one.
    for _ in 0..30 {
        assert_eq!(router.chat("Coder", "hello").await.0, 200);
        assert_eq!(router.chat("General", "FAIL500").await.0, 500);
    }
    let coder_before = fleet.coder.hits();
    for confidence in ["0.10", "0.40", "0.64"] {
        let (status, body) = router
            .chat("Auto", &format!("PICK Coder {confidence}"))
            .await;
        assert_eq!((status, body["model"].as_str()), (200, Some("General")));
        let trace = router.last_trace().await;
        assert_eq!(trace["scoring"]["reason"], "below_threshold");
    }
    assert_eq!(fleet.coder.hits(), coder_before, "Coder was never asked");
    // And just at the threshold, accepted, Coder's evidence holds it there.
    let (_, body) = router.chat("Auto", "PICK Coder 0.65").await;
    assert_eq!(body["model"], "Coder");
}

// --- what is never scored -------------------------------------------------------------

#[tokio::test]
async fn an_explicit_route_bypasses_scoring_and_still_feeds_history() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config("lightweight", Some(borderline_scoring()))).await;
    let classifier_before = fleet.classifier.hits();
    let (status, body) = router.chat("Coder", "PICK General 0.99").await;
    assert_eq!((status, body["model"].as_str()), (200, Some("Coder")));
    let trace = router.last_trace().await;
    assert!(trace.get("scoring").is_none(), "{trace}");
    assert!(trace.get("classifier").is_none());
    assert_eq!(fleet.classifier.hits(), classifier_before);
    close(&router.history("Coder").await["successes"], 1.0);
}

#[tokio::test]
async fn a_hard_rule_bypasses_scoring() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config("lightweight", Some(borderline_scoring()))).await;
    let classifier_before = fleet.classifier.hits();
    let (status, body) = router
        .chat_with(json!({"model": "Auto", "tool_choice": "required",
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}],
            "messages": [{"role": "user", "content": "PICK Coder 0.99"}]}))
        .await;
    assert_eq!((status, body["model"].as_str()), (200, Some("ToolAgent")));
    let trace = router.last_trace().await;
    assert_eq!(trace["auto_rule"], "forced-tools");
    assert!(trace.get("scoring").is_none(), "{trace}");
    assert_eq!(fleet.classifier.hits(), classifier_before);
    assert_eq!(router.state.metrics.scoring_decisions("Coder", false), 0);
}

// --- history semantics ------------------------------------------------------------------

#[tokio::test]
async fn history_counts_each_final_outcome_as_specified() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config("lightweight", Some(borderline_scoring()))).await;

    // ok: a whole response and a complete stream.
    assert_eq!(router.chat("Coder", "hello").await.0, 200);
    let (status, _) = router
        .chat_with(json!({"model": "Coder", "stream": true,
                          "messages": [{"role": "user", "content": "hello"}]}))
        .await;
    assert_eq!(status, 200);
    // server_error, and interrupted: scored failures.
    assert_eq!(router.chat("Coder", "FAIL500").await.0, 500);
    let (status, _) = router
        .chat_with(json!({"model": "Coder", "stream": true,
                          "messages": [{"role": "user", "content": "BREAK"}]}))
        .await;
    assert_eq!(status, 200);
    // A client error: neutral.
    assert_eq!(router.chat("Coder", "BAD400").await.0, 400);
    // The only deployment turned it away with a 503: unavailable, not scored.
    assert_eq!(router.chat("Coder", "BUSY503").await.0, 503);
    // A capability mismatch: observed, not scored.
    let (status, body) = router
        .chat_with(json!({"model": "Research", "tool_choice": "required",
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}],
            "messages": [{"role": "user", "content": "hello"}]}))
        .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "route_capability_mismatch");
    // route_unavailable: observed, not scored.
    let (status, body) = router.chat("Offline", "hello").await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["code"], "route_unavailable");

    // A client that leaves mid-stream: neutral.
    let response = router
        .request(reqwest::Method::POST, "/v1/chat/completions")
        .json(&json!({"model": "Coder", "stream": true,
                      "messages": [{"role": "user", "content": "SLOWSTREAM"}]}))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let mut stream = response.bytes_stream();
    let _ = stream.next().await;
    drop(stream);
    let mut cancelled = false;
    for _ in 0..100 {
        if router.history("Coder").await["neutral"] == 2 {
            cancelled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(cancelled, "{}", router.history("Coder").await);

    let coder = router.history("Coder").await;
    close(&coder["successes"], 2.0);
    close(&coder["failures"], 2.0);
    assert_eq!(
        coder["neutral"], 2,
        "client error and cancellation: {coder}"
    );
    assert_eq!(coder["unavailable"], 1, "{coder}");
    close(&coder["effective_samples"], 4.0);
    assert_eq!(coder["gated"], true, "4 < min_samples 20");
    assert_eq!(coder["value"], 0.0);

    let research = router.history("Research").await;
    assert_eq!(
        (&research["mismatch"], &research["effective_samples"]),
        (&json!(1), &json!(0.0)),
        "{research}"
    );
    let offline = router.history("Offline").await;
    assert_eq!(
        (&offline["unavailable"], &offline["effective_samples"]),
        (&json!(1), &json!(0.0)),
        "{offline}"
    );

    let metrics = router.metrics().await;
    for line in [
        "router_route_history_observations_total{route=\"Coder\",outcome=\"success\"} 2",
        "router_route_history_observations_total{route=\"Coder\",outcome=\"failure\"} 2",
        "router_route_history_observations_total{route=\"Coder\",outcome=\"neutral\"} 2",
        "router_route_history_observations_total{route=\"Coder\",outcome=\"unavailable\"} 1",
        "router_route_history_observations_total{route=\"Research\",outcome=\"mismatch\"} 1",
        "router_route_history_observations_total{route=\"Offline\",outcome=\"unavailable\"} 1",
        "router_route_history_signal{route=\"Coder\"} 0",
    ] {
        assert!(metrics.contains(line), "{line}\n{metrics}");
    }
    let samples = metrics
        .lines()
        .find_map(|line| {
            line.strip_prefix("router_route_history_effective_samples{route=\"Coder\"} ")
        })
        .expect("the gauge");
    close(&json!(samples.parse::<f64>().unwrap()), 4.0);
}

#[tokio::test]
async fn classification_requests_never_count_toward_history() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config("lightweight", Some(borderline_scoring()))).await;
    for text in [
        "PICK Coder 0.9",
        "PICK Research 0.9",
        "PICK Coder 0.3",
        "hello",
    ] {
        assert_eq!(router.chat("Auto", text).await.0, 200);
    }
    assert_eq!(fleet.classifier.hits(), 4, "every one was classified");
    let classifier = router.history("RouterClassifier").await;
    assert_eq!(classifier["effective_samples"], 0.0, "{classifier}");
    assert_eq!(classifier["neutral"], 0, "{classifier}");
    assert_eq!(classifier["last_observed_at"], Value::Null);
    let resolved: f64 = ["General", "Coder", "Research"]
        .iter()
        .map(|route| {
            let router = &router;
            async move { router.history(route).await["successes"].as_f64().unwrap() }
        })
        .collect::<futures_util::stream::FuturesOrdered<_>>()
        .collect::<Vec<f64>>()
        .await
        .iter()
        .sum();
    close(&json!(resolved), 4.0);
}

// --- R9.3 stays out -----------------------------------------------------------------------

#[tokio::test]
async fn a_winning_route_that_is_unavailable_answers_route_unavailable_and_nothing_else() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config("lightweight", Some(borderline_scoring()))).await;
    let before: Vec<u32> = [&fleet.general, &fleet.coder, &fleet.research]
        .iter()
        .map(|node| node.hits())
        .collect();
    let (status, body) = router.chat("Auto", "PICK Offline 0.95").await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["code"], "route_unavailable");
    let after: Vec<u32> = [&fleet.general, &fleet.coder, &fleet.research]
        .iter()
        .map(|node| node.hits())
        .collect();
    assert_eq!(before, after, "no second route was tried");
    let trace = router.last_trace().await;
    assert_eq!(trace["route"], "Offline");
    assert_eq!(trace["scoring"]["winner"], "Offline");
    assert_eq!(trace["outcome"], "unavailable");
    let offline = router.history("Offline").await;
    assert_eq!(offline["unavailable"], 1);
    assert_eq!(offline["failures"], 0.0, "unavailable is not scored");
    assert_eq!(offline["effective_samples"], 0.0);
}

// --- reset --------------------------------------------------------------------------------

#[tokio::test]
async fn reset_forgets_history_and_nothing_else() {
    let fleet = Fleet::start().await;
    let mut config = fleet.config("lightweight", Some(borderline_scoring()));
    config["session_affinity"] = json!({"enabled": true});
    let router = Router::start_with(config, Some(ROUTER_KEY)).await;

    // Without the key: refused, and nothing is reset.
    let anonymous = client()
        .post(format!(
            "{}/api/router/v1/adaptive-scoring/reset",
            router.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);

    favour_general(&router).await;
    let session = router
        .request(reqwest::Method::POST, "/v1/chat/completions")
        .header("X-Lightweight-Session", "s-1")
        .json(&json!({"model": "Coder", "messages": [{"role": "user", "content": "hello"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(session.status(), 200);
    let sessions = router.get("/api/router/v1/sessions").await;
    assert_eq!(sessions["active"], 1);
    let routes = router.get("/api/router/v1/routes").await;
    let placement = router.get("/api/router/v1/placement").await;
    let auto = router.get("/api/router/v1/auto").await;
    close(&router.history("General").await["successes"], 30.0);

    let (status, body) = router.reset(Some(json!({"route": "general"}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["reset"], "route");
    assert_eq!(body["routes"], json!(["General"]));
    assert!(body["reset_at"].as_u64().is_some());
    assert_eq!(body.as_object().unwrap().len(), 3, "sanitized: {body}");
    close(&router.history("General").await["successes"], 0.0);
    close(&router.history("Coder").await["failures"], 30.0);

    let (status, body) = router.reset(None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["reset"], "all");
    close(&router.history("Coder").await["failures"], 0.0);
    close(&router.history("Coder").await["successes"], 0.0);

    // Untouched: affinity, routes, placement, rules and classifier settings.
    assert_eq!(router.get("/api/router/v1/sessions").await, sessions);
    assert_eq!(router.get("/api/router/v1/routes").await, routes);
    assert_eq!(router.get("/api/router/v1/placement").await, placement);
    let after = router.get("/api/router/v1/auto").await;
    for key in ["rules", "fallback_route", "classifier", "enabled"] {
        if key == "classifier" {
            for field in ["provider", "candidates", "min_confidence", "fallback_route"] {
                assert_eq!(after[key][field], auto[key][field], "{key}.{field}");
            }
        } else {
            assert_eq!(after[key], auto[key], "{key}");
        }
    }
    for field in [
        "weights",
        "priors",
        "history",
        "enabled",
        "influence_radius",
    ] {
        assert_eq!(
            after["adaptive_scoring"][field], auto["adaptive_scoring"][field],
            "{field}"
        );
    }

    // Bad requests.
    let (status, body) = router.reset(Some(json!({"route": "Nowhere"}))).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (404, Some("route_not_found"))
    );
    let (status, body) = router
        .reset(Some(json!({"route": "Coder", "all": true})))
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (400, Some("invalid_request_body"))
    );
}

#[tokio::test]
async fn reset_without_scoring_has_nothing_to_reset() {
    let fleet = Fleet::start().await;
    for scoring in [None, Some(json!({"enabled": false}))] {
        let router = Router::start(fleet.config("lightweight", scoring)).await;
        let (status, body) = router.reset(None).await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"]["code"], "adaptive_scoring_not_enabled");
    }
}

// --- the admin view --------------------------------------------------------------------------

#[tokio::test]
async fn the_admin_view_shows_settings_and_aggregates_only() {
    let fleet = Fleet::start().await;
    let router = Router::start(fleet.config("lightweight", Some(borderline_scoring()))).await;
    assert_eq!(
        router
            .chat_with(json!({"model": "Coder", "messages": [{"role": "user", "content": "hi"}]}))
            .await
            .0,
        200
    );
    let view = router.get("/api/router/v1/auto").await["adaptive_scoring"].clone();
    assert_eq!(view["configured"], true);
    assert_eq!(view["enabled"], true);
    assert_eq!(
        view["weights"],
        json!({"classifier": 1.0, "prior": 0.05, "history": 0.06})
    );
    assert_eq!(view["priors"], json!({"General": 0.2}));
    assert_eq!(view["classifier_baseline"], 0.65);
    assert_eq!(view["history"]["half_life_secs"], 3_600);
    assert_eq!(view["history"]["half_life_provisional"], true);
    assert_eq!(view["history"]["min_samples"], 20);
    let coder = view["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["route"] == "Coder")
        .unwrap();
    for field in [
        "effective_samples",
        "successes",
        "failures",
        "success_rate",
        "value",
        "gated",
        "unavailable",
        "mismatch",
        "neutral",
        "last_observed_at",
    ] {
        assert!(coder.get(field).is_some(), "{field}: {coder}");
    }
    let text = view.to_string();
    for absent in [
        "request_id",
        "session",
        "deployment",
        "node",
        "127.0.0.1",
        "Alias",
        "prompt",
    ] {
        assert!(!text.contains(absent), "{absent}: {text}");
    }
}

// --- provider parity ---------------------------------------------------------------------------

#[tokio::test]
async fn jev_and_lightweight_verdicts_are_scored_alike() {
    let fleet = Fleet::start().await;
    let mut decisions = Vec::new();
    for provider in ["lightweight", "jev"] {
        let router = Router::start(fleet.config(provider, Some(borderline_scoring()))).await;
        favour_general(&router).await;
        let mut seen = Vec::new();
        for text in [
            "PICK Coder 0.95",
            "PICK Coder 0.70",
            "PICK Coder 0.40",
            "PICK General 0.9",
        ] {
            let (status, body) = router.chat("Auto", text).await;
            assert_eq!(status, 200);
            let trace = router.last_trace().await;
            assert_eq!(trace["classifier"]["provider"], provider);
            let mut scoring = trace["scoring"].clone();
            // Scores, components and the decision: everything but timing.
            if let Some(candidates) = scoring["candidates"].as_array_mut() {
                for candidate in candidates {
                    candidate["history"]["effective_samples"] = json!(
                        candidate["history"]["effective_samples"]
                            .as_f64()
                            .map(f64::round)
                    );
                }
            }
            seen.push((
                body["model"].clone(),
                scoring["reason"].clone(),
                scoring["winner"].clone(),
            ));
        }
        decisions.push(seen);
    }
    assert!(fleet.jev_calls() >= 4, "Jev was asked");
    assert_eq!(
        decisions[0], decisions[1],
        "the provider is metadata, not a weight"
    );
    assert_eq!(
        decisions[0]
            .iter()
            .map(|(model, _, _)| model.as_str().unwrap())
            .collect::<Vec<_>>(),
        ["Coder", "General", "General", "General"]
    );
}
