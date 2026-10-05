//! `Auto`, end to end, over real sockets.
//!
//! `Auto` chooses a logical route from the request's structure; the route's
//! own pipeline — health, capability filtering, session affinity, policy,
//! failover — then chooses the deployment, exactly as if the client had named
//! the route. These tests hold both halves to that: each route here has its
//! own scripted nodes, so which node answered says which route `Auto` chose,
//! and what the client got back says the route did the rest unchanged.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
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

#[derive(Clone)]
enum Act {
    /// Answer as a gateway would, echoing the `model` it was sent.
    Answer,
    /// Refuse with this status and body before any output.
    Refuse(u16, Value),
    /// Stay in flight until the gate opens, then answer.
    Hold(Arc<tokio::sync::Semaphore>),
}

/// What a node's capabilities probe reports.
#[derive(Clone, Copy)]
struct Caps {
    tools: bool,
    reasoning: bool,
    completions: bool,
    context_length: u32,
    limit: u32,
}

const FULL: Caps = Caps {
    tools: true,
    reasoning: true,
    completions: true,
    context_length: 4096,
    limit: 4,
};

#[derive(Clone)]
struct Script {
    serving: String,
    caps: Caps,
    act: Arc<Mutex<Act>>,
    hits: Arc<AtomicU32>,
    seen: Arc<Mutex<Vec<Value>>>,
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
    async fn start(serving: &str) -> Self {
        Self::with(serving, FULL, Act::Answer).await
    }

