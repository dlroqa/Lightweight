//! The run manager: drives agent runs in the background and lets HTTP handlers
//! observe them.
//!
//! Starting a run spawns a task that drives the core loop with a live event
//! sink; every event is buffered on the run and broadcast to any SSE subscriber,
//! so a client that connects late gets the history and then the live tail. A run
//! that pauses for approval records the pending request and waits on a decision
//! channel the approvals endpoint feeds; a cancel token stops both the model and
//! the tools. Building the concrete loop is left to a [`RunFactory`], so tests
//! inject a mock provider and production injects the Lightweight one.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use lightagent_core::{
    AgentEvent, AgentEventSink, AgentLoop, AgentProvider, ApprovalDecision, RunId, RunOutcome,
    StopReason, ToolInvoker,
};
use serde::Serialize;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

/// The lifecycle state of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// The run is executing.
    Running,
    /// The run is paused, waiting on an approval decision.
    AwaitingApproval,
    /// The run finished normally.
    Completed,
    /// The run was cancelled.
    Cancelled,
    /// The run ended with an error.
    Failed,
}

impl RunStatus {
    /// Whether the run has reached a terminal state.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }
}

/// What a caller asks to start.
#[derive(Clone, Debug)]
pub struct StartRun {
    pub message: String,
    pub profile: Option<String>,
}

/// The shared state of one run, observed by HTTP handlers.
pub struct RunState {
    id: String,
    events: Mutex<Vec<AgentEvent>>,
    status: Mutex<RunStatus>,
    pending: Mutex<Option<PendingApproval>>,
    notify: Notify,
    cancel: CancellationToken,
    decisions: mpsc::UnboundedSender<ApprovalDecision>,
}

/// A pending approval, as the approvals endpoint reports it.
#[derive(Clone, Debug, Serialize)]
pub struct PendingApproval {
    pub approval_id: String,
    pub tool: String,
    pub risk: String,
}

impl RunState {
    /// This run's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The current status.
    pub async fn status(&self) -> RunStatus {
        *self.status.lock().await
    }

    /// A snapshot of the events buffered so far.
    pub async fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().await.clone()
    }

    /// The pending approval, if the run is waiting for one.
    pub async fn pending(&self) -> Option<PendingApproval> {
        self.pending.lock().await.clone()
    }

    /// Cancel the run.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Submit an approval decision to a waiting run. Returns whether it was
    /// delivered (the run may already have moved on).
    pub fn decide(&self, decision: ApprovalDecision) -> bool {
        self.decisions.send(decision).is_ok()
    }

    /// Wait until an event beyond `seen` is available or the run is terminal.
    /// Returns the new events and the current status.
    pub async fn wait_from(&self, seen: usize) -> (Vec<AgentEvent>, RunStatus) {
        loop {
            {
                let events = self.events.lock().await;
                let status = *self.status.lock().await;
                if events.len() > seen || status.is_terminal() {
                    return (events[seen.min(events.len())..].to_vec(), status);
                }
            }
            self.notify.notified().await;
        }
    }

    async fn push_event(&self, event: AgentEvent) {
        self.events.lock().await.push(event);
        self.notify.notify_waiters();
    }

    async fn set_status(&self, status: RunStatus) {
        *self.status.lock().await = status;
        self.notify.notify_waiters();
    }

    async fn set_pending(&self, pending: Option<PendingApproval>) {
        *self.pending.lock().await = pending;
    }
}

/// Builds and drives the concrete agent loop for a run.
///
/// The manager is generic over how a run is executed: a test supplies a factory
/// over `MockProvider`; production supplies one over the Lightweight provider and
/// the bounded tool executor. The factory calls [`drive`] with the loop it built.
#[async_trait]
pub trait RunFactory: Send + Sync + 'static {
    /// Drive `request` to a terminal [`RunStatus`], emitting events to `sink`,
    /// obeying `cancel`, and taking approval decisions from `decisions`.
    async fn run(
        &self,
        request: StartRun,
        sink: AgentEventSink,
        cancel: CancellationToken,
        decisions: UnboundedReceiver<ApprovalDecision>,
    ) -> RunStatus;
}

