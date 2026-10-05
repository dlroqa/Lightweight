//! `Auto`: which logical route a request goes to, when the client asked the
//! router to choose.
//!
//! The router makes two decisions, and this module owns only the first:
//!
//! 1. **Which route?** A client normally names one (`Coder`). A client that
//!    sends `model: "Auto"` asks the router to pick, and this module picks by
//!    the operator's rules.
//! 2. **Which deployment of that route?** Health, capability filtering,
//!    session affinity and the route's policy, exactly as for a request that
//!    named the route itself. Nothing here can see a deployment or a node.
//!
//! A rule is a set of conditions on what [`crate::requirements`] already read
//! from the request — its endpoint, tools, `tool_choice`, reasoning and the
//! router's prompt estimate — and the route to use when all of them hold.
//! Rules are tried in configured order and the first that matches wins; when
//! none does, the configured fallback route is used. There is no score, no
//! weight, no history and no look at the prompt's meaning: the same request
//! against the same configuration always goes to the same route.
//!
//! Choosing a route is all `Auto` does. If the chosen route has nothing that
//! can serve the request, the client gets that route's own answer —
//! `route_unavailable`, `route_capability_mismatch` — and no other route is
//! tried. `Auto` has no deployments, no health, no affinity and no placement of
//! its own; every one of those belongs to the route it resolved to.

use std::fmt::Write as _;

use lightweight_catalog::alias;
use serde::{Deserialize, Serialize};

use crate::config::ConfigError;
use crate::domain::{Route, RouteName};
use crate::proxy::Endpoint;
use crate::requirements::{RequestRequirements, ToolChoiceRequirement};

/// The name a client sends to ask the router to choose. Matched the way route
/// names are: trimmed and ignoring case.
pub const AUTO_ROUTE: &str = "Auto";

/// The `rule` label and log value of a decision no rule made.
///
/// Rule names must start with a letter or digit, so no configured rule can
/// take it.
pub const FALLBACK_RULE: &str = "_fallback";

/// The longest rule name accepted.
const MAX_RULE_NAME_CHARS: usize = 64;

/// The most rules one configuration may hold. Rules are tried in order on
/// every `Auto` request and each name is a metric label, so both stay bounded.
pub const MAX_RULES: usize = 64;

/// The `auto_route` section, as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoRouteFile {
    /// Off unless the operator turns it on. A section that is present but off
    /// is still checked, so turning it on cannot be the moment a typo
    /// surfaces.
    #[serde(default)]
    pub enabled: bool,
    /// The route a request goes to when no rule matches. Required: `Auto`
    /// never picks a route the operator did not name.
    pub fallback_route: String,
    #[serde(default)]
    pub rules: Vec<AutoRuleFile>,
}

/// One rule, as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoRuleFile {
    pub name: String,
    pub when: AutoCondition,
    pub route: String,
}

/// The endpoint a rule can ask for, in the words the router's logs use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoEndpoint {
    /// `/v1/chat/completions`.
    Chat,
    /// `/v1/completions`.
    Completion,
}

impl AutoEndpoint {
    pub const fn endpoint(self) -> Endpoint {
        match self {
            Self::Chat => Endpoint::ChatCompletions,
            Self::Completion => Endpoint::Completions,
        }
    }
}

/// What a request must be for a rule to apply. Every condition that is set
/// must hold (AND); one that is not set is not looked at.
///
/// A boolean condition set to `false` is a condition, not an absence:
/// `"requires_tools": false` matches only a request that declares no tools.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AutoCondition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<AutoEndpoint>,
    /// The request declares at least one tool (`"tools": []` declares none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_tools: Option<bool>,
    /// The request's `tool_choice`, exactly: `unspecified` (not sent), `auto`,
    /// `none`, `required` or `function` (a named function).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoiceRequirement>,
    /// The request sends a `reasoning_effort` other than `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_reasoning: Option<bool>,
    /// The router's prompt estimate is at least this many tokens. The same
    /// lower bound capability filtering uses, not the model's own count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_prompt_tokens: Option<u32>,
    /// The router's prompt estimate is at most this many tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_prompt_tokens: Option<u32>,
}

