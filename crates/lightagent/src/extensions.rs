//! `lightagent extensions` — list installed capability bundles and toggle which
//! are active.
//!
//! An extension is discovered under the global `<home>/extensions/` directory and
//! the active profile's own; installing one is dropping its directory in place.
//! These commands only inspect the installed set and edit which are active — the
//! `enabled`/`disabled` fields of the `extensions` config — never the files.

use lightagent_core::{Config, ConfigStore, LightagentPaths, ProfileStore};
use lightagent_extensions::{Extension, ExtensionStore, extension_dirs};

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
            "No extensions installed. Drop one under {} to install it.",
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
    if json {
        let value = serde_json::json!({
            "name": ext.name,
            "version": ext.version,
            "description": ext.description,
            "active": active,
            "dir": ext.dir,
            "instructions": ext.instructions,
            "mcp_servers": ext.mcp_servers.iter().map(|s| s.name()).collect::<Vec<_>>(),
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
