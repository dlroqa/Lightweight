//! The typed global configuration (§16).
//!
//! Four sections the agent actually acts on — `inference`, `agent`,
//! `security`, `web` — each with defaults and validation, plus a catch-all for
//! keys a newer build wrote that this one does not understand, so a save here
//! never deletes a preference set elsewhere.
//!
//! Secrets are never stored in the clear. The provider API key is held as a
//! *reference* to an environment variable, so the config file, a log line, a
//! `config show` and a prompt all carry only the variable's name — the value is
//! resolved at the point of use and nowhere else.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths;

/// The on-disk format version of the config file.
const FORMAT_VERSION: u32 = 1;

/// A secret held by reference, never by value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SecretRef {
    /// Read the secret from an environment variable at point of use.
    Env { var: String },
}

impl SecretRef {
    /// A reference to the environment variable `var`.
    pub fn env(var: impl Into<String>) -> Self {
        Self::Env { var: var.into() }
    }

    /// Resolve the secret's value now, if the reference points somewhere real.
    ///
    /// The only place a secret becomes a value. Returns `None` when the
    /// variable is unset, so a caller decides what a missing key means rather
    /// than being handed an empty string that looks like one.
    pub fn resolve(&self) -> Option<String> {
        match self {
            Self::Env { var } => std::env::var(var).ok().filter(|value| !value.is_empty()),
        }
    }

    /// A display form safe for logs and `config show`: the reference, never the
    /// value.
    pub fn redacted(&self) -> String {
        match self {
            Self::Env { var } => format!("${{env:{var}}}"),
        }
    }
}

/// Which provider backend answers, and where.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InferenceConfig {
    /// The provider adapter to use. `"lightweight"` this pass.
    pub provider: String,
    /// The OpenAI-compatible base URL.
    pub base_url: String,
    /// The device hint passed to the provider.
    pub device: String,
    /// Whether the provider may fall back to CPU.
    pub allow_cpu_fallback: bool,
    /// The default model id, when a run does not name one.
    pub model: Option<String>,
    /// The provider API key, by reference. Loopback needs none.
    pub api_key: Option<SecretRef>,
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            provider: "lightweight".to_owned(),
            base_url: "http://127.0.0.1:11434".to_owned(),
            device: "auto".to_owned(),
            allow_cpu_fallback: true,
            model: None,
            api_key: None,
        }
    }
}

/// Run limits applied when a profile does not override them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub max_turns: u32,
    pub max_tool_calls: u32,
    pub wall_clock_secs: Option<u64>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        let limits = crate::limits::RunLimits::default();
        Self {
            max_turns: limits.max_turns,
            max_tool_calls: limits.max_tool_calls,
            wall_clock_secs: limits.wall_clock_secs,
        }
    }
}

/// How aggressively tool calls are gated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Never prompt; approve everything. For trusted, unattended runs.
    Permissive,
    /// The default: prompt for anything that changes state or runs code.
    #[default]
    Balanced,
    /// Prompt for anything above a bare read.
    Strict,
}

/// Security posture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    pub approval_policy: ApprovalPolicy,
    /// Whether tools that reach the network are allowed by default.
    pub remote_tools_default: bool,
    /// Whether user text is treated as private (kept out of logs).
    pub privacy_mode: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            approval_policy: ApprovalPolicy::default(),
            remote_tools_default: false,
            privacy_mode: true,
        }
    }
}

/// A search backend for `web.search`.
///
/// `web.search` sends the query to `endpoint` (adding `query_param=<query>`) and
/// parses a JSON response with a `results` array of objects carrying a `title`,
/// a `url`, and a snippet under `content`, `snippet` or `description` — SearXNG's
/// `format=json` shape, and a common minimal one. No backend ships by default, so
/// the tool reports that none is configured until an operator sets `endpoint`.
/// A key, when the endpoint needs one, is held by reference and sent as
/// `Authorization: Bearer <key>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSearchConfig {
    /// The JSON search endpoint, e.g. `https://searx.example/search?format=json`.
    pub endpoint: Option<String>,
    /// The query-string parameter the endpoint reads the query from.
    pub query_param: String,
    /// A bearer key for the endpoint, by reference. Never stored in the clear.
    pub api_key: Option<SecretRef>,
    /// The most results a single search returns.
    pub max_results: usize,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            query_param: "q".to_owned(),
            api_key: None,
            max_results: 5,
        }
    }
}

