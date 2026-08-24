//! What can go wrong while fetching a file.
//!
//! Separate from the callers' own error types on purpose. The engine installer
//! reports [`crate::DownloadError`] as a `BackendError` and the model catalog
//! reports it as a catalog error; neither vocabulary belongs here, and a
//! downloader that knew about inference engines would not be reusable by the
//! thing that fetches weights.

use std::path::PathBuf;

use hermes_core::{Actionable, ErrorKind, Remedy, RemedyAction, SettingsSection};

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// The URL is not `https`.
    ///
    /// Refused rather than downgraded. These bytes are verified against a
    /// digest and then read as a model or executed as an engine, and a
    /// plaintext transport gives an attacker on the path the chance to choose
    /// them.
    #[error("{scheme}:// is not accepted; only https URLs are downloaded")]
    InsecureUrl { scheme: String },

    #[error("could not download {what}: {reason}")]
    Failed { what: String, reason: String },

    #[error("the download failed verification: expected sha256 {expected}, got {actual}")]
    Corrupt { expected: String, actual: String },

    #[error("not enough disk space at {path}: {needed} bytes required")]
    LowDisk { path: PathBuf, needed: u64 },

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("the download was cancelled")]
    Cancelled,
}

impl DownloadError {
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

impl Actionable for DownloadError {
    fn code(&self) -> &'static str {
        match self {
            Self::InsecureUrl { .. } => "insecure_url",
            Self::Failed { .. } => "download_failed",
            Self::Corrupt { .. } => "download_corrupt",
            Self::LowDisk { .. } => "low_disk",
            Self::Io { .. } => "io_error",
            Self::Cancelled => "cancelled",
        }
    }

    fn kind(&self) -> ErrorKind {
        match self {
            Self::InsecureUrl { .. } => ErrorKind::InvalidRequest,
            // Transient far more often than not: a dropped connection, a CDN
            // hiccup, a mirror that was briefly down.
            Self::Failed { .. } => ErrorKind::Unavailable,
            // Not transient, and not the caller's fault either. A digest
            // mismatch means the bytes are wrong, and running them anyway is
            // the one thing that must not happen.
            Self::Corrupt { .. } => ErrorKind::Internal,
            Self::LowDisk { .. } => ErrorKind::ResourceExhausted,
            Self::Io { .. } => ErrorKind::Internal,
            Self::Cancelled => ErrorKind::Cancelled,
        }
    }

    fn remedies(&self) -> Vec<Remedy> {
        match self {
            Self::InsecureUrl { .. } => vec![Remedy::new(
                "Use an https link",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Models,
                },
            )],
            Self::Failed { .. } => vec![Remedy::new(
                "Retry the download; it resumes from where it stopped",
                RemedyAction::RetryAfter { seconds: 5 },
            )],
            Self::Corrupt { .. } => vec![Remedy::new(
                "Retry the download; the partial file has been discarded",
                RemedyAction::RetryAfter { seconds: 0 },
            )],
            Self::LowDisk { needed, .. } => vec![Remedy::new(
                format!("Free {needed} bytes, or choose another location"),
                RemedyAction::FreeDisk {
                    needed_bytes: *needed,
                },
            )],
            Self::Io { .. } | Self::Cancelled => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_has_a_stable_code() {
        let cases: Vec<(DownloadError, &str)> = vec![
            (
                DownloadError::InsecureUrl {
                    scheme: "http".into(),
                },
                "insecure_url",
            ),
            (
                DownloadError::Failed {
                    what: "engine".into(),
                    reason: "timed out".into(),
                },
                "download_failed",
            ),
            (
                DownloadError::Corrupt {
                    expected: "a".into(),
                    actual: "b".into(),
                },
                "download_corrupt",
            ),
            (
                DownloadError::LowDisk {
                    path: PathBuf::from("/models"),
                    needed: 100,
                },
                "low_disk",
            ),
            (
                DownloadError::io("writing", std::io::Error::other("x")),
                "io_error",
            ),
            (DownloadError::Cancelled, "cancelled"),
        ];
        for (err, code) in cases {
            assert_eq!(err.code(), code, "{err}");
        }
    }

    #[test]
    fn a_corrupt_download_is_never_reported_as_retryable_in_place() {
        // The bytes on disk are wrong. A client that treated this as a
        // transient failure and retried *without* discarding them would
        // inherit the corruption, which is why the fetch layer deletes the
        // partial file before this error is ever constructed.
        assert!(
            !DownloadError::Corrupt {
                expected: "a".into(),
                actual: "b".into(),
            }
            .kind()
            .is_retryable()
        );
    }

    #[test]
    fn a_plaintext_url_is_a_caller_mistake_not_a_server_fault() {
        let err = DownloadError::InsecureUrl {
            scheme: "http".into(),
        };
        assert_eq!(err.http_status(), 400);
        assert!(!err.remedies().is_empty());
    }
}