    async fn with(serving: &str, caps: Caps, act: Act) -> Self {
        let script = Script {
            serving: serving.to_owned(),
            caps,
            act: Arc::new(Mutex::new(act)),
            hits: Arc::default(),
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

    fn seen(&self) -> Vec<Value> {
        self.script.seen.lock().unwrap().clone()
    }

    fn act(&self, act: Act) {
        *self.script.act.lock().unwrap() = act;
    }
}

async fn capabilities(axum::extract::State(script): axum::extract::State<Script>) -> Response {
    let mut body = CapabilitiesBody::new(
        "0.5.0",
        Some(CapabilityModel {
            id: script.serving.clone(),
            context_length: script.caps.context_length,
        }),
        script.caps.limit,
    );
    let features = &mut body.features;
    features.tools = script.caps.tools;
    features.tool_choice = script.caps.tools;
    features.reasoning_content = script.caps.reasoning;
    features.completions = script.caps.completions;
    axum::Json(body).into_response()
}

async fn generate(
    axum::extract::State(script): axum::extract::State<Script>,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> Response {
    script.hits.fetch_add(1, Ordering::SeqCst);
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    script.seen.lock().unwrap().push(body.clone());
    let model = body["model"].clone();
    let act = script.act.lock().unwrap().clone();
    match act {
        Act::Refuse(status, envelope) => {
            return (StatusCode::from_u16(status).unwrap(), axum::Json(envelope)).into_response();
        }
        Act::Hold(gate) => {
            let _ = gate.acquire().await;
        }
        Act::Answer => {}
    }
    if uri.path() == "/v1/completions" {
        return axum::Json(json!({
            "id": "t1", "object": "text_completion", "model": model,
            "choices": [{"index": 0, "text": "hi", "finish_reason": "stop"}],
        }))
        .into_response();
    }
    let tools = body["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty());
    if body["stream"] == true {
        let mut text = String::from(": keep-alive\n\n");
        let delta = if tools {
            json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                   "function": {"name": "search", "arguments": "{}"}}]})
        } else {
            json!({"content": "hi"})
        };
        for frame in [
            json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                   "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                   "choices": [{"index": 0, "delta": delta}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                   "choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 1,
                   "total_tokens": 4}}),
        ] {
            text.push_str(&format!("data: {frame}\n\n"));
        }
        text.push_str("data: [DONE]\n\n");
        return ([("content-type", "text/event-stream")], text).into_response();
    }
    let message = if tools {
        json!({"role": "assistant", "content": null, "tool_calls": [{"id": "call_1",
               "type": "function", "function": {"name": "search", "arguments": "{}"}}]})
    } else {
        json!({"role": "assistant", "content": "hi"})
    };
    axum::Json(json!({
        "id": "c1", "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "message": message,
                     "finish_reason": if tools { "tool_calls" } else { "stop" }}],
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

    async fn post(&self, path: &str, body: Value, session: Option<&str>) -> reqwest::Response {
        let mut request = client().post(format!("{}{path}", self.base)).json(&body);
        if let Some(session) = session {
            request = request.header("X-Lightweight-Session", session);
        }
        request.send().await.expect("request")
    }

    async fn send(&self, body: Value) -> (u16, Value) {
        self.send_to("/v1/chat/completions", body, None).await
    }

    async fn send_to(&self, path: &str, body: Value, session: Option<&str>) -> (u16, Value) {
        let response = self.post(path, body, session).await;
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

    async fn metrics(&self) -> String {
        client()
            .get(format!("{}/metrics", self.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }

    /// The most recent trace.
    async fn last_trace(&self) -> Value {
        self.get("/api/router/v1/traces?limit=1").await["data"][0].clone()
    }
}

// --- configurations ---------------------------------------------------------------

/// A route's deployments: (node id, the node serving it).
type Members<'a> = &'a [(&'a str, &'a Node)];

/// One route per entry: (route, strategy, deployments).
fn topology(routes: &[(&str, &str, Members<'_>)]) -> Value {
    let mut nodes = Vec::new();
    let mut rows = Vec::new();
    for (name, strategy, members) in routes {
        let mut deployments = Vec::new();
        for (id, node) in *members {
            if !nodes.iter().any(|n: &Value| n["id"] == *id) {
                nodes.push(json!({"id": id, "url": node.base}));
            }
            deployments.push(json!({"node": id, "model": node.script.serving}));
        }
        rows.push(json!({"name": name, "strategy": strategy, "deployments": deployments}));
    }
    json!({"nodes": nodes, "routes": rows})
}

fn with_auto(mut config: Value, rules: Value, fallback: &str) -> Value {
    config["auto_route"] = json!({"enabled": true, "fallback_route": fallback, "rules": rules});
    config
}

fn hi(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
}

fn with(model: &str, extra: Value) -> Value {
    let mut body = hi(model);
    if let (Some(body), Value::Object(extra)) = (body.as_object_mut(), extra) {
        body.extend(extra);
    }
    body
}

fn tools() -> Value {
    json!([{"type": "function", "function": {"name": "search", "parameters": {"type": "object"}}}])
}

/// The rules every multi-route test here uses, in this order.
fn standard_rules() -> Value {
    json!([
        {"name": "agentic", "when": {"requires_tools": true, "requires_reasoning": true}, "route": "Agentic"},
        {"name": "forced-tool", "when": {"tool_choice": "required"}, "route": "Forced"},
        {"name": "tools", "when": {"requires_tools": true}, "route": "Coder"},
        {"name": "reasoning", "when": {"requires_reasoning": true}, "route": "Reasoning"},
        {"name": "large-context", "when": {"min_prompt_tokens": 1000}, "route": "LongContext"},
        {"name": "completion", "when": {"endpoint": "completion"}, "route": "Completion"}
    ])
}

/// Seven routes, one node each, so the node that answered names the route.
struct Fleet {
    general: Node,
    coder: Node,
    reasoning: Node,
    long: Node,
    completion: Node,
    agentic: Node,
    forced: Node,
}

impl Fleet {
    async fn start() -> Self {
        Self {
            general: Node::start("GeneralAlias").await,
            coder: Node::start("CoderAlias").await,
            reasoning: Node::start("ReasonerAlias").await,
            long: Node::with(
                "LongAlias",
                Caps {
                    context_length: 32_768,
                    ..FULL
                },
                Act::Answer,
            )
            .await,
            completion: Node::start("CompletionAlias").await,
            agentic: Node::start("AgenticAlias").await,
            forced: Node::start("ForcedAlias").await,
        }
    }

    fn config(&self) -> Value {
        topology(&[
            ("General", "priority", &[("general", &self.general)]),
            ("Coder", "priority", &[("coder", &self.coder)]),
            ("Reasoning", "priority", &[("reasoning", &self.reasoning)]),
            ("LongContext", "priority", &[("long", &self.long)]),
            (
                "Completion",
                "priority",
                &[("completion", &self.completion)],
            ),
            ("Agentic", "priority", &[("agentic", &self.agentic)]),
            ("Forced", "priority", &[("forced", &self.forced)]),
        ])
    }

    fn hits(&self) -> [u32; 7] {
        [
            self.general.hits(),
            self.coder.hits(),
            self.reasoning.hits(),
            self.long.hits(),
            self.completion.hits(),
            self.agentic.hits(),
            self.forced.hits(),
        ]
    }

    const ALIASES: [&str; 7] = [
        "GeneralAlias",
        "CoderAlias",
        "ReasonerAlias",
        "LongAlias",
        "CompletionAlias",
        "AgenticAlias",
        "ForcedAlias",
    ];
}

/// Which of the fleet's nodes a request reached, by the change in hits.
fn reached(before: [u32; 7], after: [u32; 7]) -> Vec<usize> {
    (0..7).filter(|&i| after[i] != before[i]).collect()
}

fn no_alias_in(text: &str) {
    for alias in Fleet::ALIASES {
        assert!(!text.contains(alias), "{alias} leaked: {text}");
    }
}

// --- discovery ------------------------------------------------------------------

#[tokio::test]
async fn auto_is_listed_as_a_router_model_and_never_as_a_physical_one() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(with_auto(fleet.config(), standard_rules(), "General")).await;

    let models = router.get("/v1/models").await;
    let rows = models["data"].as_array().unwrap();
    let auto: Vec<&Value> = rows.iter().filter(|row| row["id"] == "Auto").collect();
    assert_eq!(auto.len(), 1, "{models}");
    assert_eq!(auto[0]["object"], "model");
    assert_eq!(auto[0]["owned_by"], "lightweight-router");
    // A route's context depends on the route; Auto has none of its own.
    for key in ["context_length", "n_ctx", "max_tokens", "max_output_tokens"] {
        assert!(auto[0].get(key).is_none(), "Auto claims {key}: {}", auto[0]);
    }
    assert_eq!(rows.len(), 8, "seven routes and Auto");
    no_alias_in(&models.to_string());

    let capabilities = router.get("/v1/capabilities").await;
    // Not a route: it has no features, context or availability of its own.
    assert!(
        capabilities["routes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|route| route["id"] != "Auto")
    );
    let auto = &capabilities["auto"];
    assert_eq!(auto["id"], "Auto");
    assert_eq!(auto["router_resolved"], true);
    assert_eq!(
        auto["routes"],
        json!([
            "Agentic",
            "Forced",
            "Coder",
            "Reasoning",
            "LongContext",
            "Completion",
            "General"
        ])
    );
    assert!(auto.get("features").is_none() && auto.get("context_length").is_none());
    // The rule names are the operator's; a client is not shown them.
    assert!(!auto.to_string().contains("large-context"));
    no_alias_in(&capabilities.to_string());
}

#[tokio::test]
async fn auto_is_an_unknown_model_unless_it_is_turned_on() {
    ensure_provider();
    let fleet = Fleet::start().await;

    // No section at all: exactly as before R8.
    let router = Router::start(fleet.config()).await;
    let (status, body) = router.send(hi("Auto")).await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "model_not_found");
    let models = router.get("/v1/models").await;
    assert!(
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["id"] != "Auto")
    );
    assert!(router.get("/v1/capabilities").await.get("auto").is_none());
    drop(router);

    // A section that is present and off: checked, but not served.
    let mut config = with_auto(fleet.config(), standard_rules(), "General");
    config["auto_route"]["enabled"] = json!(false);
    let router = Router::start(config).await;
    let (status, body) = router.send(hi("auto")).await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "model_not_found");
    let models = router.get("/v1/models").await;
    assert!(
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["id"] != "Auto")
    );
    assert_eq!(router.get("/api/router/v1/auto").await["enabled"], false);
    assert_eq!(fleet.hits(), [0; 7], "nothing was routed anywhere");
}

#[tokio::test]
async fn without_an_auto_section_a_route_called_auto_is_an_ordinary_route() {
    ensure_provider();
    let node = Node::start("LegacyAlias").await;
    let router = Router::start(topology(&[("Auto", "priority", &[("n", &node)])])).await;
    let (status, body) = router.send(hi("Auto")).await;
    assert_eq!(status, 200);
    assert_eq!(body["model"], "Auto");
    assert_eq!(node.hits(), 1);
    let trace = router.last_trace().await;
    assert_eq!(trace["route"], "Auto");
    assert!(trace.get("auto_rule").is_none());
}

// --- rules ----------------------------------------------------------------------

#[tokio::test]
async fn each_trait_reaches_its_route_and_the_response_names_that_route() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(with_auto(fleet.config(), standard_rules(), "General")).await;

    let long_text = "word ".repeat(1_400); // 7 000 bytes: at least 1 167 tokens.
    let cases: [(&str, &str, Value, &str, usize); 7] = [
        ("ordinary", "/v1/chat/completions", hi("Auto"), "General", 0),
        (
            "tools",
            "/v1/chat/completions",
            with("Auto", json!({"tools": tools()})),
            "Coder",
            1,
        ),
        (
            "reasoning",
            "/v1/chat/completions",
            with("Auto", json!({"reasoning_effort": "high"})),
            "Reasoning",
            2,
        ),
        (
            "long",
            "/v1/chat/completions",
            json!({"model": "Auto", "messages": [{"role": "user", "content": long_text}]}),
            "LongContext",
            3,
        ),
        (
            "completion",
            "/v1/completions",
            json!({"model": "Auto", "prompt": "Once upon a time"}),
            "Completion",
            4,
        ),
        (
            "compound",
            "/v1/chat/completions",
            with(
                "Auto",
                json!({"tools": tools(), "reasoning_effort": "high"}),
            ),
            "Agentic",
            5,
        ),
        (
            "tool_choice",
            "/v1/chat/completions",
            with("Auto", json!({"tools": tools(), "tool_choice": "required"})),
            "Forced",
            6,
        ),
    ];
    for (what, path, body, route, node) in cases {
        let before = fleet.hits();
        let (status, answer) = router.send_to(path, body, None).await;
        assert_eq!(status, 200, "{what}: {answer}");
        assert_eq!(reached(before, fleet.hits()), [node], "{what}");
        assert_eq!(
            answer["model"], route,
            "{what}: the response names the chosen route"
        );
        no_alias_in(&answer.to_string());
        let trace = router.last_trace().await;
        assert_eq!(trace["requested_route"], "Auto", "{what}");
        assert_eq!(trace["route"], route, "{what}");
    }

    // The node was sent its own alias, never `Auto` or the route.
    assert_eq!(fleet.coder.seen()[0]["model"], "CoderAlias");
    // `reasoning_effort: none` turns thinking off; it does not ask for it.
    let before = fleet.hits();
    let (status, _) = router
        .send(with("Auto", json!({"reasoning_effort": "none"})))
        .await;
    assert_eq!(status, 200);
    assert_eq!(reached(before, fleet.hits()), [0]);
    // An empty tool list declares no tools.
    let before = fleet.hits();
    router.send(with("Auto", json!({"tools": []}))).await;
    assert_eq!(reached(before, fleet.hits()), [0]);
}

