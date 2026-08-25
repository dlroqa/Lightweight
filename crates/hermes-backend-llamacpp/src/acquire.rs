//! Fetching and installing the pinned engine.
//!
//! The engine is downloaded once, verified against the digest recorded in
//! [`crate::manifest`], and extracted into the cache directory. It is a binary
//! we fetch over the network and then execute, so verification happens **before
//! anything is extracted**, not after it has already run.
//!
//! Downloads resume. For a 16 MB engine that is a convenience; the same code
//! path serves multi-gigabyte model downloads, where it is not. Resuming
//! re-hashes the bytes already on disk rather than trusting them, because a
//! partial file left behind by a previous run has no provenance at all.
//!
//! That transfer now lives in [`hermes_download`], because the model catalog
//! needs exactly the same guarantees and two copies of an integrity check
//! diverge. What stays here is what is specific to an *engine*: which artifact
//! this platform needs, unpacking an archive whose entries are
//! attacker-controlled, and marking the result executable.

use std::path::{Path, PathBuf};

use hermes_download::{DownloadError, Fetch};
use hermes_inference::{BackendError, LoadProgress};
use hermes_observability::targets;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::manifest::{self, ArchiveFormat, RuntimeArtifact};

/// `ENOSPC` on Unix, `ERROR_DISK_FULL` on Windows.
///
/// `std::io::ErrorKind::StorageFull` is still unstable, so the raw code is
/// matched instead. Getting this right is what turns "download failed" into
/// section 27's actionable "not enough disk space".
const DISK_FULL_ERRNO: i32 = if cfg!(windows) { 112 } else { 28 };

/// Installs and locates the engine.
#[derive(Debug, Clone)]
pub struct RuntimeInstaller {
    root: PathBuf,
    client: reqwest::Client,
}

impl RuntimeInstaller {
    /// `root` is where engine builds are installed, normally the cache
    /// directory: a verified, re-downloadable artifact, so losing it costs a
    /// download and nothing else.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, BackendError> {
        let client = hermes_download::client(concat!("hermes-gateway/", env!("CARGO_PKG_VERSION")))
            .map_err(as_backend_error)?;
        Ok(Self {
            root: root.into(),
            client,
        })
    }

    /// The artifact this machine needs.
    pub fn artifact(&self) -> Result<&'static RuntimeArtifact, BackendError> {
        manifest::for_this_platform().ok_or(BackendError::UnsupportedPlatform {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        })
    }

    /// Where the server executable lives once installed.
    pub fn server_path(&self) -> Result<PathBuf, BackendError> {
        let artifact = self.artifact()?;
        Ok(self
            .root
            .join(artifact.install_dir_name())
            .join(manifest::server_executable()))
    }

    /// Whether the engine is already installed.
    pub fn is_installed(&self) -> bool {
        self.server_path().is_ok_and(|path| path.is_file())
    }

    /// Ensure the engine is installed, downloading it if necessary.
    ///
    /// Returns the path to the server executable. Doing nothing when it is
    /// already present is the common case, and costs one `stat`.
    pub async fn ensure(
        &self,
        progress: &mpsc::Sender<LoadProgress>,
        cancel: &CancellationToken,
    ) -> Result<PathBuf, BackendError> {
        let server = self.server_path()?;
        if server.is_file() {
            return Ok(server);
        }

        let artifact = self.artifact()?;
        let install_dir = self.root.join(artifact.install_dir_name());
        let archive = self.root.join(artifact.asset);

        tracing::info!(
            target: targets::BACKEND,
            build = manifest::PINNED_BUILD,
            asset = artifact.asset,
            "downloading the inference engine"
        );

        self.download(artifact, &archive, progress, cancel).await?;

        let _ = progress.try_send(LoadProgress::VerifyingRuntime);

        // Decompressing and unpacking 16 MB is CPU-bound, blocking work. Left
        // on the executor it stalls every other task on the runtime - and on a
        // current-thread runtime, which is what `#[tokio::test]` builds, that
        // includes the task draining progress. This deadlocked the integration
        // tests before it was moved off.
        let (extract_archive, extract_dir) = (archive.clone(), install_dir.clone());
        tokio::task::spawn_blocking(move || extract(artifact, &extract_archive, &extract_dir))
            .await
            .map_err(|err| BackendError::RuntimeDownloadFailed {
                reason: format!("the extraction task failed: {err}"),
            })??;

        // The archive is a re-downloadable intermediate; keeping it would
        // double the disk cost of every installed engine.
        let _ = tokio::fs::remove_file(&archive).await;

        if !server.is_file() {
            return Err(BackendError::RuntimeMissing { path: server });
        }
        make_executable(&install_dir)?;

        tracing::info!(
            target: targets::BACKEND,
            path = %server.display(),
            "inference engine installed"
        );
        Ok(server)
    }

    /// Download the artifact, resuming a previous attempt when one is present.
    ///
    /// The transfer itself is [`hermes_download::fetch`]; what is added here is
    /// the engine's vocabulary — its digest, its size, and errors a caller of
    /// this crate can act on.
    async fn download(
        &self,
        artifact: &RuntimeArtifact,
        destination: &Path,
        progress: &mpsc::Sender<LoadProgress>,
        cancel: &CancellationToken,
    ) -> Result<(), BackendError> {
        // Never `send().await`. Progress is advisory: if nobody is draining it,
        // or the channel is momentarily full, dropping an update is correct and
        // blocking the download is not.
        let sink = move |downloaded: u64, total: Option<u64>| {
            let _ = progress.try_send(LoadProgress::AcquiringRuntime { downloaded, total });
        };

        hermes_download::fetch(
            &self.client,
            Fetch {
                url: &artifact.url(),
                destination,
                expected_sha256: Some(artifact.sha256),
                total_size: Some(artifact.size),
                what: "the inference engine",
            },
            &sink,
            cancel,
        )
        .await
        .map_err(as_backend_error)?;
        Ok(())
    }
}

