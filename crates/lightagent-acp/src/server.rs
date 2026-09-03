//! The ACP server: drive the runtime from an editor over stdio JSON-RPC.
//!
//! One reader loop consumes incoming messages and routes them — a response to a
//! request the server made (a permission answer) goes to its waiter; a client
//! request or notification is dispatched. `session/prompt` runs on its own task
//! so the loop keeps reading (a `session/cancel` can arrive mid-prompt), starts a
//! run through the shared [`RunManager`], streams its events as `session/update`
//! notifications, and — when a tool needs approval — sends `session/request_permission`
//! and feeds the answer back into the run. A single writer task serializes all
//! outbound messages.
//!
//! Reuse over reinvention: an ACP session's prompt is exactly one managed run, so
//! streaming, approval and cancellation are the [`RunManager`]'s, not new code.
//!
//! Scope: `initialize`, `session/new`, `session/prompt`, `session/cancel`, and
//! outbound `session/request_permission`. Client-provided filesystem/terminal,
//! `session/load` and authentication are out of scope (the runtime uses its own
//! confined tools), and each prompt is an independent run — in-session
//! conversational history is not yet threaded.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use lightagent_api::manager::{RunManager, RunState, StartRun};
use lightagent_core::{AgentEvent, ApprovalDecision, ApprovalId, RunId, StopReason};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::protocol;

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

struct Session {
    profile: Option<String>,
    active: Option<Arc<RunState>>,
}

/// An ACP server over a shared run manager.
#[derive(Clone)]
pub struct AcpServer {
    manager: RunManager,
}

impl AcpServer {
    /// Build a server that starts runs through `manager`.
    pub fn new(manager: RunManager) -> Self {
        Self { manager }
    }

    /// Serve ACP until the reader reaches end of input.
    pub async fn serve<R, W>(self, read: R, write: W)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (outbound, mut rx) = mpsc::unbounded_channel::<Value>();
        let writer = tokio::spawn(async move {
            let mut write = write;
            while let Some(message) = rx.recv().await {
                let mut line = message.to_string();
                line.push('\n');
                if write.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                let _ = write.flush().await;
            }
        });

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let sessions: Arc<Mutex<HashMap<String, Session>>> = Arc::new(Mutex::new(HashMap::new()));
        let next_id = Arc::new(AtomicI64::new(1));

        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            // A response to a request we sent (a permission answer).
            if message.get("method").is_none()
                && (message.get("result").is_some() || message.get("error").is_some())
            {
                if let Some(id) = message.get("id").and_then(Value::as_i64)
                    && let Some(sender) = pending.lock().await.remove(&id)
                {
                    let _ = sender.send(message);
                }
                continue;
            }
            let Some(method) = message.get("method").and_then(Value::as_str) else {
                continue;
            };
            let id = message.get("id").cloned();
            let params = message.get("params").cloned().unwrap_or(json!({}));
            self.dispatch(method, id, params, &outbound, &pending, &sessions, &next_id)
                .await;
        }

        drop(outbound);
        let _ = writer.await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn dispatch(
        &self,
        method: &str,
        id: Option<Value>,
        params: Value,
        outbound: &mpsc::UnboundedSender<Value>,
        pending: &Pending,
        sessions: &Arc<Mutex<HashMap<String, Session>>>,
        next_id: &Arc<AtomicI64>,
    ) {
        match method {
            "initialize" => {
                if let Some(id) = id {
                    let result = json!({
                        "protocolVersion": protocol::PROTOCOL_VERSION,
                        "agentCapabilities": {
                            "promptCapabilities": { "image": false, "audio": false, "embeddedContext": false }
                        },
                        "authMethods": [],
                    });
                    let _ = outbound.send(protocol::response(id, result));
                }
            }
            "authenticate" => {
                if let Some(id) = id {
                    let _ = outbound.send(protocol::response(id, json!({})));
                }
            }
            "session/new" => {
                if let Some(id) = id {
                    let session_id = format!("acp-{}", RunId::new().as_str());
                    let profile = params
                        .get("profile")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    sessions.lock().await.insert(
                        session_id.clone(),
                        Session {
                            profile,
                            active: None,
                        },
                    );
                    let _ =
                        outbound.send(protocol::response(id, json!({ "sessionId": session_id })));
                }
            }
            "session/cancel" => {
                if let Some(session_id) = params.get("sessionId").and_then(Value::as_str)
                    && let Some(session) = sessions.lock().await.get(session_id)
                    && let Some(run) = &session.active
                {
                    run.cancel();
                }
            }
            "session/prompt" => {
                let Some(id) = id else { return };
                let Some(session_id) = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                else {
                    let _ = outbound.send(protocol::error(id, -32602, "missing sessionId"));
                    return;
                };
                let text = protocol::prompt_text(params.get("prompt").unwrap_or(&Value::Null));
                let profile = sessions
                    .lock()
                    .await
                    .get(&session_id)
                    .and_then(|session| session.profile.clone());
                let task = PromptTask {
                    manager: self.manager.clone(),
                    outbound: outbound.clone(),
                    pending: Arc::clone(pending),
                    sessions: Arc::clone(sessions),
                    next_id: Arc::clone(next_id),
                };
                tokio::spawn(task.run(id, session_id, text, profile));
            }
            other => {
                if let Some(id) = id {
                    let _ = outbound.send(protocol::error(
                        id,
                        -32601,
                        &format!("unknown method: {other}"),
                    ));
                }
            }
        }
    }
}

