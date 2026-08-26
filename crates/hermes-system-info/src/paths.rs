//! Where application data lives.
//!
//! Spec section 25 requires that settings, models, logs, conversations,
//! benchmarks and API configuration are stored in platform-appropriate
//! directories, separately from the application binaries, and that normal
//! operation never needs administrator or root privileges. Everything here
//! resolves under the current user's own data directory, so the second
//! requirement holds by construction.

use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use hermes_core::{Actionable, ErrorKind, Remedy, RemedyAction, SettingsSection};

/// Environment variable that overrides the entire data root.
///
/// Two uses: tests, which must never touch the real user directory; and
/// running a throwaway profile alongside a real installation.
pub const HOME_ENV: &str = "HERMES_GATEWAY_HOME";

const QUALIFIER: &str = "ai";
const ORGANISATION: &str = "Hermes";
const APPLICATION: &str = "CpuInferenceGateway";

#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    #[error(
        "could not determine a home directory for the current user; \
         set {HOME_ENV} to choose a data directory explicitly"
    )]
    NoHomeDirectory,

    #[error("could not create {path}: {source}")]
    Create {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl Actionable for PathsError {
    fn code(&self) -> &'static str {
        match self {
            Self::NoHomeDirectory => "no_home_directory",
            Self::Create { .. } => "data_directory_not_writable",
        }
    }

    fn kind(&self) -> ErrorKind {
        ErrorKind::Internal
    }

    fn remedies(&self) -> Vec<Remedy> {
        match self {
            Self::NoHomeDirectory => vec![Remedy::new(
                format!("Set {HOME_ENV} to a writable directory"),
                RemedyAction::OpenSettings {
                    section: SettingsSection::Storage,
                },
            )],
            Self::Create { path, .. } => vec![Remedy::new(
                format!("Check permissions on {}", path.display()),
                RemedyAction::OpenSettings {
                    section: SettingsSection::Storage,
                },
            )],
        }
    }
}

/// Resolved locations for everything the application persists.
///
/// Data, configuration and cache are kept apart because the platforms treat
/// them differently: on Linux the cache is `~/.cache` and may be deleted by the
/// system at any time, which is fine for a downloaded engine binary we can
/// re-fetch but not for a user's conversations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataPaths {
    data: PathBuf,
    config: PathBuf,
    cache: PathBuf,
}

impl DataPaths {
    /// Resolve the platform's standard locations, honouring [`HOME_ENV`].
    ///
    /// Typical results:
    ///
    /// | platform | data |
    /// |---|---|
    /// | Linux   | `~/.local/share/CpuInferenceGateway` |
    /// | macOS   | `~/Library/Application Support/ai.Hermes.CpuInferenceGateway` |
    /// | Windows | `%APPDATA%\Hermes\CpuInferenceGateway\data` |
    pub fn discover() -> Result<Self, PathsError> {
        if let Some(root) = std::env::var_os(HOME_ENV) {
            return Ok(Self::rooted_at(PathBuf::from(root)));
        }

        let dirs = ProjectDirs::from(QUALIFIER, ORGANISATION, APPLICATION)
            .ok_or(PathsError::NoHomeDirectory)?;

        Ok(Self {
            data: dirs.data_dir().to_path_buf(),
            config: dirs.config_dir().to_path_buf(),
            cache: dirs.cache_dir().to_path_buf(),
        })
    }

    /// Put everything under a single root.
    ///
    /// Used by [`HOME_ENV`] and by tests. Keeping data, config and cache as
    /// siblings under one directory makes a profile trivially copyable and
    /// deletable, which is the point of an override.
    pub fn rooted_at(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            data: root.join("data"),
            config: root.join("config"),
            cache: root.join("cache"),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data
    }

    pub fn config_dir(&self) -> &Path {
        &self.config
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache
    }

    /// Imported and downloaded GGUF files.
    pub fn models_dir(&self) -> PathBuf {
        self.data.join("models")
    }

    /// Conversation history.
    pub fn conversations_dir(&self) -> PathBuf {
        self.data.join("conversations")
    }

    /// Saved benchmark runs (spec section 21).
    pub fn benchmarks_dir(&self) -> PathBuf {
        self.data.join("benchmarks")
    }

    /// Rotated log files.
    pub fn logs_dir(&self) -> PathBuf {
        self.data.join("logs")
    }

    /// Downloaded inference engine binaries.
    ///
    /// Under the cache root deliberately: these are a pinned, verified,
    /// re-downloadable artifact, so losing them costs a download and nothing
    /// else.
    pub fn runtime_dir(&self) -> PathBuf {
        self.cache.join("runtime")
    }

