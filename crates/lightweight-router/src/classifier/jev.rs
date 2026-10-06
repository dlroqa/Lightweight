//! The Jev classifier provider (R9.1a): TypeSafe AI's System One API.
//!
//! Jev is not a model the router serves and not a Lightweight node: it is an
//! external decision service the route layer may consult. It is never a
//! deployment, never in a route, never probed for health, and never reached by
//! a client through the router.
//!
//! The contract used is the one TypeSafe documents (docs.typesafe.ai/api):
//!
//! * `POST {base_url}/v1/systemone` with `Authorization: Bearer <key>` and a
//!   body of `state`, `model` and a map of typed `questions`. The router asks
//!   one **Choice** question, `route`, whose `criteria` are exactly the
//!   configured candidate routes (each with its description, or `null`).
//! * The answer is `answers.route` — `type: "choice"`, `choice` (the
//!   highest-probability option), `probabilities`, and `confidence`, which
//!   TypeSafe derives from the distribution. That `confidence` is the one the
//!   router thresholds; no second one is invented.
//! * `GET {base_url}/v1/models` lists the names an account may send — the
//!   aliases such as `jev-latest`. A pinned versioned id (`jev-1.13.0`) is
//!   accepted by `/v1/systemone` without being listed.
//! * Errors are status codes: `401` (key), `422` (validation), `429` (rate
//!   limit), `529` (overloaded). Their bodies are never read into a log, a
//!   trace or the admin view.
//!
//! What is sent leaves the operator's machines: the candidate routes and
//! their descriptions, the request's structural traits, and — unless
//! `include_user_text` is off — the last user message cut to
//! `max_input_chars`. Never a conversation's history, a system prompt, a tool
//! schema, a credential, an alias or a node address.

use std::time::Instant;

use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Candidate, ClassificationInput, ClassifierOutcome, Limits, Verdict};
use crate::domain::Secret;

/// TypeSafe's API.
pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
/// The variable TypeSafe's own documentation and SDKs read the key from.
pub const DEFAULT_API_KEY_ENV: &str = "TYPESAFE_API_KEY";
/// The question id the router asks under. Not sent to the model's inference.
const QUESTION: &str = "route";
/// How much of a response is read: a choice over at most sixteen routes, or a
/// model list.
const RESPONSE_LIMIT: usize = 256 * 1024;

/// The `classifier.jev` block, as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JevFile {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// The environment variable holding the TypeSafe API key. The key itself
    /// is never written in the file.
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    /// The model to send: an alias such as `jev-latest`, or a pinned
    /// versioned id. Required: which one is the operator's choice.
    #[serde(default)]
    pub model: Option<String>,
    /// Required, 1 to [`super::MAX_TIMEOUT_MS`]: it is a network call, and
    /// what is safe depends on the network.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default = "super::default_min_confidence")]
    pub min_confidence: f64,
    #[serde(default = "super::default_max_input_chars")]
    pub max_input_chars: usize,
    /// Send the last user message. Off: Jev is sent only the request's
    /// structural traits and the route descriptions — more private, and less
    /// able to tell a greeting from a coding question.
    #[serde(default = "yes")]
    pub include_user_text: bool,
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_owned()
}
fn default_api_key_env() -> String {
    DEFAULT_API_KEY_ENV.to_owned()
}
const fn yes() -> bool {
    true
}

/// The validated Jev provider. Its `Debug` shows the key only as
/// `<redacted>`, and nothing serializes it.
#[derive(Clone, Debug)]
pub struct JevClassifier {
    /// Normalized: no trailing `/`, so an endpoint never doubles a slash.
    pub base_url: String,
    pub api_key_env: String,
    key: Option<Secret>,
    pub model: String,
    pub limits: Limits,
    pub include_user_text: bool,
}

impl JevClassifier {
    /// Whether the key's environment variable was set when the router started.
    pub const fn api_key_configured(&self) -> bool {
        self.key.is_some()
    }

