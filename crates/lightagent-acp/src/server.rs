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
//! Scope: `initialize`, `session/new`, `session/load`, `session/prompt`,
//! `session/cancel`, and outbound `session/request_permission`.
//! Client-provided filesystem/terminal and authentication are out of scope (the
//! runtime uses its own confined tools). Each prompt is a distinct managed run,
//! but completed turns are threaded through the containing session.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use lightagent_api::manager::{RunManager, RunState, RunStatus, StartRun};
use lightagent_core::provider::ProviderMessage;
use lightagent_core::{
    AgentEvent, ApprovalDecision, ApprovalId, ProfileId, ProfileStore, StopReason,
};
use lightagent_store::{
    Session as StoredSession, SessionId, SessionStore, StoreError, StoredMessage,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::protocol;

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

struct Session {
    profile: Option<String>,
    /// The editor's working directory for this session (ACP `cwd`); becomes the
    /// run's confined workspace root.
    cwd: Option<String>,
    active: Option<Arc<RunState>>,
    busy: bool,
    stored: StoredSession,
    store: Option<SessionStore>,
}

/// An ACP server over a shared run manager.
#[derive(Clone)]
pub struct AcpServer {
    manager: RunManager,
    store: Option<SessionStore>,
    profiles: Option<ProfileStore>,
    default_profile: String,
}

impl AcpServer {
    /// Build a server that starts runs through `manager`.
    pub fn new(manager: RunManager) -> Self {
        Self {
            manager,
            store: None,
            profiles: None,
            default_profile: "default".to_owned(),
        }
    }

    /// Persist ACP sessions in `store` and make `session/load` available.
    pub fn with_session_store(
        mut self,
        store: SessionStore,
        default_profile: impl Into<String>,
    ) -> Self {
        self.store = Some(store);
        self.default_profile = default_profile.into();
        self
    }

    /// Enable persistent sessions for named profiles as well as the active one.
    pub fn with_profile_store(mut self, profiles: ProfileStore) -> Self {
        self.profiles = Some(profiles);
        self
    }

    fn store_for(&self, profile: &str) -> Result<Option<SessionStore>, String> {
        if profile == self.default_profile {
            return Ok(self.store.clone());
        }
        let profiles = self
            .profiles
            .as_ref()
            .ok_or("profile sessions are unavailable")?;
        let id = ProfileId::new(profile).map_err(|error| error.to_string())?;
        profiles.load(&id).map_err(|error| error.to_string())?;
        Ok(Some(SessionStore::at_profile(&profiles.handle(&id))))
    }

    fn load_session(&self, id: &SessionId) -> Result<(StoredSession, SessionStore), String> {
        let store = self.store.as_ref().ok_or("session/load is disabled")?;
        match store.load(id) {
            Ok(session) => return Ok((session, store.clone())),
            Err(StoreError::NotFound(_)) => {}
            Err(error) => return Err(error.to_string()),
        }
        if let Some(profiles) = &self.profiles {
            for profile in profiles.list().map_err(|error| error.to_string())? {
                if profile.as_str() == self.default_profile {
                    continue;
                }
                let candidate = SessionStore::at_profile(&profiles.handle(&profile));
                match candidate.load(id) {
                    Ok(session) => return Ok((session, candidate)),
                    Err(StoreError::NotFound(_)) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        Err(StoreError::NotFound(id.as_str().to_owned()).to_string())
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
                    // Negotiate: speak the lower of our version and the client's.
                    let requested = params
                        .get("protocolVersion")
                        .and_then(Value::as_u64)
                        .unwrap_or(protocol::PROTOCOL_VERSION as u64);
                    let negotiated = requested.min(protocol::PROTOCOL_VERSION as u64);
                    let result = json!({
                        "protocolVersion": negotiated,
                        "agentCapabilities": {
                            "loadSession": self.store.is_some(),
                            "promptCapabilities": { "image": false, "audio": false, "embeddedContext": false }
                        },
                        "agentInfo": { "name": "lightagent", "version": env!("CARGO_PKG_VERSION") },
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
                    // `profile` is a Lightagent extension; `cwd` is the ACP
                    // working directory (mcpServers are accepted but ignored —
                    // the runtime uses its own configured MCP servers).
                    let profile = params
                        .get("profile")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let cwd = params.get("cwd").and_then(Value::as_str).map(str::to_owned);
                    let stored_profile = profile.as_deref().unwrap_or(&self.default_profile);
                    let store = match self.store_for(stored_profile) {
                        Ok(store) => store,
                        Err(error) => {
                            let _ = outbound.send(protocol::error(id, -32602, &error));
                            return;
                        }
                    };
                    let mut stored = StoredSession::new(stored_profile, "ACP session");
                    stored.cwd = cwd.clone();
                    let session_id = stored.id.as_str().to_owned();
                    if let Some(store) = &store
                        && let Err(error) = store.save(&stored)
                    {
                        let _ = outbound.send(protocol::error(
                            id,
                            -32603,
                            &format!("could not save session: {error}"),
                        ));
                        return;
                    }
                    sessions.lock().await.insert(
                        session_id.clone(),
                        Session {
                            profile,
                            cwd,
                            active: None,
                            busy: false,
                            stored,
                            store,
                        },
                    );
                    let _ =
                        outbound.send(protocol::response(id, json!({ "sessionId": session_id })));
                }
            }
            "session/load" => {
                let Some(id) = id else { return };
                let Some(_) = &self.store else {
                    let _ = outbound.send(protocol::error(id, -32601, "session/load is disabled"));
                    return;
                };
                let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
                    let _ = outbound.send(protocol::error(id, -32602, "missing sessionId"));
                    return;
                };
                let parsed = match SessionId::parse(session_id) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        let _ = outbound.send(protocol::error(id, -32602, &error.to_string()));
                        return;
                    }
                };
                let (stored, store) = match self.load_session(&parsed) {
                    Ok(found) => found,
                    Err(error) => {
                        let _ = outbound.send(protocol::error(id, -32602, &error.to_string()));
                        return;
                    }
                };
                let cwd = params.get("cwd").and_then(Value::as_str).map(str::to_owned);
                if stored.cwd != cwd {
                    let _ = outbound.send(protocol::error(
                        id,
                        -32602,
                        "session working directory does not match",
                    ));
                    return;
                }
                let profile = Some(stored.profile.clone());
                let mut open = sessions.lock().await;
                if open.get(session_id).is_some_and(|session| session.busy) {
                    let _ =
                        outbound.send(protocol::error(id, -32602, "session has an active prompt"));
                    return;
                }
                open.insert(
                    session_id.to_owned(),
                    Session {
                        profile,
                        cwd,
                        active: None,
                        busy: false,
                        stored: stored.clone(),
                        store: Some(store),
                    },
                );
                drop(open);
                for message in &stored.messages {
                    let session_update = match message.role.as_str() {
                        "user" => "user_message_chunk",
                        "assistant" => "agent_message_chunk",
                        _ => continue,
                    };
                    let _ = outbound.send(protocol::notification(
                        "session/update",
                        json!({
                            "sessionId": session_id,
                            "update": {
                                "sessionUpdate": session_update,
                                "content": { "type": "text", "text": message.content }
                            }
                        }),
                    ));
                }
                let _ = outbound.send(protocol::response(id, json!({})));
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
                let (profile, cwd, history) = match sessions.lock().await.get_mut(&session_id) {
                    Some(session) if session.busy => {
                        let _ = outbound.send(protocol::error(
                            id,
                            -32602,
                            &format!("session already has an active prompt: {session_id}"),
                        ));
                        return;
                    }
                    Some(session) => {
                        let history = model_history(&session.stored);
                        let mut updated = session.stored.clone();
                        updated.push_message(StoredMessage::new("user", &text));
                        if let Some(store) = &session.store
                            && let Err(error) = store.save(&updated)
                        {
                            let _ = outbound.send(protocol::error(
                                id,
                                -32603,
                                &format!("could not save session: {error}"),
                            ));
                            return;
                        }
                        session.stored = updated;
                        session.busy = true;
                        (session.profile.clone(), session.cwd.clone(), history)
                    }
                    None => {
                        let _ = outbound.send(protocol::error(
                            id,
                            -32602,
                            &format!("unknown session: {session_id}"),
                        ));
                        return;
                    }
                };
                let task = PromptTask {
                    manager: self.manager.clone(),
                    outbound: outbound.clone(),
                    pending: Arc::clone(pending),
                    sessions: Arc::clone(sessions),
                    next_id: Arc::clone(next_id),
                };
                tokio::spawn(task.run(id, session_id, text, history, profile, cwd));
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

fn model_history(session: &StoredSession) -> Vec<ProviderMessage> {
    session
        .messages
        .iter()
        .filter_map(|message| match message.role.as_str() {
            "user" => Some(ProviderMessage::user(message.content.clone())),
            "assistant" => Some(ProviderMessage::assistant(message.content.clone())),
            _ => None,
        })
        .collect()
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
    async fn run(
        self,
        req_id: Value,
        session_id: String,
        text: String,
        history: Vec<ProviderMessage>,
        profile: Option<String>,
        cwd: Option<String>,
    ) {
        let run = self
            .manager
            .start(StartRun {
                message: text,
                history,
                profile,
                cwd,
            })
            .await;
        let cancel = run.cancel_token();
        if let Some(session) = self.sessions.lock().await.get_mut(&session_id) {
            session.active = Some(Arc::clone(&run));
        }

        let mut seen = 0;
        let mut completed_reason = StopReason::EndTurn;
        // Approvals already answered, by id, so a pause that has not yet cleared
        // (a denied tool does not reset it) is never re-requested.
        let mut handled: HashSet<String> = HashSet::new();
        let final_status = loop {
            let (events, status) = run.wait_from(seen).await;
            seen += events.len();
            for event in &events {
                if let AgentEvent::RunCompleted { reason } = event {
                    completed_reason = *reason;
                }
                if let Some(update) = protocol::update_for(event) {
                    let _ = self.outbound.send(protocol::notification(
                        "session/update",
                        json!({ "sessionId": session_id, "update": update }),
                    ));
                }
            }
            if status.is_terminal() {
                break status;
            }
            if status == RunStatus::AwaitingApproval
                && let Some(approval) = run.pending().await
                && handled.insert(approval.approval_id.clone())
            {
                let granted = self
                    .request_permission(&session_id, &approval, &cancel)
                    .await;
                let decision = if granted {
                    ApprovalDecision::grant(ApprovalId::new())
                } else {
                    ApprovalDecision::deny(ApprovalId::new())
                };
                run.decide(decision);
            }
        };

        // The terminal status is authoritative — a cancel during an approval pause
        // returns Cancelled with no RunCompleted event to read.
        let reason = match final_status {
            RunStatus::Cancelled => "cancelled",
            RunStatus::Failed => "refusal",
            _ => protocol::stop_reason(&completed_reason),
        };
        let events = run.events().await;

        // Release the finished run so a later cancel is not aimed at it, and
        // atomically persist the newly completed assistant turn.
        let mut save_error = None;
        if let Some(session) = self.sessions.lock().await.get_mut(&session_id) {
            session.stored.record_run_events(&events, reason);
            if let Some(store) = &session.store
                && let Err(error) = store.save(&session.stored)
            {
                save_error = Some(error.to_string());
            }
            session.active = None;
            session.busy = false;
        }
        if let Some(error) = save_error {
            let _ = self.outbound.send(protocol::error(
                req_id,
                -32603,
                &format!("could not save session: {error}"),
            ));
            return;
        }
        let _ = self
            .outbound
            .send(protocol::response(req_id, json!({ "stopReason": reason })));
    }

    async fn request_permission(
        &self,
        session_id: &str,
        approval: &lightagent_api::manager::PendingApproval,
        cancel: &CancellationToken,
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
        // Wake on the answer or on cancellation, so a cancel (or a client that
        // will never answer) cannot park this task forever.
        tokio::select! {
            result = receiver => match result {
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
            },
            _ = cancel.cancelled() => {
                // Drop the waiter so a late answer is not routed to a gone task.
                self.pending.lock().await.remove(&id);
                false
            }
        }
    }
}
