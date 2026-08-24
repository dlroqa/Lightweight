//! OpenAI-shaped error bodies.
//!
//! Every failure the gateway can return travels in this envelope, because an
//! OpenAI client only understands one shape and a body it cannot parse becomes
//! "unknown error" on the user's screen.
//!
//! The remedies that spec section 27 requires do not fit that schema, so they
//! ride under `error.hermes` — our namespace, ignored by other clients, read by
//! our own UI. The alternative was to drop them at the boundary, which would
//! make the whole actionable-error contract stop at the last layer before the
//! user.

use hermes_core::{Actionable, ErrorReport, Remedy};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The `error` object.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ErrorBody {
    /// Human-readable, and machine-parsed more often than one would like: a
    /// client extracts the context limit from this text with a regex, so the
    /// wording of a context-overflow message is part of the contract.
    pub message: String,
    /// The OpenAI error class: `invalid_request_error`, `server_error`, …
    pub r#type: String,
    /// Which request field is at fault, when one is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    /// Our stable code — `context_length_exceeded`, `engine_crashed`, … —
    /// which is what a client should branch on rather than the message.
    pub code: String,
    /// Remedies, under our own namespace so no other client mistakes them for
    /// a standard field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hermes: Option<HermesErrorInfo>,
}

/// The remedy payload.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HermesErrorInfo {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remedies: Vec<Remedy>,
}

/// The whole body: `{"error": {...}}`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

impl ErrorEnvelope {
    /// Build from any error in the workspace.
    ///
    /// Taking [`Actionable`] rather than a string is what makes it impossible
    /// to return an error without a code and a class — the trait requires both
    /// at compile time.
    pub fn from_error<E: Actionable + ?Sized>(err: &E) -> Self {
        Self::from_report(&ErrorReport::capture(err))
    }

    pub fn from_report(report: &ErrorReport) -> Self {
        Self {
            error: ErrorBody {
                message: report.message.clone(),
                r#type: report.kind.openai_type().to_owned(),
                param: None,
                code: report.code.clone(),
                hermes: (!report.remedies.is_empty()).then(|| HermesErrorInfo {
                    remedies: report.remedies.clone(),
                }),
            },
        }
    }

    /// Name the request field at fault.
    pub fn with_param(mut self, param: impl Into<String>) -> Self {
        self.error.param = Some(param.into());
        self
    }

