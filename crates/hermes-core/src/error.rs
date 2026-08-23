//! The actionable-error contract.
//!
//! Spec section 27 lists the failures that must never crash the application:
//! a model that does not fit in RAM, an unsupported architecture, an invalid
//! GGUF file, insufficient disk, an invalid context length, an unsupported CPU
//! instruction, a malformed API request, and a model that fails to load. For
//! each of those the user must be shown *what to do next*.
//!
//! A comment cannot enforce that. [`Actionable`] can: every error type in the
//! workspace implements it, so adding a new failure mode without also stating
//! its remedies does not compile.

use serde::{Deserialize, Serialize};

/// Broad class of a failure. Determines the HTTP status and the `type` field of
/// an OpenAI-shaped error body, so that clients branch on something stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The caller sent something we cannot act on. Retrying unchanged will fail
    /// again.
    InvalidRequest,
    /// The named resource does not exist.
    NotFound,
    /// The caller must authenticate, or the key was wrong.
    Unauthorized,
    /// Too many requests in flight; retrying later may succeed.
    RateLimited,
    /// A precondition on this machine is not met — not enough RAM, not enough
    /// disk, an unsupported CPU. Retrying unchanged will fail again, but the
    /// remedies describe changes that would succeed.
    ResourceExhausted,
    /// The engine is not currently able to serve. Usually transient.
    Unavailable,
    /// We failed, and it is not the caller's fault.
    Internal,
    /// The work was cancelled, by the client disconnecting or by an explicit
    /// stop. Not an error in the usual sense; recorded so metrics can tell it
    /// apart from a failure.
    Cancelled,
}

impl ErrorKind {
    /// HTTP status to return for this class.
    pub const fn http_status(self) -> u16 {
        match self {
            Self::InvalidRequest => 400,
            Self::Unauthorized => 401,
            Self::NotFound => 404,
            Self::RateLimited => 429,
            // 507 Insufficient Storage is the honest status for "your machine
            // cannot do this", and it is distinguishable from a 500 by clients
            // that care. It is not retryable without a change.
            Self::ResourceExhausted => 507,
            Self::Unavailable => 503,
            Self::Internal => 500,
            // The client already went away, or asked us to stop. 499 is
            // nginx's non-standard "client closed request"; nothing reads it
            // over the wire in practice, but it keeps logs honest.
            Self::Cancelled => 499,
        }
    }

    /// The `error.type` string used by the OpenAI error schema.
    pub const fn openai_type(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request_error",
            Self::Unauthorized => "authentication_error",
            Self::NotFound => "not_found_error",
            Self::RateLimited => "rate_limit_error",
            Self::ResourceExhausted | Self::Unavailable => "server_error",
            Self::Internal | Self::Cancelled => "server_error",
        }
    }

    /// Whether retrying the identical request could plausibly succeed.
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::Unavailable)
    }
}

/// A concrete, machine-actionable suggestion.
///
/// The variants carry the numbers needed to *perform* the fix, not just to
/// describe it, so the UI can render a button that applies it. Section 27's
/// example error ("Reduce context size / Close other applications / Select a
/// smaller quantized model") is only useful if the app can act on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RemedyAction {
    /// Lower the context window to a value that fits.
    ReduceContext { to_tokens: u32 },
    /// Quantize the KV cache, trading a little quality for a lot of memory.
    /// `cache_type` is a ggml type name such as `q8_0`.
    QuantizeKvCache {
        cache_type: String,
        saves_bytes: u64,
    },
    /// Use a different quantization of the same model.
    UseQuantization {
        model_id: ModelIdRef,
        quantization: String,
        weight_bytes: u64,
    },
    /// Pick a different, smaller model.
    UseSmallerModel { max_weight_bytes: u64 },
    /// Free system memory. `needed_bytes` is the shortfall.
    FreeMemory { needed_bytes: u64 },
    /// Free disk space. `needed_bytes` is the shortfall.
    FreeDisk { needed_bytes: u64 },
    /// The model's architecture is not supported; choose one that is.
    SelectSupportedArchitecture { supported: Vec<String> },
    /// Wait and retry.
    RetryAfter { seconds: u32 },
    /// Nothing automatic; point the user at a settings page.
    OpenSettings { section: SettingsSection },
}

/// A page of the settings UI, for remedies that can only point the user
/// somewhere rather than apply a fix themselves.
///
/// An enum rather than a string because the UI has to route on it: a typo in a
/// free-form section name produces a remedy button that silently goes nowhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsSection {
    Storage,
    Logging,
    Api,
    Inference,
    Models,
}