/// Web access configuration.
///
/// Off by default: `web.fetch` and `web.search` are declared to the model but
/// refuse to run until `enabled` is set, so a fresh install reaches no network.
/// `allow_domains`, when non-empty, is an allow-list a fetch host must match
/// (exactly or as a subdomain); it also opts a host past the private-address
/// guard, so an operator can reach a named internal service without opening the
/// guard for everything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub enabled: bool,
    pub allow_domains: Vec<String>,
    /// The most bytes a single fetch reads before truncating.
    pub max_fetch_bytes: usize,
    /// The per-request timeout, in seconds.
    pub timeout_secs: u64,
    /// Refuse a fetch whose host resolves to a loopback, private, link-local or
    /// otherwise non-global address (an SSRF guard), unless the host is named in
    /// `allow_domains`.
    pub block_private_addresses: bool,
    /// The `web.search` backend.
    pub search: WebSearchConfig,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_domains: Vec::new(),
            max_fetch_bytes: 1_000_000,
            timeout_secs: 20,
            block_private_addresses: true,
            search: WebSearchConfig::default(),
        }
    }
}

/// One MCP server the agent connects to as a client.
///
/// `stdio` spawns a subprocess; `http` reaches a streamable-HTTP endpoint. An
/// `http` server's `auth`, when set, is a [`SecretRef`] resolved to a bearer key
/// at connect time — never stored in the clear.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpServerEntry {
    Stdio {
        name: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Http {
        name: String,
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        auth: Option<SecretRef>,
    },
}

impl McpServerEntry {
    /// The server's name (its tool namespace).
    pub fn name(&self) -> &str {
        match self {
            Self::Stdio { name, .. } | Self::Http { name, .. } => name,
        }
    }
}

/// Model Context Protocol client configuration.
///
/// Off by default, like `web`: no server is contacted until `enabled` is set.
/// Each server's tools are offered to the model namespaced as `mcp.<server>.<tool>`
/// and gated by the same policy as any tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    pub enabled: bool,
    /// The per-request (and connect) timeout, in seconds.
    pub timeout_secs: u64,
    /// The servers to connect.
    pub servers: Vec<McpServerEntry>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_secs: 30,
            servers: Vec::new(),
        }
    }
}

/// Retrieval (RAG) configuration.
///
/// Always available (retrieval reads the profile's own index); these only tune
/// it. Chunking is applied at ingest, `top_k` at search.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RagConfig {
    /// Default number of passages a search returns.
    pub top_k: usize,
    /// The most characters in one indexed chunk.
    pub max_chunk_chars: usize,
    /// How many characters consecutive chunks overlap.
    pub chunk_overlap_chars: usize,
}

impl Default for RagConfig {
    fn default() -> Self {
        Self {
            top_k: 5,
            max_chunk_chars: 1200,
            chunk_overlap_chars: 200,
        }
    }
}

/// Filesystem and terminal tool configuration.
///
/// Off by default, like `web`: `fs.*` and `terminal.run` are declared to the
/// model but refuse to run until `enabled` is set. The tools are confined to a
/// single `workspace` directory (the per-profile `workspace/` when unset), and
/// `terminal.run` needs `allow_terminal` on top of `enabled` — running a program
/// is a strictly larger grant than reading files, so it is opted into separately.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub enabled: bool,
    /// The confinement root; the per-profile `workspace/` directory when unset.
    pub workspace: Option<String>,
    /// The most bytes a single `fs.read`/`fs.write` may move.
    pub max_file_bytes: usize,
    /// Allow `terminal.run` (requires `enabled` too).
    pub allow_terminal: bool,
    /// The wall-clock timeout for one `terminal.run`, in seconds.
    pub terminal_timeout_secs: u64,
    /// When non-empty, only these program names may be run by `terminal.run`.
    pub terminal_allowlist: Vec<String>,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            workspace: None,
            max_file_bytes: 1_000_000,
            allow_terminal: false,
            terminal_timeout_secs: 30,
            terminal_allowlist: Vec::new(),
        }
    }
}

/// The whole typed configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub inference: InferenceConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub web: WebConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub rag: RagConfig,
    /// Top-level keys this build does not understand, preserved across a save.
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

