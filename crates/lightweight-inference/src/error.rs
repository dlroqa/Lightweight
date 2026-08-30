//! Backend failures.
//!
//! The variants here are not an arbitrary list. Spec section 27 enumerates the
//! things that must never crash the application, and each of them appears
//! below with the information a user would need to act on it. The
//! [`lightweight_core::Actionable`] implementation is what turns "this failed" into
//! "here is what to do", and the trait makes that obligation a compile-time
//! one.
//!
//! Several variants exist specifically because the engine is a **child
//! process**. A SIGILL, a SIGSEGV or an OOM kill inside a linked library would
//! take the whole application with it and there would be nothing left to report
//! them. Across a process boundary they are ordinary, observable exit
//! conditions, which is most of the reason for the boundary.

use std::path::PathBuf;

use lightweight_core::{Actionable, ErrorKind, Remedy, RemedyAction, SettingsSection};

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    // ---- runtime acquisition ----
    #[error("no inference engine is available for {os}/{arch}")]
    UnsupportedPlatform {
        os: &'static str,
        arch: &'static str,
    },

    #[error("could not download the inference engine: {reason}")]
    RuntimeDownloadFailed { reason: String },

    #[error("the downloaded engine failed verification: expected sha256 {expected}, got {actual}")]
    RuntimeCorrupt { expected: String, actual: String },

    #[error("the inference engine is not installed at {path}")]
    RuntimeMissing { path: PathBuf },

    #[error("not enough disk space: {needed} bytes required, {available} available")]
    LowDisk { needed: u64, available: u64 },

    // ---- model admission ----
    #[error("no model file at {path}")]
    ModelNotFound { path: PathBuf },

    #[error("the CPU backend does not support the {found:?} architecture")]
    UnsupportedArchitecture {
        found: String,
        supported: Vec<String>,
    },

    #[error(
        "a context of {requested} tokens exceeds this model's maximum context length of {max} tokens"
    )]
    InvalidContextLength { requested: u32, max: u32 },

    #[error(
        "this model's maximum context length is {n_ctx} tokens, however your messages resulted in \
         {prompt_tokens} tokens. Please reduce the length of the messages or lower the configured \
         context length."
    )]
    ContextOverflow { prompt_tokens: u32, n_ctx: u32 },

    #[error("the engine does not accept {requested:?} as a KV cache type")]
    UnsupportedKvCacheType {
        requested: String,
        supported: Vec<String>,
    },

    #[error("{model} needs {required} but only {available} is free")]
    InsufficientMemory {
        model: String,
        required: String,
        available: String,
    },

    /// A locking load mode was asked for and the kernel would not allow it.
    ///
    /// Its own variant rather than folded into `InsufficientMemory`, because
    /// the remedy is different in kind: no amount of freeing memory changes
    /// `RLIMIT_MEMLOCK`, and no amount of raising `RLIMIT_MEMLOCK` helps a
    /// model that does not fit.
    #[error(
        "locking this model's {required} of weights needs a locked-memory \
         allowance of at least that much; this user's is {limit}"
    )]
    LockedMemoryTooSmall {
        required: lightweight_core::units::Bytes,
        limit: lightweight_core::units::Bytes,
    },

    // ---- process lifecycle ----
    #[error("the engine did not become ready within {seconds} seconds")]
    StartTimeout { seconds: u64 },

    #[error("the engine stopped unexpectedly ({detail})")]
    EngineCrashed {
        detail: String,
        exit_code: Option<i32>,
        signal: Option<i32>,
        /// The last few lines the engine wrote to stderr, which is usually
        /// where it says why it stopped.
        tail: Vec<String>,
    },

    #[error("the operating system killed the engine for using too much memory")]
    EngineOom { tail: Vec<String> },

    #[error(
        "the engine used a CPU instruction this machine does not have; \
         detected instruction sets: {detected}"
    )]
    UnsupportedCpuInstruction { detected: String },

    #[error("the engine is not running")]
    EngineUnavailable,

    /// The engine accepted the request and then refused or failed it.
    ///
    /// Distinct from a crash: the process is alive and will serve the next
    /// request, so the gateway reports the failure without tearing anything
    /// down.
    #[error("the engine could not complete the request: {detail}")]
    GenerationFailed { detail: String },

    #[error("no model is loaded")]
    NoModelLoaded,

    #[error("the operation was cancelled")]
    Cancelled,

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl Actionable for BackendError {
    fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedPlatform { .. } => "unsupported_platform",
            Self::RuntimeDownloadFailed { .. } => "runtime_download_failed",
            Self::RuntimeCorrupt { .. } => "runtime_corrupt",
            Self::RuntimeMissing { .. } => "runtime_missing",
            Self::LowDisk { .. } => "low_disk",
            Self::ModelNotFound { .. } => "model_not_found",
            Self::UnsupportedArchitecture { .. } => "unsupported_architecture",
            Self::InvalidContextLength { .. } => "invalid_context_length",
            Self::UnsupportedKvCacheType { .. } => "unsupported_kv_cache_type",
            // The code OpenAI clients already recognize for this condition.
            Self::ContextOverflow { .. } => "context_length_exceeded",
            Self::InsufficientMemory { .. } => "insufficient_memory",
            Self::LockedMemoryTooSmall { .. } => "locked_memory_too_small",
            Self::StartTimeout { .. } => "engine_start_timeout",
            Self::EngineCrashed { .. } => "engine_crashed",
            Self::EngineOom { .. } => "engine_out_of_memory",
            Self::UnsupportedCpuInstruction { .. } => "unsupported_cpu_instruction",
            Self::EngineUnavailable => "engine_unavailable",
            Self::GenerationFailed { .. } => "generation_failed",
            Self::NoModelLoaded => "no_model_loaded",
            Self::Cancelled => "cancelled",
            Self::Io { .. } => "io_error",
        }
    }

    fn kind(&self) -> ErrorKind {
        match self {
            Self::ModelNotFound { .. } => ErrorKind::NotFound,
            Self::UnsupportedArchitecture { .. }
            | Self::InvalidContextLength { .. }
            | Self::ContextOverflow { .. }
            | Self::UnsupportedKvCacheType { .. }
            // A configuration the caller chose, which the caller can unchoose.
            // Not `ResourceExhausted`: no amount of freeing memory raises
            // `RLIMIT_MEMLOCK`, so a retry-when-there-is-room reading would be
            // wrong in both directions.
            | Self::LockedMemoryTooSmall { .. } => ErrorKind::InvalidRequest,
            Self::LowDisk { .. } | Self::InsufficientMemory { .. } | Self::EngineOom { .. } => {
                ErrorKind::ResourceExhausted
            }
            Self::EngineUnavailable
            | Self::NoModelLoaded
            | Self::StartTimeout { .. }
            | Self::EngineCrashed { .. } => ErrorKind::Unavailable,
            Self::Cancelled => ErrorKind::Cancelled,
            _ => ErrorKind::Internal,
        }
    }

    fn remedies(&self) -> Vec<Remedy> {
        match self {
            Self::LockedMemoryTooSmall { required, limit } => vec![
                Remedy::new(
                    format!(
                        "Load without a locking mode; the weights are then \
                         memory-mapped, which is the default and needs no \
                         allowance (this one is {limit})"
                    ),
                    RemedyAction::OpenSettings {
                        section: SettingsSection::Models,
                    },
                ),
                Remedy::new(
                    format!(
                        "Or raise this user's locked-memory allowance to at \
                         least {required} — `ulimit -l` in the shell that \
                         starts the gateway, or a limits.conf entry"
                    ),
                    RemedyAction::OpenSettings {
                        section: SettingsSection::Storage,
                    },
                ),
            ],
            Self::UnsupportedArchitecture { supported, .. } => vec![Remedy::new(
                "Choose a model with a supported architecture",
                RemedyAction::SelectSupportedArchitecture {
                    supported: supported.clone(),
                },
            )],
            // The prompt, not the configuration, is what is too long - so the
            // remedy is to send less, and only secondarily to reconfigure.
            Self::ContextOverflow { n_ctx, .. } => vec![
                Remedy::new(
                    "Shorten the conversation, or start a new one",
                    RemedyAction::ReduceContext { to_tokens: *n_ctx },
                ),
                Remedy::new(
                    "Load the model with a larger context, if this machine has the memory",
                    RemedyAction::OpenSettings {
                        section: SettingsSection::Inference,
                    },
                ),
            ],
            Self::InvalidContextLength { max, .. } => vec![Remedy::new(
                format!("Reduce the context to {max} tokens or fewer"),
                RemedyAction::ReduceContext { to_tokens: *max },
            )],
            Self::UnsupportedKvCacheType { supported, .. } => vec![Remedy::new(
                format!("Use one of: {}", supported.join(", ")),
                RemedyAction::OpenSettings {
                    section: SettingsSection::Inference,
                },
            )],
            Self::LowDisk { needed, available } => vec![Remedy::new(
                format!(
                    "Free about {} of disk space",
                    needed.saturating_sub(*available)
                ),
                RemedyAction::FreeDisk {
                    needed_bytes: needed.saturating_sub(*available),
                },
            )],
            // The OOM killer is the one case where our own estimate was wrong:
            // we admitted a load that did not fit. Say so plainly rather than
            // blaming the machine.
            Self::EngineOom { .. } => vec![
                Remedy::new(
                    "Reduce the context length, which is usually the largest term",
                    RemedyAction::OpenSettings {
                        section: SettingsSection::Inference,
                    },
                ),
                Remedy::new(
                    "Close other applications and try again",
                    RemedyAction::FreeMemory { needed_bytes: 0 },
                ),
            ],
            // Section 10: never fail merely because an advanced instruction is
            // missing. Reaching here means the engine's own runtime dispatch
            // picked a variant this CPU cannot run, which is a bug in the
            // pinned build rather than something the user did.
            Self::UnsupportedCpuInstruction { .. } => vec![Remedy::new(
                "Report this: the engine selected a CPU variant this machine cannot run",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Inference,
                },
            )],
            Self::RuntimeCorrupt { .. } | Self::RuntimeDownloadFailed { .. } => {
                vec![Remedy::new(
                    "Re-download the inference engine",
                    RemedyAction::OpenSettings {
                        section: SettingsSection::Inference,
                    },
                )]
            }
            Self::StartTimeout { .. } => vec![Remedy::new(
                "Loading a large model on a slow disk can exceed the timeout; raise it in settings",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Inference,
                },
            )],
            _ => Vec::new(),
        }
    }
}

