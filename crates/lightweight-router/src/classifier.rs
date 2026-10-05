//! Content-aware route classification (R9.1): when an `Auto` rule asks for
//! it, a classifier reads what the request says and recommends one of the
//! operator's logical routes.
//!
//! This is the only part of the router that reads a prompt for meaning, and it
//! is bounded on every side:
//!
//! * **Invoked, never ambient.** It runs only when an `Auto` rule written
//!   `"classify": true` matches. A request that names a route, an `Auto`
//!   request an ordinary rule resolves, and every configuration without such a
//!   rule never reach it.
//! * **A route, not a node.** The classifier is itself a configured logical
//!   route, called through the router's own pipeline — health, capability
//!   filtering, the route's policy, failover — like any request. It can only
//!   answer with one of the candidate routes it was given; anything else is a
//!   failure, never a new route.
//! * **Always answered.** A timeout, an unavailable classifier route, an
//!   answer that is not one of the candidates, or one below the confidence
//!   threshold falls back to the configured fallback route. `Auto` never fails
//!   because the classifier did.
//! * **Never recursive.** The classifier route cannot be `Auto`, and a
//!   classification request is marked nested: should it ever reach a
//!   classifying rule, it takes the fallback instead of classifying again.
//! * **Bounded input.** Only the last user message (a completion's first
//!   prompt), cut to `max_input_chars`, and the request's structural traits.
//!   No history, system prompt or tool schema is sent, and none of it is
//!   logged, traced or counted.
//!
//! Which deployment answers the chosen route is not decided here: the
//! recommendation is a route name, and the request then goes through that
//! route's pipeline exactly as if the client had named it.

use std::time::Duration;

use lightweight_api::chat::ChatCompletionRequest;
use lightweight_api::completions::CompletionRequest;
use lightweight_catalog::alias;
use lightweight_inference::generation::{MessageRole, Prompt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auto_route::is_auto;
use crate::config::ConfigError;
use crate::domain::{Route, RouteName};
use crate::proxy::Endpoint;
use crate::requirements::RequestRequirements;

/// How long a classification may take, by default. Classification sits in
/// front of the request's own answer, so it is short; a slow machine, or a
/// larger classifier model, raises it.
pub const DEFAULT_TIMEOUT_MS: u64 = 1_500;
/// The longest a configuration may let a classification take.
pub const MAX_TIMEOUT_MS: u64 = 120_000;
/// The confidence below which a recommendation is not taken, by default.
pub const DEFAULT_MIN_CONFIDENCE: f64 = 0.65;
/// How much of the request's text the classifier is sent, by default.
pub const DEFAULT_MAX_INPUT_CHARS: usize = 2_000;
/// The most text a configuration may let the classifier be sent.
pub const MAX_INPUT_CHARS: usize = 32_000;
/// The most candidate routes one classifier may choose among.
pub const MAX_CANDIDATES: usize = 16;
/// The longest route description accepted.
pub const MAX_DESCRIPTION_CHARS: usize = 200;
/// The output budget of a classification: one short JSON object.
const ANSWER_TOKENS: u32 = 48;

/// The `auto_route.classifier` section, as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierFile {
    /// The configured route that classifies. Never `Auto`.
    pub route: String,
    /// The routes it may recommend, in the order they are described to it.
    pub routes: Vec<String>,
    /// Where a classification that fails or is unsure goes. Absent: the
    /// `auto_route.fallback_route`.
    #[serde(default)]
    pub fallback_route: Option<String>,
    #[serde(default = "default_min_confidence")]
    pub min_confidence: f64,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_max_input_chars")]
    pub max_input_chars: usize,
}

const fn default_min_confidence() -> f64 {
    DEFAULT_MIN_CONFIDENCE
}
const fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}
const fn default_max_input_chars() -> usize {
    DEFAULT_MAX_INPUT_CHARS
}

