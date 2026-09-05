//! The session model and its store.

use std::path::PathBuf;
use std::time::SystemTime;

use lightagent_core::ProfileHandle;
use lightagent_core::paths;
use serde::{Deserialize, Serialize};

use crate::error::StoreError;

/// Bytes of randomness in an id. 128 bits: a collision is not a concern.
const ID_BYTES: usize = 16;

/// A session id: 32 lowercase hex characters, generated and never accepted from
/// a caller.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Mint a new id. Infallible: OS entropy when available, a
    /// clock-plus-counter fallback when not.
    pub fn generate() -> Self {
        let mut bytes = [0u8; ID_BYTES];
        if getrandom::fill(&mut bytes).is_err() {
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            bytes.copy_from_slice(&nanos.to_le_bytes()[..ID_BYTES]);
        }
        Self(hex(&bytes))
    }

    /// Parse an id, accepting only the generated shape.
    pub fn parse(value: &str) -> Result<Self, StoreError> {
        let ok = value.len() == ID_BYTES * 2
            && value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        if ok {
            Ok(Self(value.to_owned()))
        } else {
            Err(StoreError::MalformedId(value.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// One message in a session's transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: String,
    pub content: String,
}

impl StoredMessage {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }
}

/// A record of one tool call within a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolHistoryEntry {
    pub tool: String,
    #[serde(default)]
    pub arguments_preview: String,
    /// `"ok"` or `"error"`.
    pub outcome: String,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

/// Metadata for one agent run within a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub started_at: SystemTime,
    #[serde(default)]
    pub ended_at: Option<SystemTime>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub tools: Vec<ToolHistoryEntry>,
}

/// A persisted conversation with its run and tool history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub profile: String,
    #[serde(default)]
    pub title: String,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
    #[serde(default)]
    pub messages: Vec<StoredMessage>,
    #[serde(default)]
    pub runs: Vec<RunRecord>,
}

impl Session {
    /// A fresh, empty session for `profile`.
    pub fn new(profile: impl Into<String>, title: impl Into<String>) -> Self {
        let now = SystemTime::now();
        Self {
            id: SessionId::generate(),
            profile: profile.into(),
            title: title.into(),
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
            runs: Vec::new(),
        }
    }

    /// Append a message and stamp the update time.
    pub fn push_message(&mut self, message: StoredMessage) {
        self.messages.push(message);
        self.updated_at = SystemTime::now();
    }

    /// Append a run record and stamp the update time.
    pub fn push_run(&mut self, run: RunRecord) {
        self.runs.push(run);
        self.updated_at = SystemTime::now();
    }
}

/// A light view of a session for a listing, without its transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub profile: String,
    pub title: String,
    pub updated_at: SystemTime,
    pub message_count: usize,
    pub run_count: usize,
}

impl SessionSummary {
    fn of(session: &Session) -> Self {
        Self {
            id: session.id.clone(),
            profile: session.profile.clone(),
            title: session.title.clone(),
            updated_at: session.updated_at,
            message_count: session.messages.len(),
            run_count: session.runs.len(),
        }
    }
}

/// Reads and writes sessions under one directory (a profile's `sessions/`).
#[derive(Clone, Debug)]
pub struct SessionStore {
    directory: PathBuf,
    keep_history: bool,
}

