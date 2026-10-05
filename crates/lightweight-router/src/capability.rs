//! Whether an available deployment can serve one particular request.
//!
//! The step between eligibility and policy:
//!
//! ```text
//! select::eligible        enabled, healthy, serving its model
//!        ↓
//! capability::filter      can serve THIS request
//!        ↓
//! the route's policy      priority | round_robin | least_busy
//! ```
//!
//! It is a yes or a no per deployment, from the deployment's own last-observed
//! capabilities ([`DeploymentObservation`]) and the request's
//! [`RequestRequirements`]. It never reorders what it keeps, never prefers a
//! deployment for supporting more, and never makes a network call. A
//! deployment it removes is not in the plan at all, so no policy can choose it
//! and no failover can reach it.
//!
//! An unknown is never a yes. A deployment the router has never observed
//! cannot be vouched for, so any request that needs something of it —
//! and every request needs at least its endpoint — passes it by.

use std::collections::BTreeMap;

use crate::domain::{CapabilityGap, DeploymentId, RoutingFailure};
use crate::health::DeploymentObservation;
use crate::proxy::Endpoint;
use crate::requirements::RequestRequirements;
use crate::select::Eligible;

/// Every requirement `seen` fails, in [`CapabilityGap`] order. Empty means the
/// deployment can serve the request.
pub fn gaps(
    needs: &RequestRequirements,
    seen: Option<&DeploymentObservation>,
) -> Vec<CapabilityGap> {
    let requires_anything = needs.endpoint.is_some()
        || needs.tools
        || needs.needs_tool_choice()
        || needs.reasoning
        || needs.prompt_tokens.is_some();
    let Some(seen) = seen else {
        return if requires_anything {
            vec![CapabilityGap::Unobserved]
        } else {
            Vec::new()
        };
    };
    let features = &seen.capabilities.0;
    let mut gaps = Vec::new();
    match needs.endpoint {
        Some(Endpoint::ChatCompletions) if !features.chat_completions => {
            gaps.push(CapabilityGap::ChatUnsupported);
        }
        Some(Endpoint::Completions) if !features.completions => {
            gaps.push(CapabilityGap::CompletionUnsupported);
        }
        _ => {}
    }
    if needs.tools && !features.tools {
        gaps.push(CapabilityGap::ToolsUnsupported);
    }
    if needs.needs_tool_choice() && !(features.tools && features.tool_choice) {
        gaps.push(CapabilityGap::ToolChoiceUnsupported);
    }
    if needs.reasoning && !features.reasoning_content {
        gaps.push(CapabilityGap::ReasoningUnsupported);
    }
    // The node's own rule: it refuses a prompt that leaves no room to generate.
    if let Some(required) = needs.required_context()
        && required > seen.context_length
    {
        gaps.push(CapabilityGap::ContextTooSmall);
    }
    gaps
}

