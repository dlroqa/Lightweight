//! Conversations and settings, over the wire.
//!
//! Checked through the real router on a real socket, because the properties
//! that matter here are properties of the surface rather than of the store:
//! which verbs a path accepts, what an id in a path is allowed to be, and
//! whether a setting the user turned off is actually obeyed.

use std::path::PathBuf;
use std::sync::Arc;

use lightweight_backend_mock::MockBackend;
use lightweight_gateway::{GatewayConfig, GatewayState};
use lightweight_system_info::DataPaths;
use serde_json::{Value, json};

fn ensure_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A throwaway data directory that cleans up after itself.
struct Profile(PathBuf);

impl Profile {
    fn new(tag: &str) -> Self {
        // The clock alone is not unique: on a coarse timer two tests running in
        // parallel are handed the same name. The counter and the pid settle it.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "hermes-store-api-{tag}-{}-{unique}-{sequence}",
            std::process::id()
        )))
    }

    fn paths(&self) -> DataPaths {
        let paths = DataPaths::rooted_at(&self.0);
        paths.create_all().expect("create the data directories");
        paths
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    base: String,
    _task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(paths: Option<DataPaths>) -> Self {
        let state = Arc::new(GatewayState::new(
            Arc::new(MockBackend::default()),
            lightweight_gateway::catalog::shared(None),
            GatewayConfig {
                paths,
                ..GatewayConfig::default()
            },
        ));
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

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut builder = reqwest::Client::new().request(method, format!("{}{path}", self.base));
        if let Some(body) = body {
            builder = builder.json(&body);
        }
        let response = builder.send().await.expect("request");
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.request(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str) -> (u16, Value) {
        self.request(reqwest::Method::POST, path, None).await
    }
    async fn put(&self, path: &str, body: Value) -> (u16, Value) {
        self.request(reqwest::Method::PUT, path, Some(body)).await
    }
    async fn delete(&self, path: &str) -> (u16, Value) {
        self.request(reqwest::Method::DELETE, path, None).await
    }
}

#[tokio::test]
async fn a_conversation_can_be_started_saved_read_and_forgotten() {
    ensure_provider();
    let profile = Profile::new("lifecycle");
    let server = Server::start(Some(profile.paths())).await;

    let (status, created) = server.post("/api/v1/conversations").await;
    assert_eq!(status, 201, "{created}");
    let id = created["id"].as_str().expect("an id").to_owned();
    assert!(created["messages"].as_array().expect("messages").is_empty());

    let (status, saved) = server
        .put(
            &format!("/api/v1/conversations/{id}"),
            json!({
                "title": "CPU inference explained",
                "model": "mock-model@4k",
                "messages": [
                    {"role": "user", "content": "how does it work?", "at": 1},
                    {
                        "role": "assistant",
                        "content": "It runs the model on your processor.",
                        "at": 2,
                        "completion_tokens": 45,
                        "tokens_per_second": 82.4
                    }
                ]
            }),
        )
        .await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["title"], "CPU inference explained");

    let (status, read) = server.get(&format!("/api/v1/conversations/{id}")).await;
    assert_eq!(status, 200);
    assert_eq!(read["messages"][1]["tokens_per_second"], 82.4);
    assert_eq!(read["messages"][1]["completion_tokens"], 45);

    let (status, listed) = server.get("/api/v1/conversations").await;
    assert_eq!(status, 200);
    assert_eq!(listed["data"][0]["id"], id.as_str());
    assert_eq!(listed["data"][0]["message_count"], 2);
    // A listing carries no transcripts.
    assert!(
        !serde_json::to_string(&listed)
            .expect("encode")
            .contains("It runs the model")
    );

    let (status, _) = server.delete(&format!("/api/v1/conversations/{id}")).await;
    assert_eq!(status, 200);
    let (status, gone) = server.get(&format!("/api/v1/conversations/{id}")).await;
    assert_eq!(status, 404, "{gone}");
    assert_eq!(gone["error"]["code"], "unknown_conversation");
}

#[tokio::test]
async fn a_conversation_id_in_a_path_can_never_be_a_path() {
    ensure_provider();
    // The id becomes a file name, so this is the boundary that matters most in
    // the whole module.
    let profile = Profile::new("traversal");
    let server = Server::start(Some(profile.paths())).await;

    // Anything that reaches the handler is refused by shape, before it can be
    // joined to a directory.
    for attempt in [
        "not-a-real-id",
        "0011223344556677889900aabbccddeeff00",
        "0011223344556677889900aabbccdde",
        "................................",
    ] {
        let (status, body) = server
            .get(&format!("/api/v1/conversations/{attempt}"))
            .await;
        assert_eq!(status, 400, "{attempt} gave {body}");
        assert_eq!(body["error"]["code"], "malformed_conversation_id");
    }

    // Dot segments, plain and percent-encoded, are resolved away by RFC 3986
    // before the request is routed, so they arrive at a path that matches no
    // route rather than at the handler. Asserted as "never served as a
    // conversation" rather than as a particular status: which of the two
    // defences catches it is an implementation detail of the HTTP stack, and
    // the property worth pinning is that neither lets it through.
    for attempt in ["..", "%2e%2e", "../..", "%2e%2e%2f"] {
        let (status, body) = server
            .get(&format!("/api/v1/conversations/{attempt}"))
            .await;
        assert!(
            status == 400 || status == 404,
            "{attempt} was served with {status}: {body}"
        );
        assert!(
            body["id"].is_null() && body["messages"].is_null(),
            "{attempt} returned something shaped like a conversation: {body}"
        );
    }
}

