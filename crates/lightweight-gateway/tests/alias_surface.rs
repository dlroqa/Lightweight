//! Model aliases, end to end, through the gateway a client actually talks to.
//!
//! The alias is a name the user chose in the catalog; what matters is what a
//! client sees because of it. So every assertion here goes over a real socket:
//! the control API sets the name, `/v1/models` must then list it, a completion
//! that names it must reach that model and be answered under it, and the
//! canonical id and `default` must keep working beside it throughout.
//!
//! Two fixture models are installed so that "reached the right model" is a
//! question with a wrong answer available.

use std::sync::Arc;

use lightweight_backend_mock::MockBackend;
use lightweight_catalog::CatalogStore;
use lightweight_catalog::install::Installer;
use lightweight_core::units::Bytes;
use lightweight_gateway::manager::{ModelManager, RuntimeDefaults};
use lightweight_gateway::{GatewayConfig, GatewayState};
use lightweight_gguf::fixture::{GgufBuilder, TempDir};
use lightweight_system_info::FixedMemoryProbe;
use serde_json::{Value, json};

const CODER_FILE: &str = "Qwen3.5-9B-Fable-5-v1-Q8_0.gguf";
const FAST_FILE: &str = "LFM2.5-1.2B-Instruct-Q6_K_XL.gguf";

fn ensure_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Two installed models, nothing loaded, on a machine with room for either.
struct Fixture {
    dir: TempDir,
    server: Server,
    /// Catalog ids, as the installer derived them from the file names.
    coder: String,
    fast: String,
}

async fn fixture(tag: &str) -> Fixture {
    let dir = TempDir::new(tag);
    let coder_path = dir.write(CODER_FILE, &GgufBuilder::small_model("llama").build());
    // Different bytes, so the installer does not recognise it as the first.
    let fast_path = dir.write(
        FAST_FILE,
        &GgufBuilder::small_model("llama")
            .kv("general.name", "second fixture")
            .build(),
    );

    let manager = Arc::new(manager_for(&dir));
    let coder = manager
        .register_at_startup(coder_path)
        .await
        .expect("register the first fixture")
        .id;
    let fast = manager
        .register_at_startup(fast_path)
        .await
        .expect("register the second fixture")
        .id;
    assert_ne!(coder, fast);

    let server = Server::start(gateway_over(&dir, manager)).await;
    Fixture {
        dir,
        server,
        coder,
        fast,
    }
}

fn manager_for(dir: &TempDir) -> ModelManager {
    ModelManager::new(
        CatalogStore::open(dir.path().join("catalog.json")).expect("catalog"),
        Installer::new(dir.path().join("models"), dir.path().join("downloads")).expect("installer"),
        RuntimeDefaults::default(),
    )
}

fn gateway_over(dir: &TempDir, manager: Arc<ModelManager>) -> Arc<GatewayState> {
    Arc::new(
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
            Bytes::from_gib(32),
        ))),
    )
}

