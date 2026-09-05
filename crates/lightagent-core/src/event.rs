//! The canonical run event stream.
//!
//! Every observable thing a run does is one of these. The enum is
//! `#[non_exhaustive]`: later slices add variants (approvals, delegation) and a
//! consumer must already expect not to have seen them all. The variant set here
//! is the master-plan event vocabulary, named for what happened rather than for
//! the wire frame that carried it.

use crate::ids::RunId;
use crate::invoker::{ToolCall, ToolOutcome};
use crate::provider::Usage;

/// Why a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The model produced a final answer.
    EndTurn,
    /// The run hit its turn budget.
    MaxTurns,
    /// The run hit its tool-call budget.
    MaxToolCalls,
    /// The run repeated one identical tool call past its budget — a loop.
    RepeatedToolCalls,
    /// The run exceeded its wall-clock budget.
    WallClockExceeded,
    /// The run was cancelled.
    Cancelled,
    /// The run failed — see the preceding [`AgentEvent::Error`].
    Error,
}

/// One thing that happened during a run.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum AgentEvent {
    /// The run began. `parent` names the orchestrator run for a delegated
    /// child (Slice 3/4); `None` for a top-level run.
    RunStarted { run: RunId, parent: Option<RunId> },
    /// A fragment of the model's reasoning trace.
    Reasoning { text: String },
    /// A fragment of the model's visible answer.
    Content { text: String },
    /// The model asked for a tool.
    ToolCallRequested { call: ToolCall },
    /// A requested tool began executing.
    ToolCallStarted { id: String, name: String },
    /// A tool finished, with its result.
    ToolCallCompleted { id: String, outcome: ToolOutcome },
    /// A tool call is waiting on a human approval decision. `id` is the tool
    /// call's id, so a later same-id [`ToolCallStarted`](Self::ToolCallStarted)
    /// (on approve) or [`ToolCallCompleted`](Self::ToolCallCompleted) (on deny)
    /// resolves it.
    AwaitingApproval { id: String, name: String },
    /// One model turn completed, with its token accounting when known.
    TurnCompleted { usage: Option<Usage> },
    /// The run reached a terminal state.
    RunCompleted { reason: StopReason },
    /// Something went wrong. Followed by a terminal
    /// [`RunCompleted`](AgentEvent::RunCompleted) with
    /// [`StopReason::Error`].
    Error { message: String },
}