    /// The absolute URL of one of TypeSafe's endpoints.
    pub fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// What the admin view may say: settings and whether a key is present,
    /// never the key.
    pub fn view(&self) -> Value {
        json!({
            "base_url": self.base_url,
            "model": self.model,
            "api_key_env": self.api_key_env,
            "api_key_configured": self.api_key_configured(),
            "timeout_ms": u64::try_from(self.limits.timeout.as_millis()).unwrap_or(u64::MAX),
            "min_confidence": self.limits.min_confidence,
            "max_input_chars": self.limits.max_input_chars,
            "include_user_text": self.include_user_text,
        })
    }
}

/// Check the Jev provider's settings.
///
/// The key is demanded only when Jev is the active provider, as a disabled
/// node's key is not: a configured but inactive block must not stop a router
/// that does not use it. Inactive, its key is still read if present, so the
/// admin view can say whether switching would work.
pub(super) fn validate(
    raw: &JevFile,
    active: bool,
    env: &dyn Fn(&str) -> Option<String>,
    fail: &mut dyn FnMut(String),
    errors: &mut Vec<crate::config::ConfigError>,
) -> Option<JevClassifier> {
    let base_url = match crate::config::validate_url(&raw.base_url) {
        Ok(url) if url.scheme() == "https" || is_loopback(&url) => {
            Some(url.as_str().trim_end_matches('/').to_owned())
        }
        Ok(_) => {
            fail(format!(
                "jev.base_url {:?} must use https: the API key and request content travel \
                 over it (plain http is accepted only for a loopback address)",
                raw.base_url
            ));
            None
        }
        Err(problem) => {
            fail(format!("jev.base_url {:?} {problem}", raw.base_url));
            None
        }
    };
    let model = match raw.model.as_deref().map(str::trim) {
        None => {
            fail(
                "jev.model is required: name an alias such as \"jev-latest\", or pin a versioned \
                 id; GET /v1/models lists what the account may use"
                    .into(),
            );
            None
        }
        Some("") => {
            fail("jev.model is empty".into());
            None
        }
        Some(model) if model.chars().any(char::is_control) || model.len() > 128 => {
            fail("jev.model must be at most 128 characters with no control character".into());
            None
        }
        Some(model) => Some(model.to_owned()),
    };
    let limits = super::validate_limits(
        "jev.",
        raw.timeout_ms,
        raw.min_confidence,
        raw.max_input_chars,
        fail,
    );
    let key = if active {
        crate::config::read_secret("auto_route.classifier.jev", &raw.api_key_env, env, errors)
    } else {
        env(&raw.api_key_env)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(Secret::new)
    };
    if active && key.is_none() {
        return None;
    }
    Some(JevClassifier {
        base_url: base_url?,
        api_key_env: raw.api_key_env.clone(),
        key,
        model: model?,
        limits: limits?,
        include_user_text: raw.include_user_text,
    })
}

fn is_loopback(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

/// The System One request: the bounded state and one Choice question whose
/// options are exactly the candidates.
pub fn request_body(
    jev: &JevClassifier,
    candidates: &[Candidate],
    input: &ClassificationInput,
) -> Value {
    let mut state = json!({
        "endpoint": input.endpoint.as_str(),
        "tools_declared": input.tools,
        "tool_choice": input.tool_choice.as_str(),
        "reasoning_requested": input.reasoning,
        "estimated_prompt_tokens": input.prompt_tokens,
    });
    if jev.include_user_text
        && let Some(object) = state.as_object_mut()
    {
        object.insert("request".into(), json!(input.text));
        object.insert("request_truncated".into(), json!(input.truncated));
    }
    let criteria: serde_json::Map<String, Value> = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.route.as_str().to_owned(),
                candidate
                    .description
                    .as_ref()
                    .map_or(Value::Null, |description| json!(description)),
            )
        })
        .collect();
    let instructions = if jev.include_user_text {
        "Which route should answer this request? Judge by what `request` asks for. A client may \
         declare tools on every request, so `tools_declared` alone does not mean the request \
         needs a tool."
    } else {
        "Which route should answer this request? Only the request's traits are given, not its \
         text. A client may declare tools on every request, so `tools_declared` alone does not \
         mean the request needs a tool."
    };
    json!({
        "model": jev.model,
        "state": state,
        "questions": {
            QUESTION: {
                "type": "choice",
                "instructions": instructions,
                "criteria": criteria,
            }
        }
    })
}