/// Why a config was rejected.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("the config is invalid: {0}")]
    Invalid(String),
    #[error("could not read the config at {path}: {reason}")]
    Unreadable { path: PathBuf, reason: String },
    #[error("could not write the config at {path}: {reason}")]
    Unwritable { path: PathBuf, reason: String },
}

impl Config {
    /// Reject a config that could not be acted on.
    ///
    /// Checks the invariants the loop and provider depend on, so a bad value is
    /// caught at load time with a message that says what to fix, rather than as
    /// a confusing failure much later.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.inference.provider.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "inference.provider is empty".to_owned(),
            ));
        }
        let url = self.inference.base_url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(ConfigError::Invalid(format!(
                "inference.base_url must be an http(s) URL, got {:?}",
                self.inference.base_url
            )));
        }
        if self.agent.max_turns == 0 {
            return Err(ConfigError::Invalid(
                "agent.max_turns must be at least 1".to_owned(),
            ));
        }
        if self.agent.max_tool_calls == 0 {
            return Err(ConfigError::Invalid(
                "agent.max_tool_calls must be at least 1".to_owned(),
            ));
        }
        if self.web.enabled {
            if self.web.max_fetch_bytes == 0 {
                return Err(ConfigError::Invalid(
                    "web.max_fetch_bytes must be at least 1".to_owned(),
                ));
            }
            if self.web.timeout_secs == 0 {
                return Err(ConfigError::Invalid(
                    "web.timeout_secs must be at least 1".to_owned(),
                ));
            }
            if self.web.search.query_param.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "web.search.query_param must not be empty".to_owned(),
                ));
            }
            if let Some(endpoint) = &self.web.search.endpoint {
                let endpoint = endpoint.trim();
                if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
                    return Err(ConfigError::Invalid(format!(
                        "web.search.endpoint must be an http(s) URL, got {:?}",
                        self.web.search.endpoint
                    )));
                }
            }
        }
        if self.tools.enabled {
            if self.tools.max_file_bytes == 0 {
                return Err(ConfigError::Invalid(
                    "tools.max_file_bytes must be at least 1".to_owned(),
                ));
            }
            if self.tools.terminal_timeout_secs == 0 {
                return Err(ConfigError::Invalid(
                    "tools.terminal_timeout_secs must be at least 1".to_owned(),
                ));
            }
        }
        if self.mcp.enabled {
            if self.mcp.timeout_secs == 0 {
                return Err(ConfigError::Invalid(
                    "mcp.timeout_secs must be at least 1".to_owned(),
                ));
            }
            for server in &self.mcp.servers {
                if server.name().trim().is_empty() {
                    return Err(ConfigError::Invalid(
                        "an mcp server has an empty name".to_owned(),
                    ));
                }
                match server {
                    McpServerEntry::Stdio { command, name, .. } => {
                        if command.trim().is_empty() {
                            return Err(ConfigError::Invalid(format!(
                                "mcp server {name:?} has an empty command"
                            )));
                        }
                    }
                    McpServerEntry::Http { url, name, .. } => {
                        let url = url.trim();
                        if !(url.starts_with("http://") || url.starts_with("https://")) {
                            return Err(ConfigError::Invalid(format!(
                                "mcp server {name:?} url must be an http(s) URL"
                            )));
                        }
                    }
                }
            }
        }
        if self.rag.top_k == 0 {
            return Err(ConfigError::Invalid(
                "rag.top_k must be at least 1".to_owned(),
            ));
        }
        if self.rag.max_chunk_chars == 0 {
            return Err(ConfigError::Invalid(
                "rag.max_chunk_chars must be at least 1".to_owned(),
            ));
        }
        Ok(())
    }

    /// A JSON view with every secret shown as its reference, never its value.
    ///
    /// This is what `config show` prints. Because secrets are only ever stored
    /// as references, redaction is a matter of rendering the reference form —
    /// there is no value here to leak.
    pub fn redacted_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let Some(api_key) = &self.inference.api_key
            && let Some(inference) = value.get_mut("inference")
            && let Some(object) = inference.as_object_mut()
        {
            object.insert(
                "api_key".to_owned(),
                serde_json::Value::String(api_key.redacted()),
            );
        }
        if let Some(api_key) = &self.web.search.api_key
            && let Some(search) = value
                .get_mut("web")
                .and_then(|web| web.get_mut("search"))
                .and_then(|search| search.as_object_mut())
        {
            search.insert(
                "api_key".to_owned(),
                serde_json::Value::String(api_key.redacted()),
            );
        }
        value
    }
}