    /// A request-level error the gateway raises on its own.
    pub fn invalid_request(message: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                message: message.into(),
                r#type: "invalid_request_error".to_owned(),
                param: None,
                code: code.into(),
                hermes: None,
            },
        }
    }

    /// The body as a JSON value, for embedding in a terminal stream chunk.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(&self.error).unwrap_or_else(|_| {
            serde_json::json!({
                "message": self.error.message,
                "type": self.error.r#type,
                "code": self.error.code,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_inference::BackendError;

    #[test]
    fn a_context_overflow_body_is_parsable_by_the_clients_regex() {
        // The exact case section 27 and the client's `parse_context_limit_from_error`
        // both care about. The Python side of this is asserted for real in the
        // contract suite; this keeps the wording honest without a Python
        // interpreter.
        let envelope = ErrorEnvelope::from_error(&BackendError::ContextOverflow {
            prompt_tokens: 41_022,
            n_ctx: 32_768,
        })
        .with_param("messages");
        let json = serde_json::to_value(&envelope).expect("serialize");

        assert_eq!(json["error"]["code"], "context_length_exceeded");
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert_eq!(json["error"]["param"], "messages");
        let message = json["error"]["message"].as_str().expect("a message");
        assert!(
            message.contains("maximum context length is 32768"),
            "{message}"
        );
    }

    #[test]
    fn remedies_survive_the_trip_to_the_client() {
        // Section 27's promise is that a failure says what to do next. Dropping
        // remedies at the last layer would make that promise stop just short of
        // the user.
        let envelope = ErrorEnvelope::from_error(&BackendError::InvalidContextLength {
            requested: 99_999,
            max: 8192,
        });
        let json = serde_json::to_value(&envelope).expect("serialize");
        let remedies = json["error"]["hermes"]["remedies"]
            .as_array()
            .expect("remedies");
        assert!(!remedies.is_empty());
        assert_eq!(remedies[0]["to_tokens"], 8192);
        // The panel renders this field. It read `message` for a while, which
        // does not exist here, and drew a bullet list of `undefined` for every
        // remedy the gateway has ever sent. Pin the name server-side so the
        // next rename fails here rather than on screen.
        assert!(
            remedies[0]["label"].is_string(),
            "a remedy must carry the sentence under `label`: {remedies:?}"
        );
    }

    #[test]
    fn an_error_with_no_remedies_omits_the_namespace_entirely() {
        let envelope = ErrorEnvelope::invalid_request("no messages", "invalid_request");
        let json = serde_json::to_value(&envelope).expect("serialize");
        assert!(json["error"].get("hermes").is_none());
        assert!(json["error"].get("param").is_none());
    }

    #[test]
    fn every_backend_failure_renders_a_well_formed_body() {
        // The error-path gate: a new variant that produces a malformed body
        // must fail here rather than in front of a user.
        let cases: Vec<BackendError> = vec![
            BackendError::UnsupportedPlatform {
                os: "plan9",
                arch: "x86_64",
            },
            BackendError::RuntimeDownloadFailed {
                reason: "offline".into(),
            },
            BackendError::RuntimeCorrupt {
                expected: "a".into(),
                actual: "b".into(),
            },
            BackendError::RuntimeMissing { path: "/x".into() },
            BackendError::LowDisk {
                needed: 10,
                available: 1,
            },
            BackendError::ModelNotFound {
                path: "/x.gguf".into(),
            },
            BackendError::UnsupportedArchitecture {
                found: "xyz".into(),
                supported: vec!["llama".into()],
            },
            BackendError::InvalidContextLength {
                requested: 1,
                max: 2,
            },
            BackendError::ContextOverflow {
                prompt_tokens: 5,
                n_ctx: 4,
            },
            BackendError::UnsupportedKvCacheType {
                requested: "q6_K".into(),
                supported: vec!["f16".into()],
            },
            BackendError::InsufficientMemory {
                model: "m".into(),
                required: "8 GiB".into(),
                available: "2 GiB".into(),
            },
            BackendError::StartTimeout { seconds: 300 },
            BackendError::EngineCrashed {
                detail: "signal 11".into(),
                exit_code: None,
                signal: Some(11),
                tail: vec![],
            },
            BackendError::EngineOom { tail: vec![] },
            BackendError::UnsupportedCpuInstruction {
                detected: "SSE4.2".into(),
            },
            BackendError::EngineUnavailable,
            BackendError::GenerationFailed {
                detail: "slot lost".into(),
            },
            BackendError::NoModelLoaded,
            BackendError::Cancelled,
            BackendError::io("reading", std::io::Error::other("boom")),
        ];

        for case in &cases {
            let envelope = ErrorEnvelope::from_error(case);
            let json = serde_json::to_value(&envelope).expect("serialize");
            let body = &json["error"];
            assert!(
                body["message"].as_str().is_some_and(|m| !m.is_empty()),
                "{} has an empty message",
                case.code()
            );
            assert!(
                body["code"].as_str().is_some_and(|c| !c.is_empty()),
                "{} has an empty code",
                case.code()
            );
            assert!(
                body["type"].as_str().is_some_and(|t| t.ends_with("error")),
                "{} has a non-OpenAI type: {}",
                case.code(),
                body["type"]
            );
            // Round-trips, so a client parsing our body gets what we sent.
            let parsed: ErrorEnvelope =
                serde_json::from_value(json).expect("a client must be able to parse this");
            assert_eq!(parsed.error.code, case.code());
        }
    }
}
