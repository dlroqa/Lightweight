//! Agent profiles — the "bots".
//!
//! A profile is a fully isolated agent identity, mirroring a Hermes bot: an id,
//! a persona (its system prompt), model/provider routing, enabled toolsets, an
//! approval policy and resource limits, each living in its own owner-only
//! subtree under the Lightagent home:
//!
//! ```text
//! <home>/profiles/<id>/
//! ├── profile.json     the typed manifest
//! ├── SOUL.md          the persona / system prompt
//! ├── config.json      per-profile config overrides (optional)
//! ├── .env             secret references, owner-only
//! ├── sessions/        conversation and run history
//! ├── memory/          durable memory scope
//! ├── approvals.jsonl  approval records (Slice 4)
//! ├── history.jsonl    run / tool audit
//! ├── logs/            per-profile logs
//! └── cache/           per-profile reclaimable cache
//! ```
//!
//! Isolation is enforced by two things and no more: a [`ProfileId`] is
//! validated against `^[a-z0-9][a-z0-9_-]{0,63}$`, so it can hold no path
//! separator or `..`, and every path is produced by joining that id under the
//! home. One profile therefore can never name another's directory, and nothing
//! escapes the home.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::ApprovalPolicy;
use crate::limits::RunLimits;
use crate::paths;

/// The longest a profile id may be.
const MAX_ID_LEN: usize = 64;

/// A validated profile identifier.
///
/// The validation is the isolation boundary: an id can contain only lowercase
/// letters, digits, `_` and `-`, must start with a letter or digit, and is at
/// most 64 characters. That excludes `/`, `.` and `..`, so a
/// [`ProfileId`]-derived path is always confined to one profile's directory.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProfileId(String);