impl AutoCondition {
    /// Whether `needs` satisfies every condition that is set.
    ///
    /// A prompt threshold needs an estimate: a body the router could not read
    /// well enough to count matches neither `min_prompt_tokens` nor
    /// `max_prompt_tokens`.
    pub fn matches(&self, needs: &RequestRequirements) -> bool {
        self.endpoint
            .is_none_or(|want| needs.endpoint == Some(want.endpoint()))
            && self.requires_tools.is_none_or(|want| needs.tools == want)
            && self
                .tool_choice
                .is_none_or(|want| needs.tool_choice == want)
            && self
                .requires_reasoning
                .is_none_or(|want| needs.reasoning == want)
            && self
                .min_prompt_tokens
                .is_none_or(|min| needs.prompt_tokens.is_some_and(|tokens| tokens >= min))
            && self
                .max_prompt_tokens
                .is_none_or(|max| needs.prompt_tokens.is_some_and(|tokens| tokens <= max))
    }

    fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// `requires_tools=true AND requires_reasoning=true`, for the admin view
    /// and the startup summary.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(endpoint) = self.endpoint {
            parts.push(format!("endpoint={}", endpoint.endpoint().as_str()));
        }
        if let Some(want) = self.requires_tools {
            parts.push(format!("requires_tools={want}"));
        }
        if let Some(want) = self.tool_choice {
            parts.push(format!("tool_choice={}", want.as_str()));
        }
        if let Some(want) = self.requires_reasoning {
            parts.push(format!("requires_reasoning={want}"));
        }
        if let Some(min) = self.min_prompt_tokens {
            parts.push(format!("estimated_prompt_tokens>={min}"));
        }
        if let Some(max) = self.max_prompt_tokens {
            parts.push(format!("estimated_prompt_tokens<={max}"));
        }
        parts.join(" AND ")
    }

    /// Why no request could ever satisfy this condition, if none could.
    fn contradiction(&self) -> Option<&'static str> {
        if let (Some(min), Some(max)) = (self.min_prompt_tokens, self.max_prompt_tokens)
            && min > max
        {
            return Some("min_prompt_tokens is greater than max_prompt_tokens");
        }
        if self.endpoint == Some(AutoEndpoint::Completion) {
            // A text completion carries no tools, tool_choice or reasoning:
            // the requirement reader never sets them for one.
            if self.requires_tools == Some(true) {
                return Some("a text completion never declares tools");
            }
            if self.requires_reasoning == Some(true) {
                return Some("a text completion never asks for reasoning");
            }
            if self
                .tool_choice
                .is_some_and(|choice| choice != ToolChoiceRequirement::Unspecified)
            {
                return Some("a text completion never sends tool_choice");
            }
        }
        if self.requires_tools == Some(false)
            && matches!(
                self.tool_choice,
                Some(ToolChoiceRequirement::Required | ToolChoiceRequirement::Function)
            )
        {
            // Refused by the gateway's own validation before any rule runs.
            return Some("tool_choice required or a named function is only valid with tools");
        }
        None
    }
}

/// One validated rule.
#[derive(Clone, Debug)]
pub struct AutoRule {
    pub name: String,
    pub when: AutoCondition,
    /// A configured route, spelled as the route is configured.
    pub route: RouteName,
}

/// The validated `auto_route` section.
#[derive(Clone, Debug)]
pub struct AutoRoute {
    pub enabled: bool,
    pub fallback: RouteName,
    /// In configured order, which is the order they are tried.
    pub rules: Vec<AutoRule>,
}

/// The route `Auto` chose for one request, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoDecision<'a> {
    pub route: &'a RouteName,
    /// The rule that matched; `None` when none did and the fallback was used.
    pub rule: Option<&'a str>,
}

impl AutoDecision<'_> {
    /// The rule's name, or [`FALLBACK_RULE`].
    pub fn rule_label(&self) -> &str {
        self.rule.unwrap_or(FALLBACK_RULE)
    }

    pub const fn is_fallback(&self) -> bool {
        self.rule.is_none()
    }
}

impl AutoRoute {
    /// Whether `requested` asks the router to choose. Never while `Auto` is
    /// off: then `Auto` is an ordinary, unknown model name.
    pub fn claims(&self, requested: Option<&str>) -> bool {
        self.enabled && requested.is_some_and(is_auto)
    }

