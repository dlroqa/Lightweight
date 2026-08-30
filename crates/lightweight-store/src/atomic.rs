//! Writing a file so that a crash never leaves half of one.
//!
//! The same temp-file-plus-rename the model catalog has used since M6a, kept
//! here rather than shared out of `lightweight-catalog`: that crate is about models,
//! and depending on it to write a settings file would be a dependency that says
//! something untrue about what these files are.
//!
//! What is added over the catalog's version is **permissions**. A conversation
//! is the user's own words. On a shared machine the default mode would leave it
//! world-readable, which is a worse outcome than any this application produces
//! on purpose — the log deliberately redacts prompts, and it would be absurd to
//! redact them there and then write them to a readable file here.

use std::path::{Path, PathBuf};

use crate::error::StoreError;

/// Mode for files that contain what the user wrote: owner only.
#[cfg(unix)]
const OWNER_ONLY_FILE: u32 = 0o600;
/// Mode for the directories holding them: owner only, and not listable by
/// anyone else.
#[cfg(unix)]
const OWNER_ONLY_DIR: u32 = 0o700;

/// Create a directory the current user alone can read.
pub fn create_private_dir(path: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(path)
        .map_err(|err| StoreError::io("creating a data directory", err))?;
    restrict_dir(path);
    Ok(())
}

/// Write `bytes` to `path` atomically, readable only by this user.
///
/// The temp file is created with the restricted mode *before* it is written to,
/// not after: a file that is briefly world-readable while several megabytes of
/// conversation are written into it has already been readable for as long as it
/// takes to read it.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        create_private_dir(parent)?;
    }

    let temporary = temp_sibling(path);
    write_with_mode(&temporary, bytes)?;

    std::fs::rename(&temporary, path).map_err(|err| {
        // Leaving it behind would accumulate one per failed save, and none of
        // them is the file the caller asked for.
        let _ = std::fs::remove_file(&temporary);
        StoreError::io("replacing a data file", err)
    })
}

#[cfg(unix)]
fn write_with_mode(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(OWNER_ONLY_FILE)
        .open(path)
        .map_err(|err| StoreError::io("creating a data file", err))?;
    file.write_all(bytes)
        .map_err(|err| StoreError::io("writing a data file", err))?;
    // Flushed explicitly rather than left to `Drop`, which discards the error
    // and would report a save that did not happen as a save that did.
    file.flush()
        .map_err(|err| StoreError::io("flushing a data file", err))
}

#[cfg(not(unix))]
fn write_with_mode(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    // Windows inherits its ACL from the containing directory, which is under
    // the user's own profile. Setting a mode here would do nothing; the
    // cross-platform milestone is where this gets checked on that platform
    // rather than assumed.
    std::fs::write(path, bytes).map_err(|err| StoreError::io("writing a data file", err))
}

#[cfg(unix)]
fn restrict_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    // Best effort, and deliberately not fatal: a directory the user has
    // deliberately opened up is their choice, and refusing to save their
    // conversation over it would be this crate overruling them about their own
    // machine.
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

    fn scratch(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("hermes-atomic-{tag}-{unique}"))
    }

    #[test]
    fn a_write_replaces_the_whole_file_or_none_of_it() {
        let dir = scratch("replace");
        let path = dir.join("thing.json");
        write_private(&path, b"first").expect("first write");
        assert_eq!(std::fs::read(&path).expect("read"), b"first");

        write_private(&path, b"second").expect("second write");
        assert_eq!(std::fs::read(&path).expect("read"), b"second");

        // No temp file is left lying beside it.
        assert!(!temp_sibling(&path).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn what_the_user_wrote_is_not_readable_by_anyone_else() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = scratch("perms");
        let path = dir.join("conversation.json");
        write_private(&path, b"a private thing").expect("write");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(
            mode & 0o777,
            OWNER_ONLY_FILE,
            "a conversation must not be world-readable"
        );

        let dir_mode = std::fs::metadata(&dir).expect("stat").permissions().mode();
        assert_eq!(
            dir_mode & 0o777,
            OWNER_ONLY_DIR,
            "the directory holding them must not be listable by others"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn the_temp_file_is_never_briefly_readable() {
        use std::os::unix::fs::PermissionsExt as _;

        // The mode is set at creation rather than after writing. Checked by
        // creating the temp file directly, because the window it closes is too
        // short to observe from outside.
        let dir = scratch("temp-mode");
        create_private_dir(&dir).expect("dir");
        let temporary = dir.join("x.tmp");
        write_with_mode(&temporary, b"secret").expect("write");

        let mode = std::fs::metadata(&temporary)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, OWNER_ONLY_FILE);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_parent_directory_is_created_when_it_is_missing() {
        let dir = scratch("nested");
        let path = dir.join("a").join("b").join("thing.json");
        write_private(&path, b"deep").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"deep");
        std::fs::remove_dir_all(&dir).ok();
    }
}
