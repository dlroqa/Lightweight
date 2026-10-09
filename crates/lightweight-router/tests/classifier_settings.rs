//! Saving the classifier's provider settings and the Jev key from the panel
//! (`/api/router/v1/classifier/settings`, `/api/router/v1/classifier/key`).
//!
//! Every test runs a real bound router over a real `router.json` in a
//! temporary directory, with an in-memory credential store standing in for
//! the operating system's. What is proved: the key reaches the store and
//! nowhere else, an empty key changes nothing, only `auto_route.classifier`
//! changes, every bad input and every unauthorized or cross-origin write is
//! refused with the file untouched, a failed write puts the previous key back,
//! a stale revision cannot overwrite, the saved settings are what the next
//! start loads, and Test Connection keeps checking the running settings until
//! that start.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::http::HeaderMap;
use lightweight_router::admin::{AdminAccess, TOKEN_HEADER};
use lightweight_router::classifier_settings::{ClassifierSettings, Env, backup_path};
use lightweight_router::config::RouterFile;
use lightweight_router::secret_store::{MemoryStore, SecretStore, jev_account};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Short on purpose: the secrets gate refuses credential-shaped literals of 16
/// or more characters.
const OLD_KEY: &str = "jev-old-k1";
const NEW_KEY: &str = "jev-new-k2";
const ENV_KEY: &str = "jev-env-k3";
const CLIENT_KEY: &str = "settings-cl";
const ACCOUNT_VAR: &str = "TYPESAFE_API_KEY";

fn ensure_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Every log line any test in this binary produces. This file is its own test
/// binary, so a global subscriber sees only these tests.
fn captured() -> &'static Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&buffer);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || Capture(Arc::clone(&writer)))
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        buffer
    })
}

struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Headers a test sets on a request, by name.
type Headers = Vec<(&'static str, String)>;

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client")
}

mod tempdir {
    use std::path::{Path, PathBuf};

    pub struct Dir(PathBuf);

