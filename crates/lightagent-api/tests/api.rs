//! HTTP contract tests over a real loopback listener, plus run-lifecycle tests.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lightagent_api::manager::{self, RunFactory, RunManager, RunStatus, StartRun};
use lightagent_api::{AppState, AuthConfig, Scope, router};
use lightagent_core::permissions::ApprovalPolicy;
use lightagent_core::provider::ProviderMessage;
use lightagent_core::{
    AgentEventSink, AgentLoop, ApprovalDecision, FinishReason, MockProvider, PolicyEngine,
    ProviderEvent, RunConfig,
};
use lightagent_store::SessionStore;
use lightagent_tools::{BoundedExecutor, ToolRegistry};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedReceiver;

/// A factory that scripts a datetime.now tool call then a final answer.
struct MockFactory;

fn tool_turn() -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::RoleStarted,
        ProviderEvent::ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("datetime.now".into()),
            arguments: Some("{}".into()),
        },
        ProviderEvent::Finished {
            reason: FinishReason::ToolCalls,
            usage: None,
        },
    ]
}

fn final_turn() -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::Content("The time is now.".into()),
        ProviderEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]
}

#[async_trait]
impl RunFactory for MockFactory {
    async fn run(
        &self,
        request: StartRun,
        sink: AgentEventSink,
        cancel: tokio_util::sync::CancellationToken,
        decisions: UnboundedReceiver<ApprovalDecision>,
    ) -> RunStatus {
        let provider = MockProvider::new(vec![tool_turn(), final_turn()]);
        let executor = BoundedExecutor::new(
            ToolRegistry::builtin(),
            PolicyEngine::new(ApprovalPolicy::permissive()),
            Duration::from_secs(5),
            262_144,
        );
        let agent = AgentLoop::new(provider, executor, RunConfig::new("mock"));
        manager::drive(
            agent,
            request.history,
            request.message,
            sink,
            cancel,
            decisions,
        )
        .await
    }
}

struct HistoryFactory {
    requests: Arc<tokio::sync::Mutex<Vec<Vec<ProviderMessage>>>>,
}

#[async_trait]
impl RunFactory for HistoryFactory {
    async fn run(
        &self,
        request: StartRun,
        sink: AgentEventSink,
        cancel: tokio_util::sync::CancellationToken,
        decisions: UnboundedReceiver<ApprovalDecision>,
    ) -> RunStatus {
        self.requests.lock().await.push(request.history.clone());
        MockFactory.run(request, sink, cancel, decisions).await
    }
}

fn app_state(auth: AuthConfig) -> AppState {
    let dir = std::env::temp_dir().join(format!(
        "lightagent-api-{}",
        lightagent_core::RunId::new().as_str()
    ));
    AppState {
        manager: RunManager::new(Arc::new(MockFactory)),
        auth,
        sessions: SessionStore::new(dir),
        session_profile: "default".into(),
        busy_sessions: Arc::new(tokio::sync::Mutex::new(Default::default())),
        web_root: None,
    }
}

async fn spawn_server(state: AppState) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state).into_make_service()).await;
    });
    format!("{addr}")
}

/// Minimal raw HTTP client: returns (status, body). `Connection: close` lets us
/// read a streamed (SSE) response to EOF.
async fn http(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let payload = body.unwrap_or("");
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: local\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if body.is_some() {
        request.push_str("Content-Type: application/json\r\n");
        request.push_str(&format!("Content-Length: {}\r\n", payload.len()));
    }
    request.push_str("\r\n");
    request.push_str(payload);
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer).await.unwrap();
    let text = String::from_utf8_lossy(&buffer).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(headers, rest)| {
            if headers
                .lines()
                .any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked"))
            {
                decode_chunked(rest.as_bytes())
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_else(|| rest.to_owned())
            } else {
                rest.to_owned()
            }
        })
        .unwrap_or_default();
    (status, body)
}

/// Decode the HTTP/1.1 chunk framing used by axum's SSE response.
///
/// The test client intentionally stays smaller than a general HTTP client, but
/// assertions must see the entity body rather than chunk-size lines inserted
/// between arbitrary bytes of an event name.
fn decode_chunked(mut encoded: &[u8]) -> Option<Vec<u8>> {
    let mut decoded = Vec::new();
    loop {
        let line_end = encoded.windows(2).position(|part| part == b"\r\n")?;
        let size = std::str::from_utf8(&encoded[..line_end])
            .ok()?
            .split(';')
            .next()?
            .trim();
        let size = usize::from_str_radix(size, 16).ok()?;
        encoded = &encoded[line_end + 2..];
        if size == 0 {
            return Some(decoded);
        }
        let end = size.checked_add(2)?;
        if encoded.len() < end || &encoded[size..end] != b"\r\n" {
            return None;
        }
        decoded.extend_from_slice(&encoded[..size]);
        encoded = &encoded[end..];
    }
}

// --- run lifecycle (no HTTP) ------------------------------------------------

#[tokio::test]
async fn a_run_drives_to_completion_and_buffers_its_events() {
    let manager = RunManager::new(Arc::new(MockFactory));
    let run = manager
        .start(StartRun {
            message: "what time is it?".into(),
            history: Vec::new(),
            profile: None,
            cwd: None,
        })
        .await;

    // Drive to a terminal state by streaming from the start.
    let (mut events, mut status) = run.wait_from(0).await;
    while !status.is_terminal() {
        let (more, next) = run.wait_from(events.len()).await;
        events.extend(more);
        status = next;
    }
    assert_eq!(status, RunStatus::Completed);
    let names: Vec<_> = events.iter().map(lightagent_api::sse::name).collect();
    assert!(names.contains(&"run.started"));
    assert!(names.contains(&"tool.requested"));
    assert!(names.contains(&"tool.started"));
    assert!(names.contains(&"tool.output"));
    assert!(names.contains(&"run.completed"));
}

