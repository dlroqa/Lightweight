//! The agent loop.
//!
//! One async state machine drives a run from the first event to the terminal
//! one. It seeds the conversation, streams a model turn, and either finishes on
//! a final answer or, when the model asks for tools, takes each call through the
//! [`ToolInvoker`]: it asks the invoker whether the call may run
//! ([`approval_for`]), and then either runs it, refuses it with a controlled
//! tool-error result the model still sees, or — when a human decision is needed
//! — emits [`AgentEvent::AwaitingApproval`] and **pauses**, returning a
//! [`Suspended`] the caller resumes once a decision arrives. Every axis of
//! [`RunLimits`] bounds the run, including repeated-identical-call detection
//! that stops a model stuck in a loop.
//!
//! The wall-clock budget is checked at each turn boundary, so a turn or tool
//! already in flight always finishes. When the budget runs out right after a
//! tool batch, the model has not read those results yet, and ending there
//! would throw away the work. [`WallClockPolicy`] decides what happens
//! instead: take one final turn with tools withheld so the model answers from
//! what it gathered, or pause the run ([`RunOutcome::OutOfTime`]) so an
//! interactive caller can ask whether to continue.
//!
//! [`approval_for`]: crate::invoker::ToolInvoker::approval_for

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use futures_util::stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::event::{AgentEvent, StopReason};
use crate::ids::RunId;
use crate::invoker::{ToolCall, ToolInvoker, ToolOutcome};
use crate::limits::RunLimits;
use crate::permissions::{ApprovalDecision, ApprovalNeed, ApprovalRequest};
use crate::profile::AgentProfile;
use crate::provider::{
    AgentProvider, FinishReason, ProviderEvent, ProviderMessage, ProviderRequest, ProviderToolCall,
    Role, Usage,
};
use crate::tool_stream::ToolCallAccumulator;

/// A channel a streaming run sends each [`AgentEvent`] to as it happens.
pub type AgentEventSink = tokio::sync::mpsc::UnboundedSender<AgentEvent>;

/// The instruction added to the final, tool-less turn a run takes when its
/// wall-clock budget runs out with tool results the model has not read yet.
/// Sent only with that one request, never kept in the conversation.
const WRAP_UP_INSTRUCTION: &str = "The time budget for this request has run out, so no more \
    tools can be called. Answer the original request now, as well as you can, from the tool \
    results above, and say briefly if anything is incomplete.";

/// What a run does when its wall-clock budget runs out while tool results the
/// model has not read yet are waiting.
///
/// Either way the budget stays a turn-boundary check: nothing in flight is cut
/// short. A run with no unread results (only possible before its first turn)
/// simply ends with [`StopReason::WallClockExceeded`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WallClockPolicy {
    /// Take one final turn with tools withheld so the model answers from what
    /// it already gathered, then end with [`StopReason::WallClockExceeded`].
    /// The default: right for callers with nobody to ask.
    #[default]
    WrapUp,
    /// Pause the run and return [`RunOutcome::OutOfTime`], so an interactive
    /// caller can ask whether to continue it
    /// ([`AgentLoop::continue_out_of_time`]).
    Pause,
}

/// How to continue a run that paused on its wall-clock budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Continuation {
    /// Restart the wall clock and carry on with tools, exactly where the run
    /// stopped. Only time is renewed: the turn and tool-call budgets still
    /// count across the whole run.
    Extend,
    /// Take the final, tool-less turn now, so the model answers from what it
    /// already gathered, and end.
    WrapUp,
}

/// What a run needs beyond its provider and invoker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunConfig {
    /// The model id sent to the provider.
    pub model: String,
    /// The system prompt seeded as the first message, if any.
    pub system: Option<String>,
    /// The bounds enforced across the run.
    pub limits: RunLimits,
    /// The orchestrator run, for a delegated child (Slice 3/4).
    pub parent: Option<RunId>,
    /// What happens when the wall-clock budget runs out with unread tool
    /// results.
    pub on_wall_clock: WallClockPolicy,
}

impl RunConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: None,
            limits: RunLimits::default(),
            parent: None,
            on_wall_clock: WallClockPolicy::default(),
        }
    }
}

/// Why the loop itself could not proceed (distinct from a run that ended for a
/// [`StopReason`]).
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The provider could not start or complete a turn.
    #[error(transparent)]
    Provider(#[from] crate::provider::ProviderError),
    /// A decision was not for the approval this run is waiting on.
    #[error("approval decision does not match the pending tool call")]
    InvalidApproval,
}

/// The result of driving a run until it either finished or paused.
///
/// Each variant carries the *cumulative* event log up to that point, so a
/// caller can render everything that has happened without stitching fragments
/// together: [`run`](AgentLoop::run) returns the log through the pause, and
/// [`resume`](AgentLoop::resume) returns the whole log through completion.
#[derive(Debug)]
pub enum RunOutcome {
    /// The run reached a terminal state; the last event is a
    /// [`AgentEvent::RunCompleted`].
    Completed { events: Vec<AgentEvent> },
    /// The run paused awaiting a human decision on `request`. Resume it with
    /// [`AgentLoop::resume`], passing `suspended`.
    AwaitingApproval {
        events: Vec<AgentEvent>,
        request: ApprovalRequest,
        suspended: Box<Suspended>,
    },
    /// The run used up its wall-clock budget, `elapsed` so far, with tool
    /// results the model has not read yet, and paused instead of ending. Only
    /// returned under [`WallClockPolicy::Pause`]; the last event is an
    /// [`AgentEvent::WallClockPaused`]. Continue it with
    /// [`AgentLoop::continue_out_of_time`], passing `suspended`, or drop it.
    OutOfTime {
        events: Vec<AgentEvent>,
        elapsed: Duration,
        suspended: Box<Suspended>,
    },
}

