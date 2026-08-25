//! What can go wrong while measuring, and what to do about it.

use hermes_core::{Actionable, ErrorKind, Remedy, RemedyAction, SettingsSection};

#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("the engine could not be driven: {detail}")]
    Engine { detail: String },

    #[error("a prompt of about {requested} tokens could not be built; the closest was {achieved}")]
    PromptSize { requested: u32, achieved: u32 },

    #[error("the model's context is {n_ctx} tokens, which cannot hold a {requested}-token prompt")]
    PromptTooLarge { requested: u32, n_ctx: u32 },

    #[error("the benchmark produced no usable samples")]
    NothingMeasured,

    #[error("could not write the benchmark to {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: hermes_store::StoreError,
    },

    #[error("could not record the benchmark: {detail}")]
    Encode { detail: String },

    #[error("could not read the saved benchmarks: {detail}")]
    Read { detail: String },

    /// An id that could not have come from the store.
    ///
    /// Its own variant rather than folded into `Read`, because the taxonomy
    /// distinguishes a caller's mistake from a server's: flattening this into
    /// an internal error answers a typo with a 500 and sends somebody looking
    /// for a fault that is not there.
    #[error("`{id}` is not an id this store could have produced")]
    MalformedId { id: String },

    #[error("no benchmark `{id}` has been saved")]
    NoSuchRun { id: String },
}

impl Actionable for BenchError {
    fn code(&self) -> &'static str {
        match self {
            Self::Engine { .. } => "benchmark_engine_failed",
            Self::PromptSize { .. } => "benchmark_prompt_size",
            Self::PromptTooLarge { .. } => "benchmark_prompt_too_large",
            Self::NothingMeasured => "benchmark_nothing_measured",
            Self::Write { .. } => "benchmark_not_saved",
            Self::Encode { .. } => "benchmark_not_encoded",
            Self::Read { .. } => "benchmark_not_read",
            Self::MalformedId { .. } => "malformed_benchmark_id",
            Self::NoSuchRun { .. } => "unknown_benchmark",
        }
    }

    fn kind(&self) -> ErrorKind {
        match self {
            Self::PromptSize { .. } | Self::PromptTooLarge { .. } | Self::MalformedId { .. } => {
                ErrorKind::InvalidRequest
            }
            Self::NoSuchRun { .. } => ErrorKind::NotFound,
            _ => ErrorKind::Internal,
        }
    }

    fn remedies(&self) -> Vec<Remedy> {
        match self {
            Self::PromptTooLarge { n_ctx, .. } => vec![Remedy::new(
                format!(
                    "Ask for a prompt under {n_ctx} tokens, or load the model with a larger \
                     context"
                ),
                RemedyAction::OpenSettings {
                    section: SettingsSection::Models,
                },
            )],
            Self::PromptSize { .. } => vec![Remedy::new(
                "Ask for a different prompt size; a tokenizer cannot hit every target exactly",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Inference,
                },
            )],
            Self::Engine { .. } => vec![Remedy::new(
                "Check that a model is loaded and the engine is healthy, then run it again",
                RemedyAction::RetryAfter { seconds: 5 },
            )],
            Self::NothingMeasured => vec![Remedy::new(
                "Load a model and run it again; a benchmark with no samples is not a result",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Models,
                },
            )],
            Self::MalformedId { .. } | Self::NoSuchRun { .. } => vec![Remedy::new(
                "List the saved benchmarks and use an id from there",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Storage,
                },
            )],
            Self::Write { .. } | Self::Encode { .. } | Self::Read { .. } => vec![Remedy::new(
                "Check that the data directory is writable",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Storage,
                },
            )],
        }
    }
}
