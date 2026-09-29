//! The self-update command for the `lightweight` CLI.
//!
//! `lightweight update` replaces the running binary with the latest published
//! release. The update installs exactly what the release published: when the
//! release carries an archive for this target, the archive is downloaded,
//! checked against the release's `SHA256SUMS`, its binary is extracted and run
//! once to confirm the version, and then it is swapped in. A target with no
//! published archive builds the same tag from source with `cargo install
//! --locked`.
//!
//! The previous binary is kept aside until the new one is in place, so a
//! failure restores it rather than leaving no working binary.
//!
//! This crate is a leaf over `reqwest`, `rustls` and the archive crates.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod install;
mod release;
mod tls;
mod version;

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use release::Release;

/// One of the two CLIs this command keeps in step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cli {
    Lightweight,
}

impl Cli {
    pub(crate) fn binary(self) -> &'static str {
        match self {
            Self::Lightweight => "lightweight",
        }
    }

    /// The Cargo package that builds the binary.
    pub(crate) fn package(self) -> &'static str {
        match self {
            Self::Lightweight => "lightweight-cli",
        }
    }
}

/// The order CLIs are updated in, whichever one runs the command.
const UPDATE_ORDER: [Cli; 1] = [Cli::Lightweight];

/// What the user asked for.
#[derive(Clone, Copy, Debug, Default)]
pub struct Request {
    /// Report what would be updated without changing anything.
    pub check: bool,
    /// Reinstall even the binaries that are already on the latest release.
    pub force: bool,
    /// Print the `check` report as JSON. Valid only with `check`.
    pub json: bool,
}

/// How the new release reaches this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    ReleaseArchive,
    CargoSource,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReleaseArchive => "release-archive",
            Self::CargoSource => "cargo-source",
        }
    }
}

/// One installed (or to-be-installed) CLI.
pub(crate) struct Target {
    pub(crate) cli: Cli,
    pub(crate) path: PathBuf,
    /// `None` when the binary is absent or did not report a version.
    version: Option<String>,
    installed: bool,
    needs_update: bool,
}

/// Run the update for the CLI `invoker`, whose own version is `current`.
///
/// Progress and results go to stdout; the error is a complete sentence for the
/// caller to print.
pub async fn run(invoker: Cli, current: &str, request: Request) -> Result<(), String> {
    if request.json && !request.check {
        return Err(format!(
            "--json is supported with `{} update --check` only",
            invoker.binary()
        ));
    }

    let executable = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| format!("could not locate the running executable: {error}"))?;
    let install_dir = install::install_dir(&executable)?;

    let client = release::client(&format!("{}/{current}", invoker.binary()))?;
    let release = release::latest(&client).await?;
    let latest = release.version().to_owned();

    let mut targets = Vec::new();
    for cli in [invoker] {
        let path = install::binary_path(&install_dir, cli);
        let installed = path.is_file();
        if cli != invoker && !installed {
            continue;
        }
        let version = if same_file(&path, &executable) {
            Some(current.to_owned())
        } else if installed {
            install::installed_version(&path, cli).await
        } else {
            None
        };
        let needs_update = request.force
            || version.as_deref().is_none_or(|version| {
                version::compare_versions(&latest, version) == Ordering::Greater
            });
        targets.push(Target {
            cli,
            path,
            version,
            installed,
            needs_update,
        });
    }

    let blocked = targets
        .iter()
        .filter(|target| target.installed)
        .find_map(|target| install::managed_install(&target.path));
    let method = method(&release, &latest, &targets)?;

    if request.check {
        report_check(
            invoker,
            &release,
            &targets,
            method,
            blocked.as_deref(),
            request.json,
        );
        return Ok(());
    }

    let pending: Vec<&Target> = targets
        .iter()
        .filter(|target| target.needs_update)
        .collect();
    if pending.is_empty() {
        let verb = if targets.len() == 1 { "is" } else { "are" };
        println!("{} {verb} already up to date.", with_versions(&targets));
        return Ok(());
    }
    if let Some(reason) = blocked {
        return Err(reason);
    }

    if method == Method::CargoSource {
        println!(
            "Release {latest} publishes no archive for {}; building it from source with Cargo.",
            env!("RELEASE_UPDATE_TARGET")
        );
    }
    let source = match method {
        Method::ReleaseArchive => {
            Some(Archives::fetch(&client, &release, &install_dir, invoker).await?)
        }
        Method::CargoSource => None,
    };

    // The previous binary is kept aside until the new one is in place, so a
    // failure restores it.
    let mut transaction = install::Transaction::default();
    for cli in UPDATE_ORDER {
        let Some(target) = pending.iter().find(|target| target.cli == cli) else {
            continue;
        };
        let outcome = match &source {
            Some(archives) => archives.install(target, &mut transaction).await,
            None => {
                install::cargo_install(&install_dir, target, &release.tag_name, &mut transaction)
                    .await
            }
        };
        if let Err(error) = outcome {
            let unrestored = transaction.roll_back();
            return Err(if unrestored.is_empty() {
                format!("{error}. The update was rolled back; every installed binary is as it was")
            } else {
                format!(
                    "{error}. Rolling the update back also failed: {}",
                    unrestored.join("; ")
                )
            });
        }
        println!("Updated {} to {latest}.", cli.binary());
    }
    for leftover in transaction.commit() {
        println!(
            "Left the previous binary at {} because it is still in use; the next update removes it.",
            leftover.display()
        );
    }
    println!("Restart running `lightweight serve` processes to use {latest}.");
    Ok(())
}

