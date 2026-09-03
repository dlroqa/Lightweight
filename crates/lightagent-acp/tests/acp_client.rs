//! An in-process ACP client drives the server through prompts whose tool needs
//! approval, covering the grant, deny and cancel-during-approval paths.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lightagent_acp::AcpServer;
use lightagent_api::manager::{RunFactory, RunManager, RunStatus, StartRun};
use lightagent_core::{
    AgentEvent, AgentEventSink, ApprovalDecision, RunId, StopReason, ToolCall, ToolOutcome,
};
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
        json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {} }),
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
