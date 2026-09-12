//! `lightagent extensions` — install, remove, list and toggle capability bundles.
//!
//! An extension is discovered under the global `<home>/extensions/` directory and
//! the active profile's own; installing one is dropping its directory in place.
//! Installation copies a local bundle into the selected store; uninstall removes
//! exactly that installed copy.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use lightagent_core::{Config, ConfigStore, LightagentPaths, ProfileStore, SkillStore};
use lightagent_extensions::{Extension, ExtensionStore, extension_dirs};

fn selected_root(paths: &LightagentPaths, profile: bool) -> Result<PathBuf, String> {
    if !profile {
        return Ok(paths.extensions_dir());
    }
    let profiles = ProfileStore::new(paths.root());
    let active = profiles
        .active()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no active profile — run `lightagent init` first".to_owned())?;
    Ok(profiles.handle(&active).dir().join("extensions"))
}

/// Install a local bundle and activate the extension system it needs.
pub fn install(source: &Path, profile: bool, json: bool) -> Result<(), String> {
    let paths = LightagentPaths::resolve().map_err(|e| e.to_string())?;
    let root = selected_root(&paths, profile)?;
    let ext = install_from_source(source, &root)?;
    let config_store = ConfigStore::at(&paths);
    let mut config = config_store.load().map_err(|e| e.to_string())?;
    config.extensions.enabled = true;
    config.extensions.disabled.retain(|name| name != &ext.name);
    if !ext.mcp_servers.is_empty() {
        config.mcp.enabled = true;
    }
    if let Err(error) = config_store.save(&config) {
        let _ = lightagent_extensions::uninstall(&ext.name, &root);
        return Err(format!("could not save extension settings: {error}"));
    }
    if json {
        println!(
            "{}",
            serde_json::json!({
                "name": ext.name, "dir": ext.dir, "mcp_servers": ext.mcp_servers.len(),
                "active": true
            })
        );
    } else {
        println!("Installed '{}' in {}.", ext.name, ext.dir.display());
        if !ext.mcp_servers.is_empty() {
            println!(
                "MCP enabled; run `lightagent tools list` to verify the server and its tools."
            );
        }
    }
    Ok(())
}

/// Resolve either a local directory or a reviewed HTTPS Git repository. A
/// `#subdir` fragment selects an extension below the repository root.
fn install_from_source(source: &Path, root: &Path) -> Result<Extension, String> {
    let Some((url, subdir)) = git_source(source)? else {
        return lightagent_extensions::install(source, root);
    };
    let checkout = std::env::temp_dir().join(format!(
        "lightagent-extension-fetch-{}",
        lightagent_core::RunId::new().as_str()
    ));
    let result = (|| {
        let output = Command::new("git")
            .args(["clone", "--depth", "1", "--"])
            .arg(&url)
            .arg(&checkout)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|e| format!("could not start git: {e}"))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git clone failed: {}", detail.trim()));
        }
        lightagent_extensions::install(&checkout.join(subdir), root)
    })();
    let _ = std::fs::remove_dir_all(&checkout);
    result
}

fn git_source(source: &Path) -> Result<Option<(String, PathBuf)>, String> {
    let text = source.to_string_lossy();
    let raw = text.strip_prefix("git+").unwrap_or(&text);
    if !raw.starts_with("https://") {
        return Ok(None);
    }
    let (url, subdir) = raw.split_once('#').unwrap_or((raw, ""));
    if url.len() <= "https://".len() {
        return Err("Git URL needs a host and repository path".to_owned());
    }
    let authority = url
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("");
    if authority.contains('@') {
        return Err("Git URL must not contain credentials; use a Git credential helper".to_owned());
    }
    let subdir = PathBuf::from(subdir);
    if !subdir.as_os_str().is_empty()
        && !subdir
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err("Git extension subdirectory must stay within the repository".to_owned());
    }
    Ok(Some((url.to_owned(), subdir)))
}

/// Remove an installed bundle from the selected store.
pub fn uninstall(name: &str, profile: bool, json: bool) -> Result<(), String> {
    let paths = LightagentPaths::resolve().map_err(|e| e.to_string())?;
    let root = selected_root(&paths, profile)?;
    lightagent_extensions::uninstall(name, &root)?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "name": name, "removed_from": root })
        );
    } else {
        println!("Uninstalled '{name}' from {}.", root.display());
    }
    Ok(())
}

/// Resolve the config and the extension store for the active profile (global
/// extensions plus the profile's own).
fn load() -> Result<(Config, ExtensionStore), String> {
    let paths = LightagentPaths::resolve().map_err(|error| error.to_string())?;
    let store = ConfigStore::at(&paths);
    let config = store.load().map_err(|error| error.to_string())?;
    let profiles = ProfileStore::new(paths.root());
    let dirs = match profiles.active().map_err(|error| error.to_string())? {
        Some(active) => extension_dirs(paths.root(), profiles.handle(&active).dir()),
        None => vec![paths.extensions_dir()],
    };
    Ok((config, ExtensionStore::load(&dirs)))
}

fn describe(ext: &Extension) -> String {
    if ext.description.is_empty() {
        ext.name.clone()
    } else {
        format!("{} — {}", ext.name, ext.description)
    }
}