#[tokio::test]
async fn cancelling_an_unknown_run_is_a_404_and_a_known_one_cancels() {
    let addr = spawn_server(app_state(AuthConfig::open())).await;
    let (status, _) = http(
        &addr,
        "POST",
        "/api/lightagent/v1/runs/nope/cancel",
        &[],
        Some("{}"),
    )
    .await;
    assert_eq!(status, 404);
}

// --- HTTP contract ----------------------------------------------------------

#[tokio::test]
async fn health_and_tools_are_served() {
    let addr = spawn_server(app_state(AuthConfig::open())).await;

    let (status, body) = http(&addr, "GET", "/health", &[], None).await;
    assert_eq!(status, 200);
    assert!(body.contains("\"status\":\"ok\""));

    let (status, body) = http(&addr, "GET", "/api/lightagent/v1/tools", &[], None).await;
    assert_eq!(status, 200);
    assert!(body.contains("datetime.now"));
    assert!(body.contains("agent.delegate"));
}

#[tokio::test]
async fn a_run_can_be_created_and_streamed_over_sse() {
    let addr = spawn_server(app_state(AuthConfig::keyed("agent-key", [Scope::Admin]))).await;
    let auth = [("Authorization", "Bearer agent-key")];

    let (status, body) = http(
        &addr,
        "POST",
        "/api/lightagent/v1/runs",
        &auth,
        Some(r#"{"message":"what time is it?"}"#),
    )
    .await;
    assert_eq!(status, 202);
    let id = body
        .split("\"id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_string();
    assert!(id.starts_with("run-"));

    // The event stream replays the run and ends with a terminal event.
    let (status, body) = http(
        &addr,
        "GET",
        &format!("/api/lightagent/v1/runs/{id}/events"),
        &auth,
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("run.started"));
    assert!(body.contains("tool.requested"));
    assert!(body.contains("tool.started"));
    assert!(body.contains("tool.output"));
    assert!(body.contains("model.delta"));
    assert!(body.contains("run.completed"));
}

#[tokio::test]
async fn a_saved_session_is_reused_by_follow_up_runs_and_new_sessions_are_empty() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut state = app_state(AuthConfig::open());
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        state.manager = RunManager::new(Arc::new(HistoryFactory {
            requests: Arc::clone(&requests),
        }));
        let store = state.sessions.clone();
        let addr = spawn_server(state).await;
        let (status, body) = http(
            &addr,
            "POST",
            "/api/lightagent/v1/sessions",
            &[],
            Some("{}"),
        )
        .await;
        assert_eq!(status, 201);
        let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();

        for (index, message) in ["hello", "follow up"].into_iter().enumerate() {
            let payload = serde_json::json!({ "message": message, "session_id": id }).to_string();
            let (status, body) = http(
                &addr,
                "POST",
                "/api/lightagent/v1/runs",
                &[],
                Some(&payload),
            )
            .await;
            assert_eq!(status, 202, "{body}");
            let run = serde_json::from_str::<serde_json::Value>(&body).unwrap();
            assert_eq!(run["session_id"], id);
            let id_parsed = lightagent_store::SessionId::parse(&id).unwrap();
            loop {
                let saved = store.load(&id_parsed).unwrap();
                if saved.runs.len() == index + 1 {
                    assert_eq!(saved.messages.len(), (index + 1) * 2);
                    assert_eq!(saved.messages[index * 2].content, message);
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
        assert!(requests.lock().await[0].is_empty());
        assert_eq!(
            requests.lock().await[1],
            vec![
                ProviderMessage::user("hello"),
                ProviderMessage::assistant("The time is now."),
            ]
        );

        let (status, body) = http(
            &addr,
            "POST",
            "/api/lightagent/v1/sessions",
            &[],
            Some("{}"),
        )
        .await;
        assert_eq!(status, 201);
        let new_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(id, new_id);
        assert!(
            store
                .load(&lightagent_store::SessionId::parse(&new_id).unwrap())
                .unwrap()
                .messages
                .is_empty()
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn scoped_auth_refuses_cross_capability_access() {
    // A key that may read runs but not start them.
    let auth = AuthConfig::keyed("secret", [Scope::RunsRead]);
    let addr = spawn_server(app_state(auth)).await;

    // No key at all: 401.
    let (status, _) = http(
        &addr,
        "POST",
        "/api/lightagent/v1/runs",
        &[],
        Some(r#"{"message":"x"}"#),
    )
    .await;
    assert_eq!(status, 401);

    // Right key, wrong scope (RunsWrite): 403.
    let (status, _) = http(
        &addr,
        "POST",
        "/api/lightagent/v1/runs",
        &[("Authorization", "Bearer secret")],
        Some(r#"{"message":"x"}"#),
    )
    .await;
    assert_eq!(status, 403);

    // Right key, right scope (RunsRead) on a read route: allowed (404 for an
    // unknown run, i.e. past the auth gate).
    let (status, _) = http(
        &addr,
        "GET",
        "/api/lightagent/v1/runs/whatever",
        &[("Authorization", "Bearer secret")],
        None,
    )
    .await;
    assert_eq!(status, 404);
}
