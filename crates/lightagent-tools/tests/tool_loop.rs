//! The model→tool→model loop, closed against a real built-in.
//!
//! A scripted [`MockProvider`] asks for `datetime.now`, the [`BoundedExecutor`]
//! runs it, and the model consumes the result and answers — all offline, at
//! memory speed.

use std::time::{Duration, UNIX_EPOCH};

use lightagent_core::permissions::ApprovalPolicy;
use lightagent_core::{
    AgentEvent, AgentLoop, FinishReason, MockProvider, PolicyEngine, ProviderEvent, RunConfig,
    RunOutcome, StopReason,
};
use lightagent_tools::{BoundedExecutor, Clock, ToolRegistry};
use tokio_util::sync::CancellationToken;

/// A pinned instant so `datetime.now` renders exactly.
fn fixed_clock() -> Clock {
    Clock::Fixed(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
}

const FIXED_RENDERING: &str = "2023-11-14T22:13:20Z";

/// An executor over the worker tool set (datetime.now, no delegation), with a
/// permissive policy so an `Observe` tool runs unattended.
fn executor() -> BoundedExecutor {
    BoundedExecutor::new(
        ToolRegistry::worker_default(),
        PolicyEngine::new(ApprovalPolicy::permissive()),
        Duration::from_secs(5),
        262_144,
    )
    .with_clock(fixed_clock())
}

fn tool_call_turn(id: &str, name: &str, arguments: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::RoleStarted,
        ProviderEvent::ToolCallDelta {
            index: 0,
            id: Some(id.into()),
            name: Some(name.into()),
            arguments: Some(arguments.into()),
        },
        ProviderEvent::Finished {
            reason: FinishReason::ToolCalls,
            usage: None,
        },
    ]
}

fn final_turn(text: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::Content(text.into()),
        ProviderEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]
}

fn completed_outcome(call: &str, args: &str) -> Vec<AgentEvent> {
    let provider = MockProvider::new(vec![
        tool_call_turn("call_1", call, args),
        final_turn("Done."),
    ]);
    let agent = AgentLoop::new(provider, executor(), RunConfig::new("test-model"));
    let outcome = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(agent.run("go", CancellationToken::new()))
        .unwrap();
    assert!(matches!(outcome, RunOutcome::Completed { .. }));
    outcome.into_events()
}

#[test]
fn datetime_now_runs_and_the_model_answers() {
    let events = completed_outcome("datetime.now", "{}");

    let requested = events.iter().any(|event| {
        matches!(event, AgentEvent::ToolCallRequested { call } if call.name == "datetime.now")
    });
    assert!(requested, "the tool call was requested");

    let completed = events.iter().find_map(|event| match event {
        AgentEvent::ToolCallCompleted { outcome, .. } => Some(outcome),
        _ => None,
    });
    let outcome = completed.expect("the tool completed");
    assert!(!outcome.is_error);
    assert_eq!(outcome.content, FIXED_RENDERING);

    assert!(matches!(
        events.last(),
        Some(AgentEvent::RunCompleted {
            reason: StopReason::EndTurn
        })
    ));
}

#[test]
fn an_undeclared_tool_is_reported_not_executed() {
    let events = completed_outcome("nonexistent.tool", "{}");
    let outcome = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolCallCompleted { outcome, .. } => Some(outcome),
            _ => None,
        })
        .expect("a controlled result was produced");
    assert!(outcome.is_error);
    assert!(outcome.content.contains("not available"));
}

#[test]
fn malformed_arguments_are_reported_not_executed() {
    let events = completed_outcome("datetime.now", "{not json");
    let outcome = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolCallCompleted { outcome, .. } => Some(outcome),
            _ => None,
        })
        .expect("a controlled result was produced");
    assert!(outcome.is_error);
    assert!(outcome.content.contains("valid JSON"));
}