/// Read a System One response: `answers.route` must be a Choice naming one
/// of the candidates exactly, with a `confidence` in `[0, 1]`. Anything else
/// is refused; nothing here is echoed into a log.
pub fn parse_response(body: &[u8], candidates: &[Candidate]) -> Result<Verdict, &'static str> {
    let response: Value = serde_json::from_slice(body).map_err(|_| "not_json")?;
    let answer = response
        .get("answers")
        .and_then(|answers| answers.get(QUESTION))
        .ok_or("no_answer")?;
    if answer.get("type").and_then(Value::as_str) != Some("choice") {
        return Err("not_a_choice");
    }
    let choice = answer
        .get("choice")
        .and_then(Value::as_str)
        .ok_or("no_choice")?;
    // The options were the candidates' exact names; an answer is one of them
    // or it is not an answer.
    let route = candidates
        .iter()
        .find(|candidate| candidate.route.as_str() == choice)
        .map(|candidate| candidate.route.clone())
        .ok_or("unknown_choice")?;
    let confidence = answer
        .get("confidence")
        .and_then(Value::as_f64)
        .filter(|value| (0.0..=1.0).contains(value))
        .ok_or("bad_confidence")?;
    let model = response
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| model.len() <= 128 && !model.chars().any(char::is_control))
        .map(str::to_owned);
    Ok(Verdict {
        route,
        confidence,
        model,
    })
}

/// What a TypeSafe status means for a classification. Bounded: these are the
/// only labels a metric can carry.
fn failure_for(status: StatusCode) -> ClassifierOutcome {
    match status.as_u16() {
        401 | 403 => ClassifierOutcome::AuthError,
        429 | 529 => ClassifierOutcome::RateLimited,
        _ => ClassifierOutcome::ProviderError,
    }
}

fn transport_failure(err: &reqwest::Error) -> ClassifierOutcome {
    if err.is_timeout() {
        ClassifierOutcome::Timeout
    } else {
        ClassifierOutcome::ConnectionError
    }
}

/// Read at most [`RESPONSE_LIMIT`] bytes; `None` if the body is larger or
/// breaks off.
async fn read_limited(response: reqwest::Response) -> Option<Vec<u8>> {
    let mut stream = response.bytes_stream();
    let mut collected = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if collected.len() + chunk.len() > RESPONSE_LIMIT {
            return None;
        }
        collected.extend_from_slice(&chunk);
    }
    Some(collected)
}

/// Ask Jev which candidate should answer `input`. No retry: classification is
/// bounded by the caller's timeout, and a refusal is answered by the
/// fallback route, not by waiting.
pub(super) async fn call(
    client: &reqwest::Client,
    jev: &JevClassifier,
    candidates: &[Candidate],
    input: &ClassificationInput,
) -> Result<Verdict, ClassifierOutcome> {
    let Some(key) = &jev.key else {
        return Err(ClassifierOutcome::AuthError);
    };
    let response = client
        .post(jev.endpoint("/v1/systemone"))
        .bearer_auth(key.expose())
        .json(&request_body(jev, candidates, input))
        .send()
        .await
        .map_err(|err| transport_failure(&err))?;
    let status = response.status();
    if !status.is_success() {
        return Err(failure_for(status));
    }
    let body = read_limited(response)
        .await
        .ok_or(ClassifierOutcome::Invalid)?;
    parse_response(&body, candidates).map_err(|_| ClassifierOutcome::Invalid)
}