impl BackendError {
    /// Wrap an I/O error with the operation that produced it.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// Whether restarting the engine could plausibly help.
    ///
    /// Used by the supervisor to decide between retrying and giving up. A
    /// crash might be transient; a model that does not fit in memory will not
    /// fit any better on the second attempt, and retrying it would just burn
    /// the machine.
    pub const fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::EngineCrashed { .. } | Self::StartTimeout { .. } | Self::EngineUnavailable
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_section_27_failure_has_a_code() {
        // Section 27 lists what must never crash the application. Each entry
        // needs a distinct, stable code so the UI and logs can route on it.
        let cases: Vec<BackendError> = vec![
            BackendError::InsufficientMemory {
                model: "m".into(),
                required: "8 GiB".into(),
                available: "3 GiB".into(),
            },
            BackendError::UnsupportedArchitecture {
                found: "xyz".into(),
                supported: vec!["llama".into()],
            },
            BackendError::ModelNotFound {
                path: PathBuf::from("/x"),
            },
            BackendError::LowDisk {
                needed: 100,
                available: 1,
            },
            BackendError::InvalidContextLength {
                requested: 1 << 20,
                max: 4096,
            },
            BackendError::UnsupportedCpuInstruction {
                detected: "SSE4.2".into(),
            },
            BackendError::EngineCrashed {
                detail: "signal 11".into(),
                exit_code: None,
                signal: Some(11),
                tail: vec![],
            },
        ];

        let mut codes = std::collections::BTreeSet::new();
        for case in &cases {
            assert!(!case.code().is_empty());
            assert!(codes.insert(case.code()), "duplicate code {}", case.code());
            assert!(!case.to_string().is_empty());
        }
    }