/// A route the classifier may recommend, and what the operator said it is for.
#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub route: RouteName,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The validated classifier.
#[derive(Clone, Debug)]
pub struct RouteClassifier {
    pub route: RouteName,
    pub candidates: Vec<Candidate>,
    pub fallback: RouteName,
    pub min_confidence: f64,
    pub timeout: Duration,
    pub max_input_chars: usize,
}

/// How one classification ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClassifierOutcome {
    /// A candidate, at or above the threshold: it is the route.
    Chosen,
    /// A candidate below the threshold: the fallback is the route.
    LowConfidence,
    /// An answer that named no candidate, or was not the JSON asked for.
    Invalid,
    /// The classifier route refused or could not answer.
    Unavailable,
    /// No answer within the timeout.
    Timeout,
    /// A classification request reached a classifying rule itself. Prevented
    /// by validation; kept so recursion is impossible rather than unlikely.
    Nested,
}

impl ClassifierOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chosen => "chosen",
            Self::LowConfidence => "low_confidence",
            Self::Invalid => "invalid",
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::Nested => "nested",
        }
    }
}

/// What the classifier is told about one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassificationInput {
    pub endpoint: Endpoint,
    pub tools: bool,
    pub reasoning: bool,
    pub prompt_tokens: Option<u32>,
    /// The last user message (a completion's first prompt), at most
    /// `max_input_chars` characters.
    pub text: String,
    pub truncated: bool,
}

impl ClassificationInput {
    /// Read the text a classifier needs from `body`, through the gateway's own
    /// request types, as [`crate::requirements`] does. Traits come from the
    /// requirements already read; nothing is derived twice.
    pub fn read(
        endpoint: Endpoint,
        body: &[u8],
        needs: &RequestRequirements,
        max_chars: usize,
    ) -> Self {
        let text = match endpoint {
            Endpoint::ChatCompletions => serde_json::from_slice::<ChatCompletionRequest>(body)
                .ok()
                .and_then(|request| request.to_generation_request().ok())
                .map(|generation| match &generation.prompt {
                    Prompt::Chat(messages) => messages
                        .iter()
                        .rev()
                        .find(|message| message.role == MessageRole::User)
                        .or_else(|| messages.last())
                        .map(|message| message.content.reveal().clone())
                        .unwrap_or_default(),
                    Prompt::Text(text) => text.reveal().clone(),
                }),
            Endpoint::Completions => serde_json::from_slice::<CompletionRequest>(body)
                .ok()
                .and_then(|request| request.expand().ok())
                .and_then(|prompts| prompts.into_iter().next()),
        }
        .unwrap_or_default();
        let (text, truncated) = truncate(&text, max_chars);
        Self {
            endpoint,
            tools: needs.tools,
            reasoning: needs.reasoning,
            prompt_tokens: needs.prompt_tokens,
            text,
            truncated,
        }
    }
}

/// The first `max_chars` characters of `text`, and whether anything was cut.
fn truncate(text: &str, max_chars: usize) -> (String, bool) {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => (text[..end].to_owned(), true),
        None => (text.to_owned(), false),
    }
}

/// The chat request sent to the classifier route.
///
/// Deterministic (`temperature: 0`), short, never streamed, and with thinking
/// off: a classification is one JSON object, not an essay.
pub fn request_body(classifier: &RouteClassifier, input: &ClassificationInput) -> Value {
    let mut routes = String::new();
    for candidate in &classifier.candidates {
        routes.push_str("- ");
        routes.push_str(candidate.route.as_str());
        if let Some(description) = &candidate.description {
            routes.push_str(": ");
            routes.push_str(description);
        }
        routes.push('\n');
    }
    let system = format!(
        "You choose which route should answer a request. Choose exactly one of these routes:\n\
         {routes}\n\
         Judge by what the request asks for. A client may declare tools on every request, so \
         declared tools alone do not mean the request needs them.\n\
         Answer with one JSON object and nothing else: \
         {{\"route\": \"<one route name from the list>\", \"confidence\": <a number from 0 to 1>}}"
    );
    let user = format!(
        "Endpoint: {}. Tools declared: {}. Reasoning requested: {}. Estimated prompt tokens: {}.\n\
         Request{}:\n{}",
        input.endpoint.as_str(),
        if input.tools { "yes" } else { "no" },
        if input.reasoning { "yes" } else { "no" },
        input
            .prompt_tokens
            .map_or_else(|| "unknown".to_owned(), |tokens| tokens.to_string()),
        if input.truncated { " (truncated)" } else { "" },
        input.text,
    );
    json!({
        "model": classifier.route.as_str(),
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user}
        ],
        "max_tokens": ANSWER_TOKENS,
        "temperature": 0,
        "reasoning_effort": "none",
        "stream": false,
    })
}

