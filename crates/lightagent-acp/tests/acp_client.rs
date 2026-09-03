//! An in-process ACP client drives the server through a full prompt whose tool
//! needs approval, checking the streamed updates and the permission round-trip.

use std::sync::Arc;

use async_trait::async_trait;
use lightagent_acp::AcpServer;
use lightagent_api::manager::{RunFactory, RunManager, RunStatus, StartRun};
use lightagent_core::{
    AgentEvent, AgentEventSink, ApprovalDecision, RunId, StopReason, ToolCall, ToolOutcome,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

/// Emits a greeting, a tool call that needs approval, then a result that depends
/// on the decision it is handed.
struct MockFactory;

#[async_trait]
impl RunFactory for MockFactory {
    async fn run(
        &self,
        _request: StartRun,
        sink: AgentEventSink,
        _cancel: CancellationToken,
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
        let granted = decisions.recv().await.map(|d| d.granted).unwrap_or(false);
        if granted {
            let _ = sink.send(AgentEvent::ToolCallStarted {
                id: "t1".into(),
                name: "fs.write".into(),
            });
            let _ = sink.send(AgentEvent::ToolCallCompleted {
                id: "t1".into(),
                outcome: ToolOutcome::ok("wrote"),
            });
            let _ = sink.send(AgentEvent::Content {
                text: " done".into(),
            });
        }
        let _ = sink.send(AgentEvent::RunCompleted {
            reason: StopReason::EndTurn,
        });
        RunStatus::Completed
    }
}

#[tokio::test]
async fn full_prompt_with_approval_round_trip() {
    let (client_end, server_end) = tokio::io::duplex(1 << 16);
    let (server_read, server_write) = tokio::io::split(server_end);
    let (client_read, mut client_write) = tokio::io::split(client_end);

    let manager = RunManager::new(Arc::new(MockFactory));
    tokio::spawn(AcpServer::new(manager).serve(server_read, server_write));

    let mut reader = BufReader::new(client_read);
    async fn send(writer: &mut (impl AsyncWriteExt + Unpin), value: Value) {
        let mut line = value.to_string();
        line.push('\n');
        writer.write_all(line.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();
    }
    async fn recv(reader: &mut (impl AsyncBufReadExt + Unpin)) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    // initialize
    send(&mut client_write, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": 1 } })).await;
    let init = recv(&mut reader).await;
    assert_eq!(init["result"]["protocolVersion"], 1);

    // session/new
    send(
        &mut client_write,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {} }),
    )
    .await;
    let new = recv(&mut reader).await;
    let session_id = new["result"]["sessionId"].as_str().unwrap().to_owned();
    assert!(session_id.starts_with("acp-"));

    // session/prompt
    send(&mut client_write, json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": { "sessionId": session_id, "prompt": [ { "type": "text", "text": "please write" } ] }
    })).await;

    let mut chunks = String::new();
    let mut saw_tool_call = false;
    let mut asked_permission = false;
    let stop_reason;
    loop {
        let msg = recv(&mut reader).await;
        match msg.get("method").and_then(Value::as_str) {
            Some("session/update") => {
                let update = &msg["params"]["update"];
                match update["sessionUpdate"].as_str() {
                    Some("agent_message_chunk") => {
                        chunks.push_str(update["content"]["text"].as_str().unwrap_or(""))
                    }
                    Some("tool_call") => saw_tool_call = true,
                    _ => {}
                }
            }
            Some("session/request_permission") => {
                asked_permission = true;
                assert_eq!(msg["params"]["toolCall"]["title"], "fs.write");
                // Approve.
                send(
                    &mut client_write,
                    json!({
                        "jsonrpc": "2.0", "id": msg["id"],
                        "result": { "outcome": { "outcome": "selected", "optionId": "allow" } }
                    }),
                )
                .await;
            }
            _ => {
                // The final response to id 3.
                if msg["id"] == json!(3) {
                    stop_reason = msg["result"]["stopReason"].as_str().unwrap().to_owned();
                    break;
                }
            }
        }
    }

    assert!(saw_tool_call, "a tool_call update should be streamed");
    assert!(asked_permission, "permission should be requested");
    assert_eq!(chunks, "Hello done", "approved tool path completes");
    assert_eq!(stop_reason, "end_turn");
}