#[tokio::test]
async fn the_first_matching_rule_wins_and_reordering_changes_the_answer() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let both = with(
        "Auto",
        json!({"tools": tools(), "reasoning_effort": "high"}),
    );

    let ordered = json!([
        {"name": "agentic", "when": {"requires_tools": true, "requires_reasoning": true}, "route": "Agentic"},
        {"name": "tools", "when": {"requires_tools": true}, "route": "Coder"},
        {"name": "reasoning", "when": {"requires_reasoning": true}, "route": "Reasoning"}
    ]);
    let router = Router::start(with_auto(fleet.config(), ordered, "General")).await;
    let (_, body) = router.send(both.clone()).await;
    assert_eq!(body["model"], "Agentic");
    assert_eq!(router.last_trace().await["auto_rule"], "agentic");
    drop(router);

    let reordered = json!([
        {"name": "reasoning", "when": {"requires_reasoning": true}, "route": "Reasoning"},
        {"name": "agentic", "when": {"requires_tools": true, "requires_reasoning": true}, "route": "Agentic"},
        {"name": "tools", "when": {"requires_tools": true}, "route": "Coder"}
    ]);
    let router = Router::start(with_auto(fleet.config(), reordered, "General")).await;
    let (_, body) = router.send(both).await;
    assert_eq!(body["model"], "Reasoning");
    assert_eq!(router.last_trace().await["auto_rule"], "reasoning");
}

