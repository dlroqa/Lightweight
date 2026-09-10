//! Self-update through Cargo from the latest official release tag.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

const REPOSITORY: &str = "https://github.com/dlroqa/Lightweight.git";
const LATEST_RELEASE_API: &str = "https://api.github.com/repos/dlroqa/Lightweight/releases/latest";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
}

pub(crate) async fn run(current: &str, check: bool, force: bool, json: bool) -> Result<(), String> {
    if json && !check {
        return Err("--json is supported with `lightagent update --check` only".to_owned());
    }
    let release = latest_release().await?;
    let latest = release.tag_name.trim_start_matches('v');
    let update_available = compare_versions(latest, current) == Ordering::Greater;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "current": current,
                "latest": latest,
                "update_available": update_available,
                "release": release.html_url,
            })
        );
        return Ok(());
    }
    if check {
        if update_available {
            println!("Lightagent {latest} is available (installed: {current}).");
            println!("Run `lightagent update` to install it.");
        } else {
            println!("Lightagent {current} is up to date.");
        }
        return Ok(());
    }
    if !update_available && !force {
        println!("Lightagent {current} is already up to date.");
        return Ok(());
    }

    let executable = std::env::current_exe()
        .map_err(|error| format!("could not locate the running executable: {error}"))?;
    let root = install_root(&executable).ok_or_else(|| {
        format!(
            "could not determine the install root from {}; set LIGHTAGENT_INSTALL_ROOT",
            executable.display()
        )
    })?;
    let root = std::env::var_os("LIGHTAGENT_INSTALL_ROOT")
        .map(PathBuf::from)
        .unwrap_or(root);
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    println!(
        "Installing Lightagent {latest} from the official release into {}…",
        root.display()
    );
    let status = Command::new(cargo)
        .arg("install")
        .arg("--git")
        .arg(REPOSITORY)
        .arg("--tag")
        .arg(&release.tag_name)
        .arg("--locked")
        .arg("--force")
        .arg("--root")
        .arg(&root)
        .arg("lightagent")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .map_err(|error| {
            format!("could not run Cargo: {error}. Install Rust from https://rustup.rs, then retry")
        })?;
    if !status.success() {
        return Err(format!("Cargo could not install Lightagent {latest}"));
    }
    println!("Updated Lightagent to {latest}. Restart the CLI to use it.");
    Ok(())
}

async fn latest_release() -> Result<Release, String> {
    lightagent_provider_lightweight::ensure_provider();
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("lightagent/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("could not create the update client: {error}"))?
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| format!("could not check for updates: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "GitHub answered HTTP {} while checking for updates",
            response.status().as_u16()
        ));
    }
    response
        .json::<Release>()
        .await
        .map_err(|error| format!("could not read the latest release: {error}"))
}

fn install_root(executable: &Path) -> Option<PathBuf> {
    if let Ok(root) = std::env::var("LIGHTAGENT_INSTALL_ROOT")
        && !root.trim().is_empty()
    {
        return Some(PathBuf::from(root));
    }
    let bin = executable.parent()?;
    if bin.file_name().is_some_and(|name| name == "bin") {
        return bin.parent().map(Path::to_path_buf);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".local"))
}

fn compare_versions(left: &str, right: &str) -> Ordering {
    let mut left = version_parts(left).into_iter();
    let mut right = version_parts(right).into_iter();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (Some(a), Some(b)) if a != b => return a.cmp(&b),
            (Some(_), Some(_)) => {}
            (Some(a), None) => return a.cmp(&0),
            (None, Some(b)) => return 0.cmp(&b),
        }
    }
}

fn version_parts(version: &str) -> Vec<u64> {
    version
        .trim_start_matches('v')
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_versions_compare_numerically() {
        assert_eq!(compare_versions("0.3.5", "0.3.4"), Ordering::Greater);
        assert_eq!(compare_versions("1.0.0", "1.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.3.5", "0.4.0"), Ordering::Less);
    }

    #[test]
    fn an_installed_binary_resolves_its_cargo_root() {
        assert_eq!(
            install_root(Path::new("/opt/lightagent/bin/lightagent")),
            Some(PathBuf::from("/opt/lightagent"))
        );
    }
}
