//! An in-process ACP client drives the server through prompts whose tool needs
//! approval, covering the grant, deny and cancel-during-approval paths.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lightagent_acp::AcpServer;
use lightagent_api::manager::{RunFactory, RunManager, RunStatus, StartRun};
use lightagent_core::provider::ProviderMessage;
use lightagent_core::{
    AgentEvent, AgentEventSink, AgentProfile, ApprovalDecision, ProfileId, ProfileStore, RunId,
    StopReason, ToolCall, ToolOutcome,
};
use lightagent_store::{SessionId, SessionStore};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

/// Streams a greeting and a tool that needs approval, then reacts to the actual
/// decision (grant/deny) — or to cancellation, returning Cancelled with no
/// RunCompleted event, exactly as the real run manager's `drive` does.
struct MockFactory;

#[async_trait]
impl RunFactory for MockFactory {
    async fn run(
        &self,
        _request: StartRun,
        sink: AgentEventSink,
        cancel: CancellationToken,
        mut decisions: UnboundedReceiver<ApprovalDecision>,
    ) -> RunStatus {
        let _ = sink.send(AgentEvent::RunStarted {
            run: RunId::new(),
            parent: None,
        });
        let _ = sink.send(AgentEvent::Content {
            text: "Hello".into(),
        });
        let _ = sink.send(AgentEvent::ToolCallRequested {
            call: ToolCall {
                id: "t1".into(),
                name: "fs.write".into(),
                arguments: "{}".into(),
            },
        });
        let _ = sink.send(AgentEvent::AwaitingApproval {
            id: "t1".into(),
            name: "fs.write".into(),
        });
        tokio::select! {
            decision = decisions.recv() => {
                let granted = decision.map(|d| d.granted).unwrap_or(false);
                if granted {
                    let _ = sink.send(AgentEvent::ToolCallStarted { id: "t1".into(), name: "fs.write".into() });
                    let _ = sink.send(AgentEvent::ToolCallCompleted { id: "t1".into(), outcome: ToolOutcome::ok("wrote") });
                    let _ = sink.send(AgentEvent::Content { text: " done".into() });
                } else {
                    // A denied tool reports a controlled error, then the model ends.
                    let _ = sink.send(AgentEvent::ToolCallCompleted { id: "t1".into(), outcome: ToolOutcome::error("denied") });
                    let _ = sink.send(AgentEvent::Content { text: " denied".into() });
                }
                let _ = sink.send(AgentEvent::RunCompleted { reason: StopReason::EndTurn });
                RunStatus::Completed
            }
            _ = cancel.cancelled() => RunStatus::Cancelled,
        }
    }
}

type Reader = BufReader<ReadHalf<tokio::io::DuplexStream>>;
type Writer = WriteHalf<tokio::io::DuplexStream>;