#[tokio::test]
async fn a_false_condition_requires_the_trait_to_be_absent() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let rules = json!([
        {"name": "plain-chat", "when": {"endpoint": "chat", "requires_tools": false, "requires_reasoning": false},
         "route": "Coder"}
    ]);
    let router = Router::start(with_auto(fleet.config(), rules, "General")).await;
    let (_, body) = router.send(hi("Auto")).await;
    assert_eq!(body["model"], "Coder", "no tools and no reasoning: matches");
    let (_, body) = router.send(with("Auto", json!({"tools": tools()}))).await;
    assert_eq!(body["model"], "General", "tools declared: does not match");
    let (_, body) = router
        .send(with("Auto", json!({"reasoning_effort": "low"})))
        .await;
    assert_eq!(
        body["model"], "General",
        "reasoning asked for: does not match"
    );
    let (_, body) = router
        .send_to(
            "/v1/completions",
            json!({"model": "Auto", "prompt": "x"}),
            None,
        )
        .await;
    assert_eq!(body["model"], "General", "not a chat: does not match");
}

#[tokio::test]
async fn a_request_the_gateway_would_refuse_is_refused_before_any_route_is_chosen() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(with_auto(fleet.config(), standard_rules(), "General")).await;
    // `required` with no tools is the gateway's own 400, whichever route.
    let (status, body) = router
        .send(with("Auto", json!({"tool_choice": "required"})))
        .await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_tool_choice");
    assert_eq!(fleet.hits(), [0; 7]);
}

