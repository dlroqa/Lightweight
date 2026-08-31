//! API keys, hashed, on disk.
//!
//! The gateway used to have exactly one key, and only ever in memory or the
//! environment. Handing a gateway to another machine wants more than that: a
//! key per consumer, each nameable and revocable on its own, so that a harness
//! that leaks its key can be cut off without disturbing the others.
//!
//! **Only the hash is stored.** A key is shown once, at the moment it is
//! created, and never again — the file keeps a SHA-256 of it and a short prefix
//! for display, nothing that can be presented as a credential. The cost is
//! honest and is stated at creation: a key that is lost is replaced, not
//! recovered, because there is nothing here to recover it from. The gain is
//! that this file, unlike a plaintext key store, is worthless to anyone who
//! reads it.
//!
//! The file is written owner-only like every other file in this crate, but that
//! is not why the hash is here: a `0600` file full of live keys is still a live
//! key store one `cat` away from disaster. Hashing is what makes the file safe
//! to *have*, not merely safe to hide.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::atomic;
use crate::error::StoreError;

const FORMAT_VERSION: u32 = 1;

/// The prefix every key carries.
///
/// Not decoration: several OpenAI-compatible clients reject a key that does not
/// begin `sk-`, which is exactly the harness that cannot be wired up if the
/// shape is wrong. `-lw-` marks it as this gateway's rather than a real
/// OpenAI key, without any client having to be told.
const KEY_PREFIX: &str = "sk-lw-";

/// The secret half of a key: 24 bytes of OS entropy, hex encoded.
///
/// Kept as its own function because it is also what a bare `--api-key` wants
/// suggested when a bind is exposed without one — the same entropy, without the
/// `sk-lw-` dressing the key store adds.
pub fn generate_secret() -> Result<String, std::io::Error> {
    let mut bytes = [0_u8; 24];
    getrandom::fill(&mut bytes).map_err(|err| std::io::Error::other(err.to_string()))?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_hex(input: &str) -> String {
    hex(&Sha256::digest(input.as_bytes()))
}

/// Compare two equal-length byte strings without an early return.
///
/// The hashes here are not secret in the way a raw key is, but a compare that
/// stops at the first differing byte still tells a caller how far its guess got,
/// and a constant-time compare costs nothing to write. Unequal lengths are a
/// definite mismatch and may return at once — the length of a hash is not a
/// secret.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// How much a single key may be used. `None` on either axis is unlimited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimit {
    pub per_minute: Option<u32>,
    pub per_day: Option<u32>,
}

impl RateLimit {
    pub const fn is_unlimited(self) -> bool {
        self.per_minute.is_none() && self.per_day.is_none()
    }
}

/// One key, as stored — everything but the key itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    /// A stable handle for revoke and limit edits, never the secret.
    pub id: String,
    /// What the user called it. May be empty.
    pub name: String,
    /// The displayable head of the key, e.g. `sk-lw-5e43033a`.
    pub prefix: String,
    /// SHA-256 of the full key, hex. The only trace of the secret kept.
    pub hash: String,
    /// When it was created, unix seconds.
    pub created_at: u64,
    /// Its usage ceiling.
    #[serde(default)]
    pub limit: RateLimit,
}

/// The file on disk.
#[derive(Default, Serialize, Deserialize)]
struct ApiKeysFile {
    version: u32,
    #[serde(default)]
    keys: Vec<ApiKeyRecord>,
    #[serde(flatten)]
    unknown: serde_json::Map<String, serde_json::Value>,
}

/// The api keys file, read and written whole.
#[derive(Clone, Debug)]
pub struct ApiKeyStore {
    path: PathBuf,
}