/// The result of checking that Jev is reachable, the key is accepted and the
/// model is one the account may use. Sanitized: a status, never a body.
#[derive(Clone, Debug, Serialize)]
pub struct CheckReport {
    pub provider: &'static str,
    /// `ok`, `model_not_listed`, `api_key_missing`, `auth_error`,
    /// `rate_limited`, `provider_error`, `invalid_response`,
    /// `connection_error`, `timeout`; for the Lightweight provider `ok` or
    /// `route_unavailable`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Whether `GET /v1/models` lists the configured model. A pinned
    /// versioned id is accepted without being listed, so `false` is a warning
    /// for one, and a misconfiguration for an alias.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_listed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    pub checked_at: u64,
    pub duration_ms: f64,
}

/// `GET /v1/models`: is the key accepted, and is the configured model listed?
pub async fn check(client: &reqwest::Client, jev: &JevClassifier) -> CheckReport {
    let started = Instant::now();
    let mut report = CheckReport {
        provider: "jev",
        status: "ok",
        model: Some(jev.model.clone()),
        model_listed: None,
        http_status: None,
        checked_at: super::unix_now(),
        duration_ms: 0.0,
    };
    let Some(key) = &jev.key else {
        report.status = "api_key_missing";
        return report;
    };
    let request = client
        .get(jev.endpoint("/v1/models"))
        .bearer_auth(key.expose())
        .send();
    let outcome = match tokio::time::timeout(jev.limits.timeout, request).await {
        Err(_) => Err("timeout"),
        Ok(Err(err)) => Err(transport_failure(&err).as_str()),
        Ok(Ok(response)) => {
            let status = response.status();
            report.http_status = Some(status.as_u16());
            if status.is_success() {
                match tokio::time::timeout(jev.limits.timeout, read_limited(response)).await {
                    Ok(Some(body)) => Ok(body),
                    Ok(None) => Err("invalid_response"),
                    Err(_) => Err("timeout"),
                }
            } else {
                Err(failure_for(status).as_str())
            }
        }
    };
    report.status = match outcome {
        Err(status) => status,
        Ok(body) => match listed_models(&body) {
            None => "invalid_response",
            Some(names) => {
                let listed = names.iter().any(|name| name == &jev.model);
                report.model_listed = Some(listed);
                if listed { "ok" } else { "model_not_listed" }
            }
        },
    };
    report.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    report
}

