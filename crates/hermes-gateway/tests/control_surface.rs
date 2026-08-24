//! The control surface the desktop panel is built on.
//!
//! Two things live here because they are checked the same way — over a real
//! socket, through the real router:
//!
//! * `GET /api/v1/models/{id}`, against a real GGUF header written by the
//!   fixture builder rather than a hand-made byte array. The point of the
//!   endpoint is that it reads what the catalog's own reader reads, and a fake
//!   header would test this file's idea of a header instead of the one the
//!   loader will meet.
//! * Serving the panel's own files, where what matters is a *routing*
//!   property: a file must never shadow an endpoint, and nothing outside the
//!   web root may be reachable through it.

use std::sync::Arc;

use hermes_backend_mock::MockBackend;
use hermes_catalog::CatalogStore;
use hermes_catalog::install::Installer;
use hermes_gateway::manager::{ModelManager, RuntimeDefaults};
use hermes_gateway::{GatewayConfig, GatewayState};
use hermes_gguf::fixture::{GgufBuilder, TempDir};
use serde_json::Value;

/// Install the rustls provider `reqwest` insists on.
///
/// It panics rather than erroring when none is installed, even for a plain
/// `http://` request, so every test that builds a client calls this first.
fn ensure_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A gateway with a catalog holding one imported fixture model.
async fn gateway_with_a_model(tag: &str) -> (TempDir, Arc<GatewayState>, String) {
    let dir = TempDir::new(tag);
    let model_path = dir.write("fixture.gguf", &GgufBuilder::small_model("llama").build());

    let manager = Arc::new(ModelManager::new(
        CatalogStore::open(dir.path().join("catalog.json")).expect("catalog"),
        Installer::new(dir.path().join("models"), dir.path().join("downloads")).expect("installer"),
        RuntimeDefaults::default(),
    ));
    let installed = manager
        .register_at_startup(model_path)
        .await
        .expect("register the fixture");

    let state = Arc::new(
        GatewayState::new(
            Arc::new(MockBackend::default()),
            hermes_gateway::catalog::shared(None),
            GatewayConfig::default(),
        )
        .with_manager(Arc::clone(&manager)),
    );
    let id = installed.id.clone();
    (dir, state, id)
}

/// A server on an ephemeral loopback port, as the other suites do it.
///
/// A real socket rather than calling the handler directly: the routing itself
/// is part of what is being checked here — `GET` and `DELETE` share the
/// `/api/v1/models/{id}` path, and a mistake there is invisible to a test that
/// invokes the function by name.
struct Server {
    base: String,
    _task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(state: Arc<GatewayState>) -> Self {
        let app = hermes_gateway::app(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        Self {
            base: format!("http://127.0.0.1:{port}"),
            _task: tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            }),
        }
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let response = reqwest::Client::new()
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .expect("request");
        let status = response.status().as_u16();
        let value = response.json().await.unwrap_or(Value::Null);
        (status, value)
    }
}

#[tokio::test]
async fn the_detail_carries_the_shape_of_the_network() {
    ensure_provider();
    let (_dir, state, id) = gateway_with_a_model("detail").await;

    let server = Server::start(state).await;
    let (status, body) = server.get(&format!("/api/v1/models/{id}")).await;
    assert_eq!(status, 200, "{body}");

    // The catalog half.
    assert_eq!(body["id"], id);
    assert_eq!(body["architecture"], "llama");
    assert_eq!(body["state"], "available");

    // The half that only the file knows, and which the catalog deliberately
    // does not store.
    let header = &body["header"];
    assert_eq!(header["block_count"], 2);
    assert_eq!(header["embedding_length"], 64);
    assert_eq!(header["head_count"], 8);
    assert_eq!(header["head_count_kv"][0], 2);
    assert_eq!(header["vocab_size"], 128);
    assert_eq!(header["context_length"], 4096);
    assert!(header["tensor_count"].as_u64().unwrap() >= 4);
}

#[tokio::test]
async fn the_detail_carries_an_estimate_for_the_context_it_would_load_with() {
    ensure_provider();
    let (_dir, state, id) = gateway_with_a_model("estimate").await;

    let server = Server::start(state).await;
    let (status, body) = server.get(&format!("/api/v1/models/{id}")).await;
    assert_eq!(status, 200, "{body}");

    let estimate = &body["estimate"];
    // The four terms and their sum, so a panel can show where the memory goes
    // rather than only the total.
    for term in ["weights", "kv_cache", "compute", "overhead", "total"] {
        assert!(
            estimate[term].as_u64().is_some(),
            "the estimate is missing {term}: {estimate}"
        );
    }
    assert!(estimate["verdict"].is_string());
    assert!(estimate["confidence"].is_string());

    // The estimate is for a context, and that context is the one a load would
    // choose - not the model's advertised maximum.
    let n_ctx = estimate["params"]["n_ctx"].as_u64().expect("a context");
    assert!(n_ctx > 0);
    assert!(
        n_ctx <= 4096,
        "a fixture advertising 4096 cannot be estimated for more: {n_ctx}"
    );

    // The sum really is the sum, so the four parts and the total cannot drift.
    let total: u64 = ["weights", "kv_cache", "compute", "overhead"]
        .iter()
        .map(|term| estimate[*term].as_u64().unwrap_or_default())
        .sum();
    assert_eq!(estimate["total"].as_u64().unwrap(), total);
}