/// Report a download failure in this crate's own vocabulary.
///
/// The engine is not "a file": a caller recovering from `RuntimeCorrupt` has to
/// know it was the *engine* that failed verification, and section 27's taxonomy
/// names it that way.
fn as_backend_error(err: DownloadError) -> BackendError {
    match err {
        DownloadError::Corrupt { expected, actual } => {
            BackendError::RuntimeCorrupt { expected, actual }
        }
        DownloadError::LowDisk { needed, .. } => BackendError::LowDisk {
            needed,
            available: 0,
        },
        DownloadError::Io { context, source } => BackendError::io(context, source),
        DownloadError::Cancelled => BackendError::Cancelled,
        other @ (DownloadError::Failed { .. } | DownloadError::InsecureUrl { .. }) => {
            BackendError::RuntimeDownloadFailed {
                reason: other.to_string(),
            }
        }
    }
}

/// Unpack an archive into `install_dir`.
fn extract(
    artifact: &RuntimeArtifact,
    archive: &Path,
    install_dir: &Path,
) -> Result<(), BackendError> {
    std::fs::create_dir_all(install_dir)
        .map_err(|err| map_io("creating the engine directory", err))?;

    match artifact.format {
        ArchiveFormat::TarGz => extract_tar_gz(archive, install_dir),
        ArchiveFormat::Zip => extract_zip(archive, install_dir),
    }
}

fn extract_tar_gz(archive: &Path, install_dir: &Path) -> Result<(), BackendError> {
    // Two passes: the first learns whether every entry sits under one shared
    // top-level directory, the second extracts with that prefix removed. The
    // release archives are built with `--transform "s,^\.,llama-<build>,"`, so
    // everything is inside `llama-b10590/` and a naive extraction buries the
    // server one level deeper than expected. That is not a detail we can
    // hard-code, though - the Windows archive is packed differently - so it is
    // detected rather than assumed.
    let strip = common_prefix(&tar_entry_names(archive)?);

    let file = std::fs::File::open(archive).map_err(|err| map_io("opening the archive", err))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));

    let entries = tar
        .entries()
        .map_err(|err| map_io("reading the archive", err))?;
    for entry in entries {
        let mut entry = entry.map_err(|err| map_io("reading an archive entry", err))?;
        let raw = entry
            .path()
            .map_err(|err| map_io("reading an archive entry name", err))?
            .into_owned();
        let path = strip_prefix(&raw, strip.as_deref());
        // A stripped entry can become empty: that is the top-level directory
        // itself, which needs no extraction.
        if path.as_os_str().is_empty() {
            continue;
        }
        let Some(target) = safe_join(install_dir, &path) else {
            return Err(BackendError::RuntimeDownloadFailed {
                reason: format!(
                    "archive entry {} escapes the install directory",
                    path.display()
                ),
            });
        };
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| map_io("creating an archive directory", err))?;
        }
        entry
            .unpack(&target)
            .map_err(|err| map_io("extracting an archive entry", err))?;
    }
    Ok(())
}

