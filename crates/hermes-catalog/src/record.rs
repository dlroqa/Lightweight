//! What the catalog remembers about one installed model.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hermes_gguf::ModelMetadata;
use serde::{Deserialize, Serialize};

/// How much can be promised about a file's bytes.
///
/// Recorded per model rather than assumed, because the four cases really are
/// different and a UI that showed them all as "verified" would be lying about
/// the last one. This is the difference between "these are the bytes we
/// intended to fetch" and "these are the bytes that arrived".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Integrity {
    /// Checked against a digest recorded in the pinned manifest before the
    /// fact. The strongest of the four: the digest predates the download.
    Manifest,
    /// Checked against a digest the host published for the file — the
    /// HuggingFace LFS object id. Strong, but it comes from the same party as
    /// the bytes.
    Published,
    /// Checked against a digest the user supplied.
    Supplied,
    /// Computed on arrival and never checked against anything.
    ///
    /// Still worth having: it makes a later corruption or a silent change of
    /// the file detectable. It is not evidence that the right file arrived.
    #[default]
    Recorded,
    /// The file was already on this machine and the user chose it.
    Imported,
}

impl Integrity {
    /// Whether the bytes were checked against a digest from somewhere else.
    pub const fn verified(self) -> bool {
        matches!(self, Self::Manifest | Self::Published | Self::Supplied)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Manifest => "verified (pinned digest)",
            Self::Published => "verified (published digest)",
            Self::Supplied => "verified (digest you supplied)",
            Self::Recorded => "recorded, not verified",
            Self::Imported => "imported from this machine",
        }
    }
}

/// Where a model came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// A file that was already on this machine.
    Import { original_path: PathBuf },
    /// One of the pinned models in [`crate::manifest`].
    Manifest { manifest_id: String, url: String },
    /// A link the user pasted.
    Link { url: String },
}

impl Source {
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Import { .. } => None,
            Self::Manifest { url, .. } | Self::Link { url } => Some(url),
        }
    }
}

/// One model this machine has.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledModel {
    /// Catalog id: a slug, with no `@context` suffix.
    ///
    /// The suffix belongs to a *loaded* model, because it encodes the context
    /// the engine was started with. A file on disk has no context.
    pub id: String,
    /// What to show a person. The model's own `general.name` when it has one.
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    /// Lowercase hex sha256 of the file.
    pub sha256: String,
    pub integrity: Integrity,
    pub source: Source,

    // --- read once from the GGUF header at install time ---
    pub architecture: String,
    /// Whether the pinned engine can run this architecture.
    ///
    /// Recorded rather than enforced: a model the current engine cannot run is
    /// still a model, and a later build may well run it.
    pub supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantization: Option<String>,
    /// The largest context the model's own metadata declares.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_bytes: Option<u64>,

    /// Unix seconds.
    pub added_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_loaded_at: Option<u64>,
    /// The context it was last loaded with, offered as the default next time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_n_ctx: Option<u32>,
}

impl InstalledModel {
    /// Build a record from a file that has already been hashed.
    pub fn new(
        id: impl Into<String>,
        path: impl Into<PathBuf>,
        bytes: u64,
        sha256: impl Into<String>,
        integrity: Integrity,
        source: Source,
        metadata: &ModelMetadata,
    ) -> Self {
        let id = id.into();
        Self {
            name: metadata.name.clone().unwrap_or_else(|| id.clone()),
            id,
            path: path.into(),
            bytes,
            sha256: sha256.into(),
            integrity,
            source,
            architecture: metadata.architecture.clone(),
            supported: metadata.supported,
            param_count: metadata.param_count,
            quantization: Some(metadata.quantization_label()),
            context_length: metadata.context_length,
            weight_bytes: metadata.weight_bytes,
            added_at: unix_now(),
            last_loaded_at: None,
            last_n_ctx: None,
        }
    }

    /// Whether this program may delete the model's file.
    ///
    /// True only for a file we downloaded into our own directory. An imported
    /// one belongs to the user and was never copied, so removing it from the
    /// catalog must leave it alone — forgetting a model and destroying
    /// someone's file are very different acts.
    ///
    /// Lives on the record because three callers asked the same question and
    /// three answers is two too many.
    pub const fn is_ours_to_delete(&self) -> bool {
        matches!(self.source, Source::Manifest { .. } | Source::Link { .. })
    }

    /// Whether the file is still where the catalog left it.
    ///
    /// Checked rather than persisted. A record whose file is missing is kept —
    /// an unmounted drive must not delete a user's catalog — so "is it there?"
    /// is a question about right now, not about install time.
    pub fn is_present(&self) -> bool {
        self.path.is_file()
    }

    /// Record that this model was loaded, and at what context.
    pub fn mark_loaded(&mut self, n_ctx: u32) {
        self.last_loaded_at = Some(unix_now());
        self.last_n_ctx = Some(n_ctx);
    }
}