/// Drive one agent loop to a terminal status, handling approval pauses.
///
/// A generic helper a [`RunFactory`] calls with the concrete loop it built. The
/// event sink rides along through a pause, so resuming keeps streaming. On a
/// provider failure before any terminal event, it emits an `Error` and a
/// terminal `RunCompleted` so a stream always ends cleanly.
pub async fn drive<P: AgentProvider, I: ToolInvoker>(
    agent: AgentLoop<P, I>,
    message: String,
    sink: AgentEventSink,
    cancel: CancellationToken,
    mut decisions: UnboundedReceiver<ApprovalDecision>,
) -> RunStatus {
    let mut outcome = match agent
        .run_streaming(message, cancel.clone(), sink.clone())
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = sink.send(AgentEvent::Error {
                message: error.to_string(),
            });
            let _ = sink.send(AgentEvent::RunCompleted {
                reason: StopReason::Error,
            });
            return RunStatus::Failed;
        }
    };

    loop {
        match outcome {
            RunOutcome::Completed { events } => return status_from_events(&events),
            RunOutcome::AwaitingApproval { suspended, .. } => {
                tokio::select! {
                    () = cancel.cancelled() => return RunStatus::Cancelled,
                    decision = decisions.recv() => match decision {
                        Some(decision) => {
                            outcome = match agent.resume(suspended, decision, cancel.clone()).await {
                                Ok(outcome) => outcome,
                                Err(error) => {
                                    let _ = sink.send(AgentEvent::Error { message: error.to_string() });
                                    let _ = sink.send(AgentEvent::RunCompleted { reason: StopReason::Error });
                                    return RunStatus::Failed;
                                }
                            };
                        }
                        None => return RunStatus::Cancelled,
                    }
                }
            }
        }
    }
}

/// Map a completed run's terminal event to a status.
fn status_from_events(events: &[AgentEvent]) -> RunStatus {
    match events.last() {
        Some(AgentEvent::RunCompleted { reason }) => match reason {
            StopReason::Cancelled => RunStatus::Cancelled,
            StopReason::Error => RunStatus::Failed,
            _ => RunStatus::Completed,
        },
        _ => RunStatus::Completed,
    }
}

/// Owns the runs and starts new ones.
#[derive(Clone)]
pub struct RunManager {
    factory: Arc<dyn RunFactory>,
    runs: Arc<Mutex<HashMap<String, Arc<RunState>>>>,
}

impl RunManager {
    /// A manager that drives runs with `factory`.
    pub fn new(factory: Arc<dyn RunFactory>) -> Self {
        Self {
            factory,
            runs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Start a run and return its shared state. The run drives in the background.
    pub async fn start(&self, request: StartRun) -> Arc<RunState> {
        let id = new_run_id();
        let (decisions_tx, decisions_rx) = mpsc::unbounded_channel();
        let state = Arc::new(RunState {
            id: id.clone(),
            events: Mutex::new(Vec::new()),
            status: Mutex::new(RunStatus::Running),
            pending: Mutex::new(None),
            notify: Notify::new(),
            cancel: CancellationToken::new(),
            decisions: decisions_tx,
        });
        self.runs.lock().await.insert(id, state.clone());

        let factory = self.factory.clone();
        let run_state = state.clone();
        let cancel = state.cancel.clone();
        tokio::spawn(async move {
            let (sink_tx, mut sink_rx) = mpsc::unbounded_channel::<AgentEvent>();

            // Forward each event onto the run's buffer, deriving the pending
            // approval and status transitions from the stream.
            let forward_state = run_state.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(event) = sink_rx.recv().await {
                    match &event {
                        AgentEvent::AwaitingApproval { id, name } => {
                            forward_state
                                .set_pending(Some(PendingApproval {
                                    approval_id: id.clone(),
                                    tool: name.clone(),
                                    risk: "".to_owned(),
                                }))
                                .await;
                            forward_state.set_status(RunStatus::AwaitingApproval).await;
                        }
                        AgentEvent::ToolCallStarted { .. } | AgentEvent::Content { .. } => {
                            // Any progress past a pause means it resolved.
                            forward_state.set_pending(None).await;
                            forward_state.set_status(RunStatus::Running).await;
                        }
                        _ => {}
                    }
                    forward_state.push_event(event).await;
                }
            });

            let status = factory.run(request, sink_tx, cancel, decisions_rx).await;
            let _ = forwarder.await;
            run_state.set_pending(None).await;
            run_state.set_status(status).await;
        });

        state
    }

    /// The run with this id, if it exists.
    pub async fn get(&self, id: &str) -> Option<Arc<RunState>> {
        self.runs.lock().await.get(id).cloned()
    }

    /// The runs currently awaiting an approval decision.
    pub async fn awaiting_approval(&self) -> Vec<Arc<RunState>> {
        let runs = self.runs.lock().await;
        let mut waiting = Vec::new();
        for state in runs.values() {
            if state.status().await == RunStatus::AwaitingApproval {
                waiting.push(state.clone());
            }
        }
        waiting
    }
}

fn new_run_id() -> String {
    RunId::new().as_str().to_owned()
}