impl SessionStore {
    /// A store rooted at `directory`, keeping history.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            keep_history: true,
        }
    }

    /// The store for a profile's `sessions/` directory.
    pub fn at_profile(handle: &ProfileHandle) -> Self {
        Self::new(handle.sessions_dir())
    }

    /// Set whether writes are persisted. With history off, `save` is a no-op and
    /// reads still work.
    pub fn keep_history(mut self, keep: bool) -> Self {
        self.keep_history = keep;
        self
    }

    /// Whether this store persists writes.
    pub fn is_history_kept(&self) -> bool {
        self.keep_history
    }

    fn path_for(&self, id: &SessionId) -> PathBuf {
        self.directory.join(format!("{}.json", id.as_str()))
    }

    /// Persist a session atomically and owner-only. A no-op when history is off.
    pub fn save(&self, session: &Session) -> Result<(), StoreError> {
        if !self.keep_history {
            return Ok(());
        }
        paths::create_private_dir(&self.directory).map_err(|err| StoreError::Directory {
            path: self.directory.clone(),
            reason: err.to_string(),
        })?;
        let mut bytes =
            serde_json::to_vec_pretty(session).map_err(|err| StoreError::Unwritable {
                id: session.id.as_str().to_owned(),
                reason: err.to_string(),
            })?;
        bytes.push(b'\n');
        paths::write_private(&self.path_for(&session.id), &bytes).map_err(|err| {
            StoreError::Unwritable {
                id: session.id.as_str().to_owned(),
                reason: err.to_string(),
            }
        })
    }

    /// Load a session by id.
    pub fn load(&self, id: &SessionId) -> Result<Session, StoreError> {
        let bytes = match std::fs::read(self.path_for(id)) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(id.as_str().to_owned()));
            }
            Err(err) => {
                return Err(StoreError::Unreadable {
                    id: id.as_str().to_owned(),
                    reason: err.to_string(),
                });
            }
        };
        serde_json::from_slice(&bytes).map_err(|err| StoreError::Unreadable {
            id: id.as_str().to_owned(),
            reason: err.to_string(),
        })
    }

    /// Delete a session. Returns whether a file was removed.
    pub fn delete(&self, id: &SessionId) -> Result<bool, StoreError> {
        match std::fs::remove_file(self.path_for(id)) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(StoreError::Unwritable {
                id: id.as_str().to_owned(),
                reason: err.to_string(),
            }),
        }
    }

    /// List sessions newest-first, isolating any one damaged record.
    pub fn list(&self) -> Result<Vec<SessionSummary>, StoreError> {
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(StoreError::Directory {
                    path: self.directory.clone(),
                    reason: err.to_string(),
                });
            }
        };

        // Order by mtime before opening anything, so a large store does not
        // parse every file to show a page of the newest.
        let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            candidates.push((modified, path));
        }
        candidates.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));

        let mut summaries = Vec::new();
        for (_, path) in candidates {
            let Ok(bytes) = std::fs::read(&path) else {
                continue; // an entry that vanished between listing and reading
            };
            match serde_json::from_slice::<Session>(&bytes) {
                Ok(session) => summaries.push(SessionSummary::of(&session)),
                Err(_) => continue, // one damaged record costs one record
            }
        }
        // Re-sort on the recorded time: a backup restore rewrites every mtime at
        // once, and the order the user remembers is the one in the file.
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.updated_at));
        Ok(summaries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "lightagent-store-{}",
            SessionId::generate().as_str()
        ))
    }

    #[test]
    fn ids_are_generated_and_only_the_generated_shape_parses() {
        let id = SessionId::generate();
        assert_eq!(id.as_str().len(), 32);
        assert!(SessionId::parse(id.as_str()).is_ok());
        assert!(matches!(
            SessionId::parse("../etc"),
            Err(StoreError::MalformedId(_))
        ));
        assert!(matches!(
            SessionId::parse("ABCDEF"),
            Err(StoreError::MalformedId(_))
        ));
    }

    #[test]
    fn a_session_round_trips_and_survives_a_new_store() {
        let dir = scratch_dir();
        let store = SessionStore::new(&dir);
        let mut session = Session::new("default", "First chat");
        session.push_message(StoredMessage::new("user", "hi"));
        session.push_run(RunRecord {
            run_id: "run-1".into(),
            started_at: SystemTime::now(),
            ended_at: Some(SystemTime::now()),
            stop_reason: Some("end_turn".into()),
            tools: vec![ToolHistoryEntry {
                tool: "datetime.now".into(),
                arguments_preview: "{}".into(),
                outcome: "ok".into(),
                duration_ms: Some(2),
            }],
        });
        store.save(&session).unwrap();

        // A brand-new store instance is the "after restart" case.
        let reopened = SessionStore::new(&dir);
        let loaded = reopened.load(&session.id).unwrap();
        assert_eq!(loaded, session);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_session_is_not_found_not_malformed() {
        let store = SessionStore::new(scratch_dir());
        let id = SessionId::generate();
        assert!(matches!(store.load(&id), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn one_damaged_record_costs_one_record() {
        let dir = scratch_dir();
        let store = SessionStore::new(&dir);
        let good = Session::new("default", "Good");
        store.save(&good).unwrap();
        // A corrupt file alongside a good one.
        paths::create_private_dir(&dir).unwrap();
        std::fs::write(
            dir.join("00000000000000000000000000000000.json"),
            b"{ not json",
        )
        .unwrap();

        let listed = store.list().unwrap();
        assert_eq!(
            listed.len(),
            1,
            "the damaged record is skipped, the good one is kept"
        );
        assert_eq!(listed[0].id, good.id);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_off_refuses_writes_but_reads_still_work() {
        let dir = scratch_dir();
        let keeping = SessionStore::new(&dir);
        let session = Session::new("default", "Saved");
        keeping.save(&session).unwrap();

        let off = SessionStore::new(&dir).keep_history(false);
        let mut later = Session::new("default", "Not saved");
        later.push_message(StoredMessage::new("user", "hello"));
        off.save(&later).unwrap(); // a no-op

        assert!(matches!(off.load(&later.id), Err(StoreError::NotFound(_))));
        // The earlier session is still readable through the history-off store.
        assert_eq!(off.load(&session.id).unwrap().id, session.id);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_reports_what_it_did() {
        let dir = scratch_dir();
        let store = SessionStore::new(&dir);
        let session = Session::new("default", "Temp");
        store.save(&session).unwrap();
        assert!(store.delete(&session.id).unwrap());
        assert!(!store.delete(&session.id).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_saved_session_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch_dir();
        let store = SessionStore::new(&dir);
        let session = Session::new("default", "Private");
        store.save(&session).unwrap();
        let mode = std::fs::metadata(store.path_for(&session.id))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "a transcript must not be world-readable");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