/// A catalog id derived from a file name.
///
/// Lowercase, with anything that is not alphanumeric, `.`, `-` or `_` replaced
/// by `-`. The result goes into a URL path and into `/v1/models`, so it cannot
/// carry spaces or slashes; and it is derived rather than random so that
/// re-importing the same file twice is recognisably the same model.
pub fn slug_for(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("model");
    let slug: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "model".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// A sibling path to write to before renaming over `path`.
///
/// Deliberately in the same directory: a rename is only atomic within one
/// filesystem, and a temp file under `/tmp` can land on a different one.
pub(crate) fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_digest_from_elsewhere_counts_as_verification() {
        // The distinction the whole type exists for: a digest we computed
        // ourselves from bytes we just received proves only that the disk did
        // not corrupt them.
        assert!(Integrity::Manifest.verified());
        assert!(Integrity::Published.verified());
        assert!(Integrity::Supplied.verified());
        assert!(!Integrity::Recorded.verified());
        assert!(!Integrity::Imported.verified());
    }

    #[test]
    fn a_recorded_digest_says_so_in_words() {
        // This label reaches the UI and the CLI. It must not read as a
        // guarantee that was never made.
        assert!(Integrity::Recorded.label().contains("not verified"));
        assert!(Integrity::Manifest.label().starts_with("verified"));
    }

    #[test]
    fn a_file_name_becomes_a_url_safe_id() {
        assert_eq!(
            slug_for(Path::new("/models/SmolLM2-135M-Instruct-Q4_K_M.gguf")),
            "smollm2-135m-instruct-q4_k_m"
        );
        // Spaces and slashes would break a route and a model id.
        assert_eq!(
            slug_for(Path::new("/models/My Model (v2).gguf")),
            "my-model--v2"
        );
        assert_eq!(slug_for(Path::new("/models/....gguf")), "...");
    }

    #[test]
    fn a_nameless_file_still_gets_an_id() {
        assert_eq!(slug_for(Path::new("/models/---.gguf")), "model");
        assert_eq!(slug_for(Path::new("")), "model");
    }

    #[test]
    fn only_a_file_we_downloaded_is_ours_to_delete() {
        // The distinction that keeps `--delete` from destroying a user's own
        // file: we copied nothing on import, so we own nothing there.
        let downloaded = Source::Manifest {
            manifest_id: "m".into(),
            url: "https://example.com/m.gguf".into(),
        };
        let linked = Source::Link {
            url: "https://example.com/m.gguf".into(),
        };
        let imported = Source::Import {
            original_path: PathBuf::from("/elsewhere/m.gguf"),
        };

        let record = |source| InstalledModel {
            id: "m".into(),
            name: "m".into(),
            path: PathBuf::from("/models/m.gguf"),
            bytes: 1,
            sha256: "aa".into(),
            integrity: Integrity::Recorded,
            source,
            architecture: "llama".into(),
            supported: true,
            param_count: None,
            quantization: None,
            context_length: None,
            weight_bytes: None,
            added_at: 0,
            last_loaded_at: None,
            last_n_ctx: None,
        };

        assert!(record(downloaded).is_ours_to_delete());
        assert!(record(linked).is_ours_to_delete());
        assert!(!record(imported).is_ours_to_delete());
    }

    #[test]
    fn a_record_round_trips_through_json() {
        let record = InstalledModel {
            id: "smollm2".into(),
            name: "SmolLM2".into(),
            path: PathBuf::from("/models/s.gguf"),
            bytes: 100,
            sha256: "ab".into(),
            integrity: Integrity::Manifest,
            source: Source::Manifest {
                manifest_id: "smollm2".into(),
                url: "https://example.com/s.gguf".into(),
            },
            architecture: "llama".into(),
            supported: true,
            param_count: Some(135_000_000),
            quantization: Some("Q4_K_M".into()),
            context_length: Some(8192),
            weight_bytes: Some(90),
            added_at: 1_700_000_000,
            last_loaded_at: None,
            last_n_ctx: None,
        };
        let json = serde_json::to_string(&record).expect("serialize");
        let back: InstalledModel = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(record, back);
        // The tag is what lets a source be extended without breaking old files.
        assert!(json.contains(r#""kind":"manifest""#));
    }

    #[test]
    fn the_temporary_file_is_a_sibling_so_the_rename_stays_atomic() {
        // A rename is atomic only within one filesystem. Writing the temp file
        // to /tmp and renaming it into ~/.local/share would not be a rename at
        // all on a machine where those are separate mounts.
        let path = Path::new("/data/catalog.json");
        let temp = temp_sibling(path);
        assert_eq!(temp.parent(), path.parent());
        assert_eq!(temp, PathBuf::from("/data/catalog.json.tmp"));
    }

    #[test]
    fn a_missing_file_is_reported_as_absent_rather_than_assumed_present() {
        let mut record = InstalledModel {
            id: "x".into(),
            name: "x".into(),
            path: PathBuf::from("/definitely/not/here.gguf"),
            bytes: 0,
            sha256: String::new(),
            integrity: Integrity::Imported,
            source: Source::Import {
                original_path: PathBuf::from("/definitely/not/here.gguf"),
            },
            architecture: "llama".into(),
            supported: true,
            param_count: None,
            quantization: None,
            context_length: None,
            weight_bytes: None,
            added_at: 0,
            last_loaded_at: None,
            last_n_ctx: None,
        };
        assert!(!record.is_present());

        record.mark_loaded(4096);
        assert_eq!(record.last_n_ctx, Some(4096));
        assert!(record.last_loaded_at.is_some());
    }
}
