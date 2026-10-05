//! Placement, end to end: the controller keeps routes at their targets through
//! the nodes' own control API, and the request path never waits for it.
//!
//! Two kinds of node stand behind the router here:
//!
//! * **Real Lightweight gateways** with a model manager, over the mock engine,
//!   holding one installed fixture model. These prove what only a real node
//!   can: that the router's load request is the one the node accepts, that the
//!   node's own admission control decides (a 64 MiB machine refuses), and that
//!   a loaded model becomes routable.
//! * **Scripted nodes** speaking the same control API, whose loads take as long
//!   as a test says, fail as a test says, or never come. These prove the
//!   timing rules — a request never waits for a load — and the failure paths.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use lightweight_api::capabilities::{CapabilitiesBody, CapabilityModel};
use lightweight_backend_mock::MockBackend;
use lightweight_catalog::CatalogStore;
use lightweight_catalog::install::Installer;
use lightweight_core::units::Bytes;
use lightweight_gateway::manager::{ModelManager, RuntimeDefaults};
use lightweight_gateway::{GatewayConfig, GatewayState};
use lightweight_gguf::fixture::{GgufBuilder, TempDir};
use lightweight_router::RouterState;
use lightweight_router::config::RouterFile;
use lightweight_router::domain::NodeId;
use lightweight_system_info::FixedMemoryProbe;
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

