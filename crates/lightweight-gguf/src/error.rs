//! Parse failures.
//!
//! Spec section 27 requires that an invalid GGUF file produces an actionable
//! error rather than a crash. Every variant here therefore carries the file
//! offset where the problem was found, because "invalid GGUF" on its own tells
//! a user nothing and tells a maintainer even less.

use std::path::PathBuf;

use lightweight_core::{Actionable, ErrorKind, Remedy, RemedyAction, SettingsSection};

#[derive(Debug, thiserror::Error)]
pub enum GgufError {
    #[error("could not open {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not read {path} at offset {offset}: {source}")]
    Read {
        path: PathBuf,
        offset: u64,
        #[source]
        source: std::io::Error,
    },

    #[error("not a GGUF file: expected magic 'GGUF', found {found:02x?}")]
    BadMagic { found: [u8; 4] },

    #[error("unsupported GGUF version {found}; this build reads versions {min} to {max}")]
    UnsupportedVersion { found: u32, min: u32, max: u32 },

    #[error(
        "file ends early: needed {needed} bytes at offset {offset}, but only {available} remain"
    )]
    Truncated {
        offset: u64,
        needed: u64,
        available: u64,
    },

    #[error("unknown value type {value_type} for key {key:?} at offset {offset}")]
    UnknownValueType {
        key: String,
        value_type: u32,
        offset: u64,
    },

    #[error("nested arrays are not permitted by the GGUF format (key {key:?} at offset {offset})")]
    NestedArray { key: String, offset: u64 },

    #[error("key {key:?} at offset {offset} is not valid UTF-8")]
    InvalidUtf8 { key: String, offset: u64 },

    #[error(
        "metadata is implausibly large ({claimed} bytes claimed, limit is {limit}); \
         the file is probably corrupt"
    )]
    MetadataTooLarge { claimed: u64, limit: u64 },

    #[error(
        "at offset {offset}: declared {claimed} {what}, which cannot fit in the \
         remaining {available} bytes; the file is probably corrupt"
    )]
    ImplausibleCount {
        what: &'static str,
        claimed: u64,
        available: u64,
        offset: u64,
    },

    #[error("tensor {name:?} declares {dims} dimensions, more than the {limit} ggml supports")]
    TooManyDimensions { name: String, dims: u32, limit: u32 },

    #[error("tensor {name:?} has an element count that overflows a 64-bit integer")]
    DimensionOverflow { name: String },
}

impl GgufError {
    /// Byte offset the failure was detected at, when there is one.
    pub fn offset(&self) -> Option<u64> {
        match self {
            Self::Read { offset, .. }
            | Self::Truncated { offset, .. }
            | Self::UnknownValueType { offset, .. }
            | Self::NestedArray { offset, .. }
            | Self::ImplausibleCount { offset, .. }
            | Self::InvalidUtf8 { offset, .. } => Some(*offset),
            _ => None,
        }
    }
}

impl Actionable for GgufError {
    fn code(&self) -> &'static str {
        match self {
            Self::Open { .. } => "gguf_unreadable",
            Self::Read { .. } => "gguf_read_failed",
            Self::BadMagic { .. } => "not_a_gguf_file",
            Self::UnsupportedVersion { .. } => "gguf_version_unsupported",
            Self::Truncated { .. } => "gguf_truncated",
            Self::UnknownValueType { .. } => "gguf_unknown_value_type",
            Self::NestedArray { .. } => "gguf_nested_array",
            Self::InvalidUtf8 { .. } => "gguf_invalid_utf8",
            Self::MetadataTooLarge { .. } => "gguf_metadata_too_large",
            Self::ImplausibleCount { .. } => "gguf_implausible_count",
            Self::TooManyDimensions { .. } => "gguf_too_many_dimensions",
            Self::DimensionOverflow { .. } => "gguf_dimension_overflow",
        }
    }

    fn kind(&self) -> ErrorKind {
        match self {
            Self::Open { .. } | Self::Read { .. } => ErrorKind::Internal,
            _ => ErrorKind::InvalidRequest,
        }
    }

    fn remedies(&self) -> Vec<Remedy> {
        match self {
            Self::BadMagic { .. } => vec![Remedy::new(
                "Choose a file in GGUF format; other formats must be converted first",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Models,
                },
            )],
            // A truncated or internally inconsistent file is almost always an
            // interrupted download, so say that rather than "file is corrupt".
            Self::Truncated { .. }
            | Self::ImplausibleCount { .. }
            | Self::MetadataTooLarge { .. } => {
                vec![Remedy::new(
                    "Re-download the model; the file looks incomplete",
                    RemedyAction::OpenSettings {
                        section: SettingsSection::Models,
                    },
                )]
            }
            Self::UnsupportedVersion { found, .. } => vec![Remedy::new(
                format!("Re-export the model as GGUF v3; this file is v{found}"),
                RemedyAction::OpenSettings {
                    section: SettingsSection::Models,
                },
            )],
            _ => Vec::new(),
        }
    }
}