impl RunOutcome {
    /// The cumulative event log for this outcome.
    pub fn events(&self) -> &[AgentEvent] {
        match self {
            Self::Completed { events }
            | Self::AwaitingApproval { events, .. }
            | Self::OutOfTime { events, .. } => events,
        }
    }

    /// Take the cumulative event log, whichever variant this is.
    pub fn into_events(self) -> Vec<AgentEvent> {
        match self {
            Self::Completed { events }
            | Self::AwaitingApproval { events, .. }
            | Self::OutOfTime { events, .. } => events,
        }
    }

    /// Whether the run finished (rather than paused).
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }

    /// The pending request, when the run is awaiting approval.
    pub fn approval_request(&self) -> Option<&ApprovalRequest> {
        match self {
            Self::AwaitingApproval { request, .. } => Some(request),
            Self::Completed { .. } | Self::OutOfTime { .. } => None,
        }
    }
}

/// A paused run, resumable once a decision (or, for a run out of time, a
/// [`Continuation`]) is available.
///
/// Opaque by design: it carries exactly the state the loop needs to continue —
/// the conversation so far, the run's bounds and counters, and the tool batch
/// it stopped inside — and nothing a caller should reach into.
#[derive(Debug)]
pub struct Suspended {
    driver: Driver,
    approval: Option<ApprovalRequest>,
}

/// The tools a single model turn asked for, and how far through them the loop
/// has processed.
#[derive(Clone, Debug)]
struct Batch {
    calls: Vec<ToolCall>,
    next: usize,
}

/// The whole mutable state of a run in flight.
#[derive(Clone, Debug)]
struct Driver {
    started: Instant,
    limits: RunLimits,
    model: String,
    messages: Vec<ProviderMessage>,
    events: Vec<AgentEvent>,
    turn: u32,
    tool_calls_made: u32,
    /// How many times each identical `(name, arguments)` call has been seen, to
    /// detect a model stuck repeating itself.
    call_counts: BTreeMap<String, u32>,
    /// The tool batch in progress, if the last turn asked for tools.
    pending: Option<Batch>,
    /// When set, each emitted event is also sent here live, for a streaming
    /// caller. Persisted across a suspend so a resume keeps streaming.
    sink: Option<AgentEventSink>,
    /// Set once the wall clock has run out and the run owes its final,
    /// tool-less answer turn.
    wrap_up: bool,
}

impl Driver {
    /// Record an event: append it to the run's log, and — when a live sink is
    /// attached — send a copy to it. A send to a closed receiver is ignored, so
    /// a client that walked away never affects the run.
    fn emit(&mut self, event: AgentEvent) {
        if let Some(sink) = &self.sink {
            let _ = sink.send(event.clone());
        }
        self.events.push(event);
    }
}

/// What one model turn produced.
enum TurnResult {
    /// The run ended; its terminal event is already in the log.
    Finished(Box<RunOutcome>),
    /// The model asked for tools; `driver.pending` now holds the batch.
    Batch,
    /// The wall clock ran out with unread tool results, and the run pauses.
    OutOfTime(Duration),
}

/// What processing a tool batch produced.
enum BatchResult {
    /// Every call in the batch was handled.
    Done,
    /// A limit ended the run; its terminal event is already in the log.
    Completed,
    /// A call needs a human decision; the run pauses on `request`.
    Suspended(ApprovalRequest),
}

/// The agent loop, bound to a provider and an invoker.
pub struct AgentLoop<P: AgentProvider, I: ToolInvoker> {
    provider: P,
    invoker: I,
    config: RunConfig,
}

impl<P: AgentProvider, I: ToolInvoker> AgentLoop<P, I> {
    pub fn new(provider: P, invoker: I, config: RunConfig) -> Self {
        Self {
            provider,
            invoker,
            config,
        }
    }

    /// Seed a run from a profile: persona as the system prompt, and the model
    /// and limits from the profile's routing.
    pub fn from_profile(provider: P, invoker: I, profile: &AgentProfile) -> Self {
        let system = if profile.persona.is_empty() {
            None
        } else {
            Some(profile.persona.clone())
        };
        let config = RunConfig {
            model: profile.routing.model.clone(),
            system,
            limits: profile.limits,
            parent: None,
            on_wall_clock: WallClockPolicy::default(),
        };
        Self::new(provider, invoker, config)
    }

    /// Choose what a run does when its wall clock runs out with unread tool
    /// results. An interactive caller that can ask whether to continue sets
    /// [`WallClockPolicy::Pause`].
    pub fn with_wall_clock_policy(mut self, policy: WallClockPolicy) -> Self {
        self.config.on_wall_clock = policy;
        self
    }

    /// The run's configuration.
    pub fn config(&self) -> &RunConfig {
        &self.config
    }

    /// The bound tool invoker, for session-scoped policy changes by a caller.
    pub fn invoker(&self) -> &I {
        &self.invoker
    }