/// A model id as carried inside a remedy.
///
/// Deliberately a plain `String` rather than [`crate::ModelId`]: remedies are
/// serialized into API responses and must stay parseable even if the id no
/// longer resolves by the time the client acts on it.
pub type ModelIdRef = String;

/// A remedy: the machine-actionable part plus the sentence shown to the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remedy {
    /// Human-readable, imperative, and specific. "Reduce the context to 8192
    /// tokens", never "try a smaller context".
    pub label: String,
    #[serde(flatten)]
    pub action: RemedyAction,
}

impl Remedy {
    pub fn new(label: impl Into<String>, action: RemedyAction) -> Self {
        Self {
            label: label.into(),
            action,
        }
    }
}

/// Implemented by every error type in the workspace.
///
/// The point of the trait is [`Actionable::remedies`]. An error that cannot
/// suggest anything returns an empty list, but it has to say so explicitly,
/// which is the moment to ask whether a remedy really is impossible.
pub trait Actionable: std::error::Error {
    /// Stable, greppable identifier. Never reworded once shipped — clients and
    /// dashboards match on it. Use `snake_case`.
    fn code(&self) -> &'static str;

    /// The broad class, which fixes the HTTP status and OpenAI error type.
    fn kind(&self) -> ErrorKind;

    /// What the user can do about it, best first.
    fn remedies(&self) -> Vec<Remedy> {
        Vec::new()
    }

    /// HTTP status. Override only when a specific error needs to differ from
    /// its class default.
    fn http_status(&self) -> u16 {
        self.kind().http_status()
    }
}

/// A serializable snapshot of an [`Actionable`] error.
///
/// This is what crosses the process boundary — into an API response, a log
/// record, or the UI. It deliberately owns its data so it can outlive the error
/// it came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorReport {
    pub code: String,
    pub kind: ErrorKind,
    /// The `Display` text of the error. Must never contain user prompt text —
    /// wrap anything user-authored in [`crate::Private`] first.
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remedies: Vec<Remedy>,
}

impl ErrorReport {
    /// Capture an error implementing [`Actionable`].
    pub fn capture<E: Actionable + ?Sized>(err: &E) -> Self {
        Self {
            code: err.code().to_owned(),
            kind: err.kind(),
            message: err.to_string(),
            remedies: err.remedies(),
        }
    }

    pub fn http_status(&self) -> u16 {
        self.kind.http_status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("insufficient RAM: need {needed} bytes, {available} available")]
    struct NotEnoughRam {
        needed: u64,
        available: u64,
    }

    impl Actionable for NotEnoughRam {
        fn code(&self) -> &'static str {
            "insufficient_ram"
        }
        fn kind(&self) -> ErrorKind {
            ErrorKind::ResourceExhausted
        }
        fn remedies(&self) -> Vec<Remedy> {
            vec![Remedy::new(
                "Reduce the context to 4096 tokens",
                RemedyAction::ReduceContext { to_tokens: 4096 },
            )]
        }
    }

    #[test]
    fn report_captures_code_message_and_remedies() {
        let err = NotEnoughRam {
            needed: 8_400_000_000,
            available: 5_100_000_000,
        };
        let report = ErrorReport::capture(&err);

        assert_eq!(report.code, "insufficient_ram");
        assert_eq!(report.kind, ErrorKind::ResourceExhausted);
        assert_eq!(report.http_status(), 507);
        assert!(report.message.contains("8400000000"));
        assert_eq!(report.remedies.len(), 1);
    }

    #[test]
    fn remedy_serializes_action_inline() {
        let remedy = Remedy::new(
            "Reduce the context",
            RemedyAction::ReduceContext { to_tokens: 8192 },
        );
        let json = serde_json::to_value(&remedy).expect("serialize");

        // The UI switches on `action`, so it must sit at the top level next to
        // `label` rather than nested under an extra key.
        assert_eq!(json["action"], "reduce_context");
        assert_eq!(json["to_tokens"], 8192);
        assert_eq!(json["label"], "Reduce the context");
    }

    #[test]
    fn resource_exhausted_is_not_advertised_as_retryable() {
        // Retrying a load that did not fit, unchanged, will not fit next time
        // either. Saying otherwise would make clients spin.
        assert!(!ErrorKind::ResourceExhausted.is_retryable());
        assert!(ErrorKind::Unavailable.is_retryable());
        assert!(ErrorKind::RateLimited.is_retryable());
    }

    #[test]
    fn invalid_request_maps_to_the_openai_schema() {
        assert_eq!(ErrorKind::InvalidRequest.http_status(), 400);
        assert_eq!(
            ErrorKind::InvalidRequest.openai_type(),
            "invalid_request_error"
        );
    }
}