impl ProfileId {
    /// Validate and wrap an id.
    pub fn new(id: impl Into<String>) -> Result<Self, ProfileError> {
        let id = id.into();
        if is_valid(&id) {
            Ok(Self(id))
        } else {
            Err(ProfileError::InvalidId { id })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The rule, in one place: `^[a-z0-9][a-z0-9_-]{0,63}$`.
fn is_valid(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_ID_LEN {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl TryFrom<String> for ProfileId {
    type Error = ProfileError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ProfileId> for String {
    fn from(value: ProfileId) -> Self {
        value.0
    }
}

impl std::fmt::Display for ProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which provider backend a profile routes to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// The Lightweight OpenAI-compatible gateway.
    #[default]
    Lightweight,
}

/// Where a profile's turns are run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRouting {
    pub provider: ProviderKind,
    pub model: String,
    /// Override the global base URL; `None` uses the config's.
    #[serde(default)]
    pub base_url: Option<String>,
}

/// The typed profile manifest — everything but the persona, which lives in
/// `SOUL.md`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub id: ProfileId,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The persona / system prompt. Loaded from `SOUL.md`, not the manifest, so
    /// it is skipped in the JSON.
    #[serde(skip)]
    pub persona: String,
    #[serde(default)]
    pub routing: ModelRouting,
    #[serde(default)]
    pub toolsets: Vec<String>,
    #[serde(default)]
    pub approval_policy: ApprovalPolicy,
    #[serde(default)]
    pub limits: RunLimits,
}

impl AgentProfile {
    /// A minimal profile with the given id, persona and model.
    pub fn new(
        id: ProfileId,
        name: impl Into<String>,
        persona: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            description: String::new(),
            persona: persona.into(),
            routing: ModelRouting {
                model: model.into(),
                ..ModelRouting::default()
            },
            toolsets: Vec::new(),
            approval_policy: ApprovalPolicy::default(),
            limits: RunLimits::default(),
        }
    }
}

/// The resolved, confined paths of one profile.
///
/// A handle only ever exposes paths under its own directory, so holding one is
/// holding a capability for exactly one profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileHandle {
    id: ProfileId,
    dir: PathBuf,
}

impl ProfileHandle {
    pub fn id(&self) -> &ProfileId {
        &self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn manifest_file(&self) -> PathBuf {
        self.dir.join("profile.json")
    }

    pub fn soul_file(&self) -> PathBuf {
        self.dir.join("SOUL.md")
    }

    pub fn config_file(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    pub fn env_file(&self) -> PathBuf {
        self.dir.join(".env")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.dir.join("sessions")
    }

    pub fn memory_dir(&self) -> PathBuf {
        self.dir.join("memory")
    }

    pub fn approvals_file(&self) -> PathBuf {
        self.dir.join("approvals.jsonl")
    }

    pub fn history_file(&self) -> PathBuf {
        self.dir.join("history.jsonl")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.dir.join("cache")
    }

    /// The directories that make up a profile's isolated subtree.
    fn subdirs(&self) -> [PathBuf; 4] {
        [
            self.sessions_dir(),
            self.memory_dir(),
            self.logs_dir(),
            self.cache_dir(),
        ]
    }
}

/// Reads and writes profiles under one Lightagent home.
///
/// The home is supplied explicitly, so the whole store is offline-testable: a
/// test points it at a scratch directory and never touches a real
/// installation.
#[derive(Clone, Debug)]
pub struct ProfileStore {
    home: PathBuf,
}

impl ProfileStore {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// The store for a resolved home.
    pub fn at(paths: &crate::paths::LightagentPaths) -> Self {
        Self::new(paths.root())
    }

    fn profiles_dir(&self) -> PathBuf {
        self.home.join("profiles")
    }

    fn active_file(&self) -> PathBuf {
        self.home.join("active_profile")
    }

    /// The confined handle for `id`. Never touches the filesystem.
    pub fn handle(&self, id: &ProfileId) -> ProfileHandle {
        ProfileHandle {
            id: id.clone(),
            dir: self.profiles_dir().join(id.as_str()),
        }
    }

    /// Create a profile's directory and its subtree, owner-only.
    pub fn scaffold(&self, id: &ProfileId) -> Result<ProfileHandle, ProfileError> {
        let handle = self.handle(id);
        paths::create_private_dir(handle.dir()).map_err(profile_io)?;
        for dir in handle.subdirs() {
            paths::create_private_dir(&dir).map_err(profile_io)?;
        }
        Ok(handle)
    }

    /// Persist a profile: its manifest and persona, scaffolding the subtree.
    pub fn save(&self, profile: &AgentProfile) -> Result<(), ProfileError> {
        let handle = self.scaffold(&profile.id)?;
        let mut bytes =
            serde_json::to_vec_pretty(profile).map_err(|err| ProfileError::Corrupt {
                id: profile.id.clone(),
                reason: err.to_string(),
            })?;
        bytes.push(b'\n');
        paths::write_private(&handle.manifest_file(), &bytes).map_err(io_std)?;
        paths::write_private(&handle.soul_file(), profile.persona.as_bytes()).map_err(io_std)?;
        Ok(())
    }

    /// Load a profile: its manifest, with the persona read from `SOUL.md`.
    pub fn load(&self, id: &ProfileId) -> Result<AgentProfile, ProfileError> {
        let handle = self.handle(id);
        let bytes = match std::fs::read(handle.manifest_file()) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(ProfileError::NotFound { id: id.clone() });
            }
            Err(err) => return Err(io_std(err)),
        };
        let mut profile: AgentProfile =
            serde_json::from_slice(&bytes).map_err(|err| ProfileError::Corrupt {
                id: id.clone(),
                reason: err.to_string(),
            })?;
        // The persona is `#[serde(skip)]`, so it comes back empty; fill it from
        // SOUL.md when that file exists.
        profile.persona = match std::fs::read_to_string(handle.soul_file()) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(io_std(err)),
        };
        Ok(profile)
    }

    /// The active profile id, if one is set and still valid.
    pub fn active(&self) -> Result<Option<ProfileId>, ProfileError> {
        match std::fs::read_to_string(self.active_file()) {
            Ok(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(ProfileId::new(trimmed)?))
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(io_std(err)),
        }
    }

    /// Make `id` the active profile. The profile must already exist.
    pub fn set_active(&self, id: &ProfileId) -> Result<(), ProfileError> {
        if !self.handle(id).manifest_file().exists() {
            return Err(ProfileError::NotFound { id: id.clone() });
        }
        paths::write_private(&self.active_file(), id.as_str().as_bytes()).map_err(io_std)
    }

