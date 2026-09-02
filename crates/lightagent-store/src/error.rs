//! Why a store operation failed.

use std::path::PathBuf;

/// A failure reading or writing a session.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// An id was not of the generated shape, so it cannot name a stored file.
    /// Distinct from [`NotFound`](StoreError::NotFound): "cannot exist here" and
    /// "is gone" are different answers.
    #[error("'{0}' is not a valid session id")]
    MalformedId(String),

    /// No session with this id exists.
    #[error("no session '{0}'")]
    NotFound(String),

    /// The file exists but could not be read or parsed.
    #[error("session '{id}' is unreadable: {reason}")]
    Unreadable { id: String, reason: String },

    /// The record could not be written.
    #[error("could not write session '{id}': {reason}")]
    Unwritable { id: String, reason: String },

    /// The store's directory could not be created or listed.
    #[error("could not access the session store at {path}: {reason}")]
    Directory { path: PathBuf, reason: String },
}
