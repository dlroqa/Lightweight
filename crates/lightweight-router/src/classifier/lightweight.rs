//! The Lightweight classifier provider (R9.1): a configured logical route,
//! asked through the router's own pipeline.
//!
//! The classifier route is an ordinary route — health, capability filtering,
//! its policy and failover choose which of its deployments answers, and it
//! never names a node. It is sent one short chat completion and must answer
//! with one JSON object naming a candidate and a confidence. Everything stays
//! on the operator's own gateways.

use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Candidate, ClassificationInput, ClassifierOutcome, Limits, Verdict};
use crate::RouterState;
use crate::budget::RequestBudget;
use crate::domain::{Route, RouteName};
use crate::proxy::{Endpoint, REQUEST_ID_HEADER};

/// The output budget of a classification: one short JSON object.
const ANSWER_TOKENS: u32 = 48;
/// How much of the classifier's answer is read.
const ANSWER_LIMIT: usize = 64 * 1024;

/// The `classifier.lightweight` block, as written. The R9.1 shorthand — the
/// same keys directly in the classifier section — reads into this too.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LightweightFile {
    /// The configured route that classifies. Never `Auto`.
    pub route: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default = "super::default_min_confidence")]
    pub min_confidence: f64,
    #[serde(default = "super::default_max_input_chars")]
    pub max_input_chars: usize,
}

/// The validated Lightweight provider.
#[derive(Clone, Debug)]
pub struct LightweightClassifier {
    pub route: RouteName,
    pub limits: Limits,
}

/// Check the Lightweight provider's settings. `prefix` names where they were
/// written (`lightweight.` or, for the R9.1 shorthand, nothing).
pub(super) fn validate(
    raw: &LightweightFile,
    routes: &[Route],
    prefix: &str,
    fail: &mut dyn FnMut(String),
) -> Option<LightweightClassifier> {
    let route = super::configured(&raw.route, routes)
        .map_err(|problem| fail(format!("{prefix}route {:?} {problem}", raw.route)))
        .ok();
    let limits = super::validate_limits(
        prefix,
        raw.timeout_ms,
        raw.min_confidence,
        raw.max_input_chars,
        fail,
    );
    Some(LightweightClassifier {
        route: route?,
        limits: limits?,
    })
}

/// The chat request sent to the classifier route.
///
/// Deterministic (`temperature: 0`), short, never streamed, and with thinking
/// off: a classification is one JSON object, not an essay.
pub fn request_body(
    route: &RouteName,
    candidates: &[Candidate],
    input: &ClassificationInput,
) -> Value {
    let mut routes = String::new();
    for candidate in candidates {
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
        "model": route.as_str(),
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
    Ok(Verdict {
        route,
        confidence,
        model: None,
    })
}

/// Ask the classifier route which candidate should answer `input`.
///
/// The request goes through the router's own pipeline as a nested request
/// (`<request id>-classify`): the classifier route's health, capabilities and
/// policy choose where it runs. Dropping this future — on timeout — drops the
/// nested request, which closes its upstream connection. The nested request
/// inherits the client request's pre-commit budget and starts none of its own.
pub(super) async fn call(
    state: &Arc<RouterState>,
    classifier: &LightweightClassifier,
    candidates: &[Candidate],
    input: &ClassificationInput,
    nested_id: &str,
    budget: Option<RequestBudget>,
) -> Result<Verdict, ClassifierOutcome> {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(nested_id) {
        headers.insert(REQUEST_ID_HEADER, value);
    }
    let body = Bytes::from(request_body(&classifier.route, candidates, input).to_string());
    let response = crate::proxy::forward_nested(
        Arc::clone(state),
        Endpoint::ChatCompletions,
        headers,
        body,
        budget,
    )
    .await;
    // The nested request ran out of the budget it inherited: the router's own
    // marker, never a node's 504 of the same name.
    if response
        .extensions()
        .get::<crate::budget::ExhaustedMarker>()
        .is_some()
    {
        return Err(ClassifierOutcome::RequestBudgetExhausted);
    }
    if !response.status().is_success() {
        return Err(ClassifierOutcome::Unavailable);
    }
    let bytes = axum::body::to_bytes(response.into_body(), ANSWER_LIMIT)
        .await
        .map_err(|_| ClassifierOutcome::Unavailable)?;
    parse_answer(&bytes, candidates).map_err(|_| ClassifierOutcome::Invalid)
}