/// The config file, read and written whole.
#[derive(Clone, Debug)]
pub struct ConfigStore {
    path: PathBuf,
}

/// The on-disk envelope: a version, the config, and any unknown top-level keys.
#[derive(Serialize, Deserialize)]
struct ConfigFile {
    #[serde(default = "one")]
    version: u32,
    #[serde(flatten)]
    config: Config,
}

fn one() -> u32 {
    FORMAT_VERSION
}

impl ConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The store for the config file under a resolved home.
    pub fn at(paths: &crate::paths::LightagentPaths) -> Self {
        Self::new(paths.config_file())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the config, or the defaults when none has been written.
    ///
    /// A file that will not parse is an error, never a silent reset: quietly
    /// treating a corrupt config as "defaults" would hide a real problem behind
    /// a working-looking agent pointed somewhere unexpected.
    pub fn load(&self) -> Result<Config, ConfigError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(err) => {
                return Err(ConfigError::Unreadable {
                    path: self.path.clone(),
                    reason: err.to_string(),
                });
            }
        };
        let file: ConfigFile =
            serde_json::from_slice(&bytes).map_err(|err| ConfigError::Unreadable {
                path: self.path.clone(),
                reason: err.to_string(),
            })?;
        Ok(file.config)
    }

    /// Validate, then write the config atomically and owner-only.
    pub fn save(&self, config: &Config) -> Result<(), ConfigError> {
        config.validate()?;
        let file = ConfigFile {
            version: FORMAT_VERSION,
            config: config.clone(),
        };
        let mut bytes =
            serde_json::to_vec_pretty(&file).map_err(|err| ConfigError::Unwritable {
                path: self.path.clone(),
                reason: err.to_string(),
            })?;
        bytes.push(b'\n');
        paths::write_private(&self.path, &bytes).map_err(|err| ConfigError::Unwritable {
            path: self.path.clone(),
            reason: err.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::RunId;

    fn scratch() -> PathBuf {
        std::env::temp_dir().join(format!("lightagent-config-{}", RunId::new().as_str()))
    }

    #[test]
    fn defaults_are_valid() {
        Config::default()
            .validate()
            .expect("defaults must validate");
        assert_eq!(Config::default().inference.provider, "lightweight");
        assert_eq!(
            Config::default().security.approval_policy,
            ApprovalPolicy::Balanced
        );
    }

    #[test]
    fn config_round_trip() {
        let dir = scratch();
        let store = ConfigStore::new(dir.join("config.json"));
        let mut config = Config::default();
        config.inference.model = Some("lfm2@8k".to_owned());
        config.security.approval_policy = ApprovalPolicy::Strict;
        config.web.allow_domains.push("example.com".to_owned());

        store.save(&config).expect("save");
        let loaded = store.load().expect("load");
        assert_eq!(loaded, config);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validation_rejects_a_bad_base_url() {
        let mut config = Config::default();
        config.inference.base_url = "ftp://nope".to_owned();
        assert!(config.validate().is_err());

        config.inference.base_url = "http://127.0.0.1:11434".to_owned();
        config.agent.max_turns = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn web_validation_only_bites_when_enabled() {
        let mut config = Config::default();
        // A disabled web section is never validated, however odd its fields.
        config.web.timeout_secs = 0;
        config.web.search.endpoint = Some("not-a-url".to_owned());
        config
            .validate()
            .expect("a disabled web section is not validated");

        config.web.enabled = true;
        assert!(config.validate().is_err(), "timeout_secs 0 is rejected");
        config.web.timeout_secs = 20;
        assert!(
            config.validate().is_err(),
            "a non-http endpoint is rejected"
        );
        config.web.search.endpoint = Some("https://searx.example/search".to_owned());
        config
            .validate()
            .expect("a valid enabled web section validates");
    }

    #[test]
    fn rag_validation_rejects_zero_bounds() {
        let mut config = Config::default();
        config.validate().expect("defaults validate");
        config.rag.top_k = 0;
        assert!(config.validate().is_err());
        config.rag.top_k = 5;
        config.rag.max_chunk_chars = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn mcp_validation_checks_servers_when_enabled() {
        let mut config = Config::default();
        config.mcp.servers.push(McpServerEntry::Http {
            name: "bad".to_owned(),
            url: "not-a-url".to_owned(),
            headers: BTreeMap::new(),
            auth: None,
        });
        config
            .validate()
            .expect("a disabled mcp section is not validated");

        config.mcp.enabled = true;
        assert!(config.validate().is_err(), "a non-http url is rejected");

        config.mcp.servers.clear();
        config.mcp.servers.push(McpServerEntry::Stdio {
            name: "git".to_owned(),
            command: "mcp-git".to_owned(),
            args: vec!["--stdio".to_owned()],
            env: BTreeMap::new(),
        });
        config.validate().expect("a valid stdio server validates");
    }

    #[test]
    fn tools_validation_only_bites_when_enabled() {
        let mut config = Config::default();
        config.tools.max_file_bytes = 0;
        config
            .validate()
            .expect("a disabled tools section is not validated");

        config.tools.enabled = true;
        assert!(config.validate().is_err(), "max_file_bytes 0 is rejected");
        config.tools.max_file_bytes = 1_000;
        config.tools.terminal_timeout_secs = 0;
        assert!(
            config.validate().is_err(),
            "terminal_timeout_secs 0 is rejected"
        );
        config.tools.terminal_timeout_secs = 30;
        config
            .validate()
            .expect("a valid enabled tools section validates");
    }

    #[test]
    fn a_search_key_is_redacted_and_round_trips() {
        let mut config = Config::default();
        config.web.enabled = true;
        config.web.search.endpoint = Some("https://searx.example/search".to_owned());
        config.web.search.api_key = Some(SecretRef::env("SEARCH_TOKEN"));

        let redacted = config.redacted_json().to_string();
        assert!(redacted.contains("${env:SEARCH_TOKEN}"));

        let json = serde_json::to_string(&config).expect("serialize");
        let back: Config = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, config);
    }

    #[test]
    fn secret_redaction_never_leaks_a_value() {
        // The reference names an env var; neither the stored config nor its
        // redacted view carries the resolved value. `PATH` is used only because
        // it is reliably set and non-empty in a test environment — the point is
        // that its value never appears in what we persist or display.
        let var = "PATH";
        let value = std::env::var(var).expect("PATH is set in a test environment");
        assert!(!value.is_empty());

        let mut config = Config::default();
        config.inference.api_key = Some(SecretRef::env(var));

        let stored = serde_json::to_string(&config).expect("serialize");
        assert!(
            !stored.contains(&value),
            "the resolved value leaked into storage"
        );
        assert!(stored.contains(var), "the reference is kept, not the value");

        let redacted = config.redacted_json().to_string();
        assert!(
            !redacted.contains(&value),
            "the resolved value leaked into the redacted view"
        );
        assert!(redacted.contains("${env:PATH}"));

        // The value is only reachable by explicit resolution.
        assert_eq!(
            config
                .inference
                .api_key
                .as_ref()
                .and_then(SecretRef::resolve),
            Some(value)
        );
    }

    #[test]
    fn an_unknown_top_level_key_survives_a_save() {
        let dir = scratch();
        let path = dir.join("config.json");
        // Write a file carrying a key this build has no field for.
        paths::write_private(
            &path,
            br#"{"version":1,"inference":{"provider":"lightweight","base_url":"http://127.0.0.1:11434","device":"auto","allow_cpu_fallback":true},"future_section":{"kept":true}}"#,
        )
        .expect("seed");

        let store = ConfigStore::new(&path);
        let config = store.load().expect("load");
        assert!(config.unknown.contains_key("future_section"));

        store.save(&config).expect("save");
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(
            raw.contains("future_section"),
            "an unknown key was dropped: {raw}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_override_places_config_under_the_home() {
        let home = scratch();
        let paths = crate::paths::LightagentPaths::rooted_at(&home);
        let store = ConfigStore::at(&paths);
        assert!(store.path().starts_with(&home));
        store.save(&Config::default()).expect("save");
        assert!(store.path().exists());
        std::fs::remove_dir_all(&home).ok();
    }
}