    impl Dir {
        pub fn new(name: &str) -> Self {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).expect("random");
            let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            let path = std::env::temp_dir().join(format!("lw-router-{name}-{suffix}"));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// A scripted TypeSafe `GET /v1/models`: counts the calls it receives and
/// whether each carried `expected` as its bearer key — never recording a key.
struct Typesafe {
    url: String,
    calls: Arc<AtomicUsize>,
    with_expected_key: Arc<AtomicUsize>,
}

async fn typesafe(expected: &'static str) -> Typesafe {
    let calls = Arc::new(AtomicUsize::new(0));
    let matched = Arc::new(AtomicUsize::new(0));
    let (counted, keyed) = (Arc::clone(&calls), Arc::clone(&matched));
    let app = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(move |headers: HeaderMap| {
            let (counted, keyed) = (Arc::clone(&counted), Arc::clone(&keyed));
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                let presented = headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.strip_prefix("Bearer "));
                if presented == Some(expected) {
                    keyed.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({"models": [{"name": "jev-latest"}]})).into_response()
                } else {
                    axum::http::StatusCode::UNAUTHORIZED.into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind typesafe");
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    Typesafe {
        url,
        calls,
        with_expected_key: matched,
    }
}

use axum::response::IntoResponse as _;

fn config(classifier: Value) -> Value {
    json!({
        "listen": ["127.0.0.1:0"],
        "nodes": [{"id": "a", "url": "http://127.0.0.1:9"}],
        "health": {"interval_secs": 3600, "timeout_secs": 1, "failure_threshold": 1},
        "routes": [
            {"name": "General", "description": "Everyday conversation", "deployments": [{"node": "a", "model": "G"}]},
            {"name": "Coder", "description": "Programming and debugging", "deployments": [{"node": "a", "model": "C"}]}
        ],
        "default_route": "General",
        "auto_route": {
            "enabled": true,
            "fallback_route": "General",
            "rules": [
                {"name": "tools", "when": {"requires_tools": true}, "route": "Coder"},
                {"name": "semantic", "when": {"requires_tools": false}, "classify": true}
            ],
            "classifier": classifier
        },
        "request": {"pre_commit_budget_ms": 30000}
    })
}

fn lightweight_classifier() -> Value {
    json!({
        "provider": "lightweight",
        "routes": ["General", "Coder"],
        "fallback_route": "General",
        "lightweight": {"route": "General", "timeout_ms": 20000}
    })
}

fn jev_classifier(base_url: &str) -> Value {
    json!({
        "provider": "jev",
        "routes": ["General", "Coder"],
        "fallback_route": "General",
        "jev": {
            "base_url": base_url,
            "model": "jev-latest",
            "timeout_ms": 5000,
            "include_user_text": false,
            "max_input_chars": 900
        }
    })
}

struct Router {
    _dir: tempdir::Dir,
    path: PathBuf,
    base: String,
    token: String,
    store: Arc<MemoryStore>,
    settings: Arc<lightweight_router::RouterState>,
    stop: CancellationToken,
}

impl Drop for Router {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn env_with(key_in_env: bool) -> Env {
    Arc::new(move |name: &str| match name {
        "LW_SETTINGS_CLIENT_KEY" => Some(CLIENT_KEY.to_owned()),
        ACCOUNT_VAR if key_in_env => Some(ENV_KEY.to_owned()),
        _ => None,
    })
}

struct Start {
    config: Value,
    store: Arc<MemoryStore>,
    key_in_env: bool,
    client_key: bool,
    listen: Option<&'static str>,
}

impl Start {
    /// A start whose store already holds the old key.
    fn with_key(config: Value) -> Self {
        let start = Self::new(config);
        start.store.set(&jev_account(ACCOUNT_VAR), OLD_KEY).unwrap();
        start
    }

    fn new(config: Value) -> Self {
        Self {
            config,
            store: Arc::new(MemoryStore::default()),
            key_in_env: false,
            client_key: false,
            listen: None,
        }
    }
}

impl Router {
    async fn start(start: Start) -> Self {
        let dir = tempdir::Dir::new("settings");
        let path = dir.path().join("router.json");
        let mut config = start.config;
        if start.client_key {
            config["api_key_env"] = json!("LW_SETTINGS_CLIENT_KEY");
        }
        if let Some(listen) = start.listen {
            config["listen"] = json!([listen]);
        }
        std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
        Self::start_from(dir, path, start.store, start.key_in_env).await
    }

    /// Start (or restart) over the file at `path`, as the CLI does.
    async fn start_from(
        dir: tempdir::Dir,
        path: PathBuf,
        store: Arc<MemoryStore>,
        key_in_env: bool,
    ) -> Self {
        ensure_provider();
        captured();
        let env = env_with(key_in_env);
        let text = std::fs::read_to_string(&path).unwrap();
        let file: RouterFile = serde_json::from_str(&text).expect("config shape");
        let stored = |var: &str| {
            store
                .get(&jev_account(var))
                .ok()
                .flatten()
                .map(|secret| secret.expose().to_owned())
        };
        let config = lightweight_router::config::validate_with_store(file, &|n| env(n), &stored)
            .expect("valid config");
        let bound = lightweight_router::bind(&config).await.expect("bind");
        let token = lightweight_router::admin::generate_token().unwrap();
        let admin = lightweight_router::admin::loopback_only(&bound.addresses())
            .and_then(|()| AdminAccess::new(&token, &bound.addresses()))
            .map_err(str::to_owned);
        let dyn_store: Arc<dyn SecretStore> = store.clone();
        let bound = bound.with_settings(ClassifierSettings::new(
            path.clone(),
            &text,
            dyn_store,
            env,
            admin,
        ));
        let address = bound.addresses()[0];
        let base = if address.ip().is_unspecified() {
            format!("http://127.0.0.1:{}", address.port())
        } else {
            format!("http://{address}")
        };
        let state = bound.state();
        let stop = CancellationToken::new();
        tokio::spawn(bound.serve(stop.clone()));
        for _ in 0..200 {
            if client().get(format!("{base}/health")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Self {
            _dir: dir,
            path,
            base,
            token,
            store,
            settings: state,
            stop,
        }
    }

    /// Stop this router and start a new one over the same file and store —
    /// what an operator's restart does.
    async fn restart(mut self, key_in_env: bool) -> Self {
        self.stop.cancel();
        let dir = std::mem::replace(&mut self._dir, tempdir::Dir::new("unused"));
        let (path, store) = (self.path.clone(), Arc::clone(&self.store));
        drop(self);
        Self::start_from(dir, path, store, key_in_env).await
    }

    fn origin(&self) -> String {
        self.base.clone()
    }

    fn file(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap()
    }

    fn file_json(&self) -> Value {
        serde_json::from_str(&self.file()).unwrap()
    }

    async fn view(&self, client_key: Option<&str>) -> (u16, Value) {
        let mut request = client().get(format!("{}/api/router/v1/classifier/settings", self.base));
        if let Some(key) = client_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn revision(&self) -> String {
        self.view(None).await.1["revision"]
            .as_str()
            .expect("revision")
            .to_owned()
    }

    fn admin_request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        client()
            .request(method, format!("{}{path}", self.base))
            .header("origin", self.origin())
            .header(TOKEN_HEADER, &self.token)
    }

    async fn put(&self, body: Value) -> (u16, Value, String) {
        let revision = self.revision().await;
        self.put_with(body, &revision, |request| request).await
    }

    async fn put_with(
        &self,
        body: Value,
        revision: &str,
        adjust: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> (u16, Value, String) {
        let request = self
            .admin_request(reqwest::Method::PUT, "/api/router/v1/classifier/settings")
            .header("content-type", "application/json")
            .header("if-match", format!("\"{revision}\""))
            .body(body.to_string());
        let response = adjust(request).send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::Null),
            text,
        )
    }

    async fn delete_key(&self) -> (u16, Value) {
        let revision = self.revision().await;
        let response = self
            .admin_request(reqwest::Method::DELETE, "/api/router/v1/classifier/key")
            .header("if-match", revision)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn check(&self) -> Value {
        client()
            .post(format!("{}/api/router/v1/classifier/check", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
}

fn jev_save(base_url: &str, key: Option<&str>) -> Value {
    let mut body = json!({
        "provider": "jev",
        "jev": {"base_url": base_url, "model": "jev-latest", "timeout_ms": 5000}
    });
    if let Some(key) = key {
        body["api_key"] = json!(key);
    }
    body
}

fn assert_no_key(text: &str, what: &str) {
    for key in [OLD_KEY, NEW_KEY, ENV_KEY] {
        assert!(!text.contains(key), "{what} carries a key: {text}");
    }
}

// --- reading ----------------------------------------------------------------------------

#[tokio::test]
async fn the_existing_jev_settings_are_read_from_the_file_with_the_key_as_a_status_only() {
    let store = Arc::new(MemoryStore::default());
    store.set(&jev_account(ACCOUNT_VAR), OLD_KEY).unwrap();
    let mut start = Start::new(config(jev_classifier("https://api.typesafe.ai")));
    start.store = store;
    let router = Router::start(start).await;

    let (status, view) = router.view(None).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["configured"], true);
    assert_eq!(view["saved"]["provider"], "jev");
    assert_eq!(view["saved"]["jev"]["base_url"], "https://api.typesafe.ai");
    assert_eq!(view["saved"]["jev"]["model"], "jev-latest");
    assert_eq!(view["saved"]["jev"]["api_key_env"], ACCOUNT_VAR);
    assert_eq!(view["saved"]["candidates"], json!(["General", "Coder"]));
    assert_eq!(view["active"], {
        let mut active = view["saved"].clone();
        active["key_source"] = json!("credential_store");
        active
    });
    assert_eq!(view["restart_required"], false);
    assert_eq!(view["key"]["source"], "credential_store");
    assert_eq!(view["key"]["stored"], true);
    assert_eq!(view["key"]["environment"], false);
    assert_eq!(view["admin"]["available"], true);
    assert_eq!(view["revision"].as_str().unwrap().len(), 64);
    assert_no_key(&view.to_string(), "the settings view");

    // The running classifier says the same through the existing admin view.
    let auto: Value = client()
        .get(format!("{}/api/router/v1/auto", router.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(auto["classifier"]["jev"]["api_key_configured"], true);
    assert_eq!(
        auto["classifier"]["jev"]["api_key_source"],
        "credential_store"
    );
    assert_no_key(&auto.to_string(), "the auto view");
}

#[tokio::test]
async fn the_environment_wins_over_the_store_as_before() {
    let store = Arc::new(MemoryStore::default());
    store.set(&jev_account(ACCOUNT_VAR), OLD_KEY).unwrap();
    let mut start = Start::new(config(jev_classifier("https://api.typesafe.ai")));
    start.store = store;
    start.key_in_env = true;
    let router = Router::start(start).await;
    let (_, view) = router.view(None).await;
    assert_eq!(view["key"]["source"], "environment");
    assert_eq!(view["active"]["key_source"], "environment");
}

#[tokio::test]
async fn a_missing_key_and_an_unavailable_store_are_reported_as_such() {
    let mut start = Start::new(config(lightweight_classifier()));
    start.store = Arc::new(MemoryStore::unavailable());
    let router = Router::start(start).await;
    let (_, view) = router.view(None).await;
    assert_eq!(view["key"]["source"], "missing");
    assert_eq!(view["key"]["stored"], Value::Null);
    assert_eq!(view["key"]["store"]["available"], false);
    assert!(view["saved"]["jev"].is_null());

    // Saving a key with no store refuses, and writes nothing.
    let before = router.file();
    let (status, body, _) = router
        .put(jev_save("https://api.typesafe.ai", Some(NEW_KEY)))
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "credential_store_unavailable");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("environment variable")
    );
    assert_eq!(router.file(), before);
}

#[tokio::test]
async fn the_read_view_keeps_the_routers_own_credential() {
    let mut start = Start::new(config(lightweight_classifier()));
    start.client_key = true;
    let router = Router::start(start).await;
    assert_eq!(router.view(None).await.0, 401);
    assert_eq!(router.view(Some(CLIENT_KEY)).await.0, 200);
}

// --- saving -------------------------------------------------------------------------------

#[tokio::test]
async fn switching_provider_changes_only_the_classifier_section_and_keeps_the_layout() {
    let router = Router::start(Start::new(config(lightweight_classifier()))).await;
    let before = router.file_json();
    let before_text = router.file();

    let (status, body, text) = router
        .put(jev_save("https://api.typesafe.ai", Some(NEW_KEY)))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["outcome"], "saved");
    assert_eq!(body["key_action"], "replaced");
    assert_eq!(body["restart_required"], true);
    assert_eq!(
        body["restart_reasons"],
        json!(["settings_changed", "key_changed"])
    );
    assert_no_key(&text, "the save response");

    let after = router.file_json();
    let classifier = &after["auto_route"]["classifier"];
    assert_eq!(classifier["provider"], "jev");
    assert_eq!(classifier["jev"]["base_url"], "https://api.typesafe.ai");
    assert_eq!(classifier["jev"]["model"], "jev-latest");
    // The Lightweight block stays as a standby, candidates and fallback too.
    assert_eq!(
        classifier["lightweight"],
        before["auto_route"]["classifier"]["lightweight"]
    );
    assert_eq!(
        classifier["routes"],
        before["auto_route"]["classifier"]["routes"]
    );
    assert_eq!(
        classifier["fallback_route"],
        before["auto_route"]["classifier"]["fallback_route"]
    );
    // Everything outside the section is exactly as it was.
    for field in [
        "listen",
        "nodes",
        "routes",
        "health",
        "default_route",
        "request",
    ] {
        assert_eq!(after[field], before[field], "{field}");
    }
    assert_eq!(after["auto_route"]["rules"], before["auto_route"]["rules"]);
    // In the order it was written: the top-level keys keep their sequence.
    let order = |text: &str| -> Vec<&'static str> {
        let mut keys: Vec<(usize, &'static str)> = [
            "\n  \"listen\"",
            "\n  \"nodes\"",
            "\n  \"health\"",
            "\n  \"routes\"",
            "\n  \"default_route\"",
            "\n  \"auto_route\"",
            "\n  \"request\"",
        ]
        .iter()
        .map(|key| (text.find(key).expect(key), *key))
        .collect();
        keys.sort_unstable();
        keys.into_iter().map(|(_, key)| key).collect()
    };
    assert_eq!(order(&before_text), order(&router.file()));
    // The key went to the store, and nowhere in the files.
    assert_eq!(
        router.store.peek(&jev_account(ACCOUNT_VAR)).as_deref(),
        Some(NEW_KEY)
    );
    assert_no_key(&router.file(), "router.json");
    let backup = std::fs::read_to_string(backup_path(&router.path)).unwrap();
    assert_eq!(
        backup, before_text,
        "the backup is the file before the save"
    );
    assert_no_key(&backup, "router.json.bak");
}

#[tokio::test]
async fn an_endpoint_edit_persists_and_unrelated_jev_fields_are_kept() {
    let router = Router::start(Start::with_key(config(jev_classifier(
        "https://api.typesafe.ai",
    ))))
    .await;
    router
        .store
        .set(&jev_account(ACCOUNT_VAR), OLD_KEY)
        .unwrap();
    let mut body = jev_save("https://gw.example.net/typesafe/", None);
    body["jev"]["min_confidence"] = json!(0.8);
    body["jev"]["timeout_ms"] = json!(7000);
    let (status, response, _) = router.put(body).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["key_action"], "unchanged");
    let jev = &router.file_json()["auto_route"]["classifier"]["jev"];
    assert_eq!(jev["base_url"], "https://gw.example.net/typesafe/");
    assert_eq!(jev["min_confidence"], 0.8);
    assert_eq!(jev["timeout_ms"], 7000);
    assert_eq!(jev["include_user_text"], false, "kept");
    assert_eq!(jev["max_input_chars"], 900, "kept");
    assert!(jev.get("api_key").is_none());
    assert_eq!(response["restart_reasons"], json!(["settings_changed"]));
}

#[tokio::test]
async fn an_empty_key_leaves_the_saved_key_unchanged() {
    let router = Router::start(Start::with_key(config(jev_classifier(
        "https://api.typesafe.ai",
    ))))
    .await;
    router
        .store
        .set(&jev_account(ACCOUNT_VAR), OLD_KEY)
        .unwrap();
    for key in [None, Some(""), Some("   ")] {
        let (status, body, _) = router.put(jev_save("https://api.typesafe.ai", key)).await;
        assert_eq!(status, 200, "{key:?}: {body}");
        assert_eq!(body["key_action"], "unchanged");
        assert_eq!(
            router.store.peek(&jev_account(ACCOUNT_VAR)).as_deref(),
            Some(OLD_KEY)
        );
    }
}

#[tokio::test]
async fn jev_without_any_key_is_refused_so_the_router_can_still_start() {
    let router = Router::start(Start::new(config(lightweight_classifier()))).await;
    let before = router.file();
    let (status, body, _) = router.put(jev_save("https://api.typesafe.ai", None)).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "api_key_missing");
    assert_eq!(router.file(), before);
}

#[tokio::test]
async fn invalid_endpoints_are_refused_and_nothing_is_written() {
    let router = Router::start(Start::with_key(config(jev_classifier(
        "https://api.typesafe.ai",
    ))))
    .await;
    router
        .store
        .set(&jev_account(ACCOUNT_VAR), OLD_KEY)
        .unwrap();
    let before = router.file();
    for base_url in [
        "http://api.typesafe.ai",
        "https://user:pw@api.typesafe.ai",
        "https://api.typesafe.ai?key=1",
        "https://api.typesafe.ai/#frag",
        "ftp://api.typesafe.ai",
        "not a url",
        "",
    ] {
        let (status, body, _) = router.put(jev_save(base_url, None)).await;
        assert_eq!(status, 422, "{base_url}: {body}");
        assert_eq!(body["error"]["code"], "invalid_settings", "{base_url}");
        assert_eq!(router.file(), before, "{base_url}");
    }
    // Loopback http stays allowed, as the router has always allowed it.
    let (status, body, _) = router.put(jev_save("http://127.0.0.1:9/", None)).await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn invalid_models_and_settings_are_refused() {
    let router = Router::start(Start::with_key(config(jev_classifier(
        "https://api.typesafe.ai",
    ))))
    .await;
    router
        .store
        .set(&jev_account(ACCOUNT_VAR), OLD_KEY)
        .unwrap();
    let before = router.file();
    let long_model = "m".repeat(129);
    let cases = [
        json!({"provider": "jev", "jev": {"base_url": "https://api.typesafe.ai", "model": ""}}),
        json!({"provider": "jev", "jev": {"base_url": "https://api.typesafe.ai", "model": long_model}}),
        json!({"provider": "jev", "jev": {"base_url": "https://api.typesafe.ai", "model": "a\u{7}b"}}),
        json!({"provider": "jev", "jev": {"base_url": "https://api.typesafe.ai", "model": "jev-latest", "timeout_ms": 0}}),
        json!({"provider": "jev", "jev": {"base_url": "https://api.typesafe.ai", "model": "jev-latest", "timeout_ms": 120001}}),
        json!({"provider": "jev", "jev": {"base_url": "https://api.typesafe.ai", "model": "jev-latest", "min_confidence": 1.5}}),
        json!({"provider": "jev"}),
        json!({"provider": "mixture", "jev": {"base_url": "https://api.typesafe.ai", "model": "jev-latest"}}),
        json!({"provider": "jev", "jev": {"base_url": "https://api.typesafe.ai", "model": "jev-latest", "api_key_env": "OTHER"}}),
        json!({"provider": "jev", "routes": ["Invented"], "jev": {"base_url": "https://api.typesafe.ai", "model": "jev-latest"}}),
        json!({"provider": "jev", "jev": {"base_url": "https://api.typesafe.ai", "model": "jev-latest"}, "api_key": "has space"}),
    ];
    for case in cases {
        let (status, body, text) = router.put(case.clone()).await;
        assert_eq!(status, 422, "{case}: {body}");
        assert_eq!(router.file(), before, "{case}");
        assert!(
            !text.contains("has space"),
            "a refused key is never echoed: {text}"
        );
    }
    // Switching to a provider that has no settings in the file is refused by
    // the router's own validation, not guessed at.
    let (status, body, _) = router.put(json!({"provider": "lightweight"})).await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["errors"]
            .as_array()
            .is_some_and(|errors| !errors.is_empty())
    );
    assert_eq!(router.file(), before);
}

#[tokio::test]
async fn a_file_without_a_classifier_section_is_never_given_invented_routes() {
    let mut file = config(lightweight_classifier());
    file["auto_route"]
        .as_object_mut()
        .unwrap()
        .remove("classifier");
    file["auto_route"]["rules"] =
        json!([{"name": "tools", "when": {"requires_tools": true}, "route": "Coder"}]);
    let router = Router::start(Start::new(file)).await;
    let (_, view) = router.view(None).await;
    assert_eq!(view["configured"], false);
    let before = router.file();
    let (status, body, _) = router
        .put(jev_save("https://api.typesafe.ai", Some(NEW_KEY)))
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "classifier_not_configured");
    assert_eq!(router.file(), before);
    assert!(router.store.peek(&jev_account(ACCOUNT_VAR)).is_none());
}

// --- who may write ---------------------------------------------------------------------

#[tokio::test]
async fn writes_without_the_admin_token_are_refused() {
    let mut start = Start::new(config(lightweight_classifier()));
    start.client_key = true;
    let router = Router::start(start).await;
    let before = router.file();
    let revision = {
        let (_, view) = router.view(Some(CLIENT_KEY)).await;
        view["revision"].as_str().unwrap().to_owned()
    };
    let body = jev_save("https://api.typesafe.ai", Some(NEW_KEY));
    for (what, adjust) in [
        (
            "no token",
            Box::new(|r: reqwest::RequestBuilder| {
                let (client, request) = r.build_split();
                let mut request = request.unwrap();
                request.headers_mut().remove(TOKEN_HEADER);
                reqwest::RequestBuilder::from_parts(client, request)
            }) as Box<dyn FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder>,
        ),
        (
            "wrong token",
            Box::new(|r: reqwest::RequestBuilder| {
                let (client, request) = r.build_split();
                let mut request = request.unwrap();
                request
                    .headers_mut()
                    .insert(TOKEN_HEADER, "not-the-token".parse().unwrap());
                reqwest::RequestBuilder::from_parts(client, request)
            }),
        ),
        (
            "the client key instead",
            Box::new(|r: reqwest::RequestBuilder| {
                let (client, request) = r.build_split();
                let mut request = request.unwrap();
                request.headers_mut().remove(TOKEN_HEADER);
                request.headers_mut().insert(
                    "authorization",
                    format!("Bearer {CLIENT_KEY}").parse().unwrap(),
                );
                reqwest::RequestBuilder::from_parts(client, request)
            }),
        ),
    ] {
        let (status, response, _) = router.put_with(body.clone(), &revision, adjust).await;
        assert_eq!(status, 401, "{what}: {response}");
        assert_eq!(router.file(), before, "{what}");
        assert!(
            router.store.peek(&jev_account(ACCOUNT_VAR)).is_none(),
            "{what}"
        );
    }
    let response = client()
        .delete(format!("{}/api/router/v1/classifier/key", router.base))
        .header("origin", router.origin())
        .header("if-match", &revision)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
}

#[tokio::test]
async fn cross_origin_rebinding_and_non_json_writes_are_refused() {
    let router = Router::start(Start::new(config(lightweight_classifier()))).await;
    let before = router.file();
    let revision = router.revision().await;
    let body = jev_save("https://api.typesafe.ai", Some(NEW_KEY));
    let port = router.base.rsplit(':').next().unwrap().to_owned();
    let cases: Vec<(&str, Headers, u16)> = vec![
        (
            "foreign origin",
            vec![("origin", "http://attacker.example".into())],
            403,
        ),
        ("opaque origin", vec![("origin", "null".into())], 403),
        (
            "rebinding host",
            vec![
                ("host", format!("attacker.example:{port}")),
                ("origin", format!("http://attacker.example:{port}")),
            ],
            403,
        ),
        (
            "text body",
            vec![("content-type", "text/plain".into())],
            415,
        ),
        (
            "form body",
            vec![("content-type", "application/x-www-form-urlencoded".into())],
            415,
        ),
    ];
    for (what, headers, expected) in cases {
        let (status, response, _) = router
            .put_with(body.clone(), &revision, |request| {
                let (client, request) = request.build_split();
                let mut request = request.unwrap();
                for (name, value) in &headers {
                    request.headers_mut().insert(*name, value.parse().unwrap());
                }
                reqwest::RequestBuilder::from_parts(client, request)
            })
            .await;
        assert_eq!(status, expected, "{what}: {response}");
        assert_eq!(router.file(), before, "{what}");
    }
    let (status, _, _) = router
        .put_with(body.clone(), &revision, |request| {
            let (client, request) = request.build_split();
            let mut request = request.unwrap();
            request.headers_mut().remove("origin");
            reqwest::RequestBuilder::from_parts(client, request)
        })
        .await;
    assert_eq!(status, 403, "no origin");
    assert!(router.store.peek(&jev_account(ACCOUNT_VAR)).is_none());
}

#[tokio::test]
async fn a_router_listening_off_loopback_cannot_have_its_settings_or_key_changed() {
    let mut start = Start::new(config(lightweight_classifier()));
    start.client_key = true;
    start.listen = Some("0.0.0.0:0");
    let router = Router::start(start).await;
    let (_, view) = router.view(Some(CLIENT_KEY)).await;
    assert_eq!(view["admin"]["available"], false);
    let before = router.file();
    let revision = view["revision"].as_str().unwrap().to_owned();
    let (status, body, _) = router
        .put_with(
            jev_save("https://api.typesafe.ai", Some(NEW_KEY)),
            &revision,
            |r| r,
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "admin_unavailable");
    assert_eq!(router.file(), before);
    assert!(router.store.peek(&jev_account(ACCOUNT_VAR)).is_none());
}

#[tokio::test]
async fn oversized_bodies_are_refused() {
    let router = Router::start(Start::new(config(lightweight_classifier()))).await;
    let mut body = jev_save("https://api.typesafe.ai", None);
    body["jev"]["model"] = json!("m".repeat(20_000));
    let (status, response, _) = router.put(body).await;
    assert_eq!(status, 413, "{response}");
}

#[tokio::test]
async fn two_routers_never_accept_each_others_admin_token_but_share_one_users_saved_key() {
    // One user, one credential store, two routers with their own files.
    let store = Arc::new(MemoryStore::default());
    let mut first = Start::new(config(lightweight_classifier()));
    first.store = Arc::clone(&store);
    let mut second = Start::new(config(lightweight_classifier()));
    second.store = Arc::clone(&store);
    let a = Router::start(first).await;
    let b = Router::start(second).await;
    assert_ne!(a.token, b.token);

    // A's token is refused by B, and B's file is untouched.
    let before = b.file();
    let revision = b.revision().await;
    let (status, body, _) = b
        .put_with(
            jev_save("https://api.typesafe.ai", Some(NEW_KEY)),
            &revision,
            |request| {
                let (client, request) = request.build_split();
                let mut request = request.unwrap();
                request
                    .headers_mut()
                    .insert(TOKEN_HEADER, a.token.parse().unwrap());
                reqwest::RequestBuilder::from_parts(client, request)
            },
        )
        .await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(body["error"]["code"], "admin_token_invalid");
    assert_eq!(b.file(), before);

    // Both name TYPESAFE_API_KEY, so a key saved through A is the entry B
    // would read — exactly as the one environment variable would be shared.
    let (status, _, _) = a
        .put(jev_save("https://api.typesafe.ai", Some(NEW_KEY)))
        .await;
    assert_eq!(status, 200);
    let (_, view) = b.view(None).await;
    assert_eq!(view["key"]["source"], "credential_store");
    assert_no_key(&view.to_string(), "the other router's view");
}

// --- consistency ----------------------------------------------------------------------

#[tokio::test]
async fn a_stale_or_missing_revision_cannot_overwrite() {
    let router = Router::start(Start::with_key(config(jev_classifier(
        "https://api.typesafe.ai",
    ))))
    .await;
    router
        .store
        .set(&jev_account(ACCOUNT_VAR), OLD_KEY)
        .unwrap();
    let stale = router.revision().await;
    // Someone else saves first.
    let (status, _, _) = router
        .put(jev_save("https://first.example.net", None))
        .await;
    assert_eq!(status, 200);
    let after_first = router.file();
    let (status, body, _) = router
        .put_with(
            jev_save("https://second.example.net", Some(NEW_KEY)),
            &stale,
            |r| r,
        )
        .await;
    assert_eq!(status, 412, "{body}");
    assert_eq!(body["error"]["code"], "revision_conflict");
    assert_eq!(body["revision"].as_str().unwrap(), router.revision().await);
    assert_eq!(router.file(), after_first);
    assert_eq!(
        router.store.peek(&jev_account(ACCOUNT_VAR)).as_deref(),
        Some(OLD_KEY)
    );

    // An editor's change is a new revision too.
    let edited = router
        .file()
        .replace("first.example.net", "edited.example.net");
    std::fs::write(&router.path, edited).unwrap();
    let (status, _, _) = router
        .put_with(jev_save("https://third.example.net", None), &stale, |r| r)
        .await;
    assert_eq!(status, 412);

    let (status, body, _) = router
        .put_with(jev_save("https://third.example.net", None), "", |request| {
            let (client, request) = request.build_split();
            let mut request = request.unwrap();
            request.headers_mut().remove("if-match");
            reqwest::RequestBuilder::from_parts(client, request)
        })
        .await;
    assert_eq!(status, 428, "{body}");
}

#[tokio::test]
async fn concurrent_saves_from_one_revision_let_exactly_one_through() {
    let router = Arc::new(
        Router::start(Start::with_key(config(jev_classifier(
            "https://api.typesafe.ai",
        ))))
        .await,
    );
    router
        .store
        .set(&jev_account(ACCOUNT_VAR), OLD_KEY)
        .unwrap();
    let revision = router.revision().await;
    let saves = (0..6).map(|n| {
        let router = Arc::clone(&router);
        let revision = revision.clone();
        tokio::spawn(async move {
            router
                .put_with(
                    jev_save(&format!("https://n{n}.example.net"), None),
                    &revision,
                    |r| r,
                )
                .await
                .0
        })
    });
    let statuses: Vec<u16> = futures_util::future::join_all(saves)
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == 412).count(),
        5,
        "{statuses:?}"
    );
}

#[tokio::test]
async fn a_failed_write_leaves_the_file_and_the_previous_key_in_place() {
    let router = Router::start(Start::with_key(config(jev_classifier(
        "https://api.typesafe.ai",
    ))))
    .await;
    router
        .store
        .set(&jev_account(ACCOUNT_VAR), OLD_KEY)
        .unwrap();
    let before = router.file();
    router
        .settings
        .settings
        .get()
        .unwrap()
        .fail_next_write
        .store(true, Ordering::SeqCst);
    let (status, body, _) = router
        .put(jev_save("https://other.example.net", Some(NEW_KEY)))
        .await;
    assert_eq!(status, 500, "{body}");
    assert_eq!(body["error"]["code"], "save_failed");
    assert_eq!(router.file(), before);
    assert_eq!(
        router.store.peek(&jev_account(ACCOUNT_VAR)).as_deref(),
        Some(OLD_KEY)
    );
    let (_, view) = router.view(None).await;
    assert_eq!(view["restart_required"], false, "nothing changed");

    // With no previous key, a failed write removes the new one again.
    router.store.delete(&jev_account(ACCOUNT_VAR)).unwrap();
    std::fs::write(
        &router.path,
        serde_json::to_string_pretty(&config(lightweight_classifier())).unwrap(),
    )
    .unwrap();
    router
        .settings
        .settings
        .get()
        .unwrap()
        .fail_next_write
        .store(true, Ordering::SeqCst);
    let (status, _, _) = router
        .put(jev_save("https://api.typesafe.ai", Some(NEW_KEY)))
        .await;
    assert_eq!(status, 500);
    assert!(router.store.peek(&jev_account(ACCOUNT_VAR)).is_none());

    // A store that refuses the key leaves the file alone.
    router.store.set_failing_writes(true);
    let before = router.file();
    let (status, body, _) = router
        .put(jev_save("https://api.typesafe.ai", Some(NEW_KEY)))
        .await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["code"], "credential_store_failed");
    assert_eq!(router.file(), before);
}