    /// The first rule `needs` satisfies, in configured order, or the fallback.
    pub fn decide(&self, needs: &RequestRequirements) -> AutoDecision<'_> {
        self.rules
            .iter()
            .find(|rule| rule.when.matches(needs))
            .map_or(
                AutoDecision {
                    route: &self.fallback,
                    rule: None,
                },
                |rule| AutoDecision {
                    route: &rule.route,
                    rule: Some(rule.name.as_str()),
                },
            )
    }

    /// Every route `Auto` can resolve to, each once, rules first in order and
    /// then the fallback.
    pub fn targets(&self) -> Vec<&RouteName> {
        let mut targets: Vec<&RouteName> = Vec::new();
        for route in self
            .rules
            .iter()
            .map(|rule| &rule.route)
            .chain(std::iter::once(&self.fallback))
        {
            if !targets.contains(&route) {
                targets.push(route);
            }
        }
        targets
    }

    /// `tools -> Coder, reasoning -> Reasoning; otherwise General`.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for rule in &self.rules {
            let _ = write!(out, "{} -> {}, ", rule.name, rule.route);
        }
        let _ = write!(out, "otherwise {}", self.fallback);
        out
    }
}

/// Whether `name` is `Auto`, compared the way route names are.
pub fn is_auto(name: &str) -> bool {
    alias::same_name(AUTO_ROUTE, name)
}

/// Check the `auto_route` section against the routes that passed their own
/// validation, recording every problem.
pub(crate) fn validate(
    raw: &AutoRouteFile,
    routes: &[Route],
    errors: &mut Vec<ConfigError>,
) -> Option<AutoRoute> {
    let before = errors.len();

    // `Auto` is the router's own name while this section exists, on or off:
    // a route called that could never be reached once Auto is turned on.
    for route in routes.iter().filter(|route| is_auto(route.name.as_str())) {
        errors.push(ConfigError::AutoRouteNameTaken {
            name: route.name.as_str().to_owned(),
        });
    }

    let fallback = target(&raw.fallback_route, routes).map_err(|problem| {
        errors.push(ConfigError::BadAutoRoute {
            problem: format!("fallback_route {:?} {problem}", raw.fallback_route),
        });
    });

    if raw.rules.len() > MAX_RULES {
        errors.push(ConfigError::BadAutoRoute {
            problem: format!(
                "lists {} rules; at most {MAX_RULES} are allowed",
                raw.rules.len()
            ),
        });
    }

    let mut rules = Vec::with_capacity(raw.rules.len());
    let mut seen = Vec::<String>::new();
    for (index, entry) in raw.rules.iter().enumerate() {
        let name = entry.name.trim();
        let label = if name.is_empty() {
            format!("#{}", index + 1)
        } else {
            name.to_owned()
        };
        let mut fail = |problem: String| {
            errors.push(ConfigError::BadAutoRule {
                rule: label.clone(),
                problem,
            });
        };
        let mut ok = true;
        if let Err(problem) = rule_name(name) {
            fail(problem);
            ok = false;
        } else if seen.iter().any(|other| other.eq_ignore_ascii_case(name)) {
            fail("is used by more than one rule (names are compared ignoring case)".into());
            ok = false;
        } else {
            seen.push(name.to_owned());
        }
        if entry.when.is_empty() {
            fail(
                "has no conditions, so it would match every request; \
                 use fallback_route for that"
                    .into(),
            );
            ok = false;
        }
        for (field, value) in [
            ("min_prompt_tokens", entry.when.min_prompt_tokens),
            ("max_prompt_tokens", entry.when.max_prompt_tokens),
        ] {
            if value == Some(0) {
                fail(format!("{field} must be at least 1"));
                ok = false;
            }
        }
        if let Some(problem) = entry.when.contradiction() {
            fail(format!("can never match: {problem}"));
            ok = false;
        }
        let route = match target(&entry.route, routes) {
            Ok(route) => Some(route),
            Err(problem) => {
                fail(format!("route {:?} {problem}", entry.route));
                None
            }
        };
        if let (true, Some(route)) = (ok, route) {
            rules.push(AutoRule {
                name: name.to_owned(),
                when: entry.when.clone(),
                route,
            });
        }
    }

    if errors.len() > before {
        return None;
    }
    Some(AutoRoute {
        enabled: raw.enabled,
        fallback: fallback.ok()?,
        rules,
    })
}

