//! Where the CLIs live on disk, and replacing them safely.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::{Cli, Target};

const REPOSITORY: &str = "https://github.com/dlroqa/Lightweight.git";

/// Why the binary at `executable` must not be replaced in place, if it must not.
///
/// Both refusals are read from the path, not the environment: a desktop app
/// ships its CLIs in `resources/bin` beside the app it was released with
/// (`apps/desktop/package.json` `extraResources`), and a Cargo `target/`
/// directory holds a development build that the next `cargo build` overwrites.
pub(crate) fn managed_install(executable: &Path) -> Option<String> {
    let names: Vec<String> = executable
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    let dirs = names.get(..names.len().saturating_sub(1)).unwrap_or(&[]);
    if let [.., resources, bin] = dirs
        && resources == "resources"
        && bin == "bin"
    {
        return Some(format!(
            "{} is bundled with the Lightweight desktop app; update the desktop app instead",
            executable.display()
        ));
    }
    if dirs
        .windows(2)
        .any(|pair| pair[0] == "target" && matches!(pair[1].as_str(), "debug" | "release"))
    {
        return Some(format!(
            "{} is a development build in a Cargo target directory; rebuild it from source instead",
            executable.display()
        ));
    }
    None
}

/// The directory the CLIs are installed in.
///
/// `LIGHTAGENT_INSTALL_ROOT` names a Cargo-style root whose `bin` directory is
/// used; otherwise it is the directory holding the running executable.
pub(crate) fn install_dir(executable: &Path) -> Result<PathBuf, String> {
    if let Some(root) = std::env::var_os("LIGHTAGENT_INSTALL_ROOT")
        && !root.is_empty()
    {
        return Ok(PathBuf::from(root).join("bin"));
    }
    executable.parent().map(Path::to_path_buf).ok_or_else(|| {
        format!(
            "could not determine the directory of {}",
            executable.display()
        )
    })
}

pub(crate) fn binary_path(dir: &Path, cli: Cli) -> PathBuf {
    dir.join(format!("{}{}", cli.binary(), std::env::consts::EXE_SUFFIX))
}

/// The version an installed binary reports through `--version`, or `None` when
/// it cannot be run or answers in an unexpected form.
pub(crate) async fn installed_version(path: &Path, cli: Cli) -> Option<String> {
    let mut command = Command::new(path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_version_output(&String::from_utf8_lossy(&output.stdout), cli)
}

/// Parse clap's `--version` line, `<binary> <version>`.
fn parse_version_output(stdout: &str, cli: Cli) -> Option<String> {
    let version = stdout
        .trim()
        .strip_prefix(cli.binary())?
        .strip_prefix(' ')?
        .trim();
    (!version.is_empty() && !version.contains(char::is_whitespace)).then(|| version.to_owned())
}

/// The file name a release archive for `cli` at `version` has on this target.
pub(crate) fn archive_name(cli: Cli, version: &str) -> String {
    format!("{}.{}", archive_stem(cli, version), archive_extension())
}

/// The archive's top-level directory, as `scripts/package-cli.sh` and
/// `scripts/package-lightagent.sh` name it.
fn archive_stem(cli: Cli, version: &str) -> String {
    format!(
        "{}-{version}-{}",
        cli.binary(),
        env!("RELEASE_UPDATE_TARGET")
    )
}

fn archive_extension() -> &'static str {
    if env!("RELEASE_UPDATE_TARGET").contains("windows") {
        "zip"
    } else {
        "tar.gz"
    }
}

/// A scratch directory beside the installed binaries, so the final rename stays
/// on one filesystem. Removed when dropped.
pub(crate) struct Staging {
    dir: PathBuf,
}