async fn poll<T>(what: &str, seconds: u64, mut check: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if let Some(value) = check().await {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

// --- real gateways ------------------------------------------------------------

const FIXTURE: &str = "placement-fixture-q4_k_m.gguf";

/// A real gateway with one installed model and nothing loaded, on a machine
/// with `available_mib` free.
struct RealNode {
    _dir: TempDir,
    base: String,
    _stop: CancellationToken,
    /// The model's catalog id.
    model: String,
}

/// The alias each real node serves its model under, and the name the route's
/// deployments use: what a node's `/v1/models` advertises.
const ALIAS: &str = "QwenCoder";

impl RealNode {
    async fn start(tag: &str, available_mib: u64) -> Self {
        let dir = TempDir::new(tag);
        let path = dir.write(FIXTURE, &GgufBuilder::small_model("llama").build());
        let manager = Arc::new(ModelManager::new(
            CatalogStore::open(dir.path().join("catalog.json")).expect("catalog"),
            Installer::new(dir.path().join("models"), dir.path().join("downloads"))
                .expect("installer"),
            RuntimeDefaults::default(),
        ));
        let model = manager
            .register_at_startup(path)
            .await
            .expect("register the fixture")
            .id;
        let state = Arc::new(
            GatewayState::new(
                Arc::new(MockBackend::default()),
                lightweight_gateway::catalog::shared(None),
                GatewayConfig {
                    paths: Some(lightweight_system_info::DataPaths::rooted_at(dir.path())),
                    ..GatewayConfig::default()
                },
            )
            .with_manager(manager)
            .with_memory_probe(Arc::new(FixedMemoryProbe::with_available(
                Bytes::from_gib(64),
                Bytes::from_mib(available_mib),
            ))),
        );
        let (base, stop) = serve(lightweight_gateway::app(state)).await;
        let named = client()
            .patch(format!("{base}/api/v1/models/{model}"))
            .json(&json!({"alias": ALIAS}))
            .send()
            .await
            .expect("alias");
        assert!(named.status().is_success());
        Self {
            _dir: dir,
            base,
            _stop: stop,
            model,
        }
    }

    /// Load the model directly, as an operator would, and wait for the job.
    async fn load_now(&self) {
        let accepted: Value = client()
            .post(format!("{}/api/v1/models/{}/load", self.base, self.model))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let job = accepted["job"].as_u64().expect("a job");
        poll("the operator's load", 20, async || {
            let body: Value = client()
                .get(format!("{}/api/v1/jobs/{job}", self.base))
                .send()
                .await
                .ok()?
                .json()
                .await
                .ok()?;
            (body["status"]["state"] == "succeeded").then_some(())
        })
        .await;
    }

    /// Every load job the node has run.
    async fn load_jobs(&self) -> usize {
        let body: Value = client()
            .get(format!("{}/api/v1/jobs", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        body["data"].as_array().map_or(0, |jobs| {
            jobs.iter().filter(|job| job["kind"] == "load").count()
        })
    }
}

// --- scripted control-plane nodes --------------------------------------------

#[derive(Clone)]
struct Script {
    /// The model it has installed, and the name it serves it under.
    model: String,
    installed: bool,
    tools: bool,
    load_delay: Duration,
    /// When set, every load job fails with this error code.
    load_fails: Option<&'static str>,
    healthy: Arc<AtomicBool>,
    serving: Arc<Mutex<Option<String>>>,
    load_calls: Arc<AtomicU32>,
    catalog_reads: Arc<AtomicU32>,
    hits: Arc<AtomicU32>,
    control_auth: Arc<Mutex<Vec<Option<String>>>>,
    jobs: Arc<Mutex<BTreeMap<u64, Value>>>,
    next_job: Arc<AtomicU64>,
}

struct ScriptedNode {
    base: String,
    _stop: CancellationToken,
    script: Script,
}

#[derive(Clone, Copy)]
struct Setup {
    serving: bool,
    installed: bool,
    tools: bool,
    load_delay_ms: u64,
    load_fails: Option<&'static str>,
}

const EMPTY: Setup = Setup {
    serving: false,
    installed: true,
    tools: true,
    load_delay_ms: 50,
    load_fails: None,
};

const SERVING: Setup = Setup {
    serving: true,
    ..EMPTY
};

impl ScriptedNode {
    async fn start(model: &str, setup: Setup) -> Self {
        let script = Script {
            model: model.to_owned(),
            installed: setup.installed,
            tools: setup.tools,
            load_delay: Duration::from_millis(setup.load_delay_ms),
            load_fails: setup.load_fails,
            healthy: Arc::new(AtomicBool::new(true)),
            serving: Arc::new(Mutex::new(setup.serving.then(|| model.to_owned()))),
            load_calls: Arc::default(),
            catalog_reads: Arc::default(),
            hits: Arc::default(),
            control_auth: Arc::default(),
            jobs: Arc::default(),
            next_job: Arc::new(AtomicU64::new(1)),
        };
        let app = axum::Router::new()
            .route("/v1/capabilities", get(capabilities))
            .route("/v1/chat/completions", post(chat))
            .route("/api/v1/models", get(models))
            .route("/api/v1/models/{id}/load", post(load))
            .route("/api/v1/jobs/{id}", get(job))
            .with_state(script.clone());
        let (base, stop) = serve(app).await;
        Self {
            base,
            _stop: stop,
            script,
        }
    }

    fn loads(&self) -> u32 {
        self.script.load_calls.load(Ordering::SeqCst)
    }

    fn catalog_reads(&self) -> u32 {
        self.script.catalog_reads.load(Ordering::SeqCst)
    }

    fn hits(&self) -> u32 {
        self.script.hits.load(Ordering::SeqCst)
    }

    fn set_healthy(&self, healthy: bool) {
        self.script.healthy.store(healthy, Ordering::SeqCst);
    }

    /// The node lost its model, as a restarted gateway does.
    fn empty(&self) {
        *self.script.serving.lock().unwrap() = None;
    }
}

async fn capabilities(State(script): State<Script>) -> Response {
    if !script.healthy.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let serving = script.serving.lock().unwrap().clone();
    let mut body = CapabilitiesBody::new(
        "0.5.0",
        serving.map(|id| CapabilityModel {
            id,
            context_length: 4096,
        }),
        1,
    );
    body.features.tools = script.tools;
    axum::Json(body).into_response()
}

async fn chat(State(script): State<Script>, body: axum::body::Bytes) -> Response {
    script.hits.fetch_add(1, Ordering::SeqCst);
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    axum::Json(json!({
        "id": "c1", "object": "chat.completion", "model": body["model"],
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"},
                     "finish_reason": "stop"}],
    }))
    .into_response()
}

async fn models(State(script): State<Script>) -> Response {
    script.catalog_reads.fetch_add(1, Ordering::SeqCst);
    let loaded = script.serving.lock().unwrap().as_deref() == Some(script.model.as_str());
    let rows = if script.installed {
        vec![json!({
            "id": format!("{}-q4_k_m", script.model.to_lowercase()),
            "alias": script.model,
            "state": if loaded { "loaded" } else { "available" },
        })]
    } else {
        vec![json!({"id": "something-else", "alias": null, "state": "available"})]
    };
    axum::Json(json!({"object": "list", "data": rows})).into_response()
}

async fn load(
    State(script): State<Script>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    script.load_calls.fetch_add(1, Ordering::SeqCst);
    script.control_auth.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    );
    assert!(id.ends_with("-q4_k_m"), "loaded by catalog id, not {id}");
    let job = script.next_job.fetch_add(1, Ordering::SeqCst);
    script
        .jobs
        .lock()
        .unwrap()
        .insert(job, json!({"state": "running", "stage": {"of": "queued"}}));
    let background = script.clone();
    tokio::spawn(async move {
        tokio::time::sleep(background.load_delay).await;
        let status = match background.load_fails {
            Some(code) => json!({"state": "failed", "error": {"code": code, "kind": "unavailable",
                                  "message": "refused"}}),
            None => {
                *background.serving.lock().unwrap() = Some(background.model.clone());
                json!({"state": "succeeded", "model": id})
            }
        };
        background.jobs.lock().unwrap().insert(job, status);
    });
    (StatusCode::ACCEPTED, axum::Json(json!({"job": job}))).into_response()
}

async fn job(State(script): State<Script>, Path(id): Path<u64>) -> Response {
    match script.jobs.lock().unwrap().get(&id) {
        Some(status) => {
            axum::Json(json!({"id": id, "kind": "load", "status": status})).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// --- the router -----------------------------------------------------------------

struct Router {
    base: String,
    state: Arc<RouterState>,
    stop: CancellationToken,
    serving: Option<tokio::task::JoinHandle<Result<(), String>>>,
}

impl Drop for Router {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Router {
    async fn start(mut config: Value, env: &[(&str, &str)]) -> Self {
        config["listen"] = json!(["127.0.0.1:0"]);
        config["health"] =
            json!({"interval_secs": 3600, "timeout_secs": 2, "failure_threshold": 1});
        if config.get("placement").is_none() {
            // Passes run when a test asks; the interval is only a fallback.
            config["placement"] = json!({"interval_secs": 3600, "load_timeout_secs": 30,
                                         "backoff_secs": 3600, "backoff_max_secs": 3600});
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
        let serving = Some(tokio::spawn(bound.serve(stop.clone())));
        poll("the router", 10, async || {
            client()
                .get(format!("{base}/health"))
                .send()
                .await
                .ok()
                .map(|_| ())
        })
        .await;
        Self {
            base,
            state,
            stop,
            serving,
        }
    }

    async fn probe(&self) {
        self.state.probe_now().await;
    }

    async fn reconcile(&self) -> u16 {
        client()
            .post(format!("{}/api/router/v1/placement/reconcile", self.base))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn placement(&self) -> Value {
        client()
            .get(format!("{}/api/router/v1/placement", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// The `Coder` route's row in the placement view.
    async fn coder(&self) -> Value {
        self.placement().await["routes"][0].clone()
    }

    /// Reconcile until the route reports this many ready deployments.
    async fn until_ready(&self, ready: u64) -> Value {
        poll(&format!("{ready} ready deployments"), 20, async || {
            self.reconcile().await;
            let route = self.coder().await;
            (route["ready"] == ready).then_some(route)
        })
        .await
    }

    async fn chat(&self, body: Value) -> reqwest::Response {
        client()
            .post(format!("{}/v1/chat/completions", self.base))
            .json(&body)
            .send()
            .await
            .unwrap()
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
}

fn hi() -> Value {
    json!({"model": "Coder", "messages": [{"role": "user", "content": "hi"}]})
}

/// One `Coder` route over `nodes` (id, url, model), with a placement target.
fn coder(strategy: &str, nodes: &[(&str, &str, &str)], placement: Option<Value>) -> Value {
    let mut route = json!({
        "name": "Coder",
        "strategy": strategy,
        "deployments": nodes.iter().map(|(id, _, model)| json!({"node": id, "model": model}))
            .collect::<Vec<_>>(),
    });
    if let Some(placement) = placement {
        route["placement"] = placement;
    }
    json!({
        "default_route": "Coder",
        "nodes": nodes.iter().map(|(id, url, _)| json!({"id": id, "url": url})).collect::<Vec<_>>(),
        "routes": [route],
    })
}

/// Wait until the controller has finished every load it started.
async fn settled(router: &Router) {
    poll("loads to finish", 20, async || {
        router.state.placement.loading().is_empty().then_some(())
    })
    .await;
}

// --- tests ------------------------------------------------------------------------

#[tokio::test]
async fn an_installed_model_becomes_a_warm_standby_and_is_never_loaded_twice() {
    ensure_provider();
    let a = RealNode::start("placement-a", 32_768).await;
    a.load_now().await;
    let b = RealNode::start("placement-b", 32_768).await;
    let router = Router::start(
        coder(
            "priority",
            &[("a", &a.base, ALIAS), ("b", &b.base, ALIAS)],
            Some(json!({"min_ready": 1, "warm_standby": 1, "allowed_nodes": ["a", "b"]})),
        ),
        &[],
    )
    .await;

    // a is ready; b is installed but unloaded: the controller loads b, ahead
    // of any request needing it.
    let route = router.until_ready(2).await;
    assert_eq!(route["status"], "satisfied");
    assert_eq!(route["ready_standby"], 1);
    assert_eq!(route["deployments"][1]["state"], "ready");
    assert_eq!(
        route["deployments"][1]["last_result"]["result"],
        "succeeded"
    );
    assert_eq!(a.load_jobs().await, 1, "only the operator's own load");
    assert_eq!(b.load_jobs().await, 1);

    // More passes change nothing: what is ready is not loaded again.
    for _ in 0..3 {
        assert_eq!(router.reconcile().await, 202);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    settled(&router).await;
    assert_eq!((a.load_jobs().await, b.load_jobs().await), (1, 1));
    let metrics = &router.state.metrics;
    assert_eq!(metrics.placement_actions("Coder", "load", "succeeded"), 1);
    assert!(metrics.reconcile_passes() >= 2);

    // The standby is an ordinary deployment: routable, and priority still
    // sends traffic to a first.
    assert_eq!(router.chat(hi()).await.status(), 200);
    let text = router.metrics().await;
    assert!(text.contains("router_placement_ready_deployments{route=\"Coder\"} 2"));
    assert!(text.contains("router_placement_target_deployments{route=\"Coder\"} 2"));
    assert!(text.contains(
        "router_placement_actions_total{route=\"Coder\",action=\"load\",result=\"succeeded\"} 1"
    ));
}

#[tokio::test]
async fn the_nodes_admission_control_decides_and_a_refusal_backs_off() {
    ensure_provider();
    // 64 MiB free: the node's own estimate refuses any engine.
    let small = RealNode::start("placement-small", 64).await;
    let router = Router::start(
        coder(
            "priority",
            &[("small", &small.base, ALIAS)],
            Some(json!({"allowed_nodes": ["small"]})),
        ),
        &[],
    )
    .await;

    let route = poll("the refusal", 20, async || {
        router.reconcile().await;
        let route = router.coder().await;
        route["deployments"][0]["last_result"]["result"]
            .as_str()
            .is_some_and(|result| result == "failed")
            .then_some(route)
    })
    .await;
    let deployment = &route["deployments"][0];
    assert_eq!(deployment["last_result"]["reason"], "admission_failed");
    assert_eq!(deployment["last_result"]["code"], "insufficient_memory");
    assert_eq!(deployment["consecutive_failures"], 1);
    assert!(deployment["retry_in_secs"].as_u64().unwrap() > 0);
    assert_eq!(route["status"], "below_min");

    // Backing off: further passes do not ask again.
    for _ in 0..3 {
        router.reconcile().await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(small.load_jobs().await, 1);
    let metrics = &router.state.metrics;
    assert_eq!(metrics.placement_failures("Coder", "admission_failed"), 1);
    assert_eq!(metrics.placement_actions("Coder", "load", "failed"), 1);

    // Nothing is ready, and a request is told so at once.
    let response = router.chat(hi()).await;
    assert_eq!(response.status(), 503);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "route_unavailable");
}

#[tokio::test]
async fn a_request_never_waits_for_a_load_in_progress() {
    ensure_provider();
    let a = ScriptedNode::start(
        "QwenCoder",
        Setup {
            load_delay_ms: 2_000,
            ..EMPTY
        },
    )
    .await;
    let router = Router::start(
        coder(
            "priority",
            &[("a", &a.base, "QwenCoder")],
            Some(json!({"allowed_nodes": ["a"]})),
        ),
        &[],
    )
    .await;
    router.reconcile().await;
    poll("the load to start", 10, async || {
        (router.coder().await["deployments"][0]["state"] == "loading").then_some(())
    })
    .await;

    // While a is loading: refused immediately, never held.
    let started = Instant::now();
    let response = router.chat(hi()).await;
    assert_eq!(response.status(), 503);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "route_unavailable");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the request waited {:?}",
        started.elapsed()
    );
    assert_eq!(a.hits(), 0);

    // Once the controller has seen it ready, it serves.
    router.until_ready(1).await;
    assert_eq!(router.chat(hi()).await.status(), 200);
    assert_eq!(a.loads(), 1);
}

#[tokio::test]
async fn a_lost_deployment_fails_over_at_once_and_the_standby_is_restored() {
    ensure_provider();
    let a = ScriptedNode::start("QwenCoder", SERVING).await;
    let b = ScriptedNode::start("QwenCoder", SERVING).await;
    let c = ScriptedNode::start("QwenCoder", EMPTY).await;
    let router = Router::start(
        coder(
            "priority",
            &[
                ("a", &a.base, "QwenCoder"),
                ("b", &b.base, "QwenCoder"),
                ("c", &c.base, "QwenCoder"),
            ],
            Some(json!({"min_ready": 1, "warm_standby": 1, "allowed_nodes": ["a", "b", "c"]})),
        ),
        &[],
    )
    .await;
    let route = router.until_ready(2).await;
    assert_eq!(route["status"], "satisfied");
    assert_eq!(c.loads(), 0, "already at target: c is left alone");

    // a dies. The very next request goes to b; nothing waits for placement.
    a.set_healthy(false);
    router.probe().await;
    let response = router.chat(hi()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(b.hits(), 1);

    // The controller notices one ready short of target and loads c.
    let route = router.until_ready(2).await;
    assert_eq!(c.loads(), 1);
    assert_eq!(route["deployments"][0]["state"], "unavailable");
    assert_eq!(route["deployments"][2]["state"], "ready");
}

#[tokio::test]
async fn a_model_the_node_has_not_installed_is_never_loaded() {
    ensure_provider();
    let a = ScriptedNode::start(
        "QwenCoder",
        Setup {
            installed: false,
            ..EMPTY
        },
    )
    .await;
    let router = Router::start(
        coder(
            "priority",
            &[("a", &a.base, "QwenCoder")],
            Some(json!({"allowed_nodes": ["a"]})),
        ),
        &[],
    )
    .await;
    let route = poll("the failure", 10, async || {
        router.reconcile().await;
        let route = router.coder().await;
        route["deployments"][0]["last_result"]
            .is_object()
            .then_some(route)
    })
    .await;
    assert_eq!(
        route["deployments"][0]["last_result"]["reason"],
        "model_not_installed"
    );
    assert_eq!(a.loads(), 0, "nothing downloaded, nothing loaded");
    assert_eq!(
        router
            .state
            .metrics
            .placement_failures("Coder", "model_not_installed"),
        1
    );
}

#[tokio::test]
async fn an_unhealthy_or_occupied_node_gets_no_control_request() {
    ensure_provider();
    let down = ScriptedNode::start("QwenCoder", EMPTY).await;
    down.set_healthy(false);
    // Serving another model: a single-model node is never swapped.
    let busy = ScriptedNode::start("Other", SERVING).await;
    let router = Router::start(
        coder(
            "priority",
            &[
                ("down", &down.base, "QwenCoder"),
                ("busy", &busy.base, "QwenCoder"),
            ],
            Some(json!({"min_ready": 1, "warm_standby": 1, "allowed_nodes": ["down", "busy"]})),
        ),
        &[],
    )
    .await;
    for _ in 0..3 {
        router.reconcile().await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!((down.catalog_reads(), down.loads()), (0, 0));
    assert_eq!((busy.catalog_reads(), busy.loads()), (0, 0));
    let route = router.coder().await;
    assert_eq!(route["deployments"][0]["state"], "unavailable");
    assert_eq!(route["deployments"][1]["state"], "occupied");
    assert_eq!(route["status"], "below_min");
}

#[tokio::test]
async fn the_nodes_own_credential_is_used_and_the_routers_never_is() {
    ensure_provider();
    let a = ScriptedNode::start("QwenCoder", EMPTY).await;
    let mut config = coder(
        "priority",
        &[("a", &a.base, "QwenCoder")],
        Some(json!({"allowed_nodes": ["a"]})),
    );
    config["nodes"][0]["api_key_env"] = json!("NODE_A_KEY");
    let router = Router::start(config, &[("NODE_A_KEY", "node-a-secret")]).await;
    router.until_ready(1).await;
    assert_eq!(
        a.script.control_auth.lock().unwrap().as_slice(),
        [Some("Bearer node-a-secret".to_owned())]
    );
    let view = router.placement().await.to_string();
    assert!(!view.contains("node-a-secret"));
}

#[tokio::test]
async fn session_affinity_does_not_jump_back_to_a_restored_deployment() {
    ensure_provider();
    let a = ScriptedNode::start("QwenCoder", SERVING).await;
    let b = ScriptedNode::start("QwenCoder", SERVING).await;
    let mut config = coder(
        "priority",
        &[("a", &a.base, "QwenCoder"), ("b", &b.base, "QwenCoder")],
        Some(json!({"min_ready": 1, "warm_standby": 1, "allowed_nodes": ["a", "b"]})),
    );
    config["session_affinity"] = json!({"enabled": true});
    let router = Router::start(config, &[]).await;
    router.until_ready(2).await;
    let turn = async |router: &Router| {
        let before = (a.hits(), b.hits());
        let response = client()
            .post(format!("{}/v1/chat/completions", router.base))
            .header("X-Lightweight-Session", "conversation-1")
            .json(&hi())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        if a.hits() > before.0 { "a" } else { "b" }
    };
    assert_eq!(turn(&router).await, "a");

    // a goes down: the session moves to b.
    a.set_healthy(false);
    router.probe().await;
    assert_eq!(turn(&router).await, "b");

    // a comes back empty; placement loads it again, ahead of demand.
    a.empty();
    a.set_healthy(true);
    router.probe().await;
    router.until_ready(2).await;
    assert_eq!(a.loads(), 1);
    // The session stays where it moved.
    for _ in 0..3 {
        assert_eq!(turn(&router).await, "b");
    }
}

#[tokio::test]
async fn placement_leaves_policies_and_capability_filtering_as_they_were() {
    ensure_provider();
    let a = ScriptedNode::start("QwenCoder", SERVING).await;
    let b = ScriptedNode::start(
        "QwenCoder",
        Setup {
            tools: false,
            ..EMPTY
        },
    )
    .await;
    let router = Router::start(
        coder(
            "round_robin",
            &[("a", &a.base, "QwenCoder"), ("b", &b.base, "QwenCoder")],
            Some(json!({"min_ready": 1, "warm_standby": 1, "allowed_nodes": ["a", "b"]})),
        ),
        &[],
    )
    .await;
    router.until_ready(2).await;

    // Both ready: round-robin takes turns over them as over any deployments.
    for _ in 0..4 {
        assert_eq!(router.chat(hi()).await.status(), 200);
    }
    assert_eq!((a.hits(), b.hits()), (2, 2));

    // b was loaded by placement, but it has no tools: a tool request never
    // reaches it.
    let tools = json!({"model": "Coder", "messages": [{"role": "user", "content": "hi"}],
                       "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}]});
    for _ in 0..3 {
        assert_eq!(router.chat(tools.clone()).await.status(), 200);
    }
    assert_eq!((a.hits(), b.hits()), (5, 2));
}

#[tokio::test]
async fn without_a_placement_target_nothing_is_controlled() {
    ensure_provider();
    let a = ScriptedNode::start("QwenCoder", EMPTY).await;
    let mut config = coder("priority", &[("a", &a.base, "QwenCoder")], None);
    // A short interval that would act at once, if anything ran.
    config["placement"] = json!({"interval_secs": 1});
    let router = Router::start(config, &[]).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!((a.catalog_reads(), a.loads()), (0, 0));
    let view = router.placement().await;
    assert_eq!(view["enabled"], false);
    assert_eq!(view["routes"], json!([]));
    assert_eq!(router.reconcile().await, 409);
    assert!(
        !router
            .metrics()
            .await
            .contains("router_placement_ready_deployments")
    );
}

#[tokio::test]
async fn stopping_the_router_stops_the_controller_mid_load() {
    ensure_provider();
    let a = ScriptedNode::start(
        "QwenCoder",
        Setup {
            load_delay_ms: 60_000,
            ..EMPTY
        },
    )
    .await;
    let mut router = Router::start(
        coder(
            "priority",
            &[("a", &a.base, "QwenCoder")],
            Some(json!({"allowed_nodes": ["a"]})),
        ),
        &[],
    )
    .await;
    router.reconcile().await;
    poll("the load to start", 10, async || {
        (!router.state.placement.loading().is_empty()).then_some(())
    })
    .await;

    router.stop.cancel();
    let serving = router.serving.take().unwrap();
    let finished = tokio::time::timeout(Duration::from_secs(5), serving).await;
    assert!(finished.is_ok(), "the router did not stop within 5 s");
    assert!(
        router.state.placement.loading().is_empty(),
        "the abandoned load is no longer marked in progress"
    );
    // The health book is the router's; nothing else about the node changed.
    assert!(
        router
            .state
            .health
            .status(&NodeId::parse("a").unwrap())
            .served()
            .is_none()
    );
}