/// The configured route `name` refers to, or why it refers to none.
fn target(name: &str, routes: &[Route]) -> Result<RouteName, &'static str> {
    if is_auto(name) {
        return Err("is Auto itself; Auto must resolve to a configured route");
    }
    if alias::is_reserved(name) {
        return Err("is reserved; name a configured route");
    }
    routes
        .iter()
        .find(|route| route.name.matches(name))
        .map(|route| route.name.clone())
        .ok_or("is not one of the configured routes")
}

/// Check a rule name. It becomes a metric label and a log value, so it is held
/// to the node id's alphabet, and it must start with a letter or digit so it
/// can never be [`FALLBACK_RULE`].
fn rule_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("has no name".into());
    }
    if name.chars().count() > MAX_RULE_NAME_CHARS {
        return Err(format!(
            "name can be at most {MAX_RULE_NAME_CHARS} characters"
        ));
    }
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err("name must start with a letter or digit".into());
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(format!(
            "name may contain only letters, digits, `.`, `_` and `-`, not {bad:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RouterConfig, RouterFile};
    use crate::requirements;
    use serde_json::{Value, json};

    fn file(auto: Value) -> RouterFile {
        serde_json::from_value(json!({
            "default_route": "General",
            "nodes": [{"id": "a", "url": "http://192.0.2.10:11434"}],
            "routes": [
                {"name": "General", "deployments": [{"node": "a", "model": "G"}]},
                {"name": "Coder", "deployments": [{"node": "a", "model": "C"}]},
                {"name": "Reasoning", "deployments": [{"node": "a", "model": "R"}]},
                {"name": "Agentic", "deployments": [{"node": "a", "model": "A"}]},
                {"name": "LongContext", "deployments": [{"node": "a", "model": "L"}]},
                {"name": "Completion", "deployments": [{"node": "a", "model": "T"}]}
            ],
            "auto_route": auto,
        }))
        .expect("the file shape parses")
    }

    fn config(auto: Value) -> RouterConfig {
        crate::config::validate(file(auto), &|_| None).expect("valid")
    }

    fn errors(auto: Value) -> Vec<String> {
        crate::config::validate(file(auto), &|_| None)
            .expect_err("refused")
            .0
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn auto(rules: Value) -> AutoRoute {
        config(json!({"enabled": true, "fallback_route": "General", "rules": rules}))
            .auto
            .expect("configured")
    }

    fn chat(body: Value) -> RequestRequirements {
        requirements::extract(Endpoint::ChatCompletions, body.to_string().as_bytes()).unwrap()
    }

    fn completion(body: Value) -> RequestRequirements {
        requirements::extract(Endpoint::Completions, body.to_string().as_bytes()).unwrap()
    }

    fn hello() -> Value {
        json!([{"role": "user", "content": "hello"}])
    }

    fn tools() -> Value {
        json!([{"type": "function", "function": {"name": "search", "parameters": {"type": "object"}}}])
    }

    /// The route and rule `Auto` chose.
    fn decided(auto: &AutoRoute, needs: &RequestRequirements) -> (String, Option<String>) {
        let decision = auto.decide(needs);
        (
            decision.route.as_str().to_owned(),
            decision.rule.map(str::to_owned),
        )
    }

    fn standard() -> AutoRoute {
        auto(json!([
            {"name": "agentic", "when": {"requires_tools": true, "requires_reasoning": true}, "route": "Agentic"},
            {"name": "forced", "when": {"tool_choice": "required"}, "route": "Coder"},
            {"name": "tools", "when": {"requires_tools": true}, "route": "Coder"},
            {"name": "reasoning", "when": {"requires_reasoning": true}, "route": "Reasoning"},
            {"name": "large-context", "when": {"min_prompt_tokens": 12000}, "route": "LongContext"},
            {"name": "completion", "when": {"endpoint": "completion"}, "route": "Completion"}
        ]))
    }

    #[test]
    fn without_a_section_there_is_no_auto() {
        let config = crate::config::validate(
            serde_json::from_value(json!({
                "nodes": [{"id": "a", "url": "http://192.0.2.10:11434"}],
                "routes": [{"name": "General", "deployments": [{"node": "a", "model": "G"}]}]
            }))
            .unwrap(),
            &|_| None,
        )
        .unwrap();
        assert!(config.auto.is_none());
    }

    #[test]
    fn auto_claims_its_name_only_while_enabled() {
        let on = standard();
        assert!(on.claims(Some("Auto")));
        assert!(on.claims(Some(" auto ")), "route names ignore case");
        assert!(!on.claims(Some("Coder")));
        assert!(!on.claims(None), "an omitted model is the default route");
        assert!(!on.claims(Some("default")));

        let off = config(json!({"fallback_route": "General", "rules": []}))
            .auto
            .expect("checked even while off");
        assert!(!off.enabled, "off unless turned on");
        assert!(!off.claims(Some("Auto")));
    }

    #[test]
    fn no_matching_rule_is_the_fallback_route() {
        let auto = standard();
        let (route, rule) = decided(&auto, &chat(json!({"messages": hello()})));
        assert_eq!(route, "General");
        assert_eq!(rule, None);
        assert!(
            auto.decide(&chat(json!({"messages": hello()})))
                .is_fallback()
        );
        assert_eq!(
            auto.decide(&chat(json!({"messages": hello()})))
                .rule_label(),
            FALLBACK_RULE
        );
    }

    #[test]
    fn each_trait_reaches_its_rule() {
        let auto = standard();
        let cases = [
            (
                chat(json!({"messages": hello(), "tools": tools()})),
                "Coder",
                "tools",
            ),
            (
                chat(json!({"messages": hello(), "tools": tools(), "tool_choice": "required"})),
                "Coder",
                "forced",
            ),
            (
                chat(json!({"messages": hello(), "reasoning_effort": "high"})),
                "Reasoning",
                "reasoning",
            ),
            (
                chat(json!({"messages": [{"role": "user", "content": "w".repeat(120_000)}]})),
                "LongContext",
                "large-context",
            ),
            (
                completion(json!({"prompt": "Once upon a time"})),
                "Completion",
                "completion",
            ),
            (
                chat(json!({"messages": hello(), "tools": tools(), "reasoning_effort": "low"})),
                "Agentic",
                "agentic",
            ),
        ];
        for (needs, route, rule) in cases {
            assert_eq!(
                decided(&auto, &needs),
                (route.to_owned(), Some(rule.to_owned())),
                "{needs:?}"
            );
        }
    }

    #[test]
    fn rules_are_tried_in_order_and_the_first_match_wins() {
        let both = chat(json!({"messages": hello(), "tools": tools(), "reasoning_effort": "high"}));
        let compound_first = auto(json!([
            {"name": "agentic", "when": {"requires_tools": true, "requires_reasoning": true}, "route": "Agentic"},
            {"name": "tools", "when": {"requires_tools": true}, "route": "Coder"},
            {"name": "reasoning", "when": {"requires_reasoning": true}, "route": "Reasoning"}
        ]));
        assert_eq!(decided(&compound_first, &both).0, "Agentic");

        // The same rules reordered: a different, equally deterministic answer.
        let tools_first = auto(json!([
            {"name": "tools", "when": {"requires_tools": true}, "route": "Coder"},
            {"name": "agentic", "when": {"requires_tools": true, "requires_reasoning": true}, "route": "Agentic"},
            {"name": "reasoning", "when": {"requires_reasoning": true}, "route": "Reasoning"}
        ]));
        assert_eq!(decided(&tools_first, &both).0, "Coder");
        let reasoning_first = auto(json!([
            {"name": "reasoning", "when": {"requires_reasoning": true}, "route": "Reasoning"},
            {"name": "tools", "when": {"requires_tools": true}, "route": "Coder"}
        ]));
        assert_eq!(decided(&reasoning_first, &both).0, "Reasoning");
        for _ in 0..100 {
            assert_eq!(decided(&reasoning_first, &both).0, "Reasoning");
        }
    }

    #[test]
    fn a_compound_rule_needs_every_condition() {
        let auto = auto(json!([
            {"name": "agentic", "when": {"requires_tools": true, "requires_reasoning": true}, "route": "Agentic"}
        ]));
        let tools_only = chat(json!({"messages": hello(), "tools": tools()}));
        let reasoning_only = chat(json!({"messages": hello(), "reasoning_effort": "high"}));
        let both = chat(json!({"messages": hello(), "tools": tools(), "reasoning_effort": "high"}));
        assert_eq!(decided(&auto, &tools_only).0, "General");
        assert_eq!(decided(&auto, &reasoning_only).0, "General");
        assert_eq!(decided(&auto, &both).0, "Agentic");
    }

    #[test]
    fn false_means_absent_and_an_unset_condition_is_not_looked_at() {
        let auto = auto(json!([
            {"name": "plain", "when": {"requires_tools": false, "requires_reasoning": false}, "route": "Coder"}
        ]));
        assert_eq!(
            decided(&auto, &chat(json!({"messages": hello()}))).0,
            "Coder"
        );
        assert_eq!(
            decided(&auto, &chat(json!({"messages": hello(), "tools": tools()}))).0,
            "General"
        );
        assert_eq!(
            decided(
                &auto,
                &chat(json!({"messages": hello(), "reasoning_effort": "high"}))
            )
            .0,
            "General"
        );
        // `"tools": []` declares no tools, so it is a request without them.
        assert_eq!(
            decided(&auto, &chat(json!({"messages": hello(), "tools": []}))).0,
            "Coder"
        );

        let tools_only = super::AutoCondition {
            requires_tools: Some(true),
            ..AutoCondition::default()
        };
        // Reasoning is not mentioned, so either way it matches.
        assert!(tools_only.matches(&chat(json!({"messages": hello(), "tools": tools()}))));
        assert!(tools_only.matches(&chat(
            json!({"messages": hello(), "tools": tools(), "reasoning_effort": "high"})
        )));
    }

    #[test]
    fn tool_choice_matches_exactly() {
        let auto = auto(json!([
            {"name": "none", "when": {"tool_choice": "none"}, "route": "Reasoning"},
            {"name": "named", "when": {"tool_choice": "function"}, "route": "Agentic"},
            {"name": "auto", "when": {"tool_choice": "auto"}, "route": "Coder"},
            {"name": "unsent", "when": {"tool_choice": "unspecified", "requires_tools": true}, "route": "LongContext"}
        ]));
        let with = |choice: Value| {
            chat(json!({"messages": hello(), "tools": tools(), "tool_choice": choice}))
        };
        assert_eq!(decided(&auto, &with(json!("none"))).0, "Reasoning");
        assert_eq!(
            decided(
                &auto,
                &with(json!({"type": "function", "function": {"name": "search"}}))
            )
            .0,
            "Agentic"
        );
        assert_eq!(decided(&auto, &with(json!("auto"))).0, "Coder");
        assert_eq!(decided(&auto, &with(json!("required"))).0, "General");
        assert_eq!(
            decided(&auto, &chat(json!({"messages": hello(), "tools": tools()}))).0,
            "LongContext"
        );
    }

    #[test]
    fn the_context_thresholds_use_the_requirement_estimate_inclusively() {
        let auto = auto(json!([
            {"name": "huge", "when": {"min_prompt_tokens": 2000}, "route": "LongContext"},
            {"name": "mid", "when": {"min_prompt_tokens": 1000, "max_prompt_tokens": 1999}, "route": "Reasoning"}
        ]));
        // The estimate is message bytes / 6, rounded up — the R5 bound.
        let of = |bytes: usize| {
            chat(json!({"messages": [{"role": "user", "content": "x".repeat(bytes)}]}))
        };
        assert_eq!(of(5_994).prompt_tokens, Some(999));
        assert_eq!(decided(&auto, &of(5_994)).0, "General");
        assert_eq!(of(6_000).prompt_tokens, Some(1_000));
        assert_eq!(
            decided(&auto, &of(6_000)).0,
            "Reasoning",
            "the minimum is inclusive"
        );
        assert_eq!(
            decided(&auto, &of(11_994)).0,
            "Reasoning",
            "and so is the maximum"
        );
        assert_eq!(decided(&auto, &of(12_000)).0, "LongContext");

        // A body the router could not count matches no threshold.
        let unreadable =
            requirements::extract(Endpoint::ChatCompletions, br#"{"messages":"hi"}"#).unwrap();
        assert_eq!(unreadable.prompt_tokens, None);
        assert_eq!(decided(&auto, &unreadable).0, "General");

        // A completion is counted by its largest prompt, as R5 counts it.
        assert_eq!(
            decided(
                &auto,
                &completion(json!({"prompt": ["x", "y".repeat(12_000)]}))
            )
            .0,
            "LongContext"
        );
    }

    #[test]
    fn the_endpoint_condition_names_chat_or_completion() {
        let auto = auto(json!([
            {"name": "text", "when": {"endpoint": "completion"}, "route": "Completion"},
            {"name": "chat", "when": {"endpoint": "chat"}, "route": "Coder"}
        ]));
        assert_eq!(
            decided(&auto, &completion(json!({"prompt": "x"}))).0,
            "Completion"
        );
        assert_eq!(
            decided(&auto, &chat(json!({"messages": hello()}))).0,
            "Coder"
        );
    }

    #[test]
    fn targets_and_summaries_are_in_configured_order() {
        let auto = standard();
        let targets: Vec<&str> = auto.targets().iter().map(|r| r.as_str()).collect();
        assert_eq!(
            targets,
            [
                "Agentic",
                "Coder",
                "Reasoning",
                "LongContext",
                "Completion",
                "General"
            ]
        );
        assert_eq!(
            auto.rules[0].when.summary(),
            "requires_tools=true AND requires_reasoning=true"
        );
        assert_eq!(
            auto.rules[4].when.summary(),
            "estimated_prompt_tokens>=12000"
        );
        assert!(
            auto.summary()
                .starts_with("agentic -> Agentic, forced -> Coder")
        );
        assert!(auto.summary().ends_with("otherwise General"));
    }

    #[test]
    fn a_target_is_stored_as_the_route_is_spelled() {
        let auto = config(json!({"enabled": true, "fallback_route": "general",
            "rules": [{"name": "t", "when": {"requires_tools": true}, "route": "CODER"}]}))
        .auto
        .unwrap();
        assert_eq!(auto.fallback.as_str(), "General");
        assert_eq!(auto.rules[0].route.as_str(), "Coder");
    }

    #[test]
    fn a_missing_or_reserved_target_is_refused() {
        let found = errors(
            json!({"enabled": true, "fallback_route": "Nowhere", "rules": [
                {"name": "t", "when": {"requires_tools": true}, "route": "Missing"}
            ]}),
        );
        assert!(
            found
                .iter()
                .any(|e| e
                    .contains("fallback_route \"Nowhere\" is not one of the configured routes")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|e| e
                    .contains("rule \"t\" route \"Missing\" is not one of the configured routes")),
            "{found:?}"
        );
        let found = errors(json!({"enabled": true, "fallback_route": "default", "rules": []}));
        assert!(found.iter().any(|e| e.contains("is reserved")), "{found:?}");
    }

    #[test]
    fn auto_can_never_resolve_to_itself() {
        let found = errors(json!({"enabled": true, "fallback_route": "Auto", "rules": [
            {"name": "loop", "when": {"requires_tools": true}, "route": "auto"}
        ]}));
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found.iter().all(|e| e.contains("is Auto itself")),
            "{found:?}"
        );
    }

    #[test]
    fn a_route_called_auto_is_refused_while_the_section_exists() {
        let mut file = file(json!({"fallback_route": "General"}));
        file.routes[1].name = "AUTO".into();
        let found = crate::config::validate(file, &|_| None).unwrap_err().0;
        assert!(
            found
                .iter()
                .any(|e| matches!(e, ConfigError::AutoRouteNameTaken { name } if name == "AUTO")),
            "{found:?}"
        );
    }

    #[test]
    fn the_fallback_is_required() {
        let parsed: Result<RouterFile, _> = serde_json::from_value(json!({
            "nodes": [], "routes": [], "auto_route": {"enabled": true, "rules": []}
        }));
        let message = parsed.unwrap_err().to_string();
        assert!(message.contains("fallback_route"), "{message}");
    }

    #[test]
    fn unknown_keys_are_refused_at_every_level() {
        for auto in [
            json!({"enabled": true, "fallback_route": "General", "default_route": "General"}),
            json!({"enabled": true, "fallback_route": "General",
                   "rules": [{"name": "t", "when": {"requires_tools": true}, "route": "Coder", "weight": 2}]}),
            json!({"enabled": true, "fallback_route": "General",
                   "rules": [{"name": "t", "when": {"prompt_contains": "code"}, "route": "Coder"}]}),
            json!({"enabled": true, "fallback_route": "General",
                   "rules": [{"name": "t", "when": {"endpoint": "embeddings"}, "route": "Coder"}]}),
            json!({"enabled": true, "fallback_route": "General",
                   "rules": [{"name": "t", "when": {"tool_choice": "sometimes"}, "route": "Coder"}]}),
        ] {
            let parsed: Result<RouterFile, _> = serde_json::from_value(json!({
                "nodes": [], "routes": [], "auto_route": auto
            }));
            assert!(parsed.is_err(), "{auto}");
        }
    }

    #[test]
    fn rule_names_are_unique_safe_labels() {
        let found = errors(
            json!({"enabled": true, "fallback_route": "General", "rules": [
                {"name": "tools", "when": {"requires_tools": true}, "route": "Coder"},
                {"name": "TOOLS", "when": {"requires_reasoning": true}, "route": "Reasoning"},
                {"name": "_fallback", "when": {"requires_tools": true}, "route": "Coder"},
                {"name": "has space", "when": {"requires_tools": true}, "route": "Coder"},
                {"name": "", "when": {"requires_tools": true}, "route": "Coder"},
                {"name": "x".repeat(65), "when": {"requires_tools": true}, "route": "Coder"}
            ]}),
        );
        assert_eq!(found.len(), 5, "{found:?}");
        assert!(found[0].contains("\"TOOLS\" is used by more than one rule"));
        assert!(found[1].contains("must start with a letter or digit"));
        assert!(found[2].contains("not ' '"));
        assert!(found[3].contains("rule \"#5\" has no name"));
        assert!(found[4].contains("at most 64"));
    }

    #[test]
    fn a_rule_that_could_match_everything_or_nothing_is_refused() {
        let found = errors(
            json!({"enabled": true, "fallback_route": "General", "rules": [
                {"name": "everything", "when": {}, "route": "Coder"},
                {"name": "zero", "when": {"min_prompt_tokens": 0}, "route": "Coder"},
                {"name": "inverted", "when": {"min_prompt_tokens": 500, "max_prompt_tokens": 100}, "route": "Coder"},
                {"name": "text-tools", "when": {"endpoint": "completion", "requires_tools": true}, "route": "Coder"},
                {"name": "text-reason", "when": {"endpoint": "completion", "requires_reasoning": true}, "route": "Coder"},
                {"name": "text-choice", "when": {"endpoint": "completion", "tool_choice": "auto"}, "route": "Coder"},
                {"name": "forced-none", "when": {"requires_tools": false, "tool_choice": "required"}, "route": "Coder"}
            ]}),
        );
        assert_eq!(found.len(), 7, "{found:?}");
        assert!(found[0].contains("has no conditions"));
        assert!(found[1].contains("min_prompt_tokens must be at least 1"));
        assert!(found[2].contains("greater than max_prompt_tokens"));
        assert!(found[3].contains("never declares tools"));
        assert!(found[4].contains("never asks for reasoning"));
        assert!(found[5].contains("never sends tool_choice"));
        assert!(found[6].contains("only valid with tools"));
        // These are fine: a completion with no tools, and a bare tool_choice.
        let _ = auto(json!([
            {"name": "a", "when": {"endpoint": "completion", "requires_tools": false}, "route": "Completion"},
            {"name": "b", "when": {"endpoint": "completion", "tool_choice": "unspecified"}, "route": "Completion"},
            {"name": "c", "when": {"tool_choice": "none"}, "route": "Coder"}
        ]));
    }

    #[test]
    fn too_many_rules_are_refused() {
        let rules: Vec<Value> = (0..=MAX_RULES)
            .map(|i| json!({"name": format!("r{i}"), "when": {"requires_tools": true}, "route": "Coder"}))
            .collect();
        let found = errors(json!({"enabled": true, "fallback_route": "General", "rules": rules}));
        assert!(
            found.iter().any(|e| e.contains("at most 64 are allowed")),
            "{found:?}"
        );
    }

    #[test]
    fn a_disabled_section_is_still_checked() {
        let found = errors(json!({"enabled": false, "fallback_route": "Missing"}));
        assert!(found.iter().any(|e| e.contains("Missing")), "{found:?}");
    }

    #[test]
    fn the_top_level_default_route_cannot_be_auto() {
        let mut file = file(json!({"enabled": true, "fallback_route": "General"}));
        file.default_route = Some("Auto".into());
        let found = crate::config::validate(file, &|_| None).unwrap_err().0;
        assert!(
            found
                .iter()
                .any(|e| matches!(e, ConfigError::UnknownDefaultRoute { .. })),
            "{found:?}"
        );
    }

    #[test]
    fn deciding_is_cheap() {
        let auto = standard();
        let needs = chat(json!({"messages": hello()}));
        let started = std::time::Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(auto.decide(std::hint::black_box(&needs)));
        }
        // Every rule tried and none matched, ten thousand times. A
        // generous bound for a debug build on a slow machine.
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
    }
}