async fn send(writer: &mut Writer, value: Value) {
    let mut line = value.to_string();
    line.push('\n');
    writer.write_all(line.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();
}

async fn recv(reader: &mut Reader) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Start a server, run initialize + session/new, and return the client ends plus
/// the session id.
async fn connect() -> (Writer, Reader, String) {
    let (client_end, server_end) = tokio::io::duplex(1 << 16);
    let (server_read, server_write) = tokio::io::split(server_end);
    let (client_read, mut client_write) = tokio::io::split(client_end);
    let manager = RunManager::new(Arc::new(MockFactory));
    tokio::spawn(AcpServer::new(manager).serve(server_read, server_write));
    let mut reader = BufReader::new(client_read);

    send(&mut client_write, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": 1 } })).await;
    assert_eq!(recv(&mut reader).await["result"]["protocolVersion"], 1);
    send(
        &mut client_write,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": { "cwd": "/tmp/project", "mcpServers": [] } }),
    )
    .await;
    let session_id = recv(&mut reader).await["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    (client_write, reader, session_id)
}

#[tokio::test]
async fn approve_completes_the_tool() {
    let (mut writer, mut reader, session_id) = connect().await;
    send(
        &mut writer,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": { "sessionId": session_id, "prompt": [ { "type": "text", "text": "go" } ] } }),
    )
    .await;

    let mut chunks = String::new();
    let mut permission_requests = 0;
    let stop;
    loop {
        let msg = recv(&mut reader).await;
        match msg.get("method").and_then(Value::as_str) {
            Some("session/update") => {
                let update = &msg["params"]["update"];
                if update["sessionUpdate"] == "agent_message_chunk" {
                    chunks.push_str(update["content"]["text"].as_str().unwrap_or(""));
                }
            }
            Some("session/request_permission") => {
                permission_requests += 1;
                send(
                    &mut writer,
                    json!({ "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
                        "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "overlap" }] } }),
                )
                .await;
                assert_eq!(recv(&mut reader).await["error"]["code"], -32602);
                send(
                    &mut writer,
                    json!({ "jsonrpc": "2.0", "id": msg["id"],
                    "result": { "outcome": { "outcome": "selected", "optionId": "allow" } } }),
                )
                .await;
            }
            _ if msg["id"] == json!(3) => {
                stop = msg["result"]["stopReason"].as_str().unwrap().to_owned();
                break;
            }
            _ => {}
        }
    }
    assert_eq!(chunks, "Hello done");
    assert_eq!(permission_requests, 1);
    assert_eq!(stop, "end_turn");
}

#[tokio::test]
async fn deny_reports_the_error_and_asks_once() {
    let (mut writer, mut reader, session_id) = connect().await;
    send(
        &mut writer,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": { "sessionId": session_id, "prompt": [ { "type": "text", "text": "go" } ] } }),
    )
    .await;

    let mut chunks = String::new();
    let mut permission_requests = 0;
    let stop;
    loop {
        let msg = recv(&mut reader).await;
        match msg.get("method").and_then(Value::as_str) {
            Some("session/update") => {
                let update = &msg["params"]["update"];
                if update["sessionUpdate"] == "agent_message_chunk" {
                    chunks.push_str(update["content"]["text"].as_str().unwrap_or(""));
                }
            }
            Some("session/request_permission") => {
                permission_requests += 1;
                send(
                    &mut writer,
                    json!({ "jsonrpc": "2.0", "id": msg["id"],
                    "result": { "outcome": { "outcome": "selected", "optionId": "reject" } } }),
                )
                .await;
            }
            _ if msg["id"] == json!(3) => {
                stop = msg["result"]["stopReason"].as_str().unwrap().to_owned();
                break;
            }
            _ => {}
        }
    }
    assert!(
        chunks.contains("denied"),
        "the deny path is reported: {chunks:?}"
    );
    assert_eq!(permission_requests, 1, "a denied tool is not re-requested");
    assert_eq!(stop, "end_turn");
}

#[tokio::test]
async fn cancel_during_approval_reports_cancelled_without_hanging() {
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let (mut writer, mut reader, session_id) = connect().await;
        send(&mut writer, json!({ "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": { "sessionId": session_id, "prompt": [ { "type": "text", "text": "go" } ] } })).await;

        loop {
            let msg = recv(&mut reader).await;
            match msg.get("method").and_then(Value::as_str) {
                Some("session/request_permission") => {
                    // Cancel instead of answering.
                    send(&mut writer, json!({ "jsonrpc": "2.0", "method": "session/cancel",
                        "params": { "sessionId": session_id } })).await;
                }
                _ if msg["id"] == json!(3) => {
                    return msg["result"]["stopReason"].as_str().unwrap().to_owned();
                }
                _ => {}
            }
        }
    })
    .await;
    assert_eq!(result.unwrap(), "cancelled");
}

#[tokio::test]
async fn negotiates_version_and_errors_on_unknown_session() {
    let (client_end, server_end) = tokio::io::duplex(1 << 16);
    let (server_read, server_write) = tokio::io::split(server_end);
    let (client_read, mut writer) = tokio::io::split(client_end);
    let manager = RunManager::new(Arc::new(MockFactory));
    tokio::spawn(AcpServer::new(manager).serve(server_read, server_write));
    let mut reader = BufReader::new(client_read);

    // A client asking for a higher version is answered with the negotiated one.
    send(&mut writer, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": 99 } })).await;
    assert_eq!(recv(&mut reader).await["result"]["protocolVersion"], 1);

    // A prompt to a session that was never opened is a controlled error.
    send(
        &mut writer,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
        "params": { "sessionId": "never-made", "prompt": [] } }),
    )
    .await;
    let error = recv(&mut reader).await;
    assert_eq!(error["error"]["code"], -32602);
}

struct HistoryFactory {
    requests: Arc<tokio::sync::Mutex<Vec<StartRun>>>,
}

#[async_trait]
impl RunFactory for HistoryFactory {
    async fn run(
        &self,
        request: StartRun,
        sink: AgentEventSink,
        _cancel: CancellationToken,
        _decisions: UnboundedReceiver<ApprovalDecision>,
    ) -> RunStatus {
        let mut requests = self.requests.lock().await;
        let reply = format!("answer {}", requests.len() + 1);
        requests.push(request);
        let _ = sink.send(AgentEvent::RunStarted {
            run: RunId::new(),
            parent: None,
        });
        let _ = sink.send(AgentEvent::Content { text: reply });
        let _ = sink.send(AgentEvent::RunCompleted {
            reason: StopReason::EndTurn,
        });
        RunStatus::Completed
    }
}

async fn open_history_client(manager: RunManager, store: SessionStore) -> (Writer, Reader) {
    let (client_end, server_end) = tokio::io::duplex(1 << 16);
    let (server_read, server_write) = tokio::io::split(server_end);
    let (client_read, mut writer) = tokio::io::split(client_end);
    tokio::spawn(
        AcpServer::new(manager)
            .with_session_store(store, "default")
            .serve(server_read, server_write),
    );
    let mut reader = BufReader::new(client_read);
    send(
        &mut writer,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
    )
    .await;
    assert_eq!(
        recv(&mut reader).await["result"]["agentCapabilities"]["loadSession"],
        true
    );
    (writer, reader)
}

async fn prompt_and_finish(
    writer: &mut Writer,
    reader: &mut Reader,
    id: i64,
    session: &str,
    text: &str,
) {
    send(
        writer,
        json!({ "jsonrpc": "2.0", "id": id, "method": "session/prompt",
        "params": { "sessionId": session, "prompt": [{ "type": "text", "text": text }] } }),
    )
    .await;
    loop {
        let message = recv(reader).await;
        if message["id"] == json!(id) {
            assert_eq!(message["result"]["stopReason"], "end_turn");
            return;
        }
    }
}

#[tokio::test]
async fn history_is_scoped_to_session_and_survives_reopening_the_editor() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let directory =
            std::env::temp_dir().join(format!("lightagent-acp-{}", SessionId::generate().as_str()));
        let store = SessionStore::new(&directory);
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let manager = RunManager::new(Arc::new(HistoryFactory {
            requests: Arc::clone(&requests),
        }));
        let (mut writer, mut reader) = open_history_client(manager.clone(), store.clone()).await;

        send(
            &mut writer,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": { "cwd": "/tmp/project", "mcpServers": [] } }),
        )
        .await;
        let first = recv(&mut reader).await["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        prompt_and_finish(&mut writer, &mut reader, 3, &first, "hello").await;
        prompt_and_finish(&mut writer, &mut reader, 4, &first, "follow up").await;
        send(
            &mut writer,
            json!({ "jsonrpc": "2.0", "id": 9, "method": "session/load",
            "params": { "sessionId": first, "cwd": "/tmp/another-project", "mcpServers": [] } }),
        )
        .await;
        assert_eq!(recv(&mut reader).await["error"]["code"], -32602);
        assert_eq!(
            requests.lock().await[1].history,
            vec![
                ProviderMessage::user("hello"),
                ProviderMessage::assistant("answer 1")
            ]
        );

        // A new session has no access to the first one's context.
        send(
            &mut writer,
            json!({ "jsonrpc": "2.0", "id": 5, "method": "session/new",
            "params": { "cwd": "/tmp/project", "mcpServers": [] } }),
        )
        .await;
        let fresh = recv(&mut reader).await["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(first, fresh);
        prompt_and_finish(&mut writer, &mut reader, 6, &fresh, "fresh").await;
        assert!(requests.lock().await[2].history.is_empty());

        drop(writer);
        drop(reader);
        let (mut writer, mut reader) =
            open_history_client(manager, SessionStore::new(&directory)).await;
        send(
            &mut writer,
            json!({ "jsonrpc": "2.0", "id": 7, "method": "session/load",
            "params": { "sessionId": first, "cwd": "/tmp/project", "mcpServers": [] } }),
        )
        .await;
        let mut replay = Vec::new();
        loop {
            let message = recv(&mut reader).await;
            if message["id"] == json!(7) {
                assert_eq!(message["result"], json!({}));
                break;
            }
            replay.push((
                message["params"]["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                message["params"]["update"]["content"]["text"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            ));
        }
        assert_eq!(
            replay,
            vec![
                ("user_message_chunk".into(), "hello".into()),
                ("agent_message_chunk".into(), "answer 1".into()),
                ("user_message_chunk".into(), "follow up".into()),
                ("agent_message_chunk".into(), "answer 2".into()),
            ]
        );
        prompt_and_finish(&mut writer, &mut reader, 8, &first, "after restart").await;
        assert_eq!(requests.lock().await[3].history.len(), 4);
        let persisted = store.load(&SessionId::parse(&first).unwrap()).unwrap();
        assert_eq!(persisted.messages.len(), 6);
        assert_eq!(persisted.runs.len(), 3);
        let _ = std::fs::remove_dir_all(&directory);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn named_profile_sessions_are_stored_and_loaded_in_their_own_profile() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let directory = std::env::temp_dir().join(format!(
            "lightagent-acp-profiles-{}",
            SessionId::generate().as_str()
        ));
        let profiles = ProfileStore::new(&directory);
        let profile_id = ProfileId::new("research").unwrap();
        profiles
            .create(&AgentProfile::new(profile_id.clone(), "Research", "", "m"))
            .unwrap();
        let store = SessionStore::at_profile(&profiles.handle(&ProfileId::new("default").unwrap()));
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let manager = RunManager::new(Arc::new(HistoryFactory {
            requests: Arc::clone(&requests),
        }));
        let (client_end, server_end) = tokio::io::duplex(1 << 16);
        let (server_read, server_write) = tokio::io::split(server_end);
        let (client_read, mut writer) = tokio::io::split(client_end);
        tokio::spawn(
            AcpServer::new(manager.clone())
                .with_session_store(store.clone(), "default")
                .with_profile_store(profiles.clone())
                .serve(server_read, server_write),
        );
        let mut reader = BufReader::new(client_read);

        send(
            &mut writer,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": { "profile": "research", "cwd": "/tmp/project", "mcpServers": [] } }),
        )
        .await;
        let id = recv(&mut reader).await["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        prompt_and_finish(&mut writer, &mut reader, 2, &id, "hi").await;
        assert_eq!(
            requests.lock().await[0].profile.as_deref(),
            Some("research")
        );
        assert!(store.load(&SessionId::parse(&id).unwrap()).is_err());
        assert_eq!(
            SessionStore::at_profile(&profiles.handle(&profile_id))
                .load(&SessionId::parse(&id).unwrap())
                .unwrap()
                .messages
                .len(),
            2
        );

        drop(writer);
        drop(reader);
        let (client_end, server_end) = tokio::io::duplex(1 << 16);
        let (server_read, server_write) = tokio::io::split(server_end);
        let (client_read, mut writer) = tokio::io::split(client_end);
        tokio::spawn(
            AcpServer::new(manager)
                .with_session_store(store, "default")
                .with_profile_store(profiles)
                .serve(server_read, server_write),
        );
        let mut reader = BufReader::new(client_read);
        send(
            &mut writer,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "session/load",
            "params": { "sessionId": id, "cwd": "/tmp/project", "mcpServers": [] } }),
        )
        .await;
        loop {
            if recv(&mut reader).await["id"] == json!(3) {
                break;
            }
        }
        prompt_and_finish(&mut writer, &mut reader, 4, &id, "again").await;
        assert_eq!(requests.lock().await[1].history.len(), 2);
        let _ = std::fs::remove_dir_all(&directory);
    })
    .await
    .unwrap();
}
