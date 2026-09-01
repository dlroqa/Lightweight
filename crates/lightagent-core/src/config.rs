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

/// Web access configuration (Phase 9; a placeholder shape this pass).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub enabled: bool,
    pub allow_domains: Vec<String>,
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
