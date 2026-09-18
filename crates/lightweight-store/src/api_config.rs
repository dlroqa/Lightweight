//! The gateway's own configuration: what to bind, on disk.
//!
//! `paths.api_config_file()` was chosen in M0 and, like the settings file
//! before it, nothing had ever written to it. This is what fills it.
//!
//! It is deliberately separate from [`crate::settings`]. That file is the
//! panel's preferences — theme, whether history is kept — and is safe for a
//! user to read, paste into a bug report, or sync between machines. This file
//! decides which addresses the gateway answers on, which is a security-relevant
//! choice, and keeping the two apart is what makes "show me your settings" a
//! safe thing to ask. The credential itself lives in neither: see
//! [`crate::api_keys`].
//!
//! Like settings, it stores **only what the gateway obeys** — the bind hosts
//! and the port. A field the gateway persists but never reads is a control that
//! looks like a decision and is not, so this grows only when something starts
//! acting on it. Concurrency and the queue timeout are derived or constant
//! today, and stay out until that changes.
//!
//! Hosts are stored as the strings the user typed — a name or a literal — not
//! as resolved addresses. An overlay network can reissue an address, and a name
//! usually survives it; resolving at every start rather than freezing an
//! address into the file is the same choice `hermes serve` already makes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic;
use crate::error::StoreError;

const FORMAT_VERSION: u32 = 1;

/// What the gateway binds to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Addresses or names to bind, in the order given. Empty means "no opinion"
    /// — the caller falls back to its own default, which is loopback.
    pub hosts: Vec<String>,
    /// The port to bind. `None` means "no opinion", and the caller's default
    /// (11434) applies.
    pub port: Option<u16>,
}

/// The file on disk.
#[derive(Serialize, Deserialize)]
struct ApiConfigFile {
    version: u32,
    #[serde(flatten)]
    config: ApiConfig,
    /// Keys a newer build wrote that this one has no field for, kept so a save
    /// here does not delete them.
    #[serde(flatten)]
    unknown: serde_json::Map<String, serde_json::Value>,
}

/// The api config file, read and written whole.
#[derive(Clone, Debug)]
pub struct ApiConfigStore {
    path: PathBuf,
}

impl ApiConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the config, or the empty default when none has been saved.
    ///
    /// A file that will not parse is an error, never a silent reset: quietly
    /// treating a corrupt bind list as "no config" would drop the gateway back
    /// to loopback on the next start, which is exactly the silent un-exposure
    /// the address-probe warning exists to catch.
    pub fn load(&self) -> Result<ApiConfig, StoreError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ApiConfig::default());
            }
            Err(err) => return Err(StoreError::io("reading api config", err)),
        };

        let file: ApiConfigFile =
            serde_json::from_slice(&bytes).map_err(|err| StoreError::Unreadable {
                what: "api config",
                path: self.path.clone(),
                reason: err.to_string(),
            })?;
        Ok(file.config)
    }

    /// Write the config, keeping any keys this build does not understand.
    pub fn save(&self, config: &ApiConfig) -> Result<(), StoreError> {
        let unknown = self.unknown_keys().unwrap_or_default();
        let file = ApiConfigFile {
            version: FORMAT_VERSION,
            config: config.clone(),
            unknown,
        };
        let mut bytes = serde_json::to_vec_pretty(&file)
            .map_err(|err| StoreError::io("encoding api config", std::io::Error::other(err)))?;
        bytes.push(b'\n');
        atomic::write_private(&self.path, &bytes)
    }

    fn unknown_keys(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        let bytes = std::fs::read(&self.path).ok()?;
        let mut value: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&bytes).ok()?;
        for known in ["version", "hosts", "port"] {
            value.remove(known);
        }
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> (PathBuf, ApiConfigStore) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("hermes-apicfg-{tag}-{unique}"));
        let path = dir.join("api.json");
        (dir, ApiConfigStore::new(path))
    }

    #[test]
    fn no_file_is_the_empty_default_not_an_error() {
        let (_dir, store) = store("default");
        let config = store.load().expect("load");
        assert!(config.hosts.is_empty());
        assert_eq!(config.port, None);
    }

    #[test]
    fn config_survives_a_round_trip() {
        let (dir, store) = store("roundtrip");
        let config = ApiConfig {
            hosts: vec!["127.0.0.1".to_owned(), "my-mesh-name".to_owned()],
            port: Some(11434),
        };
        store.save(&config).expect("save");
        assert_eq!(store.load().expect("load"), config);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_key_from_a_newer_build_survives_a_save() {
        let (dir, store) = store("forward");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            store.path(),
            br#"{"version":1,"hosts":["127.0.0.1"],"tls":{"cert":"later"}}"#,
        )
        .expect("write");

        let config = store.load().expect("load");
        store.save(&config).expect("save");

        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store.path()).expect("read")).expect("json");
        assert_eq!(
            raw["tls"]["cert"], "later",
            "an unknown key was dropped: {raw}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_file_is_an_error_rather_than_a_silent_reset() {
        let (dir, store) = store("corrupt");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(store.path(), b"{ not json").expect("write");
        let err = store.load().expect_err("corrupt config is an error");
        assert!(matches!(err, StoreError::Unreadable { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }
}