// --- the route's own pipeline, unchanged -------------------------------------------

#[tokio::test]
async fn the_chosen_route_unavailable_is_that_routes_error_and_no_other_route_is_tried() {
    ensure_provider();
    let general = Node::start("GeneralAlias").await;
    let rules = json!([{"name": "tools", "when": {"requires_tools": true}, "route": "Coder"}]);
    // Coder's only node is not running at all.
    let config = json!({
        "nodes": [{"id": "general", "url": general.base}, {"id": "coder", "url": "http://127.0.0.1:9"}],
        "routes": [
            {"name": "General", "deployments": [{"node": "general", "model": "GeneralAlias"}]},
            {"name": "Coder", "deployments": [{"node": "coder", "model": "CoderAlias"}]}
        ],
    });
    let router = Router::start(with_auto(config, rules, "General")).await;
    let started = std::time::Instant::now();
    let (status, body) = router.send(with("Auto", json!({"tools": tools()}))).await;
    assert_eq!(status, 503);
    assert_eq!(body["error"]["code"], "route_unavailable");
    assert!(body["error"]["message"].as_str().unwrap().contains("Coder"));
    assert!(started.elapsed() < Duration::from_secs(2), "no waiting");
    assert_eq!(
        general.hits(),
        0,
        "General was never tried in Coder's place"
    );
    let trace = router.last_trace().await;
    assert_eq!(trace["requested_route"], "Auto");
    assert_eq!(trace["route"], "Coder");
    assert_eq!(trace["auto_rule"], "tools");
    assert_eq!(trace["outcome"], "unavailable");
}

#[tokio::test]
async fn capability_filtering_still_applies_inside_the_chosen_route() {
    ensure_provider();
    let general = Node::start("GeneralAlias").await;
    let no_tools = Caps {
        tools: false,
        ..FULL
    };
    let coder_a = Node::with("CoderA", no_tools, Act::Answer).await;
    let coder_b = Node::start("CoderB").await;
    let rules = json!([{"name": "tools", "when": {"requires_tools": true}, "route": "Coder"}]);
    let config = topology(&[
        ("General", "priority", &[("general", &general)]),
        ("Coder", "priority", &[("a", &coder_a), ("b", &coder_b)]),
    ]);
    let router = Router::start(with_auto(config, rules, "General")).await;

    // Coder's first deployment cannot take tools, so the filter passes it by.
    let (status, body) = router.send(with("Auto", json!({"tools": tools()}))).await;
    assert_eq!(status, 200);
    assert_eq!(body["model"], "Coder");
    assert_eq!((coder_a.hits(), coder_b.hits()), (0, 1));
    let trace = router.last_trace().await;
    assert_eq!(trace["unfit"][0]["deployment"], "a/CoderA");
    assert_eq!(trace["final_deployment"], "b/CoderB");
    drop(router);

    // With no Coder deployment able to, it is Coder's mismatch, not General's
    // answer.
    let config = topology(&[
        ("General", "priority", &[("general", &general)]),
        ("Coder", "priority", &[("a", &coder_a)]),
    ]);
    let rules = json!([{"name": "tools", "when": {"requires_tools": true}, "route": "Coder"}]);
    let router = Router::start(with_auto(config, rules, "General")).await;
    let (status, body) = router.send(with("Auto", json!({"tools": tools()}))).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "route_capability_mismatch");
    assert!(body["error"]["message"].as_str().unwrap().contains("Coder"));
    assert_eq!(general.hits(), 0);
}

