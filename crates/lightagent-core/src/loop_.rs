//! The agent loop.
//!
//! One async state machine drives a run from the first event to the terminal
//! one. This slice implements the shape end to end: seed the conversation,
//! stream a model turn, and either finish on a final answer or, when the model
//! asks for tools, hand each call to the [`ToolInvoker`], append the exact
//! assistant tool-call and tool-result messages, and take another turn — all
//! under the run's [`RunLimits`]. Slice 1 ships a [`NullInvoker`], so the tool
//! path is present and bounded but declines every call; Slice 3 supplies real
//! tools without changing the loop's structure.
//!
//! [`NullInvoker`]: crate::invoker::NullInvoker

use std::time::Instant;

use futures_util::stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::event::{AgentEvent, StopReason};
use crate::ids::RunId;
use crate::invoker::{ToolInvoker, ToolOutcome};
use crate::limits::RunLimits;
use crate::profile::AgentProfile;
use crate::provider::{
    AgentProvider, FinishReason, ProviderEvent, ProviderMessage, ProviderRequest, ProviderToolCall,
};
use crate::tool_stream::ToolCallAccumulator;

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
}

impl RunConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: None,
            limits: RunLimits::default(),
            parent: None,
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
        };
        Self::new(provider, invoker, config)
    }

    /// The run's configuration.
    pub fn config(&self) -> &RunConfig {
        &self.config
    }

    /// Run to completion, returning the full event stream in order.
    ///
    /// The terminal event is always a [`AgentEvent::RunCompleted`]; an
    /// [`AgentError`] is returned only when the provider itself could not be
    /// driven (an error the loop cannot express as a run outcome).
    pub async fn run(
        &self,
        user_input: impl Into<String>,
        cancel: CancellationToken,
    ) -> Result<Vec<AgentEvent>, AgentError> {
        let run = RunId::new();
        let started = Instant::now();
        let limits = self.config.limits;

        let mut events = vec![AgentEvent::RunStarted {
            run,
            parent: self.config.parent.clone(),
        }];

        let mut messages = Vec::new();
        if let Some(system) = &self.config.system {
            messages.push(ProviderMessage::system(system.clone()));
        }
        messages.push(ProviderMessage::user(user_input));

        let mut turn: u32 = 0;
        let mut tool_calls_made: u32 = 0;

        loop {
            turn += 1;

            if let Some(budget) = limits.wall_clock()
                && started.elapsed() >= budget
            {
                events.push(AgentEvent::RunCompleted {
                    reason: StopReason::WallClockExceeded,
                });
                return Ok(events);
            }

            let request = ProviderRequest {
                model: self.config.model.clone(),
                messages: messages.clone(),
                tools: self.invoker.schemas(),
                temperature: None,
                max_tokens: None,
            };

            let mut stream = self.provider.stream(request, cancel.clone()).await?;
            let mut accumulator = ToolCallAccumulator::new();
            let mut assistant_text = String::new();
            let mut finish: Option<(FinishReason, Option<crate::provider::Usage>)> = None;

            loop {
                let next = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        events.push(AgentEvent::RunCompleted { reason: StopReason::Cancelled });
                        return Ok(events);
                    }
                    item = stream.next() => item,
                };
                let Some(item) = next else { break };
                match item {
                    Ok(ProviderEvent::RoleStarted) => {}
                    Ok(ProviderEvent::Reasoning(text)) => {
                        events.push(AgentEvent::Reasoning { text });
                    }
                    Ok(ProviderEvent::Content(text)) => {
                        assistant_text.push_str(&text);
                        events.push(AgentEvent::Content { text });
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
                        events.push(AgentEvent::Error {
                            message: err.to_string(),
                        });
                        events.push(AgentEvent::RunCompleted {
                            reason: StopReason::Error,
                        });
                        return Ok(events);
                    }
                }
            }

            let (reason, usage) = finish.unwrap_or((FinishReason::Stop, None));
            events.push(AgentEvent::TurnCompleted { usage });

            match reason {
                FinishReason::Stop | FinishReason::Length => {
                    events.push(AgentEvent::RunCompleted {
                        reason: StopReason::EndTurn,
                    });
                    return Ok(events);
                }
                FinishReason::Error => {
                    events.push(AgentEvent::Error {
                        message: "the provider ended the turn with an error".to_owned(),
                    });
                    events.push(AgentEvent::RunCompleted {
                        reason: StopReason::Error,
                    });
                    return Ok(events);
                }
                FinishReason::ToolCalls => {
                    let calls = accumulator.into_calls();
                    // Replay the assistant's tool-call message so the next turn
                    // sees exactly what it asked for.
                    messages.push(ProviderMessage::assistant_tool_calls(
                        assistant_text.clone(),
                        calls
                            .iter()
                            .map(|call| ProviderToolCall {
                                id: call.id.clone(),
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            })
                            .collect(),
                    ));

                    for call in &calls {
                        events.push(AgentEvent::ToolCallRequested { call: call.clone() });

                        tool_calls_made += 1;
                        if tool_calls_made > limits.max_tool_calls {
                            events.push(AgentEvent::RunCompleted {
                                reason: StopReason::MaxToolCalls,
                            });
                            return Ok(events);
                        }

                        events.push(AgentEvent::ToolCallStarted {
                            id: call.id.clone(),
                            name: call.name.clone(),
                        });
                        let outcome: ToolOutcome = self.invoker.invoke(call, cancel.clone()).await;
                        messages.push(ProviderMessage::tool_result(
                            call.id.clone(),
                            outcome.content.clone(),
                        ));
                        events.push(AgentEvent::ToolCallCompleted {
                            id: call.id.clone(),
                            outcome,
                        });
                    }

                    if turn >= limits.max_turns {
                        events.push(AgentEvent::RunCompleted {
                            reason: StopReason::MaxTurns,
                        });
                        return Ok(events);
                    }
                    // Otherwise, loop for another turn.
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invoker::NullInvoker;
    use crate::mock::MockProvider;
    use crate::profile::{AgentProfile, ProfileId};
    use crate::provider::{ProviderEvent, Role, Usage};

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

    #[tokio::test]
    async fn mock_drives_a_toolless_turn() {
        let mock = MockProvider::new(vec![stop_turn("hello")]);
        let agent = AgentLoop::new(mock, NullInvoker, RunConfig::new("m@8k"));
        let events = agent
            .run("hi", CancellationToken::new())
            .await
            .expect("run");

        // The exact contract: started, one content delta, the turn completes,
        // the run ends on a natural stop.
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
        // The model asks for a tool every turn; the NullInvoker declines, so
        // the run only ever ends because it hits the turn budget.
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
            .expect("run");

        assert!(matches!(
            events.last(),
            Some(AgentEvent::RunCompleted {
                reason: StopReason::MaxTurns
            })
        ));
        // Two turns, so two declined tool calls.
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
        let events = agent.run("go", cancel).await.expect("run");
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
}
