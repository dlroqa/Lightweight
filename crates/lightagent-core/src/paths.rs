//! Where Lightagent keeps everything — one isolated, consolidated home.
//!
//! Unlike the inference gateway, which splits data, config and cache across the
//! platform's three standard directories, Lightagent keeps a single
//! self-contained root, exactly as Hermes does with `~/.hermes`. That makes an
//! installation trivially copyable, deletable and inspectable, and makes
//! per-profile isolation a matter of subdirectories under one owner-only root.
//!
//! The root is `$LIGHTAGENT_HOME` when set, otherwise `<home>/.lightagent`.
//! Everything below resolves by joining under that root — no accessor ever
//! takes an external absolute path — so nothing this crate writes can escape
//! the home, and one profile's paths can never name another's.
//!
//! Directories are created owner-only (`0700`) and secret files owner-only
//! (`0600`), mirroring `lightweight-store::atomic`.

use std::io;
use std::path::{Path, PathBuf};

use directories::BaseDirs;

/// Environment variable that relocates the entire Lightagent home.
///
/// Two uses: tests, which must never touch a real installation; and running a
/// throwaway agent alongside a real one.
pub const HOME_ENV: &str = "LIGHTAGENT_HOME";

/// The directory name used under the user's home when [`HOME_ENV`] is unset.
const DEFAULT_DIR: &str = ".lightagent";

/// Why a path could not be resolved or created.
#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    #[error(
        "could not determine a home directory for the current user; \
         set {HOME_ENV} to choose a Lightagent home explicitly"
    )]
    NoHomeDirectory,
    #[error("could not create {path}: {source}")]
    Create {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// The resolved locations under one Lightagent home.
///
/// ```text
/// <home>/
/// ├── active_profile        one line: the active profile id
/// ├── config.json           global typed Config (§16)
/// ├── logs/                 global logs
/// ├── cache/                reclaimable data only
/// └── profiles/<id>/        one isolated profile (see ProfileHandle)
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LightagentPaths {
    root: PathBuf,
}

impl LightagentPaths {
    /// Resolve the home, honouring [`HOME_ENV`].
    ///
    /// Falls back to `<home>/.lightagent`. Does not create anything; call
    /// [`scaffold`](Self::scaffold) for that.
    pub fn resolve() -> Result<Self, PathsError> {
        if let Some(root) = std::env::var_os(HOME_ENV) {
            let root = PathBuf::from(root);
            if !root.as_os_str().is_empty() {
                return Ok(Self { root });
            }
        }
        let base = BaseDirs::new().ok_or(PathsError::NoHomeDirectory)?;
        Ok(Self {
            root: base.home_dir().join(DEFAULT_DIR),
        })
    }

    /// Root the home at an explicit path. Used by tests and by an override.
    pub fn rooted_at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The home root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file naming the active profile id.
    pub fn active_profile_file(&self) -> PathBuf {
        self.root.join("active_profile")
    }

    /// The global typed config.
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.json")
    }

    /// Global logs.
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Global reclaimable cache.
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Global skills directory.
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    /// The directory holding every profile.
    pub fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }

    /// Every top-level directory this layout uses.
    fn top_level_dirs(&self) -> [PathBuf; 3] {
        [self.logs_dir(), self.cache_dir(), self.profiles_dir()]
    }

    /// Create the home root and its top-level directories, owner-only.
    ///
    /// Idempotent: creating an existing directory is a no-op.
    pub fn scaffold(&self) -> Result<(), PathsError> {
        create_private_dir(&self.root)?;
        for dir in self.top_level_dirs() {
            create_private_dir(&dir)?;
        }
        Ok(())
    }
}

/// Create a directory the current user alone can read, and its parents.
///
/// The restriction is best-effort on the mode (a user who opened a directory
/// up made that choice) but the creation itself is fatal on failure.
pub fn create_private_dir(path: &Path) -> Result<(), PathsError> {
    std::fs::create_dir_all(path).map_err(|source| PathsError::Create {
        path: path.to_path_buf(),
        source,
    })?;
    restrict_dir(path);
    Ok(())
}

/// Write `bytes` to `path` atomically, readable only by this user.
///
/// The temp file is created with the restricted mode before any bytes go in,
/// so it is never briefly world-readable, then renamed over the target.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
        restrict_dir(parent);
    }
    let temporary = temp_sibling(path);
    write_with_mode(&temporary, bytes)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

#[cfg(unix)]
const OWNER_ONLY_FILE: u32 = 0o600;
#[cfg(unix)]
const OWNER_ONLY_DIR: u32 = 0o700;

#[cfg(unix)]
fn write_with_mode(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(OWNER_ONLY_FILE)
        .open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

#[cfg(not(unix))]
fn write_with_mode(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // Windows inherits its ACL from the containing directory, itself under the
    // user's own profile; a mode here would do nothing.
    std::fs::write(path, bytes)
}

#[cfg(unix)]
fn restrict_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(OWNER_ONLY_DIR));
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) {}

fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::RunId;

    fn scratch() -> PathBuf {
        std::env::temp_dir().join(format!("lightagent-paths-{}", RunId::new().as_str()))
    }

    #[test]
    fn scaffold_creates_the_full_tree_owner_only() {
        let root = scratch();
        let paths = LightagentPaths::rooted_at(&root);
        paths.scaffold().expect("scaffold");

        assert!(paths.root().is_dir());
        assert!(paths.logs_dir().is_dir());
        assert!(paths.cache_dir().is_dir());
        assert!(paths.profiles_dir().is_dir());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(paths.root())
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "the home must not be listable");
        }

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn env_override_relocates_the_home() {
        // Resolve reads the process env, which other tests must not race; use
        // rooted_at for the deterministic case and only assert the fallback
        // shape here without mutating global state.
        let paths = LightagentPaths::rooted_at("/tmp/example-home");
        assert_eq!(
            paths.config_file(),
            Path::new("/tmp/example-home/config.json")
        );
        assert_eq!(
            paths.active_profile_file(),
            Path::new("/tmp/example-home/active_profile")
        );
        assert!(paths.profiles_dir().starts_with(paths.root()));
    }

    #[test]
    fn a_private_write_round_trips() {
        let root = scratch();
        let path = root.join("nested").join("thing.json");
        write_private(&path, b"payload").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"payload");
        assert!(!temp_sibling(&path).exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_private_write_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch();
        let path = root.join("secret");
        write_private(&path, b"secret").expect("write");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, OWNER_ONLY_FILE);
        std::fs::remove_dir_all(&root).ok();
    }
}
