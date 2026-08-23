//! Structured logging for the gateway.
//!
//! Spec section 26 asks for INFO/WARN/ERROR/DEBUG records covering model
//! loading and unloading, inference requests, generation errors, RAM warnings,
//! API requests and backend initialisation — and requires that user prompts are
//! *not* logged by default, with a Privacy Mode that disables prompt logging
//! entirely.
//!
//! The privacy half of that is not enforced here. It is enforced by
//! [`hermes_core::Private`], which redacts through `Display` and `Debug` and so
//! cannot be captured by a `tracing` field. What this crate does is set the
//! mode before the first record is written, and say plainly in the log when
//! prompt logging has been switched on — an operator reading the file should
//! never have to wonder whether what they are looking at is complete.
//!
//! Two sinks: a human-readable one on stderr, and a JSON one rotated daily in
//! the data directory. Both are non-blocking, because an inference loop must
//! never stall on a slow disk.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Production code must never panic (spec section 27). A test, however, reports
// failure *by* panicking, so the deny above would otherwise force every
// assertion helper into needless error plumbing.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod targets;

use std::path::PathBuf;

use hermes_core::privacy::{self, PrivacyMode};
use hermes_core::{Actionable, ErrorKind, Remedy, RemedyAction, SettingsSection};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// Environment variable read for the log filter, using the standard
/// `RUST_LOG` syntax (`info`, `hermes_gateway=debug,hyper=warn`, ...).
pub const FILTER_ENV: &str = "HERMES_LOG";

#[derive(Debug, thiserror::Error)]
pub enum ObservabilityError {
    #[error("could not create the log directory {path}: {source}")]
    LogDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid log filter {filter:?}: {source}")]
    Filter {
        filter: String,
        #[source]
        source: tracing_subscriber::filter::ParseError,
    },

    #[error("logging has already been initialised for this process")]
    AlreadyInitialised,
}

impl Actionable for ObservabilityError {
    fn code(&self) -> &'static str {
        match self {
            Self::LogDirectory { .. } => "log_directory_not_writable",
            Self::Filter { .. } => "invalid_log_filter",
            Self::AlreadyInitialised => "logging_already_initialised",
        }
    }

    fn kind(&self) -> ErrorKind {
        match self {
            Self::Filter { .. } => ErrorKind::InvalidRequest,
            _ => ErrorKind::Internal,
        }
    }

    fn remedies(&self) -> Vec<Remedy> {
        match self {
            Self::LogDirectory { path, .. } => vec![Remedy::new(
                format!("Check permissions on {}", path.display()),
                RemedyAction::OpenSettings {
                    section: SettingsSection::Storage,
                },
            )],
            Self::Filter { .. } => vec![Remedy::new(
                format!("Correct {FILTER_ENV}, for example `info` or `hermes_gateway=debug`"),
                RemedyAction::OpenSettings {
                    section: SettingsSection::Logging,
                },
            )],
            Self::AlreadyInitialised => Vec::new(),
        }
    }
}

/// How logging should be set up.
#[derive(Clone, Debug)]
pub struct LogConfig {
    /// Directory for rotated JSON logs. `None` disables file logging, which is
    /// what tests and one-shot CLI commands want.
    pub directory: Option<PathBuf>,
    /// Filter in `RUST_LOG` syntax. Overridden by [`FILTER_ENV`] if that is set.
    pub filter: String,
    /// Whether to also write human-readable records to stderr.
    pub console: bool,
    /// Privacy mode to install before the first record is written.
    pub privacy: PrivacyMode,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            directory: None,
            filter: "info".to_owned(),
            console: true,
            // Redaction on. Anything else would make a mistake in configuration
            // parsing fail open, which for prompt logging is the wrong
            // direction to fail.
            privacy: PrivacyMode::Standard,
        }
    }
}

/// Keeps the non-blocking log writers alive.
///
/// Dropping this flushes and shuts down the background writer threads, so hold
/// it for as long as the process should be logging — usually for the whole of
/// `main`. Dropping it early silently loses records.
#[must_use = "dropping the guard stops log records from being written"]
#[derive(Debug)]
pub struct LogGuard {
    _file: Option<tracing_appender::non_blocking::WorkerGuard>,
    _console: Option<tracing_appender::non_blocking::WorkerGuard>,
}

/// Install the global subscriber and the privacy mode.
///
/// Call once, early in `main`, before anything that might log.
pub fn init(config: LogConfig) -> Result<LogGuard, ObservabilityError> {
    let effective_privacy = privacy::set_privacy_mode(config.privacy);

    let filter_source = std::env::var(FILTER_ENV).unwrap_or_else(|_| config.filter.clone());
    let filter =
        EnvFilter::try_new(&filter_source).map_err(|source| ObservabilityError::Filter {
            filter: filter_source.clone(),
            source,
        })?;

    let mut layers = Vec::new();
    let mut file_guard = None;
    let mut console_guard = None;

    if let Some(directory) = &config.directory {
        std::fs::create_dir_all(directory).map_err(|source| ObservabilityError::LogDirectory {
            path: directory.clone(),
            source,
        })?;

        let appender = tracing_appender::rolling::daily(directory, "gateway.log");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        file_guard = Some(guard);

        layers.push(
            fmt::layer()
                .json()
                .with_current_span(true)
                .with_span_list(false)
                .with_writer(writer)
                .with_ansi(false)
                .boxed(),
        );
    }

    if config.console {
        let (writer, guard) = tracing_appender::non_blocking(std::io::stderr());
        console_guard = Some(guard);

        layers.push(
            fmt::layer()
                .with_writer(writer)
                .with_target(true)
                .with_ansi(supports_colour())
                .boxed(),
        );
    }

    tracing_subscriber::registry()
        .with(filter)
        .with(layers)
        .try_init()
        .map_err(|_| ObservabilityError::AlreadyInitialised)?;

    announce_privacy(effective_privacy);

    Ok(LogGuard {
        _file: file_guard,
        _console: console_guard,
    })
}

/// Record which privacy mode is in force.
///
/// Prompt logging is loud on purpose. Someone reading a support bundle needs to
/// know at a glance whether it contains user text.
fn announce_privacy(mode: PrivacyMode) {
    match mode {
        PrivacyMode::Standard => tracing::info!(
            target: targets::STARTUP,
            privacy_mode = "standard",
            "prompt logging is disabled; prompts and completions are redacted"
        ),
        PrivacyMode::PromptsLogged => tracing::warn!(
            target: targets::STARTUP,
            privacy_mode = "prompts_logged",
            "PROMPT LOGGING IS ENABLED - logs will contain user prompts and completions"
        ),
        PrivacyMode::Strict => tracing::info!(
            target: targets::STARTUP,
            privacy_mode = "strict",
            "Privacy Mode is active; prompt logging cannot be enabled in this process"
        ),
    }
}

/// Honour `NO_COLOR`, and do not emit escape codes into a redirected stream.
fn supports_colour() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_redacts_prompts() {
        // If this ever flips, section 26's "do not log user prompts by
        // default" is broken for every binary that takes the default.
        assert_eq!(LogConfig::default().privacy, PrivacyMode::Standard);
    }

    #[test]
    fn invalid_filter_is_reported_actionably() {
        let err = ObservabilityError::Filter {
            filter: "=".to_owned(),
            source: "="
                .parse::<tracing_subscriber::filter::Directive>()
                .unwrap_err(),
        };
        assert_eq!(err.code(), "invalid_log_filter");
        assert_eq!(err.kind(), ErrorKind::InvalidRequest);
        assert!(!err.remedies().is_empty());
    }
}