#[tokio::test]
async fn context_overflow_failover_stays_inside_the_chosen_route() {
    ensure_provider();
    let general = Node::start("GeneralAlias").await;
    let small = Node::with(
        "LongSmall",
        Caps {
            context_length: 2048,
            ..FULL
        },
        Act::Refuse(
            400,
            json!({"error": {"message": "the prompt is too long", "type": "invalid_request_error",
                   "param": "messages", "code": "context_length_exceeded"}}),
        ),
    )
    .await;
    let large = Node::with(
        "LongLarge",
        Caps {
            context_length: 32_768,
            ..FULL
        },
        Act::Answer,
    )
    .await;
    let rules =
        json!([{"name": "big", "when": {"min_prompt_tokens": 100}, "route": "LongContext"}]);
    let config = topology(&[
        ("General", "priority", &[("general", &general)]),
        (
            "LongContext",
            "priority",
            &[("small", &small), ("large", &large)],
        ),
    ]);
    let router = Router::start(with_auto(config, rules, "General")).await;
    // 1 200 bytes: an estimate of 200, which fits 2048 — only the node's own
    // count finds it too long.
    let body =
        json!({"model": "Auto", "messages": [{"role": "user", "content": "x".repeat(1_200)}]});
    let (status, answer) = router.send(body).await;
    assert_eq!(status, 200);
    assert_eq!(answer["model"], "LongContext");
    assert_eq!((small.hits(), large.hits(), general.hits()), (1, 1, 0));
    let trace = router.last_trace().await;
    assert_eq!(trace["route"], "LongContext");
    assert_eq!(trace["attempts"][1]["reason"], "context_overflow_failover");
    no_alias_in(&answer.to_string());
    assert!(!answer.to_string().contains("LongLarge"));
}

#[tokio::test]
async fn priority_round_robin_and_least_busy_act_only_within_the_chosen_route() {
    ensure_provider();
    let general_a = Node::start("GeneralA").await;
    let general_b = Node::start("GeneralB").await;
    let coder_a = Node::start("CoderA").await;
    let coder_b = Node::start("CoderB").await;
    let reason_a = Node::start("ReasonA").await;
    let reason_b = Node::start("ReasonB").await;
    let rules = json!([
        {"name": "tools", "when": {"requires_tools": true}, "route": "Coder"},
        {"name": "reasoning", "when": {"requires_reasoning": true}, "route": "Reasoning"}
    ]);
    let config = topology(&[
        (
            "General",
            "round_robin",
            &[("ga", &general_a), ("gb", &general_b)],
        ),
        ("Coder", "priority", &[("ca", &coder_a), ("cb", &coder_b)]),
        (
            "Reasoning",
            "least_busy",
            &[("ra", &reason_a), ("rb", &reason_b)],
        ),
    ]);
    let router = Router::start(with_auto(config, rules, "General")).await;
    let tool_request = with("Auto", json!({"tools": tools()}));

    // Interleaved: General's ring advances only on General's requests, and
    // Coder's priority always picks its first deployment.
    for _ in 0..3 {
        router.send(hi("Auto")).await;
        router.send(tool_request.clone()).await;
    }
    assert_eq!((general_a.hits(), general_b.hits()), (2, 1));
    assert_eq!((coder_a.hits(), coder_b.hits()), (3, 0));
    router.send(hi("Auto")).await;
    assert_eq!((general_a.hits(), general_b.hits()), (2, 2));
    let decisions = |route: &str, reason: &str| router.state.metrics.decisions(route, reason);
    assert_eq!(decisions("General", "round_robin"), 4);
    assert_eq!(decisions("Coder", "primary_healthy"), 3);

    // Least-busy compares Reasoning's deployments only: General's and Coder's
    // in-flight work is invisible to it.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    reason_a.act(Act::Hold(Arc::clone(&gate)));
    let held = {
        let base = router.base.clone();
        tokio::spawn(async move {
            client()
                .post(format!("{base}/v1/chat/completions"))
                .json(&with("Auto", json!({"reasoning_effort": "high"})))
                .send()
                .await
                .unwrap()
                .status()
        })
    };
    for _ in 0..100 {
        if reason_a.hits() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(reason_a.hits(), 1);
    let (status, body) = router
        .send(with("Auto", json!({"reasoning_effort": "high"})))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["model"], "Reasoning");
    assert_eq!(reason_b.hits(), 1, "the idle Reasoning deployment took it");
    gate.add_permits(10);
    assert_eq!(held.await.unwrap(), 200);
    assert_eq!(router.last_trace().await["policy"], "least_busy");
}