/// The state a spawned `session/prompt` handler carries.
struct PromptTask {
    manager: RunManager,
    outbound: mpsc::UnboundedSender<Value>,
    pending: Pending,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    next_id: Arc<AtomicI64>,
}

impl PromptTask {
    async fn run(self, req_id: Value, session_id: String, text: String, profile: Option<String>) {
        let run = self
            .manager
            .start(StartRun {
                message: text,
                profile,
            })
            .await;
        if let Some(session) = self.sessions.lock().await.get_mut(&session_id) {
            session.active = Some(Arc::clone(&run));
        }

        let mut seen = 0;
        let mut reason = StopReason::EndTurn;
        loop {
            let (events, status) = run.wait_from(seen).await;
            seen += events.len();
            for event in &events {
                if let AgentEvent::RunCompleted { reason: r } = event {
                    reason = *r;
                }
                if let Some(update) = protocol::update_for(event) {
                    let _ = self.outbound.send(protocol::notification(
                        "session/update",
                        json!({ "sessionId": session_id, "update": update }),
                    ));
                }
            }
            if let Some(approval) = run.pending().await {
                let granted = self.request_permission(&session_id, &approval).await;
                let decision = if granted {
                    ApprovalDecision::grant(ApprovalId::new())
                } else {
                    ApprovalDecision::deny(ApprovalId::new())
                };
                run.decide(decision);
            }
            if status.is_terminal() {
                break;
            }
        }

        let _ = self.outbound.send(protocol::response(
            req_id,
            json!({ "stopReason": protocol::stop_reason(&reason) }),
        ));
    }

    async fn request_permission(
        &self,
        session_id: &str,
        approval: &lightagent_api::manager::PendingApproval,
    ) -> bool {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        let params = json!({
            "sessionId": session_id,
            "toolCall": { "toolCallId": approval.approval_id, "title": approval.tool },
            "options": [
                { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
            ],
        });
        let _ = self
            .outbound
            .send(protocol::request(id, "session/request_permission", params));
        match receiver.await {
            Ok(message) => {
                let outcome = message
                    .get("result")
                    .and_then(|result| result.get("outcome"));
                let selected = outcome
                    .and_then(|outcome| outcome.get("outcome"))
                    .and_then(Value::as_str)
                    == Some("selected");
                let option = outcome
                    .and_then(|outcome| outcome.get("optionId"))
                    .and_then(Value::as_str);
                selected && option == Some("allow")
            }
            Err(_) => false,
        }
    }
}
