//! Extensions: installable capability bundles for the Lightagent runtime.
//!
//! An extension is a directory holding an `extension.json` manifest that packages
//! the primitives the runtime already understands — skills, MCP-server
//! declarations, and persona instructions — into one installable unit. Installing
//! one is dropping its directory in place; removing one is deleting it. This is
//! the Lightagent analog of a plugin, and it composes the existing seams rather
//! than adding a new capability of its own:
//!
//! ```text
//! <ext>/
//! ├── extension.json     # manifest (name, version, description, instructions?, mcp_servers?)
//! └── skills/            # optional: SKILL.md dirs the extension contributes
//!     └── <skill>/SKILL.md
//! ```
//!
//! Extensions are discovered under two roots, exactly like skills: the global
//! `<home>/extensions/` and a profile's own `<profile>/extensions/`, the profile
//! set overriding the global on a name clash. What an installed extension
//! contributes to a run is gated by [`ExtensionsConfig`]: the whole mechanism can
//! be switched off, and individual extensions disabled by name.
//!
//! An extension never bypasses an existing gate. Its skills and instructions are
//! inert composition (the same shape a hand-written skill or persona has), and an
//! MCP server it contributes is merged into the server list but still only
//! contacted when the MCP subsystem itself is enabled — so an extension widens
//! what is *available*, never what is *permitted*.
//!
//! Manifests are JSON (`serde_json`), matching the rest of the runtime's on-disk
//! format and taking on no new dependency; `mcp_servers` reuses core's
//! [`McpServerEntry`] so a contributed server is described and validated
//! identically to one written into the config by hand.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lightagent_core::{ExtensionsConfig, McpServerEntry};
use serde::Deserialize;

/// One installed extension, as discovered on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extension {
    /// The name used to enable/disable the extension (manifest `name`, else the
    /// directory name).
    pub name: String,
    /// The extension's own version string (informational; empty when unset).
    pub version: String,
    /// A one-line summary of what the extension adds.
    pub description: String,
    /// Persona text appended to the system prompt when the extension is active.
    pub instructions: String,
    /// MCP servers the extension contributes to a run.
    pub mcp_servers: Vec<McpServerEntry>,
    /// The extension's directory on disk.
    pub dir: PathBuf,
}

impl Extension {
    /// The directory holding the extension's contributed skills.
    pub fn skills_dir(&self) -> PathBuf {
        self.dir.join("skills")
    }
}

/// The manifest shape parsed from an `extension.json`.
///
/// Every field but the name is optional so a minimal extension — a manifest and
/// a `skills/` directory — is valid.
#[derive(Debug, Default, Deserialize)]
struct ExtensionManifest {
    name: Option<String>,
    #[serde(default)]
    version: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    instructions: String,
    #[serde(default)]
    mcp_servers: Vec<McpServerEntry>,
}

/// The extensions discovered under a set of directories.
#[derive(Clone, Debug, Default)]
pub struct ExtensionStore {
    extensions: BTreeMap<String, Extension>,
}