    /// Run until the first terminal state or the first pause.
    ///
    /// The terminal event of a [`RunOutcome::Completed`] is always a
    /// [`AgentEvent::RunCompleted`]; an [`AgentError`] is returned only when the
    /// provider itself could not be driven.
    pub async fn run(
        &self,
        user_input: impl Into<String>,
        cancel: CancellationToken,
    ) -> Result<RunOutcome, AgentError> {
        let driver = self.new_driver(Vec::new(), user_input, None);
        self.drive(driver, cancel).await
    }

    /// Run a new turn with the preceding conversation restored.
    ///
    /// `history` contains user and assistant messages from earlier completed
    /// turns. The configured system prompt is still owned by the loop and is
    /// prepended exactly once; `user_input` is appended as the new final user
    /// message. This is the entry point for session-aware callers.
    pub async fn run_with_history(
        &self,
        history: Vec<ProviderMessage>,
        user_input: impl Into<String>,
        cancel: CancellationToken,
    ) -> Result<RunOutcome, AgentError> {
        let driver = self.new_driver(history, user_input, None);
        self.drive(driver, cancel).await
    }

    /// Like [`run`](Self::run), but also emit each [`AgentEvent`] to `sink` as it
    /// happens, for a caller streaming the run live (the HTTP API's SSE). The
    /// sink rides along through a suspend, so a [`resume`](Self::resume) of the
    /// paused run keeps streaming to it. A closed receiver is ignored — the run
    /// is unaffected by nobody listening.
    pub async fn run_streaming(
        &self,
        user_input: impl Into<String>,
        cancel: CancellationToken,
        sink: AgentEventSink,
    ) -> Result<RunOutcome, AgentError> {
        let driver = self.new_driver(Vec::new(), user_input, Some(sink));
        self.drive(driver, cancel).await
    }

    /// Streaming counterpart to [`run_with_history`](Self::run_with_history).
    pub async fn run_streaming_with_history(
        &self,
        history: Vec<ProviderMessage>,
        user_input: impl Into<String>,
        cancel: CancellationToken,
        sink: AgentEventSink,
    ) -> Result<RunOutcome, AgentError> {
        let driver = self.new_driver(history, user_input, Some(sink));
        self.drive(driver, cancel).await
    }

    /// Build the initial driver, emitting the opening `RunStarted`.
    fn new_driver(
        &self,
        history: Vec<ProviderMessage>,
        user_input: impl Into<String>,
        sink: Option<AgentEventSink>,
    ) -> Driver {
        let run = RunId::new();
        let mut messages = Vec::new();
        if let Some(system) = &self.config.system {
            messages.push(ProviderMessage::system(system.clone()));
        }
        messages.extend(history);
        messages.push(ProviderMessage::user(user_input));

        let mut driver = Driver {
            started: Instant::now(),
            limits: self.config.limits,
            model: self.config.model.clone(),
            messages,
            events: Vec::new(),
            turn: 0,
            tool_calls_made: 0,
            call_counts: BTreeMap::new(),
            pending: None,
            sink,
            wrap_up: false,
        };
        driver.emit(AgentEvent::RunStarted {
            run,
            parent: self.config.parent.clone(),
        });
        driver
    }

    /// Resume a paused run with a human's decision.
    ///
    /// On a grant the loop persists the decision when it asked to be remembered
    /// ([`ToolInvoker::remember`]), runs the call, and continues. On a denial it
    /// appends a controlled tool-error result the model still sees, and
    /// continues. Either way the run proceeds from exactly where it paused.
    pub async fn resume(
        &self,
        suspended: Box<Suspended>,
        decision: ApprovalDecision,
        cancel: CancellationToken,
    ) -> Result<RunOutcome, AgentError> {
        if suspended
            .approval
            .as_ref()
            .is_none_or(|request| request.id != decision.id)
        {
            return Err(AgentError::InvalidApproval);
        }
        let mut driver = suspended.driver;

        // The awaited call is the one the pending batch stopped on.
        let awaited = driver
            .pending
            .as_ref()
            .and_then(|batch| batch.calls.get(batch.next).cloned());

        if let Some(call) = awaited {
            if decision.granted {
                self.invoker.remember(&decision, &call);
                self.run_call(&mut driver, &call, &cancel).await;
            } else {
                let outcome = ToolOutcome::error(format!(
                    "the approval for '{}' was denied; the tool did not run",
                    call.name
                ));
                driver.messages.push(ProviderMessage::tool_result(
                    call.id.clone(),
                    outcome.content.clone(),
                ));
                driver.emit(AgentEvent::ToolCallCompleted {
                    id: call.id.clone(),
                    outcome,
                });
            }
            if let Some(batch) = driver.pending.as_mut() {
                batch.next += 1;
            }
        }

        self.drive(driver, cancel).await
    }

    /// Continue a run that paused on its wall-clock budget
    /// ([`RunOutcome::OutOfTime`]), from exactly where it stopped: the whole
    /// conversation, including the tool results the model has not read yet, is
    /// carried over.
    ///
    /// [`Continuation::Extend`] restarts the wall clock and lets the run carry
    /// on with tools; [`Continuation::WrapUp`] takes the final, tool-less turn
    /// now. The returned log is cumulative, as with [`resume`](Self::resume).
    pub async fn continue_out_of_time(
        &self,
        suspended: Box<Suspended>,
        how: Continuation,
        cancel: CancellationToken,
    ) -> Result<RunOutcome, AgentError> {
        let mut driver = suspended.driver;
        match how {
            Continuation::Extend => driver.started = Instant::now(),
            Continuation::WrapUp => driver.wrap_up = true,
        }
        self.drive(driver, cancel).await
    }