impl Staging {
    pub(crate) fn create(install_dir: &Path, cli: Cli) -> Result<Self, String> {
        let dir = install_dir.join(format!(".{}-update-{}", cli.binary(), std::process::id()));
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
        Ok(Self { dir })
    }

    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        // Cleanup of an already-finished update; a leftover directory is
        // harmless and a destructor has no caller to report to.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Extract exactly the `<stem>/<binary>` entry from a release archive into
/// `destination`, marked executable.
pub(crate) fn extract_binary(
    archive: &Path,
    cli: Cli,
    version: &str,
    destination: &Path,
) -> Result<(), String> {
    let entry = format!(
        "{}/{}{}",
        archive_stem(cli, version),
        cli.binary(),
        std::env::consts::EXE_SUFFIX
    );
    let file = std::fs::File::open(archive)
        .map_err(|error| format!("could not open {}: {error}", archive.display()))?;
    let mut output = create_executable(destination)?;
    let found = if archive_extension() == "zip" {
        let mut zip = zip::ZipArchive::new(file)
            .map_err(|error| format!("could not read {}: {error}", archive.display()))?;
        match zip.by_name(&entry) {
            Ok(mut member) => copy(&mut member, &mut output, destination).map(|()| true),
            Err(zip::result::ZipError::FileNotFound) => Ok(false),
            Err(error) => Err(format!("could not read {}: {error}", archive.display())),
        }
    } else {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        let entries = tar
            .entries()
            .map_err(|error| format!("could not read {}: {error}", archive.display()))?;
        let mut found = false;
        for member in entries {
            let mut member =
                member.map_err(|error| format!("could not read {}: {error}", archive.display()))?;
            let path = member
                .path()
                .map_err(|error| format!("could not read {}: {error}", archive.display()))?;
            if path == Path::new(&entry) {
                copy(&mut member, &mut output, destination)?;
                found = true;
                break;
            }
        }
        Ok(found)
    }?;
    if !found {
        return Err(format!("{} does not contain {entry}", archive.display()));
    }
    output
        .sync_all()
        .map_err(|error| format!("could not write {}: {error}", destination.display()))
}

fn create_executable(path: &Path) -> Result<std::fs::File, String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o755);
    }
    options
        .open(path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))
}

fn copy(reader: &mut impl Read, writer: &mut impl Write, destination: &Path) -> Result<(), String> {
    std::io::copy(reader, writer)
        .map(|_| ())
        .map_err(|error| format!("could not write {}: {error}", destination.display()))
}

/// Confirm a binary runs and reports the release it was built or downloaded as.
pub(crate) async fn verify_staged(path: &Path, cli: Cli, version: &str) -> Result<(), String> {
    match installed_version(path, cli).await {
        Some(reported) if reported == version => Ok(()),
        Some(reported) => Err(format!(
            "the downloaded {} reports version {reported}, expected {version}",
            cli.binary()
        )),
        None => Err(format!(
            "the downloaded {} did not run on this machine",
            cli.binary()
        )),
    }
}

/// The binaries replaced so far in one update, with the previous copy of each
/// kept aside until the whole update has succeeded.
///
/// CLIs are updated one after another, never interleaved. Keeping every earlier
/// CLI's previous binary until the last one is in place is what lets a failure
/// part-way restore all of them, so the two CLIs never end up on different
/// releases.
#[derive(Default)]
pub(crate) struct Transaction {
    /// Each installed path, and where its previous binary was set aside (`None`
    /// when nothing was installed there before).
    applied: Vec<(PathBuf, Option<PathBuf>)>,
}

impl Transaction {
    /// Move a verified `staged` binary into `target`.
    ///
    /// The installed file is renamed aside first, which works for a running
    /// executable on every supported platform, then the staged file is renamed
    /// in. Both renames stay within one directory, so neither copies bytes.
    pub(crate) fn install_staged(&mut self, staged: &Path, target: &Path) -> Result<(), String> {
        let backup = if target.exists() {
            let backup = clear_backup(target)?;
            std::fs::rename(target, &backup)
                .map_err(|error| format!("could not move {} aside: {error}", target.display()))?;
            Some(backup)
        } else {
            None
        };
        if let Err(error) = std::fs::rename(staged, target) {
            let mut message = format!("could not install {}: {error}", target.display());
            if let Err(restore) = restore(target, backup.as_deref()) {
                message.push_str(&format!("; {restore}"));
            }
            return Err(message);
        }
        self.applied.push((target.to_path_buf(), backup));
        Ok(())
    }

    /// Copy `target` aside before a tool that overwrites it in place — Cargo —
    /// rebuilds it, so a later failure can still restore it.
    pub(crate) fn preserve(&mut self, target: &Path) -> Result<(), String> {
        let backup = if target.exists() {
            let backup = clear_backup(target)?;
            std::fs::copy(target, &backup)
                .map_err(|error| format!("could not back up {}: {error}", target.display()))?;
            Some(backup)
        } else {
            None
        };
        self.applied.push((target.to_path_buf(), backup));
        Ok(())
    }