impl ExtensionStore {
    /// Load every `<dir>/<ext>/extension.json` under each directory in turn; a
    /// later directory's extension replaces an earlier one of the same name (so a
    /// profile's extensions override the global set). A directory that does not
    /// exist, an entry without a readable manifest, and a manifest that does not
    /// parse are all skipped rather than failing the load.
    pub fn load(dirs: &[PathBuf]) -> Self {
        let mut extensions = BTreeMap::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(path.join("extension.json")) else {
                    continue;
                };
                let fallback = entry.file_name().to_string_lossy().into_owned();
                if let Some(ext) = parse_extension(&text, &fallback, &path) {
                    extensions.insert(ext.name.clone(), ext);
                }
            }
        }
        Self { extensions }
    }

    /// Look up an installed extension by name.
    pub fn get(&self, name: &str) -> Option<&Extension> {
        self.extensions.get(name)
    }

    /// Every installed extension, name-sorted.
    pub fn all(&self) -> impl Iterator<Item = &Extension> {
        self.extensions.values()
    }

    /// The installed extension names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.extensions.keys().cloned().collect()
    }

    /// Whether any extension was found.
    pub fn is_empty(&self) -> bool {
        self.extensions.is_empty()
    }

    /// The number of installed extensions.
    pub fn len(&self) -> usize {
        self.extensions.len()
    }

    /// Whether an installed extension is active under the given config: the
    /// mechanism is enabled and the extension is not on the disabled list.
    pub fn is_active(&self, name: &str, config: &ExtensionsConfig) -> bool {
        config.enabled && !config.disabled.iter().any(|disabled| disabled == name)
    }

    /// The active extensions under the given config, name-sorted.
    pub fn active<'a>(
        &'a self,
        config: &'a ExtensionsConfig,
    ) -> impl Iterator<Item = &'a Extension> {
        self.extensions
            .values()
            .filter(move |ext| self.is_active(&ext.name, config))
    }

    /// The skill directories contributed by the active extensions, in name order.
    ///
    /// These slot between the global and per-profile skill directories so that an
    /// extension's skills override the global defaults but a profile can still
    /// override an extension's.
    pub fn skill_dirs(&self, config: &ExtensionsConfig) -> Vec<PathBuf> {
        self.active(config).map(Extension::skills_dir).collect()
    }

    /// The MCP servers contributed by the active extensions, in name order.
    ///
    /// These are merged into the configured server list but are still only
    /// contacted when the MCP subsystem is enabled.
    pub fn mcp_servers(&self, config: &ExtensionsConfig) -> Vec<McpServerEntry> {
        self.active(config)
            .flat_map(|ext| ext.mcp_servers.iter().cloned())
            .collect()
    }

    /// The persona block contributed by the active extensions: each extension's
    /// instructions under a heading naming it. Empty when none contribute any.
    pub fn instructions(&self, config: &ExtensionsConfig) -> String {
        let mut out = String::new();
        for ext in self.active(config) {
            let body = ext.instructions.trim();
            if body.is_empty() {
                continue;
            }
            if out.is_empty() {
                out.push_str("# Active extensions\n\n");
            }
            out.push_str(&format!("## Extension: {}\n{}\n\n", ext.name, body));
        }
        out.trim_end().to_owned()
    }
}

/// Parse an `extension.json` into an [`Extension`], falling back to the directory
/// name when the manifest omits `name`.
fn parse_extension(text: &str, fallback_name: &str, dir: &Path) -> Option<Extension> {
    let manifest: ExtensionManifest = serde_json::from_str(text).ok()?;
    let name = manifest
        .name
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback_name.to_owned());
    if name.trim().is_empty() {
        return None;
    }
    Some(Extension {
        name,
        version: manifest.version,
        description: manifest.description,
        instructions: manifest.instructions,
        mcp_servers: manifest.mcp_servers,
        dir: dir.to_path_buf(),
    })
}