    /// The main turn/batch loop, shared by [`run`](Self::run),
    /// [`resume`](Self::resume) and
    /// [`continue_out_of_time`](Self::continue_out_of_time).
    async fn drive(
        &self,
        mut driver: Driver,
        cancel: CancellationToken,
    ) -> Result<RunOutcome, AgentError> {
        loop {
            // A pending batch (from a fresh turn or a just-handled decision) is
            // processed before another turn is taken.
            if driver.pending.is_none() {
                match self.take_turn(&mut driver, &cancel).await? {
                    TurnResult::Finished(outcome) => return Ok(*outcome),
                    TurnResult::Batch => {}
                    TurnResult::OutOfTime(elapsed) => {
                        return Ok(RunOutcome::OutOfTime {
                            events: driver.events.clone(),
                            elapsed,
                            suspended: Box::new(Suspended {
                                driver,
                                approval: None,
                            }),
                        });
                    }
                }
            }

            match self.process_batch(&mut driver, &cancel).await {
                BatchResult::Done => {
                    driver.pending = None;
                    if driver.turn >= driver.limits.max_turns {
                        driver.emit(AgentEvent::RunCompleted {
                            reason: StopReason::MaxTurns,
                        });
                        return Ok(RunOutcome::Completed {
                            events: driver.events,
                        });
                    }
                    // Otherwise, loop for another turn.
                }
                BatchResult::Completed => {
                    return Ok(RunOutcome::Completed {
                        events: driver.events,
                    });
                }
                BatchResult::Suspended(request) => {
                    return Ok(RunOutcome::AwaitingApproval {
                        events: driver.events.clone(),
                        suspended: Box::new(Suspended {
                            driver,
                            approval: Some(request.clone()),
                        }),
                        request,
                    });
                }
            }
        }
    }