impl ApiKeyStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every key's record, secrets excluded (there are none to exclude).
    pub fn list(&self) -> Result<Vec<ApiKeyRecord>, StoreError> {
        Ok(self.read()?.keys)
    }

    /// Whether any key at all is configured.
    ///
    /// The auth layer asks this to decide whether an exposed bind has a
    /// credential without loading every record.
    pub fn any(&self) -> Result<bool, StoreError> {
        Ok(!self.read()?.keys.is_empty())
    }

    /// Mint a new key. Returns the record and the plaintext key, which is the
    /// only time the plaintext exists outside the caller's request.
    pub fn create(
        &self,
        name: &str,
        limit: RateLimit,
    ) -> Result<(ApiKeyRecord, String), StoreError> {
        let secret = generate_secret().map_err(|err| StoreError::io("generating a key", err))?;
        let full = format!("{KEY_PREFIX}{secret}");
        let record = ApiKeyRecord {
            id: new_id().map_err(|err| StoreError::io("generating a key id", err))?,
            name: name.trim().to_owned(),
            prefix: format!("{KEY_PREFIX}{}", &secret[..8]),
            hash: sha256_hex(&full),
            created_at: now_unix(),
            limit,
        };

        let mut file = self.read()?;
        file.keys.push(record.clone());
        self.write(&file)?;
        Ok((record, full))
    }

    /// Remove a key by id. Returns whether one was there to remove.
    pub fn revoke(&self, id: &str) -> Result<bool, StoreError> {
        let mut file = self.read()?;
        let before = file.keys.len();
        file.keys.retain(|record| record.id != id);
        let removed = file.keys.len() != before;
        if removed {
            self.write(&file)?;
        }
        Ok(removed)
    }

    /// Change a key's usage ceiling. Returns whether the key was found.
    pub fn set_limit(&self, id: &str, limit: RateLimit) -> Result<bool, StoreError> {
        let mut file = self.read()?;
        let Some(record) = file.keys.iter_mut().find(|record| record.id == id) else {
            return Ok(false);
        };
        record.limit = limit;
        self.write(&file)?;
        Ok(true)
    }

    /// Find the key a presented token matches, if any, reading from disk.
    pub fn verify(&self, presented: &str) -> Result<Option<ApiKeyRecord>, StoreError> {
        Ok(verify_against(&self.read()?.keys, presented))
    }

    fn read(&self) -> Result<ApiKeysFile, StoreError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ApiKeysFile {
                    version: FORMAT_VERSION,
                    ..ApiKeysFile::default()
                });
            }
            Err(err) => return Err(StoreError::io("reading api keys", err)),
        };
        serde_json::from_slice(&bytes).map_err(|err| StoreError::Unreadable {
            what: "api keys",
            path: self.path.clone(),
            reason: err.to_string(),
        })
    }

    fn write(&self, file: &ApiKeysFile) -> Result<(), StoreError> {
        let mut file = ApiKeysFile {
            version: FORMAT_VERSION,
            keys: file.keys.clone(),
            unknown: file.unknown.clone(),
        };
        // Newest last, so the file reads in creation order.
        file.keys.sort_by_key(|record| record.created_at);
        let mut bytes = serde_json::to_vec_pretty(&file)
            .map_err(|err| StoreError::io("encoding api keys", std::io::Error::other(err)))?;
        bytes.push(b'\n');
        atomic::write_private(&self.path, &bytes)
    }
}

/// Find the key in `keys` a presented token matches, if any.
///
/// The token is hashed once and compared against every stored hash with an
/// accumulating, constant-time compare — never returning early on a match, so
/// the time taken reveals neither which key matched nor how many are held. Kept
/// as a free function over a slice so the auth layer can verify against keys it
/// already holds in memory rather than reading the file on every request.
pub fn verify_against(keys: &[ApiKeyRecord], presented: &str) -> Option<ApiKeyRecord> {
    let hash = sha256_hex(presented);
    let mut matched: Option<ApiKeyRecord> = None;
    for record in keys {
        if constant_time_eq(&record.hash, &hash) {
            matched = Some(record.clone());
        }
    }
    matched
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn new_id() -> Result<String, std::io::Error> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).map_err(|err| std::io::Error::other(err.to_string()))?;
    Ok(hex(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> (PathBuf, ApiKeyStore) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("hermes-apikeys-{tag}-{unique}"));
        (dir.clone(), ApiKeyStore::new(dir.join("api-keys.json")))
    }

    #[test]
    fn a_created_key_verifies_and_its_secret_is_never_stored() {
        let (dir, store) = store("create");
        let (record, full) = store
            .create("harness", RateLimit::default())
            .expect("create");
        assert!(full.starts_with("sk-lw-"));
        assert!(record.prefix.starts_with("sk-lw-"));

        // The file holds no copy of the plaintext, only its hash and prefix.
        let raw = std::fs::read_to_string(store.path()).expect("read");
        assert!(!raw.contains(&full), "the plaintext key is on disk");
        assert!(raw.contains(&record.hash));

        // The right token verifies; a wrong one does not.
        assert_eq!(
            store.verify(&full).expect("verify").map(|r| r.id),
            Some(record.id)
        );
        assert!(store.verify("sk-lw-nope").expect("verify").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_revoked_key_stops_verifying() {
        let (dir, store) = store("revoke");
        let (record, full) = store.create("temp", RateLimit::default()).expect("create");
        assert!(store.verify(&full).expect("verify").is_some());

        assert!(store.revoke(&record.id).expect("revoke"));
        assert!(store.verify(&full).expect("verify").is_none());
        assert!(
            !store.revoke(&record.id).expect("revoke"),
            "second revoke is a no-op"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_limit_can_be_set_after_creation() {
        let (dir, store) = store("limit");
        let (record, _) = store.create("k", RateLimit::default()).expect("create");
        let limit = RateLimit {
            per_minute: Some(60),
            per_day: Some(2000),
        };
        assert!(store.set_limit(&record.id, limit).expect("set"));
        let read = store.list().expect("list");
        assert_eq!(read[0].limit, limit);
        assert!(!store.set_limit("missing", limit).expect("set"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_top_level_key_survives_a_write() {
        let (dir, store) = store("forward");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            store.path(),
            br#"{"version":1,"keys":[],"rotation":{"days":90}}"#,
        )
        .expect("write");
        store.create("k", RateLimit::default()).expect("create");
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store.path()).expect("read")).expect("json");
        assert_eq!(raw["rotation"]["days"], 90, "unknown key dropped: {raw}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_file_is_an_error() {
        let (dir, store) = store("corrupt");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(store.path(), b"{ not json").expect("write");
        assert!(matches!(
            store.list().expect_err("err"),
            StoreError::Unreadable { .. }
        ));
        std::fs::remove_dir_all(&dir).ok();
    }
}