/// `extensions list` — every installed extension, marking the active ones.
pub fn list(json: bool) -> Result<(), String> {
    let (config, store) = load()?;
    if json {
        let value = serde_json::json!({
            "enabled": config.extensions.enabled,
            "extensions": store.all().map(|ext| serde_json::json!({
                "name": ext.name,
                "version": ext.version,
                "description": ext.description,
                "active": store.is_active(&ext.name, &config.extensions),
                "skills_dir": ext.skills_dir(),
                "mcp_servers": ext.mcp_servers.iter().map(|s| s.name()).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
        println!("{value:#}");
        return Ok(());
    }
    if store.is_empty() {
        println!(
            "No extensions installed. Use `lightagent extensions install <directory>` or add one under {}.",
            LightagentPaths::resolve()
                .map(|p| p.extensions_dir().display().to_string())
                .unwrap_or_else(|_| "<home>/extensions".to_owned())
        );
        return Ok(());
    }
    if !config.extensions.enabled {
        println!("(extensions are disabled globally — `lightagent config set` extensions.enabled)");
    }
    for ext in store.all() {
        let mark = if store.is_active(&ext.name, &config.extensions) {
            "●"
        } else {
            "○"
        };
        println!("{mark} {}", describe(ext));
    }
    Ok(())
}

/// `extensions show <name>` — one extension's manifest and contributions.
pub fn show(name: &str, json: bool) -> Result<(), String> {
    let (config, store) = load()?;
    let ext = store
        .get(name)
        .ok_or_else(|| format!("no installed extension named '{name}'"))?;
    let active = store.is_active(name, &config.extensions);
    let skills = SkillStore::load(&[ext.skills_dir()]).names();
    if json {
        let value = serde_json::json!({
            "name": ext.name,
            "version": ext.version,
            "description": ext.description,
            "active": active,
            "dir": ext.dir,
            "instructions": ext.instructions,
            "mcp_servers": ext.mcp_servers.iter().map(|s| s.name()).collect::<Vec<_>>(),
            "skills": skills,
        });
        println!("{value:#}");
        return Ok(());
    }
    println!("{}", ext.name);
    if !ext.version.is_empty() {
        println!("  version: {}", ext.version);
    }
    if !ext.description.is_empty() {
        println!("  {}", ext.description);
    }
    println!("  active:  {}", if active { "yes" } else { "no" });
    println!("  dir:     {}", ext.dir.display());
    if !ext.mcp_servers.is_empty() {
        let names: Vec<&str> = ext.mcp_servers.iter().map(|s| s.name()).collect();
        println!("  mcp:     {}", names.join(", "));
    }
    if !skills.is_empty() {
        println!("  skills:  {}", skills.join(", "));
    }
    if !ext.instructions.trim().is_empty() {
        println!("  instructions:\n{}", indent(ext.instructions.trim()));
    }
    Ok(())
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `extensions enable <name>` — clear an extension from the disabled list.
pub fn enable(name: &str, json: bool) -> Result<(), String> {
    toggle(name, false, json)
}

/// `extensions disable <name>` — keep an extension installed but inactive.
pub fn disable(name: &str, json: bool) -> Result<(), String> {
    toggle(name, true, json)
}

fn toggle(name: &str, disable: bool, json: bool) -> Result<(), String> {
    let paths = LightagentPaths::resolve().map_err(|error| error.to_string())?;
    let config_store = ConfigStore::at(&paths);
    let mut config = config_store.load().map_err(|error| error.to_string())?;

    // Confirm the name is actually installed, so a typo is caught here.
    let profiles = ProfileStore::new(paths.root());
    let dirs = match profiles.active().map_err(|error| error.to_string())? {
        Some(active) => extension_dirs(paths.root(), profiles.handle(&active).dir()),
        None => vec![paths.extensions_dir()],
    };
    let store = ExtensionStore::load(&dirs);
    if store.get(name).is_none() {
        return Err(format!("no installed extension named '{name}'"));
    }

    let was_disabled = config.extensions.disabled.iter().any(|n| n == name);
    if disable {
        if !was_disabled {
            config.extensions.disabled.push(name.to_owned());
            config.extensions.disabled.sort();
        }
    } else {
        config.extensions.disabled.retain(|n| n != name);
    }
    config_store
        .save(&config)
        .map_err(|error| error.to_string())?;

    let active = config.extensions.enabled && !disable;
    if json {
        println!("{}", serde_json::json!({ "name": name, "active": active }));
    } else if disable {
        println!("Disabled '{name}'.");
    } else if config.extensions.enabled {
        println!("Enabled '{name}'.");
    } else {
        println!(
            "Enabled '{name}', but extensions are off globally (extensions.enabled is false)."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_sources_accept_a_relative_extension_subdirectory() {
        assert_eq!(git_source(Path::new("./my-tool")).unwrap(), None);
        assert_eq!(
            git_source(Path::new(
                "git+https://example.org/tools.git#extensions/my-tool"
            ))
            .unwrap(),
            Some((
                "https://example.org/tools.git".to_owned(),
                PathBuf::from("extensions/my-tool")
            ))
        );
        assert!(git_source(Path::new("https://example.org/tools.git#../other")).is_err());
        assert!(git_source(Path::new("https://token@example.org/tools.git")).is_err());
    }
}