#[tokio::test]
async fn affinity_is_kept_per_resolved_route_and_never_under_auto() {
    ensure_provider();
    let general_a = Node::start("GeneralA").await;
    let general_b = Node::start("GeneralB").await;
    let coder_a = Node::start("CoderA").await;
    let coder_b = Node::start("CoderB").await;
    let rules = json!([{"name": "tools", "when": {"requires_tools": true}, "route": "Coder"}]);
    let mut config = with_auto(
        topology(&[
            (
                "General",
                "round_robin",
                &[("ga", &general_a), ("gb", &general_b)],
            ),
            (
                "Coder",
                "round_robin",
                &[("ca", &coder_a), ("cb", &coder_b)],
            ),
        ]),
        rules,
        "General",
    );
    config["session_affinity"] = json!({"enabled": true});
    let router = Router::start(config).await;
    let chat = "/v1/chat/completions";
    let tool_request = with("Auto", json!({"tools": tools()}));

    // Move both rings off their first deployment, without a session.
    router.send(hi("Auto")).await;
    router.send(tool_request.clone()).await;
    assert_eq!((general_a.hits(), coder_a.hits()), (1, 1));

    // session-1, an ordinary chat: General, and it settles on gb.
    let (_, first) = router.send_to(chat, hi("Auto"), Some("session-1")).await;
    assert_eq!(first["model"], "General");
    assert_eq!(general_b.hits(), 1);
    // The same session with tools: Coder, settling on cb. Not held to General.
    let (_, second) = router
        .send_to(chat, tool_request.clone(), Some("session-1"))
        .await;
    assert_eq!(second["model"], "Coder");
    assert_eq!(coder_b.hits(), 1);

    // Each route keeps its own stickiness for the session.
    for _ in 0..3 {
        router.send_to(chat, hi("Auto"), Some("session-1")).await;
        router
            .send_to(chat, tool_request.clone(), Some("session-1"))
            .await;
    }
    assert_eq!((general_a.hits(), general_b.hits()), (1, 4));
    assert_eq!((coder_a.hits(), coder_b.hits()), (1, 4));

    // And it is the same affinity a direct request for the route uses.
    router.send_to(chat, hi("Coder"), Some("session-1")).await;
    assert_eq!(coder_b.hits(), 5);

    let sessions = router.get("/api/router/v1/sessions").await;
    let routes: Vec<&str> = sessions["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["route"].as_str().unwrap())
        .collect();
    assert_eq!(sessions["active"], 2, "{sessions}");
    assert!(routes.contains(&"General") && routes.contains(&"Coder"));
    assert!(!routes.contains(&"Auto"));
}

// --- identity ---------------------------------------------------------------------

#[tokio::test]
async fn every_stream_chunk_and_tool_answer_names_the_chosen_route() {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(with_auto(fleet.config(), standard_rules(), "General")).await;

    for (body, route) in [
        (with("Auto", json!({"stream": true})), "General"),
        (
            with(
                "Auto",
                json!({"stream": true, "stream_options": {"include_usage": true}, "tools": tools()}),
            ),
            "Coder",
        ),
    ] {
        let response = router.post("/v1/chat/completions", body, None).await;
        assert_eq!(response.status(), 200);
        let bytes = response.bytes().await.unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        no_alias_in(&text);
        let mut decoder = SseDecoder::new();
        decoder.feed(&bytes).unwrap();
        let events = decoder.drain();
        assert!(events.last().unwrap().is_done());
        let chunks: Vec<Value> = events
            .iter()
            .filter(|event| !event.is_done())
            .map(|event| serde_json::from_str(&event.data).unwrap())
            .collect();
        assert_eq!(chunks.len(), 3, "role, delta and usage");
        for chunk in &chunks {
            assert_eq!(chunk["model"], route, "{chunk}");
        }
    }

    // A tool-call answer, not streamed.
    let (status, body) = router.send(with("Auto", json!({"tools": tools()}))).await;
    assert_eq!(status, 200);
    assert_eq!(body["model"], "Coder");
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    no_alias_in(&body.to_string());
}

// --- observability ----------------------------------------------------------------

#[tokio::test]
async fn traces_metrics_and_the_admin_view_separate_the_route_decision_from_the_deployment_decision()
 {
    ensure_provider();
    let fleet = Fleet::start().await;
    let router = Router::start(with_auto(fleet.config(), standard_rules(), "General")).await;

    router.send(with("Auto", json!({"tools": tools()}))).await;
    let trace = router.last_trace().await;
    assert_eq!(trace["requested_route"], "Auto");
    assert_eq!(trace["route"], "Coder");
    assert_eq!(trace["auto_rule"], "tools");
    assert!(trace.get("auto_fallback").is_none());
    // The deployment decision, inside the route, as before.
    assert_eq!(trace["policy"], "priority");
    assert_eq!(trace["selected"], "coder/CoderAlias");
    assert_eq!(trace["selection_reason"], "explicit_single_deployment");
    assert_eq!(trace["final_deployment"], "coder/CoderAlias");
    // No prompt in a trace.
    assert!(!trace.to_string().contains("\"hi\""));

    router.send(hi("Auto")).await;
    let trace = router.last_trace().await;
    assert_eq!(trace["route"], "General");
    assert_eq!(trace["auto_fallback"], true);
    assert!(trace.get("auto_rule").is_none());

    // A direct request: requested and resolved are the same route.
    router.send(hi("coder")).await;
    let trace = router.last_trace().await;
    assert_eq!(trace["requested_route"], "Coder");
    assert_eq!(trace["route"], "Coder");
    assert!(trace.get("auto_rule").is_none() && trace.get("auto_fallback").is_none());

    let metrics = router.metrics().await;
    assert!(
        metrics.contains("router_auto_route_decisions_total{rule=\"tools\",route=\"Coder\"} 1"),
        "{metrics}"
    );
    assert!(
        metrics
            .contains("router_auto_route_decisions_total{rule=\"_fallback\",route=\"General\"} 1")
    );
    assert!(metrics.contains("router_auto_route_fallback_total{route=\"General\"} 1"));
    // The route's own counters carry the resolved route, never Auto.
    assert!(metrics.contains("router_requests_total{route=\"Coder\",outcome=\"ok\"} 2"));
    assert!(!metrics.contains("route=\"Auto\""));

    let admin = router.get("/api/router/v1/auto").await;
    assert_eq!(admin["enabled"], true);
    assert_eq!(admin["name"], "Auto");
    assert_eq!(admin["fallback_route"], "General");
    assert_eq!(admin["fallback_decisions"], 1);
    let rules = admin["rules"].as_array().unwrap();
    let names: Vec<&str> = rules
        .iter()
        .map(|rule| rule["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "agentic",
            "forced-tool",
            "tools",
            "reasoning",
            "large-context",
            "completion"
        ]
    );
    assert_eq!(rules[0]["position"], 1);
    assert_eq!(
        rules[0]["condition"],
        "requires_tools=true AND requires_reasoning=true"
    );
    assert_eq!(
        rules[0]["when"],
        json!({"requires_tools": true, "requires_reasoning": true})
    );
    assert_eq!(rules[2]["route"], "Coder");
    assert_eq!(rules[2]["decisions"], 1);
    assert_eq!(rules[0]["decisions"], 0);
}