    #[test]
    fn user_correctable_failures_carry_remedies() {
        // An error the user could fix but that says nothing is a dead end.
        for case in [
            BackendError::UnsupportedArchitecture {
                found: "xyz".into(),
                supported: vec!["llama".into()],
            },
            BackendError::InvalidContextLength {
                requested: 99_999,
                max: 4096,
            },
            BackendError::LowDisk {
                needed: 100,
                available: 1,
            },
            BackendError::EngineOom { tail: vec![] },
        ] {
            assert!(!case.remedies().is_empty(), "{} has no remedy", case.code());
        }
    }

    #[test]
    fn an_overlong_prompt_names_the_window_and_the_prompt() {
        // The wording is a contract, not prose: Hermes lowercases the message
        // and pulls the limit out of it with
        // `(?:max(?:imum)?|limit)\s*(?:context\s*)?(?:length|size|window)?\s*(?:is|of|:)?\s*(\d{4,})`
        // (agent/model_metadata.py:1587). The context must appear *before* the
        // prompt size, or the client caches the wrong number and re-plans
        // every future turn against it.
        let err = BackendError::ContextOverflow {
            prompt_tokens: 41_022,
            n_ctx: 32_768,
        };
        let message = err.to_string();
        assert!(
            message.contains("maximum context length is 32768"),
            "wording changed, which breaks the client's limit parser: {message}"
        );
        assert!(message.contains("41022"), "{message}");
        assert!(
            message.find("32768") < message.find("41022"),
            "the window must be the first four-digit number in the message: {message}"
        );
        assert_eq!(err.code(), "context_length_exceeded");
        assert_eq!(err.http_status(), 400);
        assert!(!err.remedies().is_empty());
    }

