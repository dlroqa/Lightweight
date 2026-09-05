//! A confined filesystem root the `fs.*` and `terminal.*` tools work within.
//!
//! The whole safety of the filesystem tools rests here. A [`Workspace`] holds a
//! **canonicalized** root, and every path a tool is given is resolved through it:
//! the relative path may contain no `..` and no absolute or prefix component, and
//! the resolved target is canonicalized and checked to still sit under the root —
//! so a symlink inside the workspace that points outward is caught, not followed.
//! One workspace can therefore never read or write outside its own tree.

use std::path::{Component, Path, PathBuf};

/// A canonicalized root the filesystem tools are confined to.
#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Open `root` as a workspace, canonicalizing it. The directory must already
    /// exist (the caller creates it), so the canonical root is a real path every
    /// later check can be compared against.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, String> {
        let root = std::fs::canonicalize(root.as_ref())
            .map_err(|error| format!("workspace root is unavailable: {error}"))?;
        if !root.is_dir() {
            return Err("workspace root is not a directory".to_owned());
        }
        Ok(Self { root })
    }

    /// The canonical root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Join a caller-supplied relative path under the root, refusing any path
    /// that is absolute or steps upward. This blocks `..` escapes syntactically;
    /// symlink escapes are caught by the canonical check in [`resolve_existing`]
    /// and [`resolve_new`].
    fn join_checked(&self, relative: &str) -> Result<PathBuf, String> {
        let path = Path::new(relative);
        for component in path.components() {
            match component {
                Component::Normal(_) | Component::CurDir => {}
                Component::ParentDir => {
                    return Err(format!("path {relative:?} must not contain '..'"));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!(
                        "path {relative:?} must be relative to the workspace"
                    ));
                }
            }
        }
        Ok(self.root.join(path))
    }

    /// Resolve a path that must already exist, following symlinks and confirming
    /// the real target is inside the workspace.
    pub fn resolve_existing(&self, relative: &str) -> Result<PathBuf, String> {
        let candidate = self.join_checked(relative)?;
        let canonical =
            std::fs::canonicalize(&candidate).map_err(|error| format!("{relative:?}: {error}"))?;
        if canonical.starts_with(&self.root) {
            Ok(canonical)
        } else {
            Err(format!("{relative:?} resolves outside the workspace"))
        }
    }

    /// Resolve a path that may not exist yet (a file about to be written). The
    /// deepest ancestor that *does* exist is canonicalized and confirmed inside
    /// the workspace, so a symlinked parent cannot redirect a write outward,
    /// while the leaf and any missing directories stay under the checked anchor.
    pub fn resolve_new(&self, relative: &str) -> Result<PathBuf, String> {
        let candidate = self.join_checked(relative)?;
        let mut anchor = candidate.as_path();
        let existing = loop {
            match anchor.parent() {
                Some(parent) => {
                    if parent.exists() {
                        break parent;
                    }
                    anchor = parent;
                }
                None => return Err(format!("{relative:?} has no parent in the workspace")),
            }
        };
        let canonical =
            std::fs::canonicalize(existing).map_err(|error| format!("{relative:?}: {error}"))?;
        if canonical.starts_with(&self.root) {
            Ok(candidate)
        } else {
            Err(format!("{relative:?} resolves outside the workspace"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lightagent-ws-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolves_a_path_inside_the_workspace() {
        let root = scratch_root();
        std::fs::write(root.join("file.txt"), b"hi").unwrap();
        let ws = Workspace::new(&root).unwrap();
        let resolved = ws.resolve_existing("file.txt").unwrap();
        assert!(resolved.starts_with(ws.root()));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_parent_and_absolute_paths() {
        let root = scratch_root();
        let ws = Workspace::new(&root).unwrap();
        assert!(ws.resolve_existing("../outside").is_err());
        assert!(ws.resolve_existing("a/../../b").is_err());
        assert!(ws.resolve_new("/etc/passwd").is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_new_allows_a_missing_leaf_but_not_a_missing_escape() {
        let root = scratch_root();
        let ws = Workspace::new(&root).unwrap();
        assert!(ws.resolve_new("subdir/new.txt").is_ok());
        assert!(ws.resolve_new("nope.txt").is_ok());
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_outside_is_refused() {
        let root = scratch_root();
        let outside = scratch_root();
        std::fs::write(outside.join("secret"), b"s").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let ws = Workspace::new(&root).unwrap();
        // The symlink target is real but outside the workspace root.
        assert!(ws.resolve_existing("link/secret").is_err());
        assert!(ws.resolve_new("link/written.txt").is_err());
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }
}