fn extract_zip(archive: &Path, install_dir: &Path) -> Result<(), BackendError> {
    let file = std::fs::File::open(archive).map_err(|err| map_io("opening the archive", err))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|err| BackendError::RuntimeDownloadFailed {
            reason: format!("could not read the archive: {err}"),
        })?;

    let names: Vec<PathBuf> = (0..zip.len())
        .filter_map(|index| zip.by_index(index).ok()?.enclosed_name())
        .collect();
    let strip = common_prefix(&names);

    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|err| BackendError::RuntimeDownloadFailed {
                reason: format!("could not read an archive entry: {err}"),
            })?;
        // `enclosed_name` already rejects absolute paths and `..`, but the
        // result is joined through `safe_join` as well rather than relying on a
        // single check for something this consequential.
        let Some(raw) = entry.enclosed_name() else {
            return Err(BackendError::RuntimeDownloadFailed {
                reason: format!("archive entry {} has an unsafe name", entry.name()),
            });
        };
        let name = strip_prefix(&raw, strip.as_deref());
        if name.as_os_str().is_empty() {
            continue;
        }
        let Some(target) = safe_join(install_dir, &name) else {
            return Err(BackendError::RuntimeDownloadFailed {
                reason: format!(
                    "archive entry {} escapes the install directory",
                    name.display()
                ),
            });
        };

        if entry.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|err| map_io("creating an archive directory", err))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| map_io("creating an archive directory", err))?;
        }
        let mut out = std::fs::File::create(&target)
            .map_err(|err| map_io("writing an archive entry", err))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|err| map_io("extracting an archive entry", err))?;
    }
    Ok(())
}

/// The names of every entry in a tar archive.
fn tar_entry_names(archive: &Path) -> Result<Vec<PathBuf>, BackendError> {
    let file = std::fs::File::open(archive).map_err(|err| map_io("opening the archive", err))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let entries = tar
        .entries()
        .map_err(|err| map_io("reading the archive", err))?;
    Ok(entries
        .flatten()
        .filter_map(|entry| entry.path().ok().map(|path| path.into_owned()))
        .collect())
}

/// The single top-level directory every entry shares, if there is one.
///
/// Returns `None` when entries live at the archive root or under more than one
/// directory, so nothing is stripped in the cases where stripping would lose
/// files.
fn common_prefix(names: &[PathBuf]) -> Option<std::ffi::OsString> {
    let mut prefix: Option<std::ffi::OsString> = None;
    let mut saw_entry = false;

    for name in names {
        let mut components = name.components();
        let first = match components.next() {
            Some(std::path::Component::Normal(part)) => part.to_os_string(),
            // An entry at the root, or an unusual component: nothing to strip.
            _ => return None,
        };
        // A lone top-level entry with nothing beneath it is a file at the
        // root, not a wrapping directory.
        if components.next().is_none() && !name.to_string_lossy().ends_with('/') {
            return None;
        }
        saw_entry = true;
        match &prefix {
            Some(existing) if *existing != first => return None,
            Some(_) => {}
            None => prefix = Some(first),
        }
    }
    saw_entry.then_some(prefix).flatten()
}

/// Remove `prefix` from the front of `path`, if present.
fn strip_prefix(path: &Path, prefix: Option<&std::ffi::OsStr>) -> PathBuf {
    match prefix {
        Some(prefix) => path.strip_prefix(prefix).unwrap_or(path).to_path_buf(),
        None => path.to_path_buf(),
    }
}

/// Join an archive-supplied path under `root`, or refuse it.
///
/// Archive entries are attacker-controlled data. An entry named `../../.ssh/`
/// or `/etc/cron.d/x` would otherwise write outside the install directory —
/// the "zip slip" family of bugs. Every component is checked rather than the
/// string being scanned for `..`, which is easy to fool.
fn safe_join(root: &Path, entry: &Path) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for component in entry.components() {
        match component {
            std::path::Component::Normal(part) => out.push(part),
            // A leading `./` is harmless and common.
            std::path::Component::CurDir => {}
            // Anything else - `..`, a root, a Windows prefix - is refused
            // outright rather than normalised, because normalising is where
            // these bugs come from.
            _ => return None,
        }
    }
    (out != root).then_some(out)
}