    /// Put every replaced binary back, newest first. Returns what could not be
    /// restored, for the caller to report.
    pub(crate) fn roll_back(self) -> Vec<String> {
        self.applied
            .iter()
            .rev()
            .filter_map(|(target, backup)| restore(target, backup.as_deref()).err())
            .collect()
    }

    /// Keep the new binaries and remove the set-aside copies. Returns the copies
    /// that could not be removed — on Windows a running executable cannot be
    /// deleted — which the next update clears.
    pub(crate) fn commit(self) -> Vec<PathBuf> {
        self.applied
            .into_iter()
            .filter_map(|(_, backup)| backup)
            .filter(|backup| std::fs::remove_file(backup).is_err())
            .collect()
    }
}

fn backup_path(target: &Path) -> PathBuf {
    let mut name = OsString::from(target.as_os_str());
    name.push(".old");
    PathBuf::from(name)
}

/// The backup path for `target`, emptied of a copy left by an earlier update
/// whose binary was still running then.
fn clear_backup(target: &Path) -> Result<PathBuf, String> {
    let backup = backup_path(target);
    match std::fs::remove_file(&backup) {
        Ok(()) => Ok(backup),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(backup),
        Err(error) => Err(format!(
            "could not remove the previous backup {}: {error}",
            backup.display()
        )),
    }
}

fn restore(target: &Path, backup: Option<&Path>) -> Result<(), String> {
    match backup {
        Some(backup) => std::fs::rename(backup, target).map_err(|error| {
            format!(
                "could not restore {} from {}: {error}",
                target.display(),
                backup.display()
            )
        }),
        None => match std::fs::remove_file(target) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("could not remove {}: {error}", target.display())),
        },
    }
}

/// Build and install `cli` from the release tag with Cargo, for a target that
/// has no published archive.
pub(crate) async fn cargo_install(
    install_dir: &Path,
    target: &Target,
    tag: &str,
    transaction: &mut Transaction,
) -> Result<(), String> {
    let root = cargo_root(install_dir)?;
    let cli = target.cli;
    transaction.preserve(&target.path)?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args([
            "install", "--git", REPOSITORY, "--tag", tag, "--locked", "--force",
        ])
        .arg("--root")
        .arg(root)
        .arg(cli.package())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .map_err(|error| {
            format!("could not run Cargo: {error}. Install Rust from https://rustup.rs, then retry")
        })?;
    if !status.success() {
        return Err(format!("Cargo could not install {} {tag}", cli.binary()));
    }
    verify_staged(&target.path, cli, tag.trim_start_matches('v')).await
}