    /// Partially downloaded files, kept out of `models_dir` so that a scan for
    /// importable models never trips over a half-written file.
    pub fn downloads_dir(&self) -> PathBuf {
        self.cache.join("downloads")
    }

    /// Application settings.
    pub fn settings_file(&self) -> PathBuf {
        self.config.join("settings.json")
    }

    /// API server configuration: bind address, keys, allowed hosts
    /// (spec section 23). Separate from general settings because it is the
    /// security-relevant file, and keeping it distinct makes it auditable.
    pub fn api_config_file(&self) -> PathBuf {
        self.config.join("api.json")
    }

    /// The model catalog: what is installed, its metadata and its digests.
    pub fn catalog_file(&self) -> PathBuf {
        self.data.join("catalog.json")
    }

    /// Fitted memory coefficients, written by `hermes bench --fit`.
    ///
    /// Beside the catalog rather than in `benchmarks_dir`: a benchmark run is a
    /// record of one measurement and there are many of them, while this is the
    /// single conclusion drawn from all of them, and the load path reads it on
    /// every estimate. Under the data root, because losing it costs a
    /// measurement that cannot simply be downloaded again.
    pub fn calibration_file(&self) -> PathBuf {
        self.data.join("calibration.json")
    }

    /// Every directory this layout uses.
    pub fn all_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.data.clone(),
            self.config.clone(),
            self.cache.clone(),
            self.models_dir(),
            self.conversations_dir(),
            self.benchmarks_dir(),
            self.logs_dir(),
            self.runtime_dir(),
            self.downloads_dir(),
        ]
    }

    /// Create every directory, if it does not already exist.
    pub fn create_all(&self) -> Result<(), PathsError> {
        for dir in self.all_dirs() {
            std::fs::create_dir_all(&dir)
                .map_err(|source| PathsError::Create { path: dir, source })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary directory that removes itself. Small enough to own rather
    /// than take a dependency for.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            // The clock alone is not unique: on a coarse timer two tests running in
            // parallel are handed the same name. The counter and the pid settle it.
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hermes-paths-{tag}-{}-{unique}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rooted_layout_keeps_the_three_roots_as_siblings() {
        let paths = DataPaths::rooted_at("/tmp/profile");
        assert_eq!(paths.data_dir(), Path::new("/tmp/profile/data"));
        assert_eq!(paths.config_dir(), Path::new("/tmp/profile/config"));
        assert_eq!(paths.cache_dir(), Path::new("/tmp/profile/cache"));
    }

    #[test]
    fn models_and_conversations_live_in_data_not_cache() {
        // A cache directory can be cleared by the OS. Losing a downloaded
        // engine binary is a re-download; losing a user's conversations is
        // data loss.
        let paths = DataPaths::rooted_at("/tmp/profile");
        assert!(paths.models_dir().starts_with(paths.data_dir()));
        assert!(paths.conversations_dir().starts_with(paths.data_dir()));
        assert!(paths.runtime_dir().starts_with(paths.cache_dir()));
        assert!(paths.downloads_dir().starts_with(paths.cache_dir()));
    }

    #[test]
    fn partial_downloads_are_kept_out_of_the_models_directory() {
        // A model scan must never pick up a half-written file.
        let paths = DataPaths::rooted_at("/tmp/profile");
        assert!(!paths.downloads_dir().starts_with(paths.models_dir()));
    }

    #[test]
    fn create_all_is_idempotent() {
        let temp = TempDir::new("create");
        let paths = DataPaths::rooted_at(&temp.0);

        paths.create_all().expect("first create");
        paths.create_all().expect("second create should be a no-op");

        for dir in paths.all_dirs() {
            assert!(dir.is_dir(), "{} was not created", dir.display());
        }
    }

    #[test]
    fn nothing_resolves_outside_the_override_root() {
        // Guards against a stray absolute path in the layout leaking writes
        // outside a throwaway profile.
        let temp = TempDir::new("contained");
        let paths = DataPaths::rooted_at(&temp.0);

        for dir in paths.all_dirs() {
            assert!(
                dir.starts_with(&temp.0),
                "{} escaped the root",
                dir.display()
            );
        }
        for file in [
            paths.settings_file(),
            paths.api_config_file(),
            paths.catalog_file(),
            paths.calibration_file(),
        ] {
            assert!(
                file.starts_with(&temp.0),
                "{} escaped the root",
                file.display()
            );
        }
    }

    #[test]
    fn paths_errors_are_actionable() {
        let err = PathsError::NoHomeDirectory;
        assert_eq!(err.code(), "no_home_directory");
        assert!(!err.remedies().is_empty());
    }
}