/// Mark the extracted binaries executable.
///
/// tar preserves the mode, but a zip built on Windows carries none, so the
/// files come out unreadable as programs. Setting it unconditionally on Unix
/// is simpler than caring which archive it came from.
#[cfg(unix)]
fn make_executable(install_dir: &Path) -> Result<(), BackendError> {
    use std::os::unix::fs::PermissionsExt;

    let entries = std::fs::read_dir(install_dir)
        .map_err(|err| map_io("listing the engine directory", err))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Only the programs. Shared objects and headers do not need it.
        let is_program = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("llama-") && !name.contains('.'));
        if !is_program {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let mut permissions = metadata.permissions();
        permissions.set_mode(permissions.mode() | 0o755);
        let _ = std::fs::set_permissions(&path, permissions);
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_install_dir: &Path) -> Result<(), BackendError> {
    // Windows decides by file extension, so there is nothing to set.
    Ok(())
}

/// Turn an I/O error into the most specific backend error available.
fn map_io(context: &str, err: std::io::Error) -> BackendError {
    if err.raw_os_error() == Some(DISK_FULL_ERRNO) {
        // Section 27 lists insufficient disk space as something that must be
        // reported actionably rather than surfacing as a generic write failure.
        return BackendError::LowDisk {
            needed: 0,
            available: 0,
        };
    }
    BackendError::io(context.to_owned(), err)
}

#[cfg(test)]
mod tests {
    use super::*;
    // `code()` is an `Actionable` method, used only by these assertions.
    use hermes_core::Actionable;
    use std::io::Write;

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
                "hermes-acquire-{tag}-{}-{unique}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ---- archive entries are attacker-controlled data ----

    #[test]
    fn ordinary_archive_entries_land_inside_the_install_directory() {
        let root = Path::new("/install");
        assert_eq!(
            safe_join(root, Path::new("llama-b10590/llama-server")),
            Some(PathBuf::from("/install/llama-b10590/llama-server"))
        );
        // A leading "./" is common and harmless.
        assert_eq!(
            safe_join(root, Path::new("./libggml.so")),
            Some(PathBuf::from("/install/libggml.so"))
        );
    }

    #[test]
    fn an_entry_that_climbs_out_of_the_directory_is_refused() {
        // "Zip slip": an archive entry that writes outside the extraction
        // directory. Refused rather than normalised, because normalising is
        // where these bugs come from.
        let root = Path::new("/install");
        assert_eq!(safe_join(root, Path::new("../evil")), None);
        assert_eq!(safe_join(root, Path::new("a/../../evil")), None);
        assert_eq!(
            safe_join(root, Path::new("../../.ssh/authorized_keys")),
            None
        );
    }

    #[test]
    fn an_absolute_entry_is_refused() {
        assert_eq!(
            safe_join(Path::new("/install"), Path::new("/etc/passwd")),
            None
        );
    }

    #[test]
    fn an_empty_entry_is_refused() {
        // Would otherwise resolve to the install directory itself and try to
        // overwrite it with a file.
        assert_eq!(safe_join(Path::new("/install"), Path::new("")), None);
        assert_eq!(safe_join(Path::new("/install"), Path::new(".")), None);
    }

    // Resuming a partial download is exercised in `hermes-download`, which
    // owns that code now.

    // ---- extraction ----

    #[test]
    fn a_tar_gz_extracts_and_its_programs_become_executable() {
        let temp = TempDir::new("targz");
        let archive_path = temp.0.join("engine.tar.gz");

        // Built here rather than committed, so the test needs no fixture file.
        let file = std::fs::File::create(&archive_path).expect("create");
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        {
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            // Deliberately not executable, as a Windows-built archive would be.
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "llama-server", &b"fake"[..])
                .expect("append");
            builder.finish().expect("finish");
        }

        let artifact = crate::manifest::for_platform("linux", "x86_64").expect("artifact");
        let install = temp.0.join("install");
        extract(artifact, &archive_path, &install).expect("extract");

        let server = install.join("llama-server");
        assert!(server.is_file(), "llama-server was not extracted");

