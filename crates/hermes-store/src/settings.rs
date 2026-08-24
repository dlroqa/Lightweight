//! Settings, on disk.
//!
//! `paths.settings_file()` was chosen in M0 and nothing had ever written to it.
//!
//! The file has two halves, and the split is the point:
//!
//! * **`gateway` is typed, and every field in it is acted on.** A setting the
//!   gateway stores but never reads is the same mistake as a control on screen
//!   that changes nothing — it looks like a decision the user made and is not.
//!   So this half stays small and grows only when something starts obeying it.
//! * **`ui` is opaque.** Theme, accent, which panel was open: the gateway has
//!   no opinion about any of it and should not pretend to. Keeping it as an
//!   unexamined object means the panel can add a preference without a change
//!   here, and means this crate never has to have a view on what "compact mode"
//!   is.
//!
//! Unknown keys in either half are preserved across a write. A user who runs a
//! newer panel against an older gateway must not have their preferences
//! silently deleted by the older one saving over them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic;
use crate::error::StoreError;

const FORMAT_VERSION: u32 = 1;

/// Settings the gateway itself acts on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewaySettings {
    /// Whether conversations are written to disk at all.
    ///
    /// On by default, because a chat panel that forgets everything the moment
    /// it closes is not what anyone means by a chat panel. Off is honoured
    /// completely: nothing is written, rather than written and hidden.
    pub keep_history: bool,
    /// Context to load a model with when a request does not say.
    ///
    /// `None` keeps the behaviour every existing deployment has: the largest
    /// context this machine can safely give the model. A number here is the
    /// user overriding that, and it is still checked against the RAM estimate
    /// like any other context — this makes a load smaller than it might have
    /// been, never larger than it should be.
    pub default_n_ctx: Option<u32>,
}

impl Default for GatewaySettings {
    fn default() -> Self {
        Self {
            keep_history: true,
            default_n_ctx: None,
        }
    }
}

/// Everything persisted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub gateway: GatewaySettings,
    /// The panel's own preferences, never interpreted here.
    #[serde(default = "empty_object")]
    pub ui: serde_json::Value,
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            gateway: GatewaySettings::default(),
            ui: empty_object(),
        }
    }
}

/// The file on disk.
#[derive(Serialize, Deserialize)]
struct SettingsFile {
    version: u32,
    #[serde(flatten)]
    settings: Settings,
    /// Anything this build does not know about, kept so that saving does not
    /// delete a newer panel's preferences.
    #[serde(flatten)]
    unknown: serde_json::Map<String, serde_json::Value>,
}

/// The settings file, read and written whole.
#[derive(Clone, Debug)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the settings, or the defaults when none have been saved.
    ///
    /// A file that will not parse is an error rather than a silent reset. The
    /// alternative — treating unreadable settings as "no settings" — answers a
    /// corrupt file by discarding the user's configuration and then overwriting
    /// it with defaults on the next save.
    pub fn load(&self) -> Result<Settings, StoreError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Settings::default());
            }
            Err(err) => return Err(StoreError::io("reading settings", err)),
        };

        let file: SettingsFile =
            serde_json::from_slice(&bytes).map_err(|err| StoreError::Unreadable {
                what: "settings",
                path: self.path.clone(),
                reason: err.to_string(),
            })?;
        Ok(file.settings)
    }

    /// Write the settings, keeping any keys this build does not understand.
    pub fn save(&self, settings: &Settings) -> Result<(), StoreError> {
        // Read first so that unknown keys survive. A failure to read is not
        // fatal here: it means there is nothing to preserve.
        let unknown = self.unknown_keys().unwrap_or_default();

        let file = SettingsFile {
            version: FORMAT_VERSION,
            settings: settings.clone(),
            unknown,
        };
        let mut bytes = serde_json::to_vec_pretty(&file)
            .map_err(|err| StoreError::io("encoding settings", std::io::Error::other(err)))?;
        bytes.push(b'\n');
        atomic::write_private(&self.path, &bytes)
    }

    /// Top-level keys in the file that this build has no field for.
    fn unknown_keys(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        let bytes = std::fs::read(&self.path).ok()?;
        let mut value: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&bytes).ok()?;
        for known in ["version", "gateway", "ui"] {
            value.remove(known);
        }
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> (PathBuf, SettingsStore) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("hermes-settings-{tag}-{unique}"));
        let path = dir.join("settings.json");
        (dir, SettingsStore::new(path))
    }

    #[test]
    fn a_gateway_with_no_settings_file_gets_the_defaults() {
        let (_dir, store) = store("defaults");
        let settings = store.load().expect("load");
        assert!(settings.gateway.keep_history, "history is on by default");
        assert_eq!(settings.gateway.default_n_ctx, None);
        assert!(settings.ui.is_object());
    }

    #[test]
    fn settings_survive_a_round_trip() {
        let (dir, store) = store("roundtrip");
        let settings = Settings {
            gateway: GatewaySettings {
                keep_history: false,
                default_n_ctx: Some(8192),
            },
            ui: serde_json::json!({"theme": "dark", "accent": "#2563EB"}),
        };

        store.save(&settings).expect("save");
        assert_eq!(store.load().expect("load"), settings);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_ui_half_is_kept_without_being_understood() {
        // The gateway must never need a code change to let the panel remember
        // something new.
        let (dir, store) = store("opaque");
        let settings = Settings {
            ui: serde_json::json!({
                "somethingInventedLater": {"nested": [1, 2, 3]},
            }),
            ..Settings::default()
        };
        store.save(&settings).expect("save");

        let read = store.load().expect("load");
        assert_eq!(read.ui["somethingInventedLater"]["nested"][2], 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_older_build_does_not_delete_a_newer_ones_settings() {
        // A user running a newer panel, then an older gateway, must not lose
        // configuration to the older one saving over it.
        let (dir, store) = store("forward");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            store.path(),
            br#"{"version":1,"gateway":{"keep_history":true},"ui":{},"shell":{"tray":true}}"#,
        )
        .expect("write");

        let settings = store.load().expect("load");
        store.save(&settings).expect("save");

        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store.path()).expect("read")).expect("json");
        assert_eq!(
            raw["shell"]["tray"], true,
            "an unknown top-level key was dropped: {raw}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_partial_file_still_yields_the_defaults_for_what_is_absent() {
        let (dir, store) = store("partial");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            store.path(),
            br#"{"version":1,"gateway":{"default_n_ctx":4096}}"#,
        )
        .expect("write");

        let settings = store.load().expect("load");
        assert_eq!(settings.gateway.default_n_ctx, Some(4096));
        // Not mentioned in the file, so it takes its default rather than false.
        assert!(settings.gateway.keep_history);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_file_is_an_error_rather_than_a_silent_reset() {
        // Treating unreadable settings as "no settings" answers a corrupt file
        // by discarding the user's configuration, and then overwrites it with
        // defaults on the next save.
        let (dir, store) = store("corrupt");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(store.path(), b"{ not json").expect("write");

        let err = store.load().expect_err("corrupt settings are an error");
        assert!(matches!(err, StoreError::Unreadable { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }
}
