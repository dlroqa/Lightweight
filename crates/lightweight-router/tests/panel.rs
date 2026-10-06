//! The control panel served from the router's own origin
//! (`hermes router --web-root`), and the read views the panel's router
//! screens depend on.
//!
//! The panel is a static bundle; what matters here is the seam: its files are
//! served without a credential, every API path keeps the router's own JSON
//! answers and its own credential check, a deep link gets the document, and a
//! router started without a web root is exactly what it was before.

use std::path::{Path, PathBuf};
use std::time::Duration;

use lightweight_router::config::RouterFile;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Short on purpose: the secrets gate refuses a committed bearer literal of
/// 16 or more characters.
const CLIENT_KEY: &str = "panel-client";
const JEV_KEY: &str = "jev-panel-key";

fn ensure_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client")
}

/// A built panel, as far as the router can tell: a document and one asset.
fn panel_dir() -> (tempdir::Dir, PathBuf) {
    let dir = tempdir::Dir::new("panel");
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("assets")).expect("assets dir");
    std::fs::write(
        root.join("index.html"),
        "<!doctype html><title>panel</title>",
    )
    .expect("index");
    std::fs::write(root.join("assets/main.abc123.js"), "console.log(1)").expect("asset");
    (dir, root)
}

/// A tiny self-cleaning temporary directory, so the test needs no new
/// dependency.
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

fn config(classifier: Value) -> Value {
    json!({
        "nodes": [{"id": "a", "url": "http://127.0.0.1:9"}],
        "routes": [
            {"name": "General", "description": "Everyday conversation", "deployments": [{"node": "a", "model": "G"}]},
            {"name": "Coder", "description": "Programming and debugging", "deployments": [{"node": "a", "model": "C"}]},
            {"name": "Research", "deployments": [{"node": "a", "model": "R"}]}
        ],
        "auto_route": {
            "enabled": true,
            "fallback_route": "General",
            "rules": [
                {"name": "tools", "when": {"requires_tools": true}, "route": "Coder"},
                {"name": "semantic", "when": {}, "classify": true}
            ],
            "classifier": classifier
        }
    })
}

fn jev_classifier() -> Value {
    json!({
        "provider": "jev",
        "routes": ["General", "Coder"],
        "fallback_route": "General",
        "jev": {
            "base_url": "http://127.0.0.1:9",
            "model": "jev-1.13.0",
            "timeout_ms": 5000,
            "include_user_text": false
        }
    })
}

struct Router {
    base: String,
    stop: CancellationToken,
}