#[tokio::test]
async fn a_model_whose_file_is_gone_is_described_without_being_read() {
    ensure_provider();
    let (dir, state, id) = gateway_with_a_model("missing").await;

    // The catalog keeps the record; the drive is unmounted, as far as it knows.
    // `register_at_startup` registers a file where it lies rather than copying
    // it, so the file to remove is the fixture itself.
    std::fs::remove_file(dir.path().join("fixture.gguf")).expect("remove the model file");

    let server = Server::start(state).await;
    let (status, body) = server.get(&format!("/api/v1/models/{id}")).await;
    assert_eq!(status, 200, "a known model is still known: {body}");
    assert_eq!(body["state"], "missing");
    // Neither is invented from a file that is not there.
    assert!(body["header"].is_null());
    assert!(body["estimate"].is_null());
}

#[tokio::test]
async fn an_unknown_model_is_a_404_with_a_code() {
    let (_dir, state, _id) = gateway_with_a_model("unknown").await;

    let server = Server::start(state).await;
    let (status, body) = server.get("/api/v1/models/not-a-model").await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"]["code"], "unknown_model");
}

#[tokio::test]
async fn the_list_and_the_detail_agree_about_the_same_model() {
    ensure_provider();
    // They are built by one constructor precisely so they cannot disagree;
    // this is the assertion that keeps it that way.
    let (_dir, state, id) = gateway_with_a_model("agreement").await;

    let server = Server::start(state).await;
    let (_, list) = server.get("/api/v1/models").await;
    let (_, detail) = server.get(&format!("/api/v1/models/{id}")).await;
    let row = &list["data"][0];

    for field in [
        "id",
        "name",
        "state",
        "verified",
        "integrity_label",
        "bytes",
    ] {
        assert_eq!(row[field], detail[field], "the two disagree about {field}");
    }
}

/// The panel's files, served from a gateway that also answers the API.
async fn gateway_with_a_panel(tag: &str) -> (TempDir, Server) {
    let dir = TempDir::new(tag);
    let root = dir.path().join("web");
    std::fs::create_dir_all(root.join("assets")).expect("web root");
    std::fs::write(
        root.join("index.html"),
        "<!doctype html><title>Panel</title>",
    )
    .expect("index");
    std::fs::write(root.join("assets/main.abc123.js"), "export const x = 1;").expect("asset");
    // A file a traversal would be aiming for, one level above the web root.
    std::fs::write(dir.path().join("secret.txt"), "not for the browser").expect("secret");

    let state = Arc::new(GatewayState::new(
        Arc::new(MockBackend::default()),
        hermes_gateway::catalog::shared(None),
        GatewayConfig {
            web_root: Some(root),
            ..GatewayConfig::default()
        },
    ));
    let server = Server::start(state).await;
    (dir, server)
}

#[tokio::test]
async fn the_panel_is_served_from_the_gateway_that_answers_its_calls() {
    ensure_provider();
    // The property that makes a CORS layer unnecessary: one origin serves both.
    let (_dir, server) = gateway_with_a_panel("panel").await;

    let document = reqwest::get(format!("{}/", server.base))
        .await
        .expect("request");
    assert_eq!(document.status(), 200);
    assert_eq!(
        document
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/html; charset=utf-8")
    );
    // The document must never be cached: it names the hashed assets, and a
    // stale copy points a browser at scripts a redeploy has removed.
    assert_eq!(
        document
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-cache")
    );
    assert!(document.text().await.expect("body").contains("Panel"));

    let asset = reqwest::get(format!("{}/assets/main.abc123.js", server.base))
        .await
        .expect("request");
    assert_eq!(asset.status(), 200);
    assert_eq!(
        asset
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/javascript; charset=utf-8")
    );
}

#[tokio::test]
async fn a_file_can_never_shadow_an_endpoint() {
    ensure_provider();
    // The fallback is installed last precisely so this cannot happen. Checked
    // over a real socket, because it is a property of the router and not of any
    // one handler.
    let (_dir, server) = gateway_with_a_panel("shadow").await;

    let (status, body) = server.get("/health").await;
    assert_eq!(status, 200);
    assert!(
        body["status"].is_string(),
        "/health must still be JSON, not the panel: {body}"
    );

    let version = reqwest::get(format!("{}/version", server.base))
        .await
        .expect("request");
    assert_eq!(
        version
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
}

#[tokio::test]
async fn a_client_side_route_gets_the_document_and_a_missing_asset_does_not() {
    ensure_provider();
    let (_dir, server) = gateway_with_a_panel("routes").await;

    // A deep link the panel's own router will render.
    let route = reqwest::get(format!("{}/models", server.base))
        .await
        .expect("request");
    assert_eq!(route.status(), 200);
    assert!(route.text().await.expect("body").contains("Panel"));

    // A script that is not there is a 404, not HTML. Answering with the
    // document turns a missing file into an unexplained syntax error.
    let missing = reqwest::get(format!("{}/assets/gone.js", server.base))
        .await
        .expect("request");
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn no_request_can_read_a_file_outside_the_web_root() {
    ensure_provider();
    let (_dir, server) = gateway_with_a_panel("traversal").await;

    for attempt in [
        "/../secret.txt",
        "/assets/../../secret.txt",
        "/./../secret.txt",
    ] {
        let response = reqwest::get(format!("{}{attempt}", server.base))
            .await
            .expect("request");
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        assert!(
            !body.contains("not for the browser"),
            "{attempt} escaped the web root (status {status})"
        );
    }
}

#[tokio::test]
async fn a_gateway_with_no_panel_answers_unmatched_paths_exactly_as_before() {
    ensure_provider();
    // Additive: every deployment that predates the panel must be unchanged.
    let state = Arc::new(GatewayState::new(
        Arc::new(MockBackend::default()),
        hermes_gateway::catalog::shared(None),
        GatewayConfig::default(),
    ));
    let server = Server::start(state).await;

    let response = reqwest::get(format!("{}/", server.base))
        .await
        .expect("request");
    assert_eq!(response.status(), 404);
    assert!(response.text().await.expect("body").is_empty());
}
