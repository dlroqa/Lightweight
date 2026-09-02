//! `agent.delegate` — hand a bounded task to a worker profile.
//!
//! Orchestration without a second agent loop: this tool loads a worker
//! [`AgentProfile`], builds a **fresh** [`RunConfig`] (the worker's persona as
//! system, the task as the only user message, none of the orchestrator's
//! history), intersects the worker's limits with the caller's caps, scopes the
//! worker's tools (with `agent.delegate` removed, so delegation is one level
//! deep), and drives it through the very same [`AgentLoop`]. The worker's final
//! answer is returned to the orchestrator as the tool result.
//!
//! It is [`RiskClass::Executable`](lightagent_core::RiskClass::Executable), so
//! the default policy pauses the run for approval before a worker ever starts.

use async_trait::async_trait;
use lightagent_core::ProfileId;
use lightagent_core::{
    AgentEvent, AgentLoop, PolicyEngine, RiskClass, RunConfig, RunLimits, RunOutcome, Scope,
    ToolOutcome,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::context::{Clock, ToolCtx};
use crate::definition::{Tool, ToolDefinition};
use crate::executor::BoundedExecutor;

/// The `agent.delegate` tool.
pub struct AgentDelegate {
    definition: ToolDefinition,
}

/// The parsed arguments. The schema has already validated shape and required
/// fields, so this deserialization does not fail in practice.
#[derive(Debug, Deserialize)]
struct DelegateArgs {
    profile: String,
    task: String,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    max_seconds: Option<u64>,
    #[serde(default)]
    tool_scope: Option<Vec<String>>,
}

impl AgentDelegate {
    /// The tool's stable name.
    pub const NAME: &'static str = "agent.delegate";

    /// Build the tool with its declaration.
    pub fn new() -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "profile": { "type": "string", "description": "The worker profile id to run the task." },
                "task": { "type": "string", "description": "The instruction for the worker." },
                "max_turns": { "type": "integer", "minimum": 1, "description": "Cap the worker's model turns." },
                "max_seconds": { "type": "integer", "minimum": 1, "description": "Cap the worker's wall-clock seconds." },
                "tool_scope": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Restrict the worker to these tool names.",
                }
            },
            "required": ["profile", "task"],
            "additionalProperties": false,
        });
        Self {
            definition: ToolDefinition::new(
                Self::NAME,
                "Delegate a bounded task to a worker profile and return its answer.",
                parameters,
                RiskClass::Executable,
                vec![Scope::new("agent:spawn")],
            ),
        }
    }
}

impl Default for AgentDelegate {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for AgentDelegate {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolOutcome {
        let Some(delegation) = ctx.delegation.clone() else {
            return ToolOutcome::error("delegation is not enabled for this run");
        };
        let args: DelegateArgs = match serde_json::from_value(args.clone()) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutcome::error(format!("could not read delegate arguments: {error}"));
            }
        };

        let profile_id = match ProfileId::new(&args.profile) {
            Ok(id) => id,
            Err(error) => return ToolOutcome::error(format!("invalid worker profile id: {error}")),
        };
        let worker = match delegation.profiles.load(&profile_id) {
            Ok(worker) => worker,
            Err(error) => {
                return ToolOutcome::error(format!(
                    "could not load worker profile '{}': {error}",
                    args.profile
                ));
            }
        };

        let provider = match delegation.factory.provider(&worker.routing) {
            Ok(provider) => provider,
            Err(error) => {
                return ToolOutcome::error(format!(
                    "no provider for worker '{}': {error}",
                    args.profile
                ));
            }
        };

        // A fresh, scoped tool set: the worker's tools, narrowed to `tool_scope`
        // when given, and always without `agent.delegate` (single level deep).
        let mut registry = match &args.tool_scope {
            Some(scope) => delegation.worker_registry.scoped(scope),
            None => delegation.worker_registry.clone(),
        };
        registry = registry.without(Self::NAME);

        let limits = intersect_caps(&worker.limits, args.max_turns, args.max_seconds);
        let executor = BoundedExecutor::new(
            registry,
            PolicyEngine::new(worker.approval_policy.into()),
            delegation.worker_per_call,
            delegation.worker_max_output_bytes,
        )
        .with_clock(Clock::System);

        let mut config = RunConfig::new(worker.routing.model.clone());
        config.system = Some(worker.persona.clone());
        config.limits = limits;
        config.parent = ctx.run.clone();

        let child = AgentLoop::new(provider, executor, config);
        match child.run(args.task, ctx.cancel.clone()).await {
            Ok(RunOutcome::Completed { events }) => ToolOutcome::ok(final_content(&events)),
            Ok(RunOutcome::AwaitingApproval { .. }) => ToolOutcome::error(format!(
                "worker '{}' paused awaiting approval, which delegation cannot grant",
                args.profile
            )),
            Err(error) => ToolOutcome::error(format!("worker '{}' failed: {error}", args.profile)),
        }
    }
}

/// Intersect the worker's own limits with the caller's optional caps, taking the
/// tighter of each — a delegate can only ever narrow what a worker allows.
fn intersect_caps(base: &RunLimits, max_turns: Option<u32>, max_seconds: Option<u64>) -> RunLimits {
    let mut limits = *base;
    if let Some(turns) = max_turns {
        limits.max_turns = limits.max_turns.min(turns);
    }
    if let Some(seconds) = max_seconds {
        limits.wall_clock_secs = Some(match limits.wall_clock_secs {
            Some(existing) => existing.min(seconds),
            None => seconds,
        });
    }
    limits
}

/// The worker's visible answer: every [`AgentEvent::Content`] fragment joined.
fn final_content(events: &[AgentEvent]) -> String {
    let mut answer = String::new();
    for event in events {
        if let AgentEvent::Content { text } = event {
            answer.push_str(text);
        }
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_caps_takes_the_tighter_bound() {
        let base = RunLimits {
            max_turns: 8,
            wall_clock_secs: Some(600),
            ..RunLimits::default()
        };
        let tightened = intersect_caps(&base, Some(2), Some(60));
        assert_eq!(tightened.max_turns, 2);
        assert_eq!(tightened.wall_clock_secs, Some(60));

        let loosened = intersect_caps(&base, Some(20), None);
        assert_eq!(
            loosened.max_turns, 8,
            "a cap cannot loosen the worker's own limit"
        );
    }

    #[test]
    fn final_content_joins_content_events() {
        let events = vec![
            AgentEvent::Content { text: "4".into() },
            AgentEvent::Content { text: "2".into() },
        ];
        assert_eq!(final_content(&events), "42");
    }
}