/// A candidate the classifier named, and how sure it said it was.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub route: RouteName,
    pub confidence: f64,
}

/// Read the classifier's answer: a chat completion whose message is one JSON
/// object naming a candidate and a confidence in `[0, 1]`.
///
/// The object may be wrapped in other text (a model's preamble, a code fence):
/// the first `{` to the last `}` is read. Anything else — no object, a route
/// that is not a candidate, a confidence that is missing or out of range — is
/// refused. The answer is never echoed into a log: it may quote the prompt.
pub fn parse_answer(body: &[u8], candidates: &[Candidate]) -> Result<Verdict, &'static str> {
    let response: Value = serde_json::from_slice(body).map_err(|_| "not_json")?;
    let content = response["choices"][0]["message"]["content"]
        .as_str()
        .ok_or("no_content")?;
    let start = content.find('{').ok_or("no_object")?;
    let end = content.rfind('}').ok_or("no_object")?;
    if end < start {
        return Err("no_object");
    }
    let object: Value = serde_json::from_str(&content[start..=end]).map_err(|_| "no_object")?;
    let named = object["route"].as_str().ok_or("no_route")?;
    let route = candidates
        .iter()
        .find(|candidate| candidate.route.matches(named))
        .map(|candidate| candidate.route.clone())
        .ok_or("unknown_route")?;
    let confidence = object["confidence"]
        .as_f64()
        .filter(|value| (0.0..=1.0).contains(value))
        .ok_or("bad_confidence")?;
    Ok(Verdict { route, confidence })
}

/// Check the classifier section against the configured routes.
pub(crate) fn validate(
    raw: &ClassifierFile,
    routes: &[Route],
    auto_fallback: &str,
    errors: &mut Vec<ConfigError>,
) -> Option<RouteClassifier> {
    let before = errors.len();
    let mut fail = |problem: String| {
        errors.push(ConfigError::BadAutoClassifier { problem });
    };
    let route = configured(&raw.route, routes)
        .map_err(|problem| fail(format!("route {:?} {problem}", raw.route)))
        .ok();
    let fallback_name = raw.fallback_route.as_deref().unwrap_or(auto_fallback);
    let fallback = configured(fallback_name, routes)
        .map_err(|problem| fail(format!("fallback_route {fallback_name:?} {problem}")))
        .ok();
    if raw.routes.is_empty() {
        fail("routes must name at least one route to choose among".into());
    }
    if raw.routes.len() > MAX_CANDIDATES {
        fail(format!(
            "routes lists {}; at most {MAX_CANDIDATES} are allowed",
            raw.routes.len()
        ));
    }
    let mut candidates: Vec<Candidate> = Vec::with_capacity(raw.routes.len());
    for name in &raw.routes {
        match configured(name, routes) {
            Ok(found) if candidates.iter().any(|c| c.route == found) => {
                fail(format!("routes lists {name:?} more than once"));
            }
            Ok(found) => {
                let description = routes
                    .iter()
                    .find(|route| route.name == found)
                    .and_then(|route| route.description.clone());
                candidates.push(Candidate {
                    route: found,
                    description,
                });
            }
            Err(problem) => fail(format!("routes entry {name:?} {problem}")),
        }
    }
    if !(raw.min_confidence.is_finite() && (0.0..=1.0).contains(&raw.min_confidence)) {
        fail("min_confidence must be between 0 and 1".into());
    }
    if raw.timeout_ms == 0 || raw.timeout_ms > MAX_TIMEOUT_MS {
        fail(format!("timeout_ms must be between 1 and {MAX_TIMEOUT_MS}"));
    }
    if raw.max_input_chars == 0 || raw.max_input_chars > MAX_INPUT_CHARS {
        fail(format!(
            "max_input_chars must be between 1 and {MAX_INPUT_CHARS}"
        ));
    }
    if errors.len() > before {
        return None;
    }
    Some(RouteClassifier {
        route: route?,
        candidates,
        fallback: fallback?,
        min_confidence: raw.min_confidence,
        timeout: Duration::from_millis(raw.timeout_ms),
        max_input_chars: raw.max_input_chars,
    })
}

