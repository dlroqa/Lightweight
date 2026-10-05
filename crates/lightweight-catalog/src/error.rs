//! What can go wrong managing the catalog.

use std::path::PathBuf;

use lightweight_core::{Actionable, ErrorKind, Remedy, RemedyAction, SettingsSection};
use lightweight_download::DownloadError;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("no model in the catalog with id {id:?}")]
    UnknownModel { id: String },

    #[error("no pinned model with id {id:?}")]
    UnknownManifestModel { id: String },

    #[error("there is already a model with id {id:?} in the catalog")]
    DuplicateModel { id: String },

    #[error("no file at {path}")]
    FileNotFound { path: PathBuf },

    #[error("{path} is not a GGUF file: {reason}")]
    NotAGguf { path: PathBuf, reason: String },

    #[error("{id} is loaded right now and cannot be removed")]
    InUse { id: String },

    #[error("the catalog file at {path} could not be read: {reason}")]
    CatalogUnreadable { path: PathBuf, reason: String },

    #[error("{sha256:?} is not a sha256 digest")]
    NotADigest { sha256: String },

    #[error("{alias:?} cannot be used as an alias: {problem}")]
    InvalidAlias {
        alias: String,
        problem: crate::alias::AliasProblem,
    },

    /// Said in the user's own spelling of the alias, because that is what they
    /// will look for; the model holding it is named so they can find it.
    #[error(
        "alias {alias:?} is already assigned to another model ({owner}); choose a different alias"
    )]
    AliasInUse { alias: String, owner: String },

    /// An alias spelled like a canonical id, which would make one name mean
    /// two things depending on which was looked up first.
    #[error(
        "alias {alias:?} is the id of a model ({id}); an alias must differ from every model id, so choose a different alias"
    )]
    AliasIsModelId { alias: String, id: String },

    /// The other direction: a model whose id cannot change — a pinned id, a
    /// link's file name — would arrive under a name that is already an alias.
    #[error(
        "cannot add {id:?}: that id is already the alias {alias:?} of {owner}, and one name can only mean one model; rename or clear that alias first"
    )]
    ModelIdIsAlias {
        id: String,
        alias: String,
        owner: String,
    },

    #[error(transparent)]
    Download(#[from] DownloadError),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl CatalogError {
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

impl Actionable for CatalogError {
    fn code(&self) -> &'static str {
        match self {
            Self::UnknownModel { .. } => "unknown_model",
            Self::UnknownManifestModel { .. } => "unknown_pinned_model",
            Self::DuplicateModel { .. } => "duplicate_model",
            Self::FileNotFound { .. } => "model_file_not_found",
            Self::NotAGguf { .. } => "not_a_gguf",
            Self::InUse { .. } => "model_in_use",
            Self::CatalogUnreadable { .. } => "catalog_unreadable",
            Self::NotADigest { .. } => "not_a_digest",
            Self::InvalidAlias { .. } => "invalid_alias",
            Self::AliasInUse { .. } => "alias_in_use",
            Self::AliasIsModelId { .. } => "alias_is_model_id",
            Self::ModelIdIsAlias { .. } => "model_id_is_alias",
            Self::Download(err) => err.code(),
            Self::Io { .. } => "io_error",
        }
    }

    fn kind(&self) -> ErrorKind {
        match self {
            Self::UnknownModel { .. }
            | Self::UnknownManifestModel { .. }
            | Self::FileNotFound { .. } => ErrorKind::NotFound,
            Self::DuplicateModel { .. }
            | Self::NotAGguf { .. }
            | Self::NotADigest { .. }
            | Self::InvalidAlias { .. }
            | Self::AliasInUse { .. }
            | Self::AliasIsModelId { .. }
            | Self::ModelIdIsAlias { .. } => ErrorKind::InvalidRequest,
            // Not an error the caller can fix by retrying, and not a failure
            // either: unloading first is a real, ordered thing to do.
            Self::InUse { .. } => ErrorKind::InvalidRequest,
            Self::CatalogUnreadable { .. } | Self::Io { .. } => ErrorKind::Internal,
            Self::Download(err) => err.kind(),
        }
    }

    fn remedies(&self) -> Vec<Remedy> {
        match self {
            Self::InUse { .. } => vec![Remedy::new(
                "Unload the model first, then remove it",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Models,
                },
            )],
            Self::NotAGguf { .. } => vec![Remedy::new(
                "Choose a .gguf file, or check that the link points at one",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Models,
                },
            )],
            Self::FileNotFound { .. } | Self::UnknownModel { .. } => vec![Remedy::new(
                "Import or download the model again",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Models,
                },
            )],
            Self::CatalogUnreadable { path, .. } => vec![Remedy::new(
                format!("Check permissions on {}", path.display()),
                RemedyAction::OpenSettings {
                    section: SettingsSection::Storage,
                },
            )],
            Self::AliasInUse { owner, .. } => vec![Remedy::new(
                format!("Choose a different alias, or clear the one on {owner} first"),
                RemedyAction::OpenSettings {
                    section: SettingsSection::Models,
                },
            )],
            Self::ModelIdIsAlias { owner, alias, .. } => vec![Remedy::new(
                format!("Rename or clear the alias {alias:?} on {owner}, then add the model again"),
                RemedyAction::OpenSettings {
                    section: SettingsSection::Models,
                },
            )],
            Self::Download(err) => err.remedies(),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_download_failure_keeps_its_own_code_rather_than_being_flattened() {
        // A caller branching on "why did adding a model fail" needs the
        // download's own answer - corrupt, low disk, insecure URL - not a
        // single "catalog error".
        let err = CatalogError::Download(DownloadError::Corrupt {
            expected: "a".into(),
            actual: "b".into(),
        });
        assert_eq!(err.code(), "download_corrupt");
        assert_eq!(err.kind(), ErrorKind::Internal);
    }

    #[test]
    fn removing_a_loaded_model_is_refused_with_something_to_do_about_it() {
        let err = CatalogError::InUse { id: "qwen3".into() };
        assert_eq!(err.http_status(), 400);
        assert_eq!(err.remedies().len(), 1);
    }

    #[test]
    fn an_alias_clash_is_the_callers_to_fix_and_names_both_sides() {
        let err = CatalogError::AliasInUse {
            alias: "Coder".into(),
            owner: "qwen3".into(),
        };
        assert_eq!(err.http_status(), 400);
        assert_eq!(err.code(), "alias_in_use");
        let said = err.to_string();
        assert!(
            said.contains("\"Coder\"") && said.contains("qwen3"),
            "{said}"
        );
        assert_eq!(err.remedies().len(), 1);
    }

    #[test]
    fn an_absent_model_is_a_404_not_a_500() {
        assert_eq!(
            CatalogError::UnknownModel { id: "nope".into() }.http_status(),
            404
        );
    }
}