    #[test]
    fn a_failed_generation_leaves_the_engine_alive() {
        // Distinct from a crash on purpose: the process is still serving, so
        // nothing should be torn down or restarted because of it.
        let err = BackendError::GenerationFailed {
            detail: "slot unavailable".into(),
        };
        assert_eq!(err.code(), "generation_failed");
        assert!(!err.is_transient());
    }

    #[test]
    fn a_context_overflow_message_carries_a_machine_parsable_limit() {
        // Hermes parses the limit out of our error text with a regex requiring
        // a run of at least four digits near the words "maximum context
        // length". Losing that wording would make it re-plan blindly.
        let err = BackendError::InvalidContextLength {
            requested: 41_022,
            max: 32_768,
        };
        let message = err.to_string();
        assert!(
            message.contains("maximum context length of 32768"),
            "wording changed, which breaks the client's limit parser: {message}"
        );
    }

    #[test]
    fn only_genuinely_transient_failures_invite_a_retry() {
        // Retrying a model that does not fit would just burn the machine.
        assert!(
            BackendError::EngineCrashed {
                detail: String::new(),
                exit_code: Some(1),
                signal: None,
                tail: vec![],
            }
            .is_transient()
        );
        assert!(!BackendError::EngineOom { tail: vec![] }.is_transient());
        assert!(
            !BackendError::UnsupportedArchitecture {
                found: "x".into(),
                supported: vec![],
            }
            .is_transient()
        );
        assert!(!BackendError::Cancelled.is_transient());
    }

    #[test]
    fn resource_exhaustion_is_not_advertised_as_retryable() {
        assert!(
            !BackendError::EngineOom { tail: vec![] }
                .kind()
                .is_retryable()
        );
        assert_eq!(BackendError::EngineOom { tail: vec![] }.http_status(), 507);
    }
}