    /// Stream one model turn, folding its events into the driver.
    async fn take_turn(
        &self,
        driver: &mut Driver,
        cancel: &CancellationToken,
    ) -> Result<TurnResult, AgentError> {
        // The budget is checked before the turn is counted, so a pause that is
        // later continued does not spend a turn.
        if !driver.wrap_up
            && let Some(budget) = driver.limits.wall_clock()
            && driver.started.elapsed() >= budget
        {
            // Every call in a batch leaves a result, so a tool message last
            // means the model has not read what its tools returned.
            let unread_results = driver
                .messages
                .last()
                .is_some_and(|message| message.role == Role::Tool);
            if !unread_results {
                driver.emit(AgentEvent::RunCompleted {
                    reason: StopReason::WallClockExceeded,
                });
                return Ok(TurnResult::Finished(Box::new(RunOutcome::Completed {
                    events: std::mem::take(&mut driver.events),
                })));
            }
            match self.config.on_wall_clock {
                WallClockPolicy::Pause => {
                    let elapsed = driver.started.elapsed();
                    driver.emit(AgentEvent::WallClockPaused {
                        elapsed_secs: elapsed.as_secs(),
                    });
                    return Ok(TurnResult::OutOfTime(elapsed));
                }
                WallClockPolicy::WrapUp => driver.wrap_up = true,
            }
        }

        driver.turn += 1;

        // The final turn after the clock ran out offers no tools and says why,
        // so the model answers from what it has instead of asking for more.
        // The instruction rides on this one request only.
        let (messages, tools) = if driver.wrap_up {
            let mut messages = driver.messages.clone();
            messages.push(ProviderMessage::user(WRAP_UP_INSTRUCTION));
            (messages, Vec::new())
        } else {
            (driver.messages.clone(), self.invoker.schemas())
        };
        let request = ProviderRequest {
            model: driver.model.clone(),
            messages,
            tools,
            temperature: None,
            max_tokens: None,
        };

        let mut stream = self.provider.stream(request, cancel.clone()).await?;
        let mut accumulator = ToolCallAccumulator::new();
        let mut assistant_text = String::new();
        let mut finish: Option<(FinishReason, Option<Usage>)> = None;

        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    driver.emit(AgentEvent::RunCompleted { reason: StopReason::Cancelled });
                    return Ok(TurnResult::Finished(Box::new(RunOutcome::Completed {
                        events: std::mem::take(&mut driver.events),
                    })));
                }
                item = stream.next() => item,
            };
            let Some(item) = next else { break };
            match item {
                Ok(ProviderEvent::RoleStarted) => {}
                Ok(ProviderEvent::Reasoning(text)) => {
                    driver.emit(AgentEvent::Reasoning { text });
                }
                Ok(ProviderEvent::Content(text)) => {
                    assistant_text.push_str(&text);
                    driver.emit(AgentEvent::Content { text });
                }
                Ok(ProviderEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments,
                }) => {
                    accumulator.push(index, id, name, arguments);
                }
                Ok(ProviderEvent::Finished { reason, usage }) => {
                    finish = Some((reason, usage));
                }
                Err(err) => {
                    driver.emit(AgentEvent::Error {
                        message: err.to_string(),
                    });
                    driver.emit(AgentEvent::RunCompleted {
                        reason: StopReason::Error,
                    });
                    return Ok(TurnResult::Finished(Box::new(RunOutcome::Completed {
                        events: std::mem::take(&mut driver.events),
                    })));
                }
            }
        }

        let (reason, usage) = finish.unwrap_or((FinishReason::Stop, None));
        driver.emit(AgentEvent::TurnCompleted { usage });

        // The wrap-up turn is the last one whatever it produced: its answer
        // stands, and any tool calls it asked for anyway are not run. The stop
        // reason still records that the clock, not the model, ended the run.
        if driver.wrap_up && !matches!(reason, FinishReason::Error) {
            driver.emit(AgentEvent::RunCompleted {
                reason: StopReason::WallClockExceeded,
            });
            return Ok(TurnResult::Finished(Box::new(RunOutcome::Completed {
                events: std::mem::take(&mut driver.events),
            })));
        }

        match reason {
            FinishReason::Stop | FinishReason::Length => {
                driver.emit(AgentEvent::RunCompleted {
                    reason: StopReason::EndTurn,
                });
                Ok(TurnResult::Finished(Box::new(RunOutcome::Completed {
                    events: std::mem::take(&mut driver.events),
                })))
            }
            FinishReason::Error => {
                driver.emit(AgentEvent::Error {
                    message: "the provider ended the turn with an error".to_owned(),
                });
                driver.emit(AgentEvent::RunCompleted {
                    reason: StopReason::Error,
                });
                Ok(TurnResult::Finished(Box::new(RunOutcome::Completed {
                    events: std::mem::take(&mut driver.events),
                })))
            }
            FinishReason::ToolCalls => {
                let calls = accumulator.into_calls();
                // Replay the assistant's tool-call message so the next turn sees
                // exactly what it asked for.
                driver.messages.push(ProviderMessage::assistant_tool_calls(
                    assistant_text,
                    calls
                        .iter()
                        .map(|call| ProviderToolCall {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        })
                        .collect(),
                ));
                driver.pending = Some(Batch { calls, next: 0 });
                Ok(TurnResult::Batch)
            }
        }
    }

    /// Process the pending tool batch from where it left off.
    async fn process_batch(&self, driver: &mut Driver, cancel: &CancellationToken) -> BatchResult {
        loop {
            // The next unhandled call, cloned so nothing borrows `driver` while
            // its events and counters are mutated below.
            let (index, call) = match &driver.pending {
                Some(batch) if batch.next < batch.calls.len() => {
                    (batch.next, batch.calls[batch.next].clone())
                }
                _ => return BatchResult::Done,
            };

            driver.emit(AgentEvent::ToolCallRequested { call: call.clone() });

            driver.tool_calls_made += 1;
            if driver.tool_calls_made > driver.limits.max_tool_calls {
                driver.emit(AgentEvent::RunCompleted {
                    reason: StopReason::MaxToolCalls,
                });
                return BatchResult::Completed;
            }

            let key = format!("{}\u{0}{}", call.name, call.arguments);
            let count = driver.call_counts.entry(key).or_insert(0);
            *count += 1;
            if *count > driver.limits.max_repeated_identical_calls {
                driver.emit(AgentEvent::RunCompleted {
                    reason: StopReason::RepeatedToolCalls,
                });
                return BatchResult::Completed;
            }

            match self.invoker.approval_for(&call) {
                ApprovalNeed::AutoApprove => {
                    self.run_call(driver, &call, cancel).await;
                }
                ApprovalNeed::Deny(reason) => {
                    let outcome = ToolOutcome::error(reason);
                    driver.messages.push(ProviderMessage::tool_result(
                        call.id.clone(),
                        outcome.content.clone(),
                    ));
                    driver.emit(AgentEvent::ToolCallCompleted {
                        id: call.id.clone(),
                        outcome,
                    });
                }
                ApprovalNeed::Require(request) => {
                    driver.emit(AgentEvent::AwaitingApproval {
                        id: call.id.clone(),
                        name: call.name.clone(),
                    });
                    // Leave `next` pointing at this call so a resume acts on it.
                    return BatchResult::Suspended(request);
                }
            }

            if let Some(batch) = driver.pending.as_mut() {
                batch.next = index + 1;
            }
        }
    }

    /// Run one approved call: start, invoke, append its result, complete.
    async fn run_call(&self, driver: &mut Driver, call: &ToolCall, cancel: &CancellationToken) {
        driver.emit(AgentEvent::ToolCallStarted {
            id: call.id.clone(),
            name: call.name.clone(),
        });
        let outcome = self.invoker.invoke(call, cancel.clone()).await;
        driver.messages.push(ProviderMessage::tool_result(
            call.id.clone(),
            outcome.content.clone(),
        ));
        driver.emit(AgentEvent::ToolCallCompleted {
            id: call.id.clone(),
            outcome,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invoker::{NullInvoker, ToolSchema};
    use crate::mock::MockProvider;
    use crate::permissions::{ApprovalRequest, RiskClass};
    use crate::profile::{AgentProfile, ProfileId};
    use crate::provider::{ProviderEvent, Role, Usage};
    use async_trait::async_trait;

    #[tokio::test]
    async fn run_streaming_emits_each_event_live() {
        let mock = MockProvider::new(vec![stop_turn("hi")]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = AgentLoop::new(mock, NullInvoker, RunConfig::new("m"));
        let outcome = agent
            .run_streaming("go", CancellationToken::new(), tx)
            .await
            .unwrap();

        let mut streamed = Vec::new();
        while let Ok(event) = rx.try_recv() {
            streamed.push(event);
        }
        assert!(matches!(
            streamed.first(),
            Some(AgentEvent::RunStarted { .. })
        ));
        // The live stream and the recorded log are the same events, in order.
        assert_eq!(streamed, outcome.into_events());
    }

    #[tokio::test]
    async fn new_run_replays_only_the_given_session_history() {
        let mock = MockProvider::new(vec![stop_turn("reply"), stop_turn("fresh")]);
        let agent = AgentLoop::new(mock.clone(), NullInvoker, RunConfig::new("m"));
        agent
            .run_with_history(
                vec![
                    ProviderMessage::user("first"),
                    ProviderMessage::assistant("answer"),
                ],
                "second",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        agent
            .run("new session", CancellationToken::new())
            .await
            .unwrap();

        let requests = mock.requests();
        assert_eq!(requests[0].messages.len(), 3);
        assert_eq!(requests[0].messages[0], ProviderMessage::user("first"));
        assert_eq!(
            requests[0].messages[1],
            ProviderMessage::assistant("answer")
        );
        assert_eq!(requests[0].messages[2], ProviderMessage::user("second"));
        assert_eq!(
            requests[1].messages,
            vec![ProviderMessage::user("new session")]
        );
    }

    fn stop_turn(content: &str) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::RoleStarted,
            ProviderEvent::Content(content.to_owned()),
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
                usage: Some(Usage {
                    prompt_tokens: 10,
                    completion_tokens: 2,
                    total_tokens: 12,
                }),
            },
        ]
    }

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

    /// An invoker that declares one tool and answers a fixed approval need.
    struct ScriptedInvoker {
        need: ApprovalNeed,
    }

    #[async_trait]
    impl ToolInvoker for ScriptedInvoker {
        fn schemas(&self) -> Vec<ToolSchema> {
            vec![ToolSchema::new(
                "datetime.now",
                "the time",
                serde_json::json!({"type": "object"}),
            )]
        }

        fn approval_for(&self, _call: &ToolCall) -> ApprovalNeed {
            self.need.clone()
        }

        async fn invoke(&self, _call: &ToolCall, _cancel: CancellationToken) -> ToolOutcome {
            ToolOutcome::ok("2026-09-01T00:00:00Z")
        }
    }

    #[tokio::test]
    async fn mock_drives_a_toolless_turn() {
        let mock = MockProvider::new(vec![stop_turn("hello")]);
        let agent = AgentLoop::new(mock, NullInvoker, RunConfig::new("m@8k"));
        let events = agent
            .run("hi", CancellationToken::new())
            .await
            .expect("run")
            .into_events();

        assert!(matches!(events[0], AgentEvent::RunStarted { .. }));
        assert!(matches!(events[1], AgentEvent::Content { ref text } if text == "hello"));
        assert!(matches!(events[2], AgentEvent::TurnCompleted { .. }));
        assert!(matches!(
            events[3],
            AgentEvent::RunCompleted {
                reason: StopReason::EndTurn
            }
        ));
        assert_eq!(events.len(), 4);
    }

    #[tokio::test]
    async fn limit_max_turns_stops_the_run() {
        let mock = MockProvider::looping(vec![tool_turn()]);
        let mut config = RunConfig::new("m@8k");
        config.limits = RunLimits {
            max_turns: 2,
            max_tool_calls: 100,
            wall_clock_secs: None,
            ..RunLimits::default()
        };
        let agent = AgentLoop::new(mock, NullInvoker, config);
        let events = agent
            .run("go", CancellationToken::new())
            .await
            .expect("run")
            .into_events();

        assert!(matches!(
            events.last(),
            Some(AgentEvent::RunCompleted {
                reason: StopReason::MaxTurns
            })
        ));
        let declined = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolCallCompleted { outcome, .. } if outcome.is_error))
            .count();
        assert_eq!(declined, 2);
    }

    #[tokio::test]
    async fn cancellation_before_a_turn_stops_the_run() {
        let mock = MockProvider::looping(vec![tool_turn()]);
        let agent = AgentLoop::new(mock, NullInvoker, RunConfig::new("m@8k"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let events = agent.run("go", cancel).await.expect("run").into_events();
        assert!(matches!(
            events.last(),
            Some(AgentEvent::RunCompleted {
                reason: StopReason::Cancelled
            })
        ));
    }

    #[tokio::test]
    async fn from_profile_seeds_persona_as_system() {
        let profile = AgentProfile::new(
            ProfileId::new("aria").expect("id"),
            "Aria",
            "You are Aria.",
            "lfm2@8k",
        );
        let mock = MockProvider::new(vec![stop_turn("done")]);
        let inspector = mock.clone();
        let agent = AgentLoop::from_profile(mock, NullInvoker, &profile);

        assert_eq!(agent.config().model, "lfm2@8k");
        assert_eq!(agent.config().system.as_deref(), Some("You are Aria."));

        let _ = agent
            .run("hi", CancellationToken::new())
            .await
            .expect("run");

        let requests = inspector.requests();
        let first = &requests[0];
        assert_eq!(first.model, "lfm2@8k");
        assert_eq!(first.messages[0].role, Role::System);
        assert_eq!(first.messages[0].content, "You are Aria.");
        assert_eq!(first.messages[1].role, Role::User);
    }

    #[tokio::test]
    async fn awaiting_approval_pauses_then_resumes() {
        // Turn 1 asks for the tool; turn 2 stops. The invoker requires approval,
        // so the run pauses, and resuming with a grant runs the tool and lets
        // the run finish.
        let mock = MockProvider::new(vec![tool_turn(), stop_turn("all done")]);
        let request = ApprovalRequest::new("datetime.now", RiskClass::Mutating, vec![], "{}");
        let invoker = ScriptedInvoker {
            need: ApprovalNeed::Require(request),
        };
        let agent = AgentLoop::new(mock, invoker, RunConfig::new("m@8k"));

        let paused = agent
            .run("go", CancellationToken::new())
            .await
            .expect("run");
        assert!(!paused.is_completed(), "the run must pause");
        let request = paused.approval_request().expect("a request").clone();
        assert!(matches!(
            paused.events().last(),
            Some(AgentEvent::AwaitingApproval { .. })
        ));
        let RunOutcome::AwaitingApproval { suspended, .. } = paused else {
            panic!("expected a paused run");
        };

        let decision = ApprovalDecision::grant(request.id);
        let done = agent
            .resume(suspended, decision, CancellationToken::new())
            .await
            .expect("resume");
        assert!(done.is_completed());
        let events = done.into_events();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolCallStarted { .. })),
            "the granted tool must have started"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::RunCompleted {
                reason: StopReason::EndTurn
            })
        ));
    }

    #[tokio::test]
    async fn a_decision_for_another_request_cannot_resume_a_tool() {
        let mock = MockProvider::new(vec![tool_turn()]);
        let request = ApprovalRequest::new("datetime.now", RiskClass::Mutating, vec![], "{}");
        let agent = AgentLoop::new(
            mock,
            ScriptedInvoker {
                need: ApprovalNeed::Require(request),
            },
            RunConfig::new("m@8k"),
        );
        let paused = agent.run("go", CancellationToken::new()).await.unwrap();
        let RunOutcome::AwaitingApproval { suspended, .. } = paused else {
            panic!("expected approval pause");
        };
        let wrong = ApprovalDecision::grant(
            ApprovalRequest::new("other", RiskClass::Mutating, vec![], "{}").id,
        );
        assert!(matches!(
            agent
                .resume(suspended, wrong, CancellationToken::new())
                .await,
            Err(AgentError::InvalidApproval)
        ));
    }

    #[tokio::test]
    async fn denied_tool_becomes_a_tool_error_message() {
        let mock = MockProvider::new(vec![tool_turn(), stop_turn("ok")]);
        let invoker = ScriptedInvoker {
            need: ApprovalNeed::Deny("policy forbids this".to_owned()),
        };
        let agent = AgentLoop::new(mock, invoker, RunConfig::new("m@8k"));

        let outcome = agent
            .run("go", CancellationToken::new())
            .await
            .expect("run");
        assert!(outcome.is_completed(), "a denial does not pause the run");
        let events = outcome.into_events();

        // The denied call never started, but produced an error result.
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolCallStarted { .. })),
            "a denied tool must not start"
        );
        let denied = events.iter().any(|event| {
            matches!(event, AgentEvent::ToolCallCompleted { outcome, .. }
                if outcome.is_error && outcome.content.contains("policy forbids this"))
        });
        assert!(denied, "the model must see a controlled error result");
        assert!(matches!(
            events.last(),
            Some(AgentEvent::RunCompleted {
                reason: StopReason::EndTurn
            })
        ));
    }

    #[tokio::test]
    async fn repeated_identical_calls_stop_the_run() {
        // The model asks for the same call forever; auto-approved, so it runs,
        // but the repeat detector stops it once the identical call recurs past
        // its budget.
        let mock = MockProvider::looping(vec![tool_turn()]);
        let mut config = RunConfig::new("m@8k");
        config.limits = RunLimits {
            max_turns: 100,
            max_tool_calls: 100,
            max_repeated_identical_calls: 2,
            wall_clock_secs: None,
            ..RunLimits::default()
        };
        let invoker = ScriptedInvoker {
            need: ApprovalNeed::AutoApprove,
        };
        let agent = AgentLoop::new(mock, invoker, config);
        let events = agent
            .run("go", CancellationToken::new())
            .await
            .expect("run")
            .into_events();

        assert!(matches!(
            events.last(),
            Some(AgentEvent::RunCompleted {
                reason: StopReason::RepeatedToolCalls
            })
        ));
        // The 3rd identical call trips the detector, so exactly two ran.
        let started = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolCallStarted { .. }))
            .count();
        assert_eq!(started, 2);
    }

    /// An auto-approving invoker whose one tool outlasts a one-second budget,
    /// so the wall clock runs out right after its result lands — the moment a
    /// fetched page would otherwise be thrown away.
    struct SlowInvoker;

    #[async_trait]
    impl ToolInvoker for SlowInvoker {
        fn schemas(&self) -> Vec<ToolSchema> {
            vec![ToolSchema::new(
                "datetime.now",
                "the time",
                serde_json::json!({"type": "object"}),
            )]
        }

        fn approval_for(&self, _call: &ToolCall) -> ApprovalNeed {
            ApprovalNeed::AutoApprove
        }

        async fn invoke(&self, _call: &ToolCall, _cancel: CancellationToken) -> ToolOutcome {
            tokio::time::sleep(Duration::from_millis(1_100)).await;
            ToolOutcome::ok("the fetched evidence")
        }
    }

    fn one_second_budget(model: &str) -> RunConfig {
        let mut config = RunConfig::new(model);
        config.limits.wall_clock_secs = Some(1);
        config
    }

    fn stop_reason(events: &[AgentEvent]) -> Option<StopReason> {
        match events.last() {
            Some(AgentEvent::RunCompleted { reason }) => Some(*reason),
            _ => None,
        }
    }

    #[tokio::test]
    async fn out_of_time_before_any_tool_ends_without_a_turn() {
        let mock = MockProvider::new(vec![stop_turn("never")]);
        let inspector = mock.clone();
        let mut config = RunConfig::new("m@8k");
        config.limits.wall_clock_secs = Some(0);
        let agent = AgentLoop::new(mock, NullInvoker, config);
        let events = agent
            .run("go", CancellationToken::new())
            .await
            .expect("run")
            .into_events();

        assert_eq!(stop_reason(&events), Some(StopReason::WallClockExceeded));
        assert!(inspector.requests().is_empty(), "no turn is taken");
    }

    #[tokio::test]
    async fn out_of_time_with_unread_results_wraps_up_without_tools() {
        let mock = MockProvider::new(vec![tool_turn(), stop_turn("answer from evidence")]);
        let inspector = mock.clone();
        let agent = AgentLoop::new(mock, SlowInvoker, one_second_budget("m@8k"));
        let events = agent
            .run("go", CancellationToken::new())
            .await
            .expect("run")
            .into_events();

        let requests = inspector.requests();
        assert_eq!(requests.len(), 2, "one final turn after the clock ran out");
        let wrap_up = &requests[1];
        assert!(wrap_up.tools.is_empty(), "the final turn offers no tools");
        assert!(
            wrap_up
                .messages
                .iter()
                .any(|m| m.role == Role::Tool && m.content == "the fetched evidence"),
            "the model reads what its tool returned"
        );
        let last = wrap_up.messages.last().expect("a message");
        assert_eq!(last.role, Role::User);
        assert_eq!(last.content, WRAP_UP_INSTRUCTION);
        assert!(events.iter().any(
            |event| matches!(event, AgentEvent::Content { text } if text == "answer from evidence")
        ));
        assert_eq!(stop_reason(&events), Some(StopReason::WallClockExceeded));
    }

    #[tokio::test]
    async fn a_wrap_up_turn_that_asks_for_tools_runs_none() {
        let mock = MockProvider::new(vec![tool_turn(), tool_turn()]);
        let agent = AgentLoop::new(mock, SlowInvoker, one_second_budget("m@8k"));
        let events = agent
            .run("go", CancellationToken::new())
            .await
            .expect("run")
            .into_events();

        let started = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolCallStarted { .. }))
            .count();
        assert_eq!(started, 1, "the wrap-up turn's tool call is not run");
        assert_eq!(stop_reason(&events), Some(StopReason::WallClockExceeded));
    }

    #[tokio::test]
    async fn out_of_time_pauses_then_extends_where_it_stopped() {
        let mock = MockProvider::new(vec![tool_turn(), stop_turn("all done")]);
        let inspector = mock.clone();
        let agent = AgentLoop::new(mock, SlowInvoker, one_second_budget("m@8k"))
            .with_wall_clock_policy(WallClockPolicy::Pause);

        let paused = agent
            .run("go", CancellationToken::new())
            .await
            .expect("run");
        let RunOutcome::OutOfTime {
            events,
            elapsed,
            suspended,
        } = paused
        else {
            panic!("expected the run to pause out of time");
        };
        assert!(elapsed >= Duration::from_secs(1));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::WallClockPaused { elapsed_secs: 1.. })
        ));
        assert_eq!(inspector.requests().len(), 1, "no turn after the pause");

        let done = agent
            .continue_out_of_time(suspended, Continuation::Extend, CancellationToken::new())
            .await
            .expect("continue");
        let requests = inspector.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            !requests[1].tools.is_empty(),
            "an extended run keeps its tools"
        );
        assert_eq!(
            requests[1]
                .messages
                .last()
                .map(|m| (m.role, m.content.as_str())),
            Some((Role::Tool, "the fetched evidence")),
            "the run picks up with the unread result, and no wrap-up instruction"
        );
        let events = done.into_events();
        assert!(
            matches!(events[0], AgentEvent::RunStarted { .. }),
            "cumulative log"
        );
        assert_eq!(stop_reason(&events), Some(StopReason::EndTurn));
    }

    #[tokio::test]
    async fn a_paused_run_can_wrap_up_on_request() {
        let mock = MockProvider::new(vec![tool_turn(), stop_turn("best effort")]);
        let inspector = mock.clone();
        let agent = AgentLoop::new(mock, SlowInvoker, one_second_budget("m@8k"))
            .with_wall_clock_policy(WallClockPolicy::Pause);

        let RunOutcome::OutOfTime { suspended, .. } = agent
            .run("go", CancellationToken::new())
            .await
            .expect("run")
        else {
            panic!("expected the run to pause out of time");
        };
        let events = agent
            .continue_out_of_time(suspended, Continuation::WrapUp, CancellationToken::new())
            .await
            .expect("continue")
            .into_events();

        assert!(inspector.requests()[1].tools.is_empty());
        assert_eq!(stop_reason(&events), Some(StopReason::WallClockExceeded));
    }
}