    /// Every profile that has a manifest, in sorted order.
    pub fn list(&self) -> Result<Vec<ProfileId>, ProfileError> {
        let mut ids = Vec::new();
        let entries = match std::fs::read_dir(self.profiles_dir()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
            Err(err) => return Err(io_std(err)),
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(id) = ProfileId::new(name) else {
                continue;
            };
            if self.handle(&id).manifest_file().exists() {
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
    }
}

/// Wrap a paths error as a profile IO error.
fn profile_io(source: paths::PathsError) -> ProfileError {
    ProfileError::Io {
        reason: source.to_string(),
    }
}

/// Wrap a std IO error as a profile IO error.
fn io_std(err: std::io::Error) -> ProfileError {
    ProfileError::Io {
        reason: err.to_string(),
    }
}

/// Why a profile operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error(
        "invalid profile id {id:?}: ids must match ^[a-z0-9][a-z0-9_-]{{0,63}}$ \
         (lowercase letters, digits, '_' and '-', no path separators)"
    )]
    InvalidId { id: String },
    #[error("no profile named {id:?}")]
    NotFound { id: ProfileId },
    #[error("the profile {id:?} manifest is corrupt: {reason}")]
    Corrupt { id: ProfileId, reason: String },
    #[error("a profile store operation failed: {reason}")]
    Io { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::RunId;

    fn scratch_home() -> PathBuf {
        std::env::temp_dir().join(format!("lightagent-profile-{}", RunId::new().as_str()))
    }

    fn sample(id: &str) -> AgentProfile {
        AgentProfile::new(
            ProfileId::new(id).expect("valid id"),
            "Aria",
            "You are Aria, a careful assistant.",
            "lfm2@8k",
        )
    }

    #[test]
    fn profile_id_rejects_traversal() {
        // Neither reaches the filesystem: validation refuses them outright.
        assert!(matches!(
            ProfileId::new("../etc"),
            Err(ProfileError::InvalidId { .. })
        ));
        assert!(matches!(
            ProfileId::new("a/b"),
            Err(ProfileError::InvalidId { .. })
        ));
        assert!(ProfileId::new("").is_err());
        assert!(ProfileId::new("A-caps").is_err());
        assert!(ProfileId::new("-leading").is_err());
        assert!(ProfileId::new("ok_id-9").is_ok());
    }

    #[test]
    fn profile_round_trips_from_disk() {
        let home = scratch_home();
        let store = ProfileStore::new(&home);
        let profile = sample("aria");

        store.save(&profile).expect("save");
        let loaded = store.load(&profile.id).expect("load");
        assert_eq!(loaded, profile);
        assert_eq!(loaded.persona, "You are Aria, a careful assistant.");
        assert_eq!(loaded.routing.model, "lfm2@8k");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn active_profile_selector() {
        let home = scratch_home();
        let store = ProfileStore::new(&home);
        assert_eq!(store.active().expect("active"), None);

        // Cannot activate a profile that does not exist.
        let id = ProfileId::new("aria").expect("id");
        assert!(matches!(
            store.set_active(&id),
            Err(ProfileError::NotFound { .. })
        ));

        store.save(&sample("aria")).expect("save");
        store.set_active(&id).expect("set active");
        assert_eq!(store.active().expect("active"), Some(id));

        let listed = store.list().expect("list");
        assert_eq!(listed, vec![ProfileId::new("aria").expect("id")]);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn two_profiles_get_independent_subtrees() {
        let home = scratch_home();
        let store = ProfileStore::new(&home);
        store.save(&sample("aria")).expect("aria");
        store.save(&sample("boris")).expect("boris");

        let aria = store.handle(&ProfileId::new("aria").expect("id"));
        let boris = store.handle(&ProfileId::new("boris").expect("id"));

        // No shared subtree, and both confined under the home.
        assert!(!aria.dir().starts_with(boris.dir()));
        assert!(!boris.dir().starts_with(aria.dir()));
        assert!(aria.dir().starts_with(&home));
        assert!(boris.dir().starts_with(&home));
        assert!(aria.memory_dir().is_dir());
        assert!(boris.sessions_dir().is_dir());

        let mut listed = store.list().expect("list");
        listed.sort();
        assert_eq!(
            listed,
            vec![
                ProfileId::new("aria").expect("id"),
                ProfileId::new("boris").expect("id")
            ]
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