/// The `name` of every entry in a `GET /v1/models` response.
pub fn listed_models(body: &[u8]) -> Option<Vec<String>> {
    let response: Value = serde_json::from_slice(body).ok()?;
    let models = response.get("models")?.as_array()?;
    Some(
        models
            .iter()
            .filter_map(|model| model.get("name").and_then(Value::as_str))
            .map(str::to_owned)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::{ClassifierProvider, ProviderKind, RouteClassifier};
    use crate::config::{ConfigError, RouterConfig, RouterFile};
    use crate::proxy::Endpoint;
    use crate::requirements::ToolChoiceRequirement;

    /// Short on purpose: the secrets gate refuses a committed bearer literal
    /// of 16 or more characters.
    const KEY: &str = "jev-test-key";

    fn file(classifier: Value) -> RouterFile {
        serde_json::from_value(json!({
            "nodes": [{"id": "a", "url": "http://192.0.2.10:11434"}],
            "routes": [
                {"name": "General", "description": "General conversation", "deployments": [{"node": "a", "model": "G"}]},
                {"name": "Coder", "description": "Programming, debugging, code generation",
                 "deployments": [{"node": "a", "model": "C"}]},
                {"name": "Research", "deployments": [{"node": "a", "model": "R"}]},
                {"name": "Reasoning", "deployments": [{"node": "a", "model": "M"}]},
                {"name": "ToolAgent", "deployments": [{"node": "a", "model": "T"}]},
                {"name": "RouterClassifier", "deployments": [{"node": "a", "model": "Q"}]}
            ],
            "auto_route": {"enabled": true, "fallback_route": "General", "classifier": classifier,
                           "rules": [{"name": "semantic", "when": {}, "classify": true}]},
        }))
        .expect("the file shape parses")
    }

    fn with_key(name: &str) -> Option<String> {
        (name == DEFAULT_API_KEY_ENV || name == "OTHER_KEY").then(|| KEY.to_owned())
    }

    fn validated(classifier: Value) -> Result<RouterConfig, Vec<ConfigError>> {
        crate::config::validate(file(classifier), &with_key).map_err(|errors| errors.0)
    }

    fn jev_section(jev: Value) -> Value {
        json!({"provider": "jev", "routes": ["General", "Coder", "Research", "Reasoning"], "jev": jev})
    }

    fn classifier(jev: Value) -> RouteClassifier {
        validated(jev_section(jev))
            .expect("valid")
            .auto
            .unwrap()
            .classifier
            .unwrap()
    }

    fn jev_of(classifier: &RouteClassifier) -> &JevClassifier {
        match &classifier.provider {
            ClassifierProvider::Jev(jev) => jev,
            ClassifierProvider::Lightweight(_) => panic!("not Jev"),
        }
    }

    fn minimal() -> Value {
        json!({"model": "jev-latest", "timeout_ms": 5000})
    }

    fn messages(errors: &[ConfigError]) -> Vec<String> {
        errors.iter().map(ToString::to_string).collect()
    }

    fn input(text: &str) -> ClassificationInput {
        ClassificationInput {
            endpoint: Endpoint::ChatCompletions,
            tools: true,
            tool_choice: ToolChoiceRequirement::Unspecified,
            reasoning: false,
            prompt_tokens: Some(12),
            text: text.to_owned(),
            truncated: false,
        }
    }

    fn answer(choice: &str, confidence: f64) -> Vec<u8> {
        json!({"model": "jev-1.13.0",
               "answers": {"route": {"type": "choice", "choice": choice, "confidence": confidence,
                                     "probabilities": {choice: confidence}}},
               "usage": {"input_tokens": 300, "output_tokens": 20}})
        .to_string()
        .into_bytes()
    }

    #[test]
    fn the_jev_block_is_read_with_its_defaults() {
        let classifier = classifier(minimal());
        assert_eq!(classifier.provider.kind(), ProviderKind::Jev);
        let jev = jev_of(&classifier);
        assert_eq!(jev.base_url, "https://api.typesafe.ai");
        assert_eq!(jev.api_key_env, "TYPESAFE_API_KEY");
        assert!(jev.api_key_configured());
        assert_eq!(jev.model, "jev-latest");
        assert_eq!(jev.limits.timeout, std::time::Duration::from_secs(5));
        assert_eq!(jev.limits.min_confidence, 0.65);
        assert_eq!(jev.limits.max_input_chars, 2000);
        assert!(jev.include_user_text);
        assert_eq!(classifier.fallback.as_str(), "General");
        assert!(classifier.standby.is_none());
    }

    #[test]
    fn model_and_timeout_are_required_and_limits_bounded() {
        let found = messages(&validated(jev_section(json!({}))).unwrap_err());
        assert!(
            found.iter().any(|e| e.contains("jev.model is required")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|e| e.contains("jev.timeout_ms is required")),
            "{found:?}"
        );
        let found = messages(
            &validated(jev_section(json!({"model": " ", "timeout_ms": 120_001,
                "min_confidence": -0.1, "max_input_chars": 0})))
            .unwrap_err(),
        );
        for expected in [
            "jev.model is empty",
            "jev.timeout_ms must be between 1 and 120000",
            "jev.min_confidence must be between 0 and 1",
            "jev.max_input_chars must be between 1 and 32000",
        ] {
            assert!(
                found.iter().any(|e| e.contains(expected)),
                "{expected}: {found:?}"
            );
        }
        let unknown: Result<RouterFile, _> = serde_json::from_value(json!({
            "nodes": [], "routes": [], "auto_route": {"fallback_route": "G", "classifier":
                {"provider": "jev", "routes": ["G"], "jev": {"model": "m", "timeout_ms": 1, "api_key": "x"}}}}));
        assert!(
            unknown.is_err(),
            "a literal key is an unknown field, refused"
        );
        let provider: Result<RouterFile, _> = serde_json::from_value(json!({
            "nodes": [], "routes": [], "auto_route": {"fallback_route": "G", "classifier":
                {"provider": "openai", "routes": ["G"]}}}));
        assert!(provider.is_err(), "only the providers there are");
    }

    #[test]
    fn the_key_comes_from_its_environment_variable_and_only_the_active_provider_demands_it() {
        let missing = |name: &str| (name == "SOMETHING_ELSE").then(|| KEY.to_owned());
        let errors = crate::config::validate(file(jev_section(minimal())), &missing)
            .unwrap_err()
            .0;
        assert!(
            errors.iter().any(
                |e| matches!(e, ConfigError::MissingEnv { var, .. } if var == "TYPESAFE_API_KEY")
            ),
            "{errors:?}"
        );
        let named = classifier(
            json!({"model": "jev-latest", "timeout_ms": 5000, "api_key_env": "OTHER_KEY"}),
        );
        assert!(jev_of(&named).api_key_configured());
        let bad = validated(jev_section(
            json!({"model": "jev-latest", "timeout_ms": 5000, "api_key_env": "has space"}),
        ))
        .unwrap_err();
        assert!(
            bad.iter()
                .any(|e| matches!(e, ConfigError::BadEnvName { .. })),
            "{bad:?}"
        );

        // Configured but not active: no key needed, and its absence is shown.
        let standby = crate::config::validate(
            file(
                json!({"provider": "lightweight", "routes": ["General", "Coder"],
                        "lightweight": {"route": "RouterClassifier", "timeout_ms": 30_000},
                        "jev": minimal()}),
            ),
            &missing,
        )
        .expect("an inactive Jev block needs no key")
        .auto
        .unwrap()
        .classifier
        .unwrap();
        assert_eq!(standby.provider.kind(), ProviderKind::Lightweight);
        match &standby.standby {
            Some(ClassifierProvider::Jev(jev)) => assert!(!jev.api_key_configured()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_key_is_never_shown() {
        let classifier = classifier(minimal());
        let jev = jev_of(&classifier);
        let printed = format!("{classifier:?} {jev:?}");
        assert!(!printed.contains(KEY), "{printed}");
        assert!(printed.contains("<redacted>"));
        let view = jev.view().to_string();
        assert!(!view.contains(KEY), "{view}");
        assert!(view.contains("\"api_key_configured\":true"));
        assert!(
            view.contains("TYPESAFE_API_KEY"),
            "the variable's name is shown, not its value"
        );
        let report = CheckReport {
            provider: "jev",
            status: "auth_error",
            model: Some("jev-latest".into()),
            model_listed: None,
            http_status: Some(401),
            checked_at: 0,
            duration_ms: 1.0,
        };
        assert!(!serde_json::to_string(&report).unwrap().contains(KEY));
    }

    #[test]
    fn the_base_url_is_normalized_and_must_be_https_off_loopback() {
        for (raw, endpoint) in [
            (
                "https://api.typesafe.ai",
                "https://api.typesafe.ai/v1/systemone",
            ),
            (
                "https://api.typesafe.ai///",
                "https://api.typesafe.ai/v1/systemone",
            ),
            (
                "https://gw.example.net/typesafe/",
                "https://gw.example.net/typesafe/v1/systemone",
            ),
            (
                "http://127.0.0.1:9100",
                "http://127.0.0.1:9100/v1/systemone",
            ),
            (
                "http://localhost:9100/",
                "http://localhost:9100/v1/systemone",
            ),
            ("http://[::1]:9100", "http://[::1]:9100/v1/systemone"),
        ] {
            let classifier =
                classifier(json!({"model": "jev-latest", "timeout_ms": 5000, "base_url": raw}));
            assert_eq!(
                jev_of(&classifier).endpoint("/v1/systemone"),
                endpoint,
                "{raw}"
            );
        }
        for raw in [
            "http://api.typesafe.ai",
            "http://192.0.2.50",
            "ftp://api.typesafe.ai",
            "https://user:pw@api.typesafe.ai",
            "https://api.typesafe.ai?key=1",
            "not a url",
        ] {
            let found = messages(
                &validated(jev_section(
                    json!({"model": "jev-latest", "timeout_ms": 5000, "base_url": raw}),
                ))
                .unwrap_err(),
            );
            assert!(
                found.iter().any(|e| e.contains("jev.base_url")),
                "{raw}: {found:?}"
            );
        }
    }

    #[test]
    fn switching_provider_changes_one_word_and_no_rule() {
        let both = |provider: &str| {
            validated(json!({"provider": provider, "routes": ["General", "Coder"],
                "lightweight": {"route": "RouterClassifier", "timeout_ms": 30_000},
                "jev": minimal()}))
            .unwrap()
            .auto
            .unwrap()
        };
        let lightweight = both("lightweight");
        let jev = both("jev");
        assert_eq!(
            lightweight.classifier.as_ref().unwrap().provider.kind(),
            ProviderKind::Lightweight
        );
        assert_eq!(
            jev.classifier.as_ref().unwrap().provider.kind(),
            ProviderKind::Jev
        );
        assert_eq!(
            lightweight
                .classifier
                .as_ref()
                .unwrap()
                .standby
                .as_ref()
                .map(ClassifierProvider::kind),
            Some(ProviderKind::Jev)
        );
        assert_eq!(
            lightweight.summary(),
            jev.summary(),
            "the rules are the same"
        );
        assert!(lightweight.rules[0].classify && jev.rules[0].classify);

        // The R9.1 shorthand is the Lightweight block; both at once is refused.
        let found = messages(
            &validated(
                json!({"routes": ["General"], "route": "RouterClassifier", "timeout_ms": 1,
                "lightweight": {"route": "RouterClassifier", "timeout_ms": 1}}),
            )
            .unwrap_err(),
        );
        assert!(found.iter().any(|e| e.contains("not both")), "{found:?}");
        let found =
            messages(&validated(json!({"provider": "jev", "routes": ["General"]})).unwrap_err());
        assert!(
            found
                .iter()
                .any(|e| e.contains("provider \"jev\" needs a \"jev\" block")),
            "{found:?}"
        );
        let found = messages(&validated(json!({"routes": ["General"]})).unwrap_err());
        assert!(
            found
                .iter()
                .any(|e| e.contains("provider \"lightweight\" needs its settings")),
            "{found:?}"
        );
    }

    #[test]
    fn the_request_is_one_choice_over_exactly_the_candidates() {
        let classifier = classifier(minimal());
        let jev = jev_of(&classifier);
        let body = request_body(
            jev,
            &classifier.candidates,
            &input("write a Rust async TCP server"),
        );
        assert_eq!(body["model"], "jev-latest");
        let question = &body["questions"]["route"];
        assert_eq!(question["type"], "choice");
        assert_eq!(
            question["criteria"],
            json!({"General": "General conversation",
                   "Coder": "Programming, debugging, code generation",
                   "Research": null, "Reasoning": null})
        );
        assert!(
            question["instructions"]
                .as_str()
                .unwrap()
                .contains("tools_declared")
        );
        let state = &body["state"];
        assert_eq!(state["request"], "write a Rust async TCP server");
        assert_eq!(state["request_truncated"], false);
        assert_eq!(state["endpoint"], "chat");
        assert_eq!(state["tools_declared"], true);
        assert_eq!(state["tool_choice"], "unspecified");
        assert_eq!(state["reasoning_requested"], false);
        assert_eq!(state["estimated_prompt_tokens"], 12);
        let text = body.to_string();
        for absent in [
            "ToolAgent",
            "RouterClassifier",
            "192.0.2.10",
            KEY,
            "Authorization",
        ] {
            assert!(!text.contains(absent), "{absent}: {text}");
        }

        let private = classifier_with_text_off();
        let body = request_body(
            jev_of(&private),
            &private.candidates,
            &input("SECRET-PROMPT"),
        );
        assert!(body["state"].get("request").is_none());
        assert!(!body.to_string().contains("SECRET-PROMPT"));
        assert_eq!(body["state"]["tools_declared"], true);
    }

    fn classifier_with_text_off() -> RouteClassifier {
        classifier(json!({"model": "jev-latest", "timeout_ms": 5000, "include_user_text": false}))
    }

    #[test]
    fn only_an_exact_candidate_with_a_confidence_in_range_is_a_verdict() {
        let classifier = classifier(minimal());
        let candidates = &classifier.candidates;
        let verdict = parse_response(&answer("Coder", 0.93), candidates).unwrap();
        assert_eq!(verdict.route.as_str(), "Coder");
        assert!((verdict.confidence - 0.93).abs() < 1e-9);
        assert_eq!(verdict.model.as_deref(), Some("jev-1.13.0"));
        assert_eq!(
            parse_response(&answer("Research", 0.0), candidates)
                .unwrap()
                .route
                .as_str(),
            "Research"
        );

        for (choice, why) in [
            ("ToolAgent", "configured, but not a candidate"),
            ("Auto", "Auto is never a candidate"),
            ("node-a", "a node is never a route"),
            ("coder", "the options were exact names"),
            ("", "empty"),
        ] {
            assert_eq!(
                parse_response(&answer(choice, 0.9), candidates),
                Err("unknown_choice"),
                "{why}"
            );
        }
        assert_eq!(
            parse_response(&answer("Coder", 1.5), candidates),
            Err("bad_confidence")
        );
        assert_eq!(
            parse_response(&answer("Coder", -0.1), candidates),
            Err("bad_confidence")
        );
        let no_confidence = json!({"answers": {"route": {"type": "choice", "choice": "Coder"}}});
        assert_eq!(
            parse_response(no_confidence.to_string().as_bytes(), candidates),
            Err("bad_confidence")
        );
        let noul = json!({"answers": {"route": {"type": "noul", "noul": 0.9}}});
        assert_eq!(
            parse_response(noul.to_string().as_bytes(), candidates),
            Err("not_a_choice")
        );
        assert_eq!(
            parse_response(br#"{"answers": {}}"#, candidates),
            Err("no_answer")
        );
        assert_eq!(parse_response(b"<html>", candidates), Err("not_json"));
    }

    #[test]
    fn statuses_map_to_bounded_outcomes() {
        for (status, outcome) in [
            (401, ClassifierOutcome::AuthError),
            (403, ClassifierOutcome::AuthError),
            (429, ClassifierOutcome::RateLimited),
            (529, ClassifierOutcome::RateLimited),
            (422, ClassifierOutcome::ProviderError),
            (500, ClassifierOutcome::ProviderError),
            (503, ClassifierOutcome::ProviderError),
        ] {
            assert_eq!(
                failure_for(StatusCode::from_u16(status).unwrap()),
                outcome,
                "{status}"
            );
        }
    }

    #[test]
    fn the_model_list_is_read_by_name() {
        let body = json!({"models": [
            {"name": "jev-latest", "description": "flagship", "release_date": "2026-09-01"},
            {"name": "jev-preview", "description": "preview", "release_date": "2026-09-01"}
        ]});
        assert_eq!(
            listed_models(body.to_string().as_bytes()),
            Some(vec!["jev-latest".to_owned(), "jev-preview".to_owned()])
        );
        assert_eq!(listed_models(br#"{"data": []}"#), None);
        assert_eq!(listed_models(b"nope"), None);
    }
}