/// The configured route `name` refers to, or why none: never `Auto`, never
/// `default`, never a name the configuration does not have.
fn configured(name: &str, routes: &[Route]) -> Result<RouteName, &'static str> {
    if is_auto(name) {
        return Err("is Auto itself; the classifier works only with configured routes");
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

/// Check a route's description: what the classifier is told the route is for.
pub(crate) fn validate_description(raw: &str) -> Result<String, String> {
    let description = raw.trim();
    if description.is_empty() {
        return Err("description is empty".into());
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "description can be at most {MAX_DESCRIPTION_CHARS} characters"
        ));
    }
    if description.chars().any(char::is_control) {
        return Err("description has a control character".into());
    }
    Ok(description.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_route::AutoRoute;
    use crate::config::{RouterConfig, RouterFile};
    use crate::requirements;
    use serde_json::{Value, json};

    fn file(auto: Value) -> RouterFile {
        serde_json::from_value(json!({
            "nodes": [{"id": "a", "url": "http://192.0.2.10:11434"}],
            "routes": [
                {"name": "General", "description": "Everyday conversation", "deployments": [{"node": "a", "model": "G"}]},
                {"name": "Coder", "description": "Software engineering, debugging, code generation",
                 "deployments": [{"node": "a", "model": "C"}]},
                {"name": "Research", "deployments": [{"node": "a", "model": "R"}]},
                {"name": "ToolAgent", "deployments": [{"node": "a", "model": "T"}]},
                {"name": "RouterClassifier", "deployments": [{"node": "a", "model": "Q"}]}
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

    fn classifier_section() -> Value {
        json!({"route": "RouterClassifier", "routes": ["General", "Coder", "Research"]})
    }

    fn semantic(rules: Value) -> AutoRoute {
        config(json!({"enabled": true, "fallback_route": "General",
                      "classifier": classifier_section(), "rules": rules}))
        .auto
        .unwrap()
    }

    fn chat(body: Value) -> RequestRequirements {
        requirements::extract(Endpoint::ChatCompletions, body.to_string().as_bytes()).unwrap()
    }

    fn input(body: &Value, max: usize) -> ClassificationInput {
        ClassificationInput::read(
            Endpoint::ChatCompletions,
            body.to_string().as_bytes(),
            &chat(body.clone()),
            max,
        )
    }

    fn tools() -> Value {
        json!([{"type": "function", "function": {"name": "web_search", "parameters": {"type": "object"}}}])
    }

    fn answer(content: &str) -> Vec<u8> {
        json!({"object": "chat.completion", "model": "RouterClassifier",
               "choices": [{"index": 0, "message": {"role": "assistant", "content": content}}]})
        .to_string()
        .into_bytes()
    }

    #[test]
    fn the_classifier_is_read_with_its_defaults_and_route_descriptions() {
        let auto = semantic(json!([{"name": "semantic", "when": {}, "classify": true}]));
        let classifier = auto.classifier.as_ref().unwrap();
        assert_eq!(classifier.route.as_str(), "RouterClassifier");
        assert_eq!(
            classifier.fallback.as_str(),
            "General",
            "the Auto fallback, by default"
        );
        assert_eq!(classifier.min_confidence, DEFAULT_MIN_CONFIDENCE);
        assert_eq!(
            classifier.timeout,
            Duration::from_millis(DEFAULT_TIMEOUT_MS)
        );
        assert_eq!(classifier.max_input_chars, DEFAULT_MAX_INPUT_CHARS);
        let described: Vec<(&str, Option<&str>)> = classifier
            .candidates
            .iter()
            .map(|c| (c.route.as_str(), c.description.as_deref()))
            .collect();
        assert_eq!(
            described,
            [
                ("General", Some("Everyday conversation")),
                (
                    "Coder",
                    Some("Software engineering, debugging, code generation")
                ),
                ("Research", None),
            ]
        );
        // The classifying rule resolves to the fallback unless the classifier
        // chooses.
        assert!(auto.rules[0].classify);
        assert_eq!(auto.rules[0].route.as_str(), "General");
    }

    #[test]
    fn a_classifier_section_without_a_classifying_rule_is_inert() {
        let auto = semantic(
            json!([{"name": "tools", "when": {"requires_tools": true}, "route": "Coder"}]),
        );
        let decision = auto.decide(&chat(
            json!({"messages": [{"role": "user", "content": "hi"}], "tools": tools()}),
        ));
        assert!(!decision.classify);
        assert_eq!(decision.route.as_str(), "Coder");
        let decision = auto.decide(&chat(
            json!({"messages": [{"role": "user", "content": "hi"}]}),
        ));
        assert!(!decision.classify, "the plain fallback never classifies");
    }

    #[test]
    fn deterministic_rules_still_come_first_and_classification_is_a_rule_action() {
        let auto = semantic(json!([
            {"name": "forced-tools", "when": {"tool_choice": "required"}, "route": "ToolAgent"},
            {"name": "large-context", "when": {"min_prompt_tokens": 12000}, "route": "Research"},
            {"name": "semantic", "when": {}, "classify": true}
        ]));
        let forced = auto.decide(&chat(
            json!({"messages": [{"role": "user", "content": "go"}],
                                             "tools": tools(), "tool_choice": "required"}),
        ));
        assert_eq!(
            (forced.route.as_str(), forced.classify),
            ("ToolAgent", false)
        );
        // Tools present but not forced: no deterministic rule matches, so the
        // classifier is asked — what Lightagent's every turn looks like.
        let lightagent = auto.decide(&chat(
            json!({"messages": [{"role": "user", "content": "hello"}],
                                                 "tools": tools()}),
        ));
        assert_eq!(lightagent.rule, Some("semantic"));
        assert!(lightagent.classify);
        assert_eq!(lightagent.route.as_str(), "General");
    }

    #[test]
    fn targets_include_the_candidates_in_place() {
        let auto = semantic(json!([
            {"name": "forced-tools", "when": {"tool_choice": "required"}, "route": "ToolAgent"},
            {"name": "semantic", "when": {}, "classify": true}
        ]));
        let targets: Vec<&str> = auto.targets().iter().map(|r| r.as_str()).collect();
        assert_eq!(targets, ["ToolAgent", "General", "Coder", "Research"]);
        assert!(
            !targets.contains(&"RouterClassifier"),
            "the classifier is not a target"
        );
        assert_eq!(
            auto.summary(),
            "forced-tools -> ToolAgent, semantic -> classify (else General), otherwise General"
        );
    }

    #[test]
    fn a_classifying_rule_needs_a_classifier_and_exactly_one_action() {
        let found = errors(
            json!({"enabled": true, "fallback_route": "General", "rules": [
                {"name": "semantic", "when": {}, "classify": true},
                {"name": "both", "when": {"requires_tools": true}, "route": "Coder", "classify": true},
                {"name": "neither", "when": {"requires_tools": true}}
            ]}),
        );
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found[0].contains("has no \"classifier\""));
        assert!(found[1].contains("needs exactly one"));
        assert!(found[2].contains("needs a \"route\", or \"classify\": true"));
        // An empty `when` is still refused for an ordinary rule.
        let found = errors(json!({"enabled": true, "fallback_route": "General",
            "classifier": classifier_section(),
            "rules": [{"name": "everything", "when": {}, "route": "Coder"}]}));
        assert!(found[0].contains("has no conditions"), "{found:?}");
    }

    #[test]
    fn the_classifier_can_only_name_configured_routes_and_never_auto() {
        let found = errors(json!({"enabled": true, "fallback_route": "General",
            "classifier": {"route": "Auto", "routes": ["General", "auto", "Nowhere", "Coder", "coder"],
                           "fallback_route": "default"},
            "rules": [{"name": "semantic", "when": {}, "classify": true}]}));
        assert!(
            found
                .iter()
                .any(|e| e.contains("route \"Auto\" is Auto itself")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|e| e.contains("routes entry \"auto\" is Auto itself")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|e| e.contains("\"Nowhere\" is not one of the configured routes"))
        );
        assert!(
            found
                .iter()
                .any(|e| e.contains("lists \"coder\" more than once"))
        );
        assert!(
            found
                .iter()
                .any(|e| e.contains("fallback_route \"default\" is reserved"))
        );
    }

    #[test]
    fn classifier_limits_are_checked() {
        let found = errors(json!({"enabled": true, "fallback_route": "General",
            "classifier": {"route": "RouterClassifier", "routes": [],
                           "min_confidence": 1.5, "timeout_ms": 0, "max_input_chars": 0},
            "rules": [{"name": "semantic", "when": {}, "classify": true}]}));
        for expected in [
            "at least one route",
            "min_confidence must be between 0 and 1",
            "timeout_ms must be between 1 and 120000",
            "max_input_chars must be between 1 and 32000",
        ] {
            assert!(
                found.iter().any(|e| e.contains(expected)),
                "{expected}: {found:?}"
            );
        }
        let unknown: Result<RouterFile, _> = serde_json::from_value(json!({
            "nodes": [], "routes": [], "auto_route": {"fallback_route": "G",
                "classifier": {"route": "C", "routes": ["G"], "provider_url": "http://x"}}}));
        assert!(unknown.is_err(), "unknown classifier keys are refused");
    }

    #[test]
    fn route_descriptions_are_bounded_and_optional() {
        let mut bad = file(json!({"fallback_route": "General"}));
        bad.routes[0].description = Some("x".repeat(MAX_DESCRIPTION_CHARS + 1));
        bad.routes[1].description = Some("line\nbreak".into());
        bad.routes[2].description = Some("   ".into());
        let found = crate::config::validate(bad, &|_| None).unwrap_err().0;
        assert_eq!(found.len(), 3, "{found:?}");
        // A configuration without descriptions, or without auto_route, is as
        // before.
        let plain = crate::config::validate(
            serde_json::from_value(json!({
                "nodes": [{"id": "a", "url": "http://192.0.2.10:11434"}],
                "routes": [{"name": "General", "deployments": [{"node": "a", "model": "G"}]}]
            }))
            .unwrap(),
            &|_| None,
        )
        .unwrap();
        assert_eq!(plain.topology.routes()[0].description, None);
    }

    #[test]
    fn the_input_is_the_last_user_message_cut_to_the_limit() {
        let body = json!({"model": "Auto", "tools": tools(), "messages": [
            {"role": "system", "content": "You are a secret system prompt."},
            {"role": "user", "content": "first question"},
            {"role": "assistant", "content": "an answer"},
            {"role": "user", "content": "write a Rust async TCP server"}
        ]});
        let read = input(&body, 100);
        assert_eq!(read.text, "write a Rust async TCP server");
        assert!(!read.truncated);
        assert!(read.tools && !read.reasoning);
        assert_eq!(read.endpoint, Endpoint::ChatCompletions);

        let cut = input(&body, 5);
        assert_eq!(cut.text, "write");
        assert!(cut.truncated);
        // Cut on a character, never inside one.
        let wide = input(
            &json!({"messages": [{"role": "user", "content": "héllo wörld"}]}),
            4,
        );
        assert_eq!(wide.text, "héll");

        let completion = ClassificationInput::read(
            Endpoint::Completions,
            json!({"prompt": ["Once upon a time", "ignored"]})
                .to_string()
                .as_bytes(),
            &RequestRequirements::none(),
            100,
        );
        assert_eq!(completion.text, "Once upon a time");
        let unreadable = ClassificationInput::read(
            Endpoint::ChatCompletions,
            br#"{"messages":"hi"}"#,
            &RequestRequirements::none(),
            100,
        );
        assert_eq!(unreadable.text, "");
    }

    #[test]
    fn the_request_names_only_the_candidates_and_carries_no_history() {
        let auto = semantic(json!([{"name": "semantic", "when": {}, "classify": true}]));
        let classifier = auto.classifier.as_ref().unwrap();
        let body = json!({"model": "Auto", "tools": tools(), "messages": [
            {"role": "system", "content": "You are a secret system prompt."},
            {"role": "user", "content": "compare today's GPU announcements"}
        ]});
        let request = request_body(classifier, &input(&body, 2_000));
        assert_eq!(request["model"], "RouterClassifier");
        assert_eq!(request["stream"], false);
        assert_eq!(request["temperature"], 0);
        assert_eq!(request["reasoning_effort"], "none");
        assert!(
            request.get("tools").is_none(),
            "the client's tools are not forwarded"
        );
        let text = request.to_string();
        for name in [
            "General",
            "Coder",
            "Research",
            "Software engineering, debugging",
        ] {
            assert!(text.contains(name), "{name}");
        }
        for absent in [
            "ToolAgent",
            "RouterClassifier\\n",
            "secret system prompt",
            "web_search",
            "node",
            "\"a\"",
        ] {
            assert!(
                !text.contains(absent),
                "{absent} reached the classifier: {text}"
            );
        }
        assert!(text.contains("compare today's GPU announcements"));
        assert!(text.contains("Tools declared: yes"));
    }

    #[test]
    fn only_a_candidate_with_a_confidence_in_range_is_a_verdict() {
        let auto = semantic(json!([{"name": "semantic", "when": {}, "classify": true}]));
        let candidates = &auto.classifier.as_ref().unwrap().candidates;
        let parse = |content: &str| parse_answer(&answer(content), candidates);

        let verdict = parse(r#"{"route": "Coder", "confidence": 0.91}"#).unwrap();
        assert_eq!(verdict.route.as_str(), "Coder");
        assert!((verdict.confidence - 0.91).abs() < 1e-9);
        // Case is the route's to keep, and a preamble or fence is tolerated.
        let verdict =
            parse("Sure.\n```json\n{\"route\": \"research\", \"confidence\": 1}\n```").unwrap();
        assert_eq!(verdict.route.as_str(), "Research");

        assert_eq!(
            parse(r#"{"route": "ToolAgent", "confidence": 0.9}"#),
            Err("unknown_route"),
            "configured, but not a candidate"
        );
        assert_eq!(
            parse(r#"{"route": "node-a", "confidence": 0.9}"#),
            Err("unknown_route")
        );
        assert_eq!(
            parse(r#"{"route": "Auto", "confidence": 0.9}"#),
            Err("unknown_route")
        );
        assert_eq!(parse(r#"{"route": "Coder"}"#), Err("bad_confidence"));
        assert_eq!(
            parse(r#"{"route": "Coder", "confidence": 1.2}"#),
            Err("bad_confidence")
        );
        assert_eq!(
            parse(r#"{"route": "Coder", "confidence": "high"}"#),
            Err("bad_confidence")
        );
        assert_eq!(parse("Coder"), Err("no_object"));
        assert_eq!(parse(r#"{"confidence": 0.9}"#), Err("no_route"));
        assert_eq!(parse_answer(b"not json", candidates), Err("not_json"));
        assert_eq!(
            parse_answer(br#"{"choices": []}"#, candidates),
            Err("no_content")
        );
    }
}