struct Server {
    base: String,
    _task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(state: Arc<GatewayState>) -> Self {
        let app = lightweight_gateway::app(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        Self {
            base: format!("http://127.0.0.1:{port}"),
            _task: tokio::spawn(async move {
                let _ = axum::serve(listener, lightweight_gateway::service(app)).await;
            }),
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> (u16, Value) {
        let response = request.send().await.expect("request");
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.send(reqwest::Client::new().get(self.url(path))).await
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Client::new().post(self.url(path)).json(&body))
            .await
    }

    async fn patch(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Client::new().patch(self.url(path)).json(&body))
            .await
    }

    async fn delete(&self, path: &str) -> (u16, Value) {
        self.send(reqwest::Client::new().delete(self.url(path)))
            .await
    }

    async fn alias(&self, model: &str, alias: Option<&str>) -> (u16, Value) {
        self.patch(
            &format!("/api/v1/models/{model}"),
            json!({ "alias": alias }),
        )
        .await
    }

    /// Start a job and wait for it to settle, returning its final status.
    async fn settle(&self, accepted: (u16, Value)) -> Value {
        let (status, accepted) = accepted;
        assert_eq!(status, 202, "{accepted}");
        let job = accepted["job"].as_u64().expect("a job id");
        for _ in 0..400 {
            let (_, described) = self.get(&format!("/api/v1/jobs/{job}")).await;
            if described["status"]["state"] != "running" {
                return described["status"].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("the job never settled");
    }

    async fn load(&self, model: &str) -> Value {
        let accepted = self
            .post(&format!("/api/v1/models/{model}/load"), json!({}))
            .await;
        self.settle(accepted).await
    }

    async fn listed(&self) -> Vec<String> {
        let (status, body) = self.get("/v1/models").await;
        assert_eq!(status, 200, "{body}");
        body["data"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|row| row["id"].as_str().expect("an id").to_owned())
            .collect()
    }

    /// A chat completion naming `model`, or with no `model` field at all.
    async fn chat(&self, model: Option<&str>) -> (u16, Value) {
        let mut body = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        if let Some(model) = model {
            body["model"] = json!(model);
        }
        self.post("/v1/chat/completions", body).await
    }

    /// The catalog row for one model, by canonical id.
    async fn row(&self, id: &str) -> Value {
        let (status, body) = self.get("/api/v1/models").await;
        assert_eq!(status, 200, "{body}");
        body["data"]
            .as_array()
            .expect("a list")
            .iter()
            .find(|row| row["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("{id} is not in the catalog: {body}"))
    }
}

#[tokio::test]
async fn a_model_with_no_alias_is_listed_and_served_exactly_as_before() {
    ensure_provider();
    let f = fixture("legacy").await;

    // Unaliased rows say so rather than leaving the field out.
    assert_eq!(f.server.row(&f.coder).await["alias"], Value::Null);

    assert_eq!(f.server.load(&f.coder).await["state"], "succeeded");
    let listed = f.server.listed().await;
    assert_eq!(listed.len(), 1);
    assert!(
        listed[0].starts_with(&format!("{}@", f.coder)),
        "the canonical id, context suffix and all: {listed:?}"
    );

    let (status, body) = f.server.chat(Some(&listed[0])).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["model"], listed[0]);
}

#[tokio::test]
async fn an_alias_is_what_a_client_discovers_sends_and_is_answered_as() {
    ensure_provider();
    let f = fixture("discover").await;

    let (status, row) = f.server.alias(&f.coder, Some("Coder")).await;
    assert_eq!(status, 200, "{row}");
    // The control API keeps both identities in view.
    assert_eq!(row["id"], f.coder.as_str());
    assert_eq!(row["alias"], "Coder");

    // Loaded by its alias: the control API resolves it like any other name.
    assert_eq!(f.server.load("Coder").await["state"], "succeeded");
    assert_eq!(f.server.row(&f.coder).await["state"], "loaded");

    // Discovery offers the alias and nothing else.
    assert_eq!(f.server.listed().await, vec!["Coder".to_owned()]);
    let (_, capabilities) = f.server.get("/v1/capabilities").await;
    assert_eq!(
        capabilities["state"]["model"]["id"], "Coder",
        "{capabilities}"
    );

    // The Lightagent loop: send back what was discovered, get it back.
    let (status, body) = f.server.chat(Some("Coder")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["model"], "Coder");
    let text = body.to_string();
    assert!(
        !text.contains(&f.coder),
        "a client asking for Coder must not be shown the file-derived id: {text}"
    );
}

#[tokio::test]
async fn the_alias_the_canonical_id_and_default_all_reach_the_same_model() {
    ensure_provider();
    let f = fixture("selectors").await;
    f.server.alias(&f.coder, Some("Coder")).await;
    assert_eq!(f.server.load(&f.coder).await["state"], "succeeded");

    for named in [
        Some("Coder"),
        Some("coder"),
        Some(f.coder.as_str()),
        Some("default"),
        None,
    ] {
        let (status, body) = f.server.chat(named).await;
        assert_eq!(status, 200, "{named:?}: {body}");
        assert_eq!(body["model"], "Coder", "{named:?}");
    }

    // The other model's name does not reach this one.
    let (status, body) = f.server.chat(Some(&f.fast)).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"]["code"], "model_not_found");
    let (status, _) = f.server.chat(Some("Research")).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn text_completions_resolve_the_alias_too() {
    ensure_provider();
    let f = fixture("completions").await;
    f.server.alias(&f.coder, Some("Coder")).await;
    assert_eq!(f.server.load("Coder").await["state"], "succeeded");

    for named in ["Coder", f.coder.as_str(), "default"] {
        let (status, body) = f
            .server
            .post(
                "/v1/completions",
                json!({ "model": named, "prompt": "def fibonacci(", "max_tokens": 8 }),
            )
            .await;
        assert_eq!(status, 200, "{named}: {body}");
        assert_eq!(body["model"], "Coder", "{named}");
    }
}

#[tokio::test]
async fn a_streamed_reply_names_the_alias_on_every_chunk() {
    ensure_provider();
    let f = fixture("stream").await;
    f.server.alias(&f.coder, Some("Coder")).await;
    assert_eq!(f.server.load("Coder").await["state"], "succeeded");

    let text = reqwest::Client::new()
        .post(f.server.url("/v1/chat/completions"))
        .json(&json!({
            "model": "Coder",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true,
            "stream_options": { "include_usage": true }
        }))
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body");

    let chunks: Vec<Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("a JSON chunk"))
        .collect();
    assert!(chunks.len() >= 2, "{text}");
    assert!(
        chunks.iter().all(|chunk| chunk["model"] == "Coder"),
        "every chunk names the alias: {text}"
    );
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
}

#[tokio::test]
async fn renaming_takes_effect_at_once_without_a_reload() {
    ensure_provider();
    let f = fixture("rename").await;
    f.server.alias(&f.coder, Some("Coder")).await;
    assert_eq!(f.server.load("Coder").await["state"], "succeeded");
    let (_, before) = f.server.get("/api/v1/gateway").await;

    // Renamed by its current alias, which resolves like the id would.
    let (status, row) = f.server.alias("Coder", Some("Programming")).await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(row["alias"], "Programming");
    assert_eq!(row["state"], "loaded", "still the resident model");

    assert_eq!(f.server.listed().await, vec!["Programming".to_owned()]);
    assert_eq!(f.server.chat(Some("Programming")).await.0, 200);
    assert_eq!(
        f.server.chat(Some("Coder")).await.0,
        404,
        "no alias history"
    );
    assert_eq!(f.server.chat(Some(&f.coder)).await.0, 200);

    // Nothing about the engine moved.
    let (_, after) = f.server.get("/api/v1/gateway").await;
    assert_eq!(
        before["model"], after["model"],
        "the engine was not reloaded"
    );

    // Cleared, the canonical id is what is listed again.
    let (status, row) = f.server.alias(&f.coder, None).await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(row["alias"], Value::Null);
    assert!(f.server.listed().await[0].starts_with(&f.coder));
}

#[tokio::test]
async fn switching_models_switches_the_name_with_no_stale_alias_left_behind() {
    ensure_provider();
    let f = fixture("switch").await;
    f.server.alias(&f.fast, Some("Fast")).await;
    f.server.alias(&f.coder, Some("Coder")).await;

    assert_eq!(f.server.load("Fast").await["state"], "succeeded");
    assert_eq!(f.server.listed().await, vec!["Fast".to_owned()]);
    assert_eq!(f.server.chat(Some("Fast")).await.1["model"], "Fast");
    assert_eq!(f.server.chat(Some("Coder")).await.0, 404);

    assert_eq!(f.server.load("Coder").await["state"], "succeeded");
    assert_eq!(f.server.listed().await, vec!["Coder".to_owned()]);
    assert_eq!(f.server.chat(Some("Coder")).await.1["model"], "Coder");
    assert_eq!(f.server.chat(Some("default")).await.1["model"], "Coder");
    assert_eq!(
        f.server.chat(Some("Fast")).await.0,
        404,
        "the old model's name"
    );
    assert_eq!(f.server.row(&f.coder).await["state"], "loaded");
    assert_eq!(f.server.row(&f.fast).await["state"], "available");
}

#[tokio::test]
async fn a_duplicate_or_reserved_alias_is_refused_and_nothing_changes() {
    ensure_provider();
    let f = fixture("refuse").await;
    assert_eq!(f.server.alias(&f.coder, Some("Coder")).await.0, 200);

    for clash in ["Coder", "coder", "CODER"] {
        let (status, body) = f.server.alias(&f.fast, Some(clash)).await;
        assert_eq!(status, 400, "{clash}: {body}");
        assert_eq!(body["error"]["code"], "alias_in_use", "{body}");
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("already assigned"), "{message}");
    }
    for bad in ["default", "../model", "foo/bar", "   ", ""] {
        let (status, body) = f.server.alias(&f.fast, Some(bad)).await;
        assert_eq!(status, 400, "{bad:?}: {body}");
        assert_eq!(body["error"]["code"], "invalid_alias", "{body}");
    }
    assert_eq!(f.server.row(&f.fast).await["alias"], Value::Null);
    assert_eq!(f.server.row(&f.coder).await["alias"], "Coder");
}

#[tokio::test]
async fn a_patch_that_changes_nothing_says_so() {
    ensure_provider();
    let f = fixture("patch-shape").await;
    let path = format!("/api/v1/models/{}", f.coder);

    let (status, body) = f.server.patch(&path, json!({})).await;
    assert_eq!(
        (status, body["error"]["param"].clone()),
        (400, json!("alias"))
    );
    let (status, body) = f.server.patch(&path, json!({ "name": "x" })).await;
    assert_eq!(
        (status, body["error"]["param"].clone()),
        (400, json!("name"))
    );
    let (status, _) = f.server.patch(&path, json!({ "alias": 7 })).await;
    assert_eq!(status, 400);
    let (status, body) = f.server.alias("no-such-model", Some("Coder")).await;
    assert_eq!(status, 404, "{body}");
}

#[tokio::test]
async fn removing_a_model_retires_its_alias() {
    ensure_provider();
    let f = fixture("remove").await;
    f.server.alias(&f.fast, Some("Fast")).await;

    let (status, body) = f.server.delete("/api/v1/models/Fast").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["removed"], f.fast.as_str());

    let (status, _) = f.server.get("/api/v1/models/Fast").await;
    assert_eq!(status, 404);
    assert_eq!(f.server.load("Fast").await["state"], "failed");
    // The name is free again.
    assert_eq!(f.server.alias(&f.coder, Some("Fast")).await.0, 200);
}

#[tokio::test]
async fn a_loaded_model_cannot_be_removed_by_its_alias_either() {
    ensure_provider();
    let f = fixture("in-use").await;
    f.server.alias(&f.coder, Some("Coder")).await;
    assert_eq!(f.server.load("Coder").await["state"], "succeeded");
    let (status, body) = f.server.delete("/api/v1/models/Coder").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "model_in_use");
}

#[tokio::test]
async fn an_alias_survives_a_restart_of_the_gateway() {
    ensure_provider();
    let f = fixture("restart").await;
    assert_eq!(f.server.alias(&f.coder, Some("Coder")).await.0, 200);
    drop(f.server);

    // A new manager over the same catalog file: what a restart is.
    let reopened = Server::start(gateway_over(&f.dir, Arc::new(manager_for(&f.dir)))).await;
    assert_eq!(reopened.row(&f.coder).await["alias"], "Coder");
    let (status, detail) = reopened.get("/api/v1/models/Coder").await;
    assert_eq!(status, 200, "{detail}");
    assert_eq!(detail["id"], f.coder.as_str());
    assert_eq!(reopened.load("Coder").await["state"], "succeeded");
    assert_eq!(reopened.listed().await, vec!["Coder".to_owned()]);
}

#[tokio::test]
async fn an_import_can_name_the_model_and_a_taken_name_is_refused_up_front() {
    ensure_provider();
    let f = fixture("import").await;
    f.server.alias(&f.coder, Some("Coder")).await;

    let third = f.dir.write(
        "Llama-3.2-3B-Instruct-Q4_K_M.gguf",
        &GgufBuilder::small_model("llama")
            .kv("general.name", "third fixture")
            .build(),
    );

    // Refused before any hashing starts: a 400, not a job.
    let (status, body) = f
        .server
        .post(
            "/api/v1/models/import",
            json!({ "path": third, "alias": "coder" }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "alias_in_use");

    let accepted = f
        .server
        .post(
            "/api/v1/models/import",
            json!({ "path": third, "alias": "Research" }),
        )
        .await;
    let settled = f.server.settle(accepted).await;
    assert_eq!(settled["state"], "succeeded", "{settled}");
    let id = settled["model"]
        .as_str()
        .expect("the canonical id")
        .to_owned();
    assert_eq!(f.server.row(&id).await["alias"], "Research");
    assert_eq!(f.server.load("Research").await["state"], "succeeded");
    assert_eq!(f.server.listed().await, vec!["Research".to_owned()]);
}
