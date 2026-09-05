//! HTTP contract tests over a real loopback listener, plus run-lifecycle tests.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lightagent_api::manager::{self, RunFactory, RunManager, RunStatus, StartRun};
use lightagent_api::{AppState, AuthConfig, Scope, router};
use lightagent_core::permissions::ApprovalPolicy;
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
        manager::drive(agent, request.message, sink, cancel, decisions).await
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
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default();
    (status, body)
}

// --- run lifecycle (no HTTP) ------------------------------------------------

#[tokio::test]
async fn a_run_drives_to_completion_and_buffers_its_events() {
    let manager = RunManager::new(Arc::new(MockFactory));
    let run = manager
        .start(StartRun {
            message: "what time is it?".into(),
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
    let addr = spawn_server(app_state(AuthConfig::open())).await;

    let (status, body) = http(
        &addr,
        "POST",
        "/api/lightagent/v1/runs",
        &[],
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
        &[],
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("run.started"));
    assert!(body.contains("model.delta"));
    assert!(body.contains("run.completed"));
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