// --- restart and activation -----------------------------------------------------------

#[tokio::test]
async fn saved_settings_and_key_are_what_the_next_start_runs_and_test_connection_tracks_it() {
    let old = typesafe(OLD_KEY).await;
    let new = typesafe(NEW_KEY).await;
    let router = Router::start({
        let start = Start::new(config(jev_classifier(&old.url)));
        start.store.set(&jev_account(ACCOUNT_VAR), OLD_KEY).unwrap();
        start
    })
    .await;
    let check = router.check().await;
    assert_eq!(check["status"], "ok", "{check}");
    assert_eq!(
        old.with_expected_key.load(Ordering::SeqCst),
        old.calls.load(Ordering::SeqCst)
    );

    let (status, body, _) = router.put(jev_save(&new.url, Some(NEW_KEY))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["restart_required"], true);
    assert_eq!(body["active"]["jev"]["base_url"], old.url.as_str());
    assert_eq!(body["saved"]["jev"]["base_url"], new.url.as_str());
    // A save contacts nothing.
    assert_eq!(new.calls.load(Ordering::SeqCst), 0);

    // Until a restart, Test Connection checks what is running: the old
    // endpoint, with the old key it loaded. The saved key is not sent anywhere.
    // (The router's own start-up check also lands on the old endpoint, at a
    // moment of its choosing, so calls are compared as "more", not "one more".)
    let old_calls = old.calls.load(Ordering::SeqCst);
    let check = router.check().await;
    assert_eq!(check["status"], "ok", "{check}");
    assert!(old.calls.load(Ordering::SeqCst) > old_calls);
    assert_eq!(
        old.with_expected_key.load(Ordering::SeqCst),
        old.calls.load(Ordering::SeqCst),
        "every call carried the running key"
    );
    assert_eq!(new.calls.load(Ordering::SeqCst), 0);

    // The restart loads the saved file and the stored key.
    let router = router.restart(false).await;
    let (_, view) = router.view(None).await;
    assert_eq!(view["restart_required"], false, "{view}");
    assert_eq!(view["active"]["jev"]["base_url"], new.url.as_str());
    assert_eq!(view["active"]["key_source"], "credential_store");
    let check = router.check().await;
    assert_eq!(check["status"], "ok", "{check}");
    assert!(new.calls.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        new.with_expected_key.load(Ordering::SeqCst),
        new.calls.load(Ordering::SeqCst),
        "every call carried the saved key"
    );

    // And `load_with_store` — the CLI's path — reads the same.
    let loaded =
        lightweight_router::load_with_store(&router.path, router.store.as_ref()).expect("loads");
    assert!(loaded.store_note.is_none());
    let auto = loaded.config.auto.expect("auto");
    match &auto.classifier.expect("classifier").provider {
        lightweight_router::classifier::ClassifierProvider::Jev(jev) => {
            assert_eq!(jev.base_url, new.url);
            assert_eq!(
                jev.key_source,
                lightweight_router::classifier::jev::KeySource::CredentialStore
            );
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn test_connection_reports_a_rejected_key_as_an_auth_error() {
    let typesafe = typesafe(NEW_KEY).await;
    let router = Router::start({
        let start = Start::new(config(jev_classifier(&typesafe.url)));
        start.store.set(&jev_account(ACCOUNT_VAR), OLD_KEY).unwrap();
        start
    })
    .await;
    let check = router.check().await;
    assert_eq!(check["status"], "auth_error", "{check}");
    assert_no_key(&check.to_string(), "the check report");
}

#[tokio::test]
async fn the_saved_key_can_be_removed_unless_the_saved_settings_need_it() {
    let router = Router::start(Start::with_key(config(jev_classifier(
        "https://api.typesafe.ai",
    ))))
    .await;
    router
        .store
        .set(&jev_account(ACCOUNT_VAR), OLD_KEY)
        .unwrap();
    let (status, body) = router.delete_key().await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "key_in_use");
    assert_eq!(
        router.store.peek(&jev_account(ACCOUNT_VAR)).as_deref(),
        Some(OLD_KEY)
    );

    // Saved as Lightweight (the Jev block stays as a standby), the key can go.
    let mut file = router.file_json();
    file["auto_route"]["classifier"]["provider"] = json!("lightweight");
    file["auto_route"]["classifier"]["lightweight"] =
        json!({"route": "General", "timeout_ms": 20000});
    std::fs::write(&router.path, serde_json::to_string_pretty(&file).unwrap()).unwrap();
    let (status, body) = router.delete_key().await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["key_action"], "removed");
    assert_eq!(body["key"]["source"], "missing");
    assert_eq!(body["restart_required"], true);
    assert!(router.store.peek(&jev_account(ACCOUNT_VAR)).is_none());
}

#[tokio::test]
async fn a_router_without_settings_answers_with_a_clear_refusal() {
    ensure_provider();
    let file: RouterFile =
        serde_json::from_value(config(lightweight_classifier())).expect("config shape");
    let config = lightweight_router::validate(file, &|_| None).expect("valid");
    let bound = lightweight_router::bind(&config).await.expect("bind");
    let base = format!("http://{}", bound.addresses()[0]);
    let stop = CancellationToken::new();
    tokio::spawn(bound.serve(stop.clone()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let response = client()
        .get(format!("{base}/api/router/v1/classifier/settings"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "settings_unavailable");
    stop.cancel();
}

// --- no secret in any log -------------------------------------------------------------

#[tokio::test]
async fn no_key_or_admin_token_ever_reaches_a_log_line() {
    let typesafe = typesafe(NEW_KEY).await;
    let router = Router::start({
        let start = Start::new(config(jev_classifier(&typesafe.url)));
        start.store.set(&jev_account(ACCOUNT_VAR), OLD_KEY).unwrap();
        start
    })
    .await;
    // A save, a refused save, a failed save, a check, a removal attempt.
    let _ = router.put(jev_save(&typesafe.url, Some(NEW_KEY))).await;
    let revision = router.revision().await;
    let _ = router
        .put_with(
            jev_save(&typesafe.url, Some(NEW_KEY)),
            &revision,
            |request| {
                let (client, request) = request.build_split();
                let mut request = request.unwrap();
                request
                    .headers_mut()
                    .insert("origin", "http://attacker.example".parse().unwrap());
                reqwest::RequestBuilder::from_parts(client, request)
            },
        )
        .await;
    let _ = router
        .put(jev_save("http://plain.example.net", Some(NEW_KEY)))
        .await;
    let _ = router.check().await;
    let _ = router.delete_key().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let logs = String::from_utf8(captured().lock().unwrap().clone()).unwrap();
    assert!(logs.contains("classifier_settings_saved"), "{logs}");
    assert!(logs.contains("classifier_settings_refused"), "{logs}");
    assert_no_key(&logs, "the log");
    assert!(
        !logs.contains(&router.token),
        "the admin token reached the log"
    );
}

// --- the real credential store ----------------------------------------------------------

/// The operating system's own store, under a test-only service name, through
/// two separate reads — what a restart does. On macOS and Windows CI the store
/// is there and must work; on a Linux runner with no Secret Service it must
/// say so rather than pretend.
#[test]
fn the_os_credential_store_round_trips_or_says_it_is_unavailable() {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        let mut bytes = [0u8; 6];
        getrandom::fill(&mut bytes).unwrap();
        let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let service = format!("lightweight-router-test-{suffix}");
        let account = jev_account(ACCOUNT_VAR);
        let writer = lightweight_router::secret_store::os_store_for_service(&service);
        match writer.set(&account, NEW_KEY) {
            Ok(()) => {
                let reader = lightweight_router::secret_store::os_store_for_service(&service);
                let read = reader.get(&account).expect("read back");
                assert_eq!(read.as_ref().map(|s| s.expose()), Some(NEW_KEY));
                reader.delete(&account).expect("delete");
                assert!(reader.get(&account).expect("read after delete").is_none());
            }
            Err(failure) => {
                assert!(
                    !cfg!(any(target_os = "macos", target_os = "windows"))
                        || std::env::var_os("CI").is_none(),
                    "the credential store must work on {} CI: {failure}",
                    std::env::consts::OS
                );
                assert!(!failure.message().contains(NEW_KEY));
                eprintln!("credential store unavailable here: {failure}");
            }
        }
    }
}