/// Keep the candidates that can serve `needs`, in the order they came.
///
/// The rest move to [`Eligible::unfit`] with their gaps, for the log line.
/// When nothing is left:
///
/// * if a deployment that is *unavailable* right now was last seen able to
///   serve this request, the route is [`RoutingFailure::RouteUnavailable`] —
///   the answer is to wait for it, not to change the request;
/// * otherwise it is [`RoutingFailure::CapabilityMismatch`], naming every gap
///   seen.
pub fn filter(
    mut eligible: Eligible,
    needs: &RequestRequirements,
    observed: &BTreeMap<DeploymentId, DeploymentObservation>,
) -> Result<Eligible, RoutingFailure> {
    let mut fit = Vec::with_capacity(eligible.candidates.len());
    for candidate in eligible.candidates {
        let found = gaps(needs, observed.get(&candidate.deployment));
        if found.is_empty() {
            fit.push(candidate);
        } else {
            eligible.unfit.push((candidate.deployment, found));
        }
    }
    eligible.candidates = fit;
    if !eligible.candidates.is_empty() {
        return Ok(eligible);
    }

    let a_down_deployment_would_do = eligible.skipped.iter().any(|(deployment, _)| {
        observed
            .get(deployment)
            .is_some_and(|seen| gaps(needs, Some(seen)).is_empty())
    });
    if a_down_deployment_would_do {
        return Err(RoutingFailure::RouteUnavailable {
            route: eligible.route,
        });
    }
    let mut unmet: Vec<CapabilityGap> = eligible
        .unfit
        .iter()
        .flat_map(|(_, gaps)| gaps.iter().copied())
        .collect();
    unmet.sort_unstable();
    unmet.dedup();
    Err(RoutingFailure::CapabilityMismatch {
        route: eligible.route,
        unmet,
        unfit: eligible.unfit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::CapabilitySet;
    use crate::requirements::ToolChoiceRequirement;
    use std::time::SystemTime;

    /// A deployment that offers everything, at `context` tokens.
    fn everything(context: u32) -> DeploymentObservation {
        let mut capabilities = CapabilitySet::none();
        let f = &mut capabilities.0;
        f.streaming = true;
        f.chat_completions = true;
        f.completions = true;
        f.tools = true;
        f.tool_choice = true;
        f.reasoning_content = true;
        DeploymentObservation {
            capabilities,
            context_length: context,
            max_concurrent_requests: 1,
            observed_at: SystemTime::now(),
        }
    }

    fn without(
        mut seen: DeploymentObservation,
        f: impl FnOnce(&mut CapabilitySet),
    ) -> DeploymentObservation {
        f(&mut seen.capabilities);
        seen
    }

    fn chat() -> RequestRequirements {
        RequestRequirements {
            endpoint: Some(Endpoint::ChatCompletions),
            prompt_tokens: Some(10),
            ..RequestRequirements::none()
        }
    }

    #[test]
    fn an_ordinary_chat_fits_any_chat_deployment_with_room() {
        assert!(gaps(&chat(), Some(&everything(4096))).is_empty());
        // A node without tools or reasoning still serves a request that asks
        // for neither: capability is a floor, not a ranking.
        let plain = without(everything(4096), |c| {
            c.0.tools = false;
            c.0.tool_choice = false;
            c.0.reasoning_content = false;
        });
        assert!(gaps(&chat(), Some(&plain)).is_empty());
    }

    #[test]
    fn the_endpoint_must_be_offered() {
        let completion_only = without(everything(4096), |c| c.0.chat_completions = false);
        assert_eq!(
            gaps(&chat(), Some(&completion_only)),
            [CapabilityGap::ChatUnsupported]
        );
        let chat_only = without(everything(4096), |c| c.0.completions = false);
        let completion = RequestRequirements {
            endpoint: Some(Endpoint::Completions),
            ..chat()
        };
        assert_eq!(
            gaps(&completion, Some(&chat_only)),
            [CapabilityGap::CompletionUnsupported]
        );
        assert!(gaps(&chat(), Some(&chat_only)).is_empty());
        assert!(gaps(&completion, Some(&completion_only)).is_empty());
    }

    #[test]
    fn tools_and_each_tool_choice_need_what_they_force() {
        let no_tools = without(everything(4096), |c| {
            c.0.tools = false;
            c.0.tool_choice = false;
        });
        let tools_no_choice = without(everything(4096), |c| c.0.tool_choice = false);
        let with = |choice| RequestRequirements {
            tools: true,
            tool_choice: choice,
            ..chat()
        };

        assert_eq!(
            gaps(&with(ToolChoiceRequirement::Unspecified), Some(&no_tools)),
            [CapabilityGap::ToolsUnsupported]
        );
        for easy in [
            ToolChoiceRequirement::Unspecified,
            ToolChoiceRequirement::Auto,
        ] {
            assert!(
                gaps(&with(easy), Some(&tools_no_choice)).is_empty(),
                "{easy:?}"
            );
        }
        for forced in [
            ToolChoiceRequirement::None,
            ToolChoiceRequirement::Required,
            ToolChoiceRequirement::Function,
        ] {
            assert_eq!(
                gaps(&with(forced), Some(&tools_no_choice)),
                [CapabilityGap::ToolChoiceUnsupported],
                "{forced:?}"
            );
            assert!(gaps(&with(forced), Some(&everything(4096))).is_empty());
        }
        // A tool_choice flag without tools is not tool support.
        let choice_only = without(everything(4096), |c| c.0.tools = false);
        assert_eq!(
            gaps(&with(ToolChoiceRequirement::Required), Some(&choice_only)),
            [
                CapabilityGap::ToolsUnsupported,
                CapabilityGap::ToolChoiceUnsupported
            ]
        );
    }

    #[test]
    fn reasoning_is_needed_only_when_asked_for() {
        let plain = without(everything(4096), |c| c.0.reasoning_content = false);
        let asks = RequestRequirements {
            reasoning: true,
            ..chat()
        };
        assert_eq!(
            gaps(&asks, Some(&plain)),
            [CapabilityGap::ReasoningUnsupported]
        );
        assert!(gaps(&asks, Some(&everything(4096))).is_empty());
        assert!(gaps(&chat(), Some(&plain)).is_empty());
    }

    #[test]
    fn the_prompt_must_leave_one_token_as_the_node_requires() {
        let at = |tokens| RequestRequirements {
            prompt_tokens: Some(tokens),
            ..chat()
        };
        // The node refuses `prompt_tokens >= n_ctx`.
        assert!(
            gaps(&at(4094), Some(&everything(4096))).is_empty(),
            "just under"
        );
        assert!(
            gaps(&at(4095), Some(&everything(4096))).is_empty(),
            "exactly at the limit"
        );
        assert_eq!(
            gaps(&at(4096), Some(&everything(4096))),
            [CapabilityGap::ContextTooSmall],
            "just over"
        );
        // An unreadable request has no count, and is left to the node.
        let unknown = RequestRequirements {
            prompt_tokens: None,
            ..chat()
        };
        assert!(gaps(&unknown, Some(&everything(16))).is_empty());
    }

    #[test]
    fn an_unobserved_deployment_is_never_assumed_capable() {
        assert_eq!(gaps(&chat(), None), [CapabilityGap::Unobserved]);
        // Outside a request, nothing is required, and nothing is refused.
        assert!(gaps(&RequestRequirements::none(), None).is_empty());
    }

    #[test]
    fn every_unmet_requirement_is_reported_together() {
        let poor = without(everything(1024), |c| {
            c.0.tools = false;
            c.0.reasoning_content = false;
        });
        let needs = RequestRequirements {
            tools: true,
            reasoning: true,
            prompt_tokens: Some(5000),
            ..chat()
        };
        assert_eq!(
            gaps(&needs, Some(&poor)),
            [
                CapabilityGap::ToolsUnsupported,
                CapabilityGap::ReasoningUnsupported,
                CapabilityGap::ContextTooSmall
            ]
        );
    }
}
