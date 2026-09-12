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
//! ├── extension.json     # manifest (name, version, instructions_file?, mcp_servers?)
//! ├── ONBOARDING.md      # optional Markdown referenced by instructions_file
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

use lightagent_core::paths::create_private_dir;
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
    /// Optional Markdown file inside the extension, appended to instructions.
    instructions_file: Option<String>,
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
    let mut instructions = manifest.instructions;
    if let Some(file) = manifest.instructions_file {
        let relative = Path::new(&file);
        if relative.as_os_str().is_empty()
            || !relative
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
        {
            return None;
        }
        let root = dir.canonicalize().ok()?;
        let full = dir.join(relative).canonicalize().ok()?;
        if !full.starts_with(root) || std::fs::metadata(&full).ok()?.len() > 65_536 {
            return None;
        }
        let file_text = std::fs::read_to_string(full).ok()?;
        if !instructions.trim().is_empty() {
            instructions.push_str("\n\n");
        }
        instructions.push_str(&file_text);
    }
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
        instructions,
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

/// Install a local extension directory atomically into an extension root.
/// Symlinks are refused so a package cannot copy files outside its source.
pub fn install(source: &Path, root: &Path) -> Result<Extension, String> {
    let source = source.canonicalize().map_err(|e| format!("source: {e}"))?;
    if !source.is_dir() {
        return Err("extension source must be a directory".to_owned());
    }
    let manifest = std::fs::read_to_string(source.join("extension.json"))
        .map_err(|e| format!("extension.json: {e}"))?;
    let fallback = source.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let ext = parse_extension(&manifest, fallback, &source)
        .ok_or_else(|| "invalid extension.json".to_owned())?;
    if !valid_name(&ext.name) {
        return Err(
            "extension name must contain only ASCII letters, numbers, '-' or '_'".to_owned(),
        );
    }
    for server in &ext.mcp_servers {
        if server.name().trim().is_empty() {
            return Err("an MCP server has an empty name".to_owned());
        }
        match server {
            McpServerEntry::Stdio { command, .. } if command.trim().is_empty() => {
                return Err(format!(
                    "MCP server '{}' has an empty command",
                    server.name()
                ));
            }
            McpServerEntry::Http { url, .. }
                if !(url.starts_with("http://") || url.starts_with("https://")) =>
            {
                return Err(format!(
                    "MCP server '{}' needs an http(s) URL",
                    server.name()
                ));
            }
            _ => {}
        }
    }
    create_private_dir(root).map_err(|e| format!("extension root: {e}"))?;
    let root = root
        .canonicalize()
        .map_err(|e| format!("extension root: {e}"))?;
    if root.starts_with(&source) {
        return Err("extension root cannot be inside its source directory".to_owned());
    }
    let destination = root.join(&ext.name);
    if std::fs::symlink_metadata(&destination).is_ok() {
        return Err(format!(
            "extension '{}' is already installed in {}",
            ext.name,
            root.display()
        ));
    }
    let staging = root.join(format!(".{}.install-{}", ext.name, std::process::id()));
    if staging.exists() {
        return Err(format!(
            "staging directory {} already exists",
            staging.display()
        ));
    }
    let result = (|| {
        copy_tree(&source, &staging)?;
        std::fs::rename(&staging, &destination)
            .map_err(|e| format!("could not finish installation: {e}"))
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(Extension {
        dir: destination,
        ..ext
    })
}

/// Remove exactly one installed extension from a selected root.
pub fn uninstall(name: &str, root: &Path) -> Result<(), String> {
    if !valid_name(name) {
        return Err("invalid extension name".to_owned());
    }
    let destination = root.join(name);
    let metadata = std::fs::symlink_metadata(&destination)
        .map_err(|e| format!("extension '{name}' is not installed: {e}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!("extension '{name}' is not a regular directory"));
    }
    let manifest = std::fs::read_to_string(destination.join("extension.json"))
        .map_err(|e| format!("extension.json: {e}"))?;
    let ext = parse_extension(&manifest, name, &destination)
        .ok_or_else(|| "invalid extension.json".to_owned())?;
    if ext.name != name {
        return Err(format!(
            "extension directory '{name}' declares name '{}'",
            ext.name
        ));
    }
    std::fs::remove_dir_all(&destination).map_err(|e| format!("could not uninstall '{name}': {e}"))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    create_private_dir(destination)
        .map_err(|e| format!("could not create {}: {e}", destination.display()))?;
    for entry in std::fs::read_dir(source).map_err(|e| format!("{}: {e}", source.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_name().to_string_lossy() == ".git" {
            continue;
        }
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        let target = destination.join(entry.file_name());
        if kind.is_symlink() {
            return Err(format!(
                "extension contains a symlink: {}",
                entry.path().display()
            ));
        }
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("could not copy {}: {e}", entry.path().display()))?;
        } else {
            return Err(format!(
                "extension contains an unsupported file: {}",
                entry.path().display()
            ));
        }
    }
    Ok(())
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

    #[test]
    fn a_markdown_instructions_file_is_loaded_and_removable() {
        let scratch = scratch();
        let source = scratch.join("source");
        let root = scratch.join("installed");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("extension.json"),
            r#"{
            "name":"onboarding", "instructions_file":"ONBOARDING.md"
        }"#,
        )
        .unwrap();
        std::fs::write(
            source.join("ONBOARDING.md"),
            "Use the available tools wisely.",
        )
        .unwrap();
        install(&source, &root).unwrap();
        let store = ExtensionStore::load(std::slice::from_ref(&root));
        assert!(
            store
                .instructions(&ExtensionsConfig::default())
                .contains("Use the available tools wisely.")
        );
        uninstall("onboarding", &root).unwrap();
        assert!(ExtensionStore::load(std::slice::from_ref(&root)).is_empty());
        std::fs::remove_dir_all(scratch).ok();
    }

    #[test]
    fn instructions_file_cannot_escape_the_extension() {
        let scratch = scratch();
        std::fs::write(
            scratch.join("extension.json"),
            r#"{"name":"bad","instructions_file":"../outside.md"}"#,
        )
        .unwrap();
        assert!(
            parse_extension(
                &std::fs::read_to_string(scratch.join("extension.json")).unwrap(),
                "bad",
                &scratch
            )
            .is_none()
        );
        std::fs::remove_dir_all(scratch).ok();
    }

    #[test]
    fn install_discover_and_uninstall_a_bundle() {
        let scratch = scratch();
        let source = scratch.join("source");
        let root = scratch.join("installed");
        std::fs::create_dir_all(source.join("skills/echo")).unwrap();
        std::fs::write(
            source.join("extension.json"),
            r#"{
            "name":"echo", "mcp_servers":[
                {"transport":"stdio","name":"echo","command":"python3","args":["server.py"]}
            ]
        }"#,
        )
        .unwrap();
        std::fs::write(source.join("server.py"), "print('hello')").unwrap();
        std::fs::write(source.join("skills/echo/SKILL.md"), "# Echo").unwrap();

        let installed = install(&source, &root).unwrap();
        assert_eq!(installed.name, "echo");
        assert!(installed.dir.join("server.py").is_file());
        assert_eq!(ExtensionStore::load(std::slice::from_ref(&root)).len(), 1);
        assert!(
            install(&source, &root)
                .unwrap_err()
                .contains("already installed")
        );
        uninstall("echo", &root).unwrap();
        assert!(ExtensionStore::load(std::slice::from_ref(&root)).is_empty());
        assert!(source.join("server.py").is_file());
        std::fs::remove_dir_all(scratch).ok();
    }

    #[test]
    fn rejects_path_names_and_symlinked_package_content() {
        let scratch = scratch();
        let source = scratch.join("source");
        let root = scratch.join("installed");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("extension.json"), r#"{"name":"../escape"}"#).unwrap();
        assert!(install(&source, &root).unwrap_err().contains("name must"));
        assert!(uninstall("../escape", &root).is_err());
        std::fs::write(source.join("extension.json"), r#"{"name":"safe"}"#).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", source.join("outside")).unwrap();
            assert!(install(&source, &root).unwrap_err().contains("symlink"));
            assert!(!root.join("safe").exists());
        }
        std::fs::remove_dir_all(scratch).ok();
    }
}