/// Every binary that needs updating must come the same way: a release that
/// publishes an archive for one of them on this target but not the other is
/// incomplete.
fn method(release: &Release, latest: &str, targets: &[Target]) -> Result<Method, String> {
    let names: Vec<String> = targets
        .iter()
        .filter(|target| target.needs_update)
        .map(|target| install::archive_name(target.cli, latest))
        .collect();
    let published = names
        .iter()
        .filter(|name| release.asset(name).is_some())
        .count();
    if published == names.len() {
        Ok(Method::ReleaseArchive)
    } else if published == 0 {
        Ok(Method::CargoSource)
    } else {
        Err(format!(
            "release {latest} is incomplete for {}: it publishes only some of {}",
            env!("RELEASE_UPDATE_TARGET"),
            names.join(", ")
        ))
    }
}

/// A release's archives for this target, and where they are unpacked.
struct Archives<'a> {
    client: &'a reqwest::Client,
    release: &'a Release,
    checksums: String,
    staging: install::Staging,
}

impl<'a> Archives<'a> {
    async fn fetch(
        client: &'a reqwest::Client,
        release: &'a Release,
        install_dir: &Path,
        invoker: Cli,
    ) -> Result<Self, String> {
        let asset = release.asset(release::CHECKSUMS_ASSET).ok_or_else(|| {
            format!(
                "release {} has no {} to verify its archives against",
                release.version(),
                release::CHECKSUMS_ASSET
            )
        })?;
        Ok(Self {
            client,
            release,
            checksums: release::text(client, asset).await?,
            staging: install::Staging::create(install_dir, invoker)?,
        })
    }

    /// Download, verify and install one CLI completely.
    async fn install(
        &self,
        target: &Target,
        transaction: &mut install::Transaction,
    ) -> Result<(), String> {
        let latest = self.release.version();
        let name = install::archive_name(target.cli, latest);
        let asset = self
            .release
            .asset(&name)
            .ok_or_else(|| format!("release {latest} does not publish {name}"))?;
        let digest = release::checksum_for(&self.checksums, &name).ok_or_else(|| {
            format!(
                "{} in release {latest} has no entry for {name}",
                release::CHECKSUMS_ASSET
            )
        })?;

        println!("Downloading {name} ({})…", megabytes(asset.size));
        let archive = self.staging.path(&name);
        release::download_verified(self.client, asset, &archive, digest).await?;

        let staged = self.staging.path(&format!(
            "{}{}",
            target.cli.binary(),
            std::env::consts::EXE_SUFFIX
        ));
        let (archive_path, staged_path, cli, version) =
            (archive, staged.clone(), target.cli, latest.to_owned());
        tokio::task::spawn_blocking(move || {
            install::extract_binary(&archive_path, cli, &version, &staged_path)
        })
        .await
        .map_err(|error| format!("the extraction task failed: {error}"))??;
        install::verify_staged(&staged, target.cli, latest).await?;

        transaction.install_staged(&staged, &target.path)
    }
}

