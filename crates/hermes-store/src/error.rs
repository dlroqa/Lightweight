//! What can go wrong reading or writing the user's own files.

use std::path::PathBuf;

use hermes_core::{Actionable, ErrorKind, Remedy, RemedyAction, SettingsSection};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("could not {action}: {source}")]
    Io {
        action: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} could not be read as {what}: {reason}")]
    Unreadable {
        what: &'static str,
        path: PathBuf,
        reason: String,
    },

    #[error("no conversation with id {id:?}")]
    NoSuchConversation { id: String },

    /// An id that could not have come from here.
    ///
    /// Separate from "not found" on purpose: an id becomes a file name, and one
    /// that is not in the generated shape is a request to read somewhere it was
    /// never meant to. Answering `404` would be true but would quietly accept
    /// the attempt; this says the id itself is wrong.
    #[error("{id:?} is not a conversation id")]
    MalformedId { id: String },

    #[error("this gateway is not keeping conversation history")]
    HistoryDisabled,
}

impl StoreError {
    pub(crate) fn io(action: &'static str, source: std::io::Error) -> Self {
        Self::Io { action, source }
    }
}

impl Actionable for StoreError {
    fn code(&self) -> &'static str {
        match self {
            Self::Io { .. } => "store_io_failed",
            Self::Unreadable { .. } => "store_unreadable",
            Self::NoSuchConversation { .. } => "unknown_conversation",
            Self::MalformedId { .. } => "malformed_conversation_id",
            Self::HistoryDisabled => "history_disabled",
        }
    }

    fn kind(&self) -> ErrorKind {
        match self {
            Self::Io { .. } | Self::Unreadable { .. } => ErrorKind::Internal,
            Self::NoSuchConversation { .. } => ErrorKind::NotFound,
            Self::MalformedId { .. } => ErrorKind::InvalidRequest,
            // Not a failure of this request so much as a standing decision, and
            // the remedy is a setting rather than a retry.
            Self::HistoryDisabled => ErrorKind::InvalidRequest,
        }
    }

    fn remedies(&self) -> Vec<Remedy> {
        match self {
            Self::HistoryDisabled => vec![Remedy::new(
                "Turn conversation history on in settings, or keep it off and \
                 use chat without a saved transcript",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Storage,
                },
            )],
            Self::Io { .. } | Self::Unreadable { .. } => vec![Remedy::new(
                "Check that the data directory is writable and has free space",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Storage,
                },
            )],
            _ => Vec::new(),
        }
    }
}