/// The extension directories to load for a profile: the global set, then the
/// profile's own (which overrides on a name clash). Mirrors
/// `lightagent_core::skill_dirs`.
pub fn extension_dirs(home: &Path, profile_dir: &Path) -> Vec<PathBuf> {
    vec![home.join("extensions"), profile_dir.join("extensions")]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lightagent-ext-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_extension(root: &Path, dir: &str, manifest: &str) {
        let path = root.join(dir);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("extension.json"), manifest).unwrap();
    }

    #[test]
    fn parses_a_full_manifest() {
        let manifest = r#"{
            "name": "web-research",
            "version": "0.2.0",
            "description": "Deep web research",
            "instructions": "Prefer primary sources.",
            "mcp_servers": [
                { "transport": "stdio", "name": "fetch", "command": "fetch-server" }
            ]
        }"#;
        let ext = parse_extension(manifest, "fallback", Path::new("/x")).unwrap();
        assert_eq!(ext.name, "web-research");
        assert_eq!(ext.version, "0.2.0");
        assert_eq!(ext.description, "Deep web research");
        assert_eq!(ext.instructions, "Prefer primary sources.");
        assert_eq!(ext.mcp_servers.len(), 1);
        assert_eq!(ext.mcp_servers[0].name(), "fetch");
        assert_eq!(ext.skills_dir(), Path::new("/x/skills"));
    }

    #[test]
    fn name_falls_back_to_the_directory() {
        let ext = parse_extension("{}", "dirname", Path::new("/x")).unwrap();
        assert_eq!(ext.name, "dirname");
        assert!(ext.mcp_servers.is_empty());
    }

    #[test]
    fn malformed_manifest_is_skipped() {
        assert!(parse_extension("{ not json", "d", Path::new("/x")).is_none());
    }

    #[test]
    fn profile_overrides_global_on_name_clash() {
        let global = scratch();
        let profile = scratch();
        write_extension(
            &global,
            "a",
            r#"{ "name": "a", "description": "global a" }"#,
        );
        write_extension(&global, "b", r#"{ "name": "b", "description": "b" }"#);
        write_extension(
            &profile,
            "a",
            r#"{ "name": "a", "description": "profile a" }"#,
        );

        let store = ExtensionStore::load(&[global.clone(), profile.clone()]);
        assert_eq!(store.len(), 2);
        assert_eq!(store.get("a").unwrap().description, "profile a");
        assert_eq!(store.get("a").unwrap().dir, profile.join("a"));

        std::fs::remove_dir_all(&global).ok();
        std::fs::remove_dir_all(&profile).ok();
    }

    #[test]
    fn config_gates_what_is_active() {
        let root = scratch();
        write_extension(
            &root,
            "keep",
            r#"{ "name": "keep", "instructions": "keep me", "mcp_servers": [
                { "transport": "http", "name": "k", "url": "http://127.0.0.1:1" } ] }"#,
        );
        write_extension(
            &root,
            "drop",
            r#"{ "name": "drop", "instructions": "drop me", "mcp_servers": [
                { "transport": "http", "name": "d", "url": "http://127.0.0.1:2" } ] }"#,
        );
        let store = ExtensionStore::load(std::slice::from_ref(&root));

        // Both active by default.
        let all_on = ExtensionsConfig::default();
        assert_eq!(store.active(&all_on).count(), 2);
        assert_eq!(store.mcp_servers(&all_on).len(), 2);
        assert!(store.instructions(&all_on).contains("keep me"));
        assert!(store.instructions(&all_on).contains("drop me"));

        // Disabling one removes exactly its contributions.
        let one_off = ExtensionsConfig {
            enabled: true,
            disabled: vec!["drop".to_owned()],
        };
        assert!(store.is_active("keep", &one_off));
        assert!(!store.is_active("drop", &one_off));
        assert_eq!(store.mcp_servers(&one_off).len(), 1);
        assert_eq!(store.mcp_servers(&one_off)[0].name(), "k");
        assert!(store.instructions(&one_off).contains("keep me"));
        assert!(!store.instructions(&one_off).contains("drop me"));
        assert_eq!(store.skill_dirs(&one_off), vec![root.join("keep/skills")]);

        // The master switch stops everything.
        let all_off = ExtensionsConfig {
            enabled: false,
            disabled: Vec::new(),
        };
        assert_eq!(store.active(&all_off).count(), 0);
        assert!(store.mcp_servers(&all_off).is_empty());
        assert!(store.instructions(&all_off).is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn extension_dirs_are_global_then_profile() {
        let dirs = extension_dirs(Path::new("/root-x"), Path::new("/root-x/profiles/p"));
        assert_eq!(dirs[0], Path::new("/root-x/extensions"));
        assert_eq!(dirs[1], Path::new("/root-x/profiles/p/extensions"));
    }
}