fn report_check(
    invoker: Cli,
    release: &Release,
    targets: &[Target],
    method: Method,
    blocked: Option<&str>,
    json: bool,
) {
    let latest = release.version();
    let update_available = targets.iter().any(|target| target.needs_update);
    let current = targets
        .iter()
        .find(|target| target.cli == invoker)
        .and_then(|target| target.version.as_deref());
    if json {
        let binaries: Vec<serde_json::Value> = targets
            .iter()
            .map(|target| {
                serde_json::json!({
                    "name": target.cli.binary(),
                    "path": target.path,
                    "installed": target.installed,
                    "version": target.version,
                    "update_available": target.needs_update,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "current": current,
                "latest": latest,
                "update_available": update_available,
                "release": release.html_url,
                "method": method.as_str(),
                "binaries": binaries,
                "blocked": blocked,
            })
        );
        return;
    }

    println!("Latest release: {latest}");
    for target in targets {
        let installed = target.version.as_deref().unwrap_or(if target.installed {
            "unknown version"
        } else {
            "not installed"
        });
        let state = if target.needs_update {
            format!("{installed} → {latest}")
        } else {
            format!("{installed}, up to date")
        };
        println!(
            "  {} {state}  ({})",
            target.cli.binary(),
            target.path.display()
        );
    }
    if let Some(reason) = blocked {
        println!("Cannot update here: {reason}.");
    } else if update_available {
        println!("Run `{} update` to install it.", invoker.binary());
    }
}

/// "lightweight 0.3.22".
fn with_versions(targets: &[Target]) -> String {
    targets
        .iter()
        .map(|target| {
            let version = target.version.as_deref().unwrap_or("unknown version");
            format!("{} {version}", target.cli.binary())
        })
        .collect::<Vec<_>>()
        .join(" and ")
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

fn same_file(left: &Path, right: &Path) -> bool {
    left.canonicalize()
        .is_ok_and(|left| right.canonicalize().is_ok_and(|right| left == right))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(assets: &[&str]) -> Release {
        let assets: Vec<serde_json::Value> = assets
            .iter()
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "browser_download_url": format!("https://example.org/{name}"),
                    "size": 1,
                })
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "tag_name": "v9.9.9",
            "html_url": "https://example.org/release",
            "assets": assets,
        }))
        .unwrap()
    }

    fn target(cli: Cli, version: Option<&str>) -> Target {
        Target {
            cli,
            path: PathBuf::from(cli.binary()),
            version: version.map(str::to_owned),
            installed: true,
            needs_update: true,
        }
    }

    #[test]
    fn the_update_order_is_just_lightweight() {
        assert_eq!(UPDATE_ORDER, [Cli::Lightweight]);
    }

    #[test]
    fn lightweight_packages_as_lightweight_cli() {
        assert_eq!(Cli::Lightweight.package(), "lightweight-cli");
    }

    #[test]
    fn an_archive_is_used_only_when_it_is_published() {
        let mut targets = [target(Cli::Lightweight, None)];
        let archive = install::archive_name(Cli::Lightweight, "9.9.9");
        assert_eq!(
            method(&release(&[&archive]), "9.9.9", &targets).unwrap(),
            Method::ReleaseArchive
        );
        assert_eq!(
            method(&release(&["SHA256SUMS"]), "9.9.9", &targets).unwrap(),
            Method::CargoSource
        );
        // A current binary's missing archive does not matter.
        targets[0].needs_update = false;
        assert_eq!(
            method(&release(&["SHA256SUMS"]), "9.9.9", &targets).unwrap(),
            Method::CargoSource
        );
    }

    #[test]
    fn summaries_read_naturally() {
        let one = [target(Cli::Lightweight, Some("0.3.22"))];
        assert_eq!(with_versions(&one), "lightweight 0.3.22");
    }

    #[tokio::test]
    async fn json_without_check_is_refused_before_any_network_access() {
        let error = run(
            Cli::Lightweight,
            "0.3.22",
            Request {
                json: true,
                ..Request::default()
            },
        )
        .await
        .unwrap_err();
        assert!(error.contains("`lightweight update --check`"));
    }
}