/// Cargo installs into `<root>/bin`, so the install directory must be a `bin`.
fn cargo_root(install_dir: &Path) -> Result<&Path, String> {
    if install_dir.file_name().is_some_and(|name| name == "bin")
        && let Some(root) = install_dir.parent()
    {
        return Ok(root);
    }
    Err(format!(
        "{} is not a `bin` directory Cargo can install into; set LIGHTAGENT_INSTALL_ROOT to a Cargo root",
        install_dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "release-update-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn desktop_bundled_and_development_binaries_are_not_replaced() {
        for path in [
            "/Applications/Lightweight.app/Contents/Resources/bin/lightweight",
            "/tmp/.mount_Lightw/resources/bin/lightagent",
            "C:/Program Files/Lightweight/resources/bin/lightweight.exe",
            "/srv/src/Lightweight/target/debug/lightagent",
            "/srv/src/Lightweight/target/release/lightweight",
        ] {
            assert!(managed_install(Path::new(path)).is_some(), "{path}");
        }
        for path in [
            "/usr/local/bin/lightagent",
            "/opt/cargo/bin/lightweight",
            "/opt/lightweight-0.3.22-x86_64-unknown-linux-gnu/lightweight",
            "/srv/target/lightagent",
        ] {
            assert!(managed_install(Path::new(path)).is_none(), "{path}");
        }
    }

    #[test]
    fn version_output_is_read_only_from_the_expected_binary() {
        assert_eq!(
            parse_version_output("lightagent 0.3.22\n", Cli::Lightagent),
            Some("0.3.22".to_owned())
        );
        assert_eq!(
            parse_version_output("lightweight 0.3.22", Cli::Lightagent),
            None
        );
        assert_eq!(parse_version_output("lightagent", Cli::Lightagent), None);
        assert_eq!(
            parse_version_output("lightagent 0.3 beta", Cli::Lightagent),
            None
        );
    }

    #[test]
    fn archive_names_follow_the_packaging_scripts() {
        let name = archive_name(Cli::Lightweight, "0.3.22");
        assert!(name.starts_with("lightweight-0.3.22-"));
        assert!(name.contains(env!("RELEASE_UPDATE_TARGET")));
        assert!(name.ends_with(".tar.gz") || name.ends_with(".zip"));
    }

    #[test]
    fn cargo_installs_only_into_a_bin_directory() {
        assert_eq!(
            cargo_root(Path::new("/opt/lightagent/bin")).unwrap(),
            Path::new("/opt/lightagent")
        );
        assert!(cargo_root(Path::new("/opt/lightagent")).is_err());
    }

    #[test]
    fn the_binary_is_extracted_from_its_archive_directory_only() {
        if archive_extension() != "tar.gz" {
            return;
        }
        let temp = TempDir::new("extract");
        let stem = archive_stem(Cli::Lightagent, "9.9.9");
        let archive = temp.0.join("archive.tar.gz");
        {
            let encoder = flate2::write::GzEncoder::new(
                std::fs::File::create(&archive).unwrap(),
                flate2::Compression::fast(),
            );
            let mut builder = tar::Builder::new(encoder);
            for (name, contents) in [
                ("lightagent".to_owned(), &b"decoy"[..]),
                (format!("{stem}/README.txt"), &b"readme"[..]),
                (format!("{stem}/lightagent"), &b"binary"[..]),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(contents.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, name, contents).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
        let destination = temp.0.join("lightagent.new");
        extract_binary(&archive, Cli::Lightagent, "9.9.9", &destination).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&destination)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "extracted binary must be executable");
        }

        let missing = extract_binary(&archive, Cli::Lightagent, "1.0.0", &temp.0.join("other"));
        assert!(missing.unwrap_err().contains("does not contain"));
    }

    #[test]
    fn committed_binaries_stay_and_their_old_copies_are_removed() {
        let temp = TempDir::new("commit");
        let mut transaction = Transaction::default();
        for name in ["lightweight", "lightagent"] {
            let target = temp.0.join(name);
            let staged = temp.0.join(format!("{name}.new"));
            std::fs::write(&target, b"old").unwrap();
            std::fs::write(&staged, b"new").unwrap();
            transaction.install_staged(&staged, &target).unwrap();
            assert!(!staged.exists());
        }
        assert!(transaction.commit().is_empty());
        for name in ["lightweight", "lightagent"] {
            let target = temp.0.join(name);
            assert_eq!(std::fs::read(&target).unwrap(), b"new");
            assert!(!backup_path(&target).exists());
        }
    }

    #[test]
    fn a_failure_on_the_second_cli_restores_the_first() {
        let temp = TempDir::new("rollback");
        let first = temp.0.join("lightweight");
        let second = temp.0.join("lightagent");
        std::fs::write(&first, b"old-lightweight").unwrap();
        std::fs::write(temp.0.join("lightweight.new"), b"new").unwrap();
        std::fs::write(&second, b"old-lightagent").unwrap();

        let mut transaction = Transaction::default();
        transaction
            .install_staged(&temp.0.join("lightweight.new"), &first)
            .unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"new");
        // Never staged, so the second CLI fails after the first is in place.
        let error = transaction
            .install_staged(&temp.0.join("lightagent.new"), &second)
            .unwrap_err();
        assert!(error.contains("could not install"));
        assert_eq!(std::fs::read(&second).unwrap(), b"old-lightagent");

        assert!(transaction.roll_back().is_empty());
        assert_eq!(std::fs::read(&first).unwrap(), b"old-lightweight");
        assert!(!backup_path(&first).exists());
        assert!(!backup_path(&second).exists());
    }

    #[test]
    fn a_preserved_binary_is_restored_after_it_was_rebuilt_in_place() {
        let temp = TempDir::new("preserve");
        let target = temp.0.join("lightweight");
        std::fs::write(&target, b"old").unwrap();
        let mut transaction = Transaction::default();
        transaction.preserve(&target).unwrap();
        // What `cargo install --force` does to the installed file.
        std::fs::write(&target, b"rebuilt").unwrap();
        assert!(transaction.roll_back().is_empty());
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
    }
}