#[tokio::test]
async fn when_the_conversation_began_is_not_rewritten_by_a_later_save() {
    ensure_provider();
    let profile = Profile::new("created-at");
    let server = Server::start(Some(profile.paths())).await;

    let (_, created) = server.post("/api/v1/conversations").await;
    let id = created["id"].as_str().expect("id").to_owned();
    let began = created["created_at"].as_u64().expect("created_at");

    // A client that does not track `created_at` saves without it.
    let (status, saved) = server
        .put(
            &format!("/api/v1/conversations/{id}"),
            json!({"title": "later", "messages": []}),
        )
        .await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(
        saved["created_at"].as_u64(),
        Some(began),
        "a save without created_at must not reset when it began"
    );
}

#[tokio::test]
async fn turning_history_off_refuses_writes_and_still_allows_reads() {
    ensure_provider();
    // Reads stay open on purpose: conversations saved before the setting
    // changed are still the user's, and hiding them would leave no way to look
    // at them or delete them.
    let profile = Profile::new("history-off");
    let server = Server::start(Some(profile.paths())).await;

    let (_, created) = server.post("/api/v1/conversations").await;
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, settings) = server
        .put(
            "/api/v1/settings",
            json!({"gateway": {"keep_history": false}, "ui": {}}),
        )
        .await;
    assert_eq!(status, 200, "{settings}");
    assert_eq!(settings["gateway"]["keep_history"], false);

    let (status, refused) = server
        .put(
            &format!("/api/v1/conversations/{id}"),
            json!({"title": "nope", "messages": []}),
        )
        .await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["error"]["code"], "history_disabled");
    // The refusal says what to do about it rather than only that it happened.
    assert!(
        refused["error"]["hermes"]["remedies"][0].is_object(),
        "{refused}"
    );

    let (status, _) = server.post("/api/v1/conversations").await;
    assert_eq!(status, 400, "starting one is a write too");

    // ...and what is already saved is still readable and deletable.
    let (status, _) = server.get(&format!("/api/v1/conversations/{id}")).await;
    assert_eq!(status, 200);
    let (status, _) = server.get("/api/v1/conversations").await;
    assert_eq!(status, 200);
    let (status, _) = server.delete(&format!("/api/v1/conversations/{id}")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn settings_round_trip_and_the_ui_half_is_kept_verbatim() {
    ensure_provider();
    let profile = Profile::new("settings");
    let server = Server::start(Some(profile.paths())).await;

    let (status, defaults) = server.get("/api/v1/settings").await;
    assert_eq!(status, 200);
    assert_eq!(defaults["gateway"]["keep_history"], true);
    assert!(defaults["gateway"]["default_n_ctx"].is_null());

    let (status, saved) = server
        .put(
            "/api/v1/settings",
            json!({
                "gateway": {"keep_history": true, "default_n_ctx": 8192},
                "ui": {"theme": "dark", "somethingOnlyThePanelKnows": [1, 2, 3]}
            }),
        )
        .await;
    assert_eq!(status, 200, "{saved}");

    let (_, read) = server.get("/api/v1/settings").await;
    assert_eq!(read["gateway"]["default_n_ctx"], 8192);
    assert_eq!(read["ui"]["theme"], "dark");
    assert_eq!(read["ui"]["somethingOnlyThePanelKnows"][2], 3);
}

#[tokio::test]
async fn a_gateway_with_no_data_directory_says_so_rather_than_failing_oddly() {
    ensure_provider();
    // Every existing test and the contract suite's mock gateway build one this
    // way; they must get a clear answer, not a panic or a 500.
    let server = Server::start(None).await;

    for path in ["/api/v1/conversations", "/api/v1/settings"] {
        let (status, body) = server.get(path).await;
        assert_eq!(status, 501, "{path} gave {body}");
        assert_eq!(body["error"]["code"], "no_data_directory");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn what_the_user_typed_is_not_left_world_readable() {
    ensure_provider();
    use std::os::unix::fs::PermissionsExt as _;

    // The log redacts prompts by default. Writing the same words to a readable
    // file would make that redaction decorative.
    let profile = Profile::new("perms");
    let paths = profile.paths();
    let server = Server::start(Some(paths.clone())).await;

    let (_, created) = server.post("/api/v1/conversations").await;
    let id = created["id"].as_str().expect("id").to_owned();
    server
        .put(
            &format!("/api/v1/conversations/{id}"),
            json!({"title": "private", "messages": [{"role": "user", "content": "my secret"}]}),
        )
        .await;

    let file = paths.conversations_dir().join(format!("{id}.json"));
    let mode = std::fs::metadata(&file).expect("stat").permissions().mode();
    assert_eq!(
        mode & 0o077,
        0,
        "a conversation is readable by others: {mode:o}"
    );
}