impl Drop for Router {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Router {
    async fn start(mut config: Value, web_root: Option<&Path>, key: bool) -> Self {
        ensure_provider();
        config["listen"] = json!(["127.0.0.1:0"]);
        config["health"] =
            json!({"interval_secs": 3600, "timeout_secs": 1, "failure_threshold": 1});
        if key {
            config["api_key_env"] = json!("LW_PANEL_CLIENT_KEY");
        }
        let file: RouterFile = serde_json::from_value(config).expect("config shape");
        let config = lightweight_router::validate(file, &|name| match name {
            "TYPESAFE_API_KEY" => Some(JEV_KEY.to_owned()),
            "LW_PANEL_CLIENT_KEY" => Some(CLIENT_KEY.to_owned()),
            _ => None,
        })
        .expect("valid config");
        let bound = lightweight_router::bind(&config)
            .await
            .expect("bind")
            .with_web_root(web_root.map(Path::to_path_buf));
        let base = format!("http://{}", bound.addresses()[0]);
        let stop = CancellationToken::new();
        tokio::spawn(bound.serve(stop.clone()));
        for _ in 0..200 {
            if client().get(format!("{base}/health")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Self { base, stop }
    }

    async fn get(&self, path: &str, key: Option<&str>) -> (u16, String, String) {
        let mut request = client().get(format!("{}{path}", self.base));
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.expect("request");
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        (status, content_type, response.text().await.expect("body"))
    }
}

#[tokio::test]
async fn a_router_with_a_web_root_serves_the_panel_and_keeps_its_api() {
    let (_dir, root) = panel_dir();
    let router = Router::start(config(jev_classifier()), Some(&root), false).await;

    let (status, content_type, body) = router.get("/", None).await;
    assert_eq!(status, 200);
    assert!(content_type.starts_with("text/html"), "{content_type}");
    assert!(body.contains("<title>panel</title>"));

    // A client-side route gets the document; a missing asset does not.
    let (status, _, body) = router.get("/classifier", None).await;
    assert_eq!((status, body.contains("panel")), (200, true));
    let (status, content_type, _) = router.get("/assets/main.abc123.js", None).await;
    assert_eq!(status, 200);
    assert!(
        content_type.starts_with("text/javascript"),
        "{content_type}"
    );
    assert_eq!(router.get("/assets/missing.js", None).await.0, 404);

    // An unknown API path is the router's JSON error, never the document.
    for path in ["/api/router/v1/nope", "/v1/nope", "/api", "/v1"] {
        let (status, content_type, body) = router.get(path, None).await;
        assert_eq!(status, 404, "{path}");
        assert!(
            content_type.starts_with("application/json"),
            "{path}: {content_type}"
        );
        assert!(body.contains("not_found"), "{path}: {body}");
    }
    // The API itself is untouched.
    let (status, _, body) = router.get("/api/router/v1/auto", None).await;
    assert_eq!(status, 200);
    let auto: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(auto["classifier"]["provider"], "jev");
}

#[tokio::test]
async fn a_router_without_a_web_root_serves_no_panel() {
    let router = Router::start(config(jev_classifier()), None, false).await;
    for path in ["/", "/classifier", "/assets/main.abc123.js"] {
        let (status, content_type, body) = router.get(path, None).await;
        assert_eq!(status, 404, "{path}");
        assert!(content_type.starts_with("application/json"), "{path}");
        assert!(body.contains("not_found"), "{path}");
    }
}

#[tokio::test]
async fn the_panel_needs_no_key_but_the_api_still_does() {
    let (_dir, root) = panel_dir();
    let router = Router::start(config(jev_classifier()), Some(&root), true).await;

    assert_eq!(router.get("/", None).await.0, 200);
    assert_eq!(router.get("/assets/main.abc123.js", None).await.0, 200);
    // `/version` is how the panel learns it is on a router; it is public on
    // the gateway too.
    let (status, _, body) = router.get("/version", None).await;
    assert_eq!(status, 200);
    assert!(body.contains("lightweight-router-"), "{body}");

    assert_eq!(router.get("/api/router/v1/auto", None).await.0, 401);
    assert_eq!(router.get("/api/router/v1/routes", None).await.0, 401);
    assert_eq!(
        router.get("/api/router/v1/auto", Some(CLIENT_KEY)).await.0,
        200
    );
}

#[tokio::test]
async fn the_routes_view_carries_each_route_description() {
    let router = Router::start(config(jev_classifier()), None, false).await;
    let (status, _, body) = router.get("/api/router/v1/routes", None).await;
    assert_eq!(status, 200);
    let routes: Value = serde_json::from_str(&body).expect("json");
    let described: Vec<(String, Value)> = routes["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|route| {
            (
                route["name"].as_str().unwrap().to_owned(),
                route["description"].clone(),
            )
        })
        .collect();
    assert_eq!(
        described,
        vec![
            ("General".to_owned(), json!("Everyday conversation")),
            ("Coder".to_owned(), json!("Programming and debugging")),
            ("Research".to_owned(), Value::Null),
        ]
    );
}

#[tokio::test]
async fn no_view_the_panel_reads_carries_the_provider_key() {
    // The panel reads these and nothing else; none may carry the TypeSafe key
    // or the router's client key, whatever the provider's state.
    let (_dir, root) = panel_dir();
    let router = Router::start(config(jev_classifier()), Some(&root), false).await;
    let check = client()
        .post(format!("{}/api/router/v1/classifier/check", router.base))
        .send()
        .await
        .expect("check");
    let check_body = check.text().await.expect("body");
    let mut bodies = vec![check_body];
    for path in [
        "/version",
        "/api/router/v1/auto",
        "/api/router/v1/routes",
        "/",
    ] {
        bodies.push(router.get(path, None).await.2);
    }
    for body in &bodies {
        assert!(!body.contains(JEV_KEY), "the provider key leaked: {body}");
        assert!(!body.to_ascii_lowercase().contains("bearer"), "{body}");
    }
    let auto: Value = serde_json::from_str(&bodies[2]).expect("json");
    let jev = &auto["classifier"]["jev"];
    assert_eq!(jev["api_key_env"], "TYPESAFE_API_KEY");
    assert_eq!(jev["api_key_configured"], true);
    assert_eq!(jev["include_user_text"], false);
    assert_eq!(jev["active"], true);
    let invoked: Vec<&str> = auto["classifier"]["invoked_by"]
        .as_array()
        .expect("invoked_by")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(invoked, vec!["semantic"]);
}