        make_executable(&install).expect("chmod");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&server)
                .expect("metadata")
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0, "the engine was not made executable");
        }
    }

    /// Build a tar archive containing one entry with an arbitrary name.
    ///
    /// Written byte by byte because the `tar` crate's *builder* refuses to
    /// emit a path containing `..` - which is good of it, but means it cannot
    /// produce the archive an attacker would. The reader is what we are
    /// testing, so the archive has to be forged the way a real one would be.
    fn forge_tar(entry_name: &str, contents: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];

        let write = |header: &mut [u8; 512], at: usize, bytes: &[u8]| {
            for (index, byte) in bytes.iter().enumerate() {
                if let Some(slot) = header.get_mut(at + index) {
                    *slot = *byte;
                }
            }
        };

        write(&mut header, 0, entry_name.as_bytes()); // name
        write(&mut header, 100, b"0000644\0"); // mode
        write(&mut header, 108, b"0000000\0"); // uid
        write(&mut header, 116, b"0000000\0"); // gid
        write(
            &mut header,
            124,
            format!("{:011o}\0", contents.len()).as_bytes(),
        );
        write(&mut header, 136, b"00000000000\0"); // mtime
        write(&mut header, 156, b"0"); // regular file
        write(&mut header, 257, b"ustar\x0000"); // magic + version

        // The checksum is computed with the checksum field itself read as
        // spaces, then written back as octal.
        write(&mut header, 148, b"        ");
        let checksum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        write(&mut header, 148, format!("{checksum:06o}\0 ").as_bytes());

        let mut archive = header.to_vec();
        archive.extend_from_slice(contents);
        // Pad the data to a 512-byte boundary, then append the end-of-archive
        // marker: two zero blocks.
        let padding = (512 - contents.len() % 512) % 512;
        archive.extend(std::iter::repeat_n(0u8, padding + 1024));
        archive
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(bytes).expect("compress");
        encoder.finish().expect("finish")
    }

    #[test]
    fn a_forged_archive_is_still_readable_so_the_test_below_is_meaningful() {
        // Guards the test helper itself: if the forged tar were malformed, the
        // escape test would pass for the wrong reason.
        let temp = TempDir::new("forgeok");
        let archive_path = temp.0.join("ok.tar.gz");
        std::fs::write(&archive_path, gzip(&forge_tar("llama-server", b"fake"))).expect("write");

        let artifact = crate::manifest::for_platform("linux", "x86_64").expect("artifact");
        let install = temp.0.join("install");
        extract(artifact, &archive_path, &install).expect("a well-formed forged tar extracts");
        assert!(install.join("llama-server").is_file());
    }

    #[test]
    fn an_archive_entry_escaping_the_directory_aborts_extraction() {
        // "Zip slip", end to end rather than only against `safe_join`.
        let temp = TempDir::new("evil");
        let archive_path = temp.0.join("evil.tar.gz");
        std::fs::write(&archive_path, gzip(&forge_tar("../escaped", b"evil"))).expect("write");

        let artifact = crate::manifest::for_platform("linux", "x86_64").expect("artifact");
        let install = temp.0.join("install");
        let result = extract(artifact, &archive_path, &install);

        assert!(result.is_err(), "an escaping entry was extracted");
        assert!(
            !temp.0.join("escaped").exists(),
            "a file was written outside the install directory"
        );
    }

    // ---- installer plumbing ----

    #[test]
    fn the_installer_reports_where_the_engine_will_live() {
        let temp = TempDir::new("paths");
        let installer = RuntimeInstaller::new(&temp.0).expect("installer");

        let server = installer.server_path().expect("path");
        assert!(server.starts_with(&temp.0));
        assert!(server.ends_with(crate::manifest::server_executable()));
        // Nothing has been installed yet.
        assert!(!installer.is_installed());
    }

    #[test]
    fn the_installer_targets_this_platform() {
        let temp = TempDir::new("artifact");
        let installer = RuntimeInstaller::new(&temp.0).expect("installer");
        let artifact = installer.artifact().expect("this platform is supported");
        assert_eq!(artifact.os, std::env::consts::OS);
        assert_eq!(artifact.arch, std::env::consts::ARCH);
    }

    #[test]
    fn a_disk_full_error_is_reported_as_such_rather_than_as_a_generic_failure() {
        // Section 27 lists insufficient disk space as needing an actionable
        // error of its own.
        let err = map_io(
            "writing",
            std::io::Error::from_raw_os_error(DISK_FULL_ERRNO),
        );
        assert_eq!(err.code(), "low_disk");

        let other = map_io("writing", std::io::Error::other("something else"));
        assert_eq!(other.code(), "io_error");
    }
}
