//! Conversations, on disk.
//!
//! `paths.conversations_dir()` was chosen in M0 and nothing had ever written to
//! it. This is what writes to it.
//!
//! Three decisions shape the whole module:
//!
//! * **One file per conversation.** A single document holding every
//!   conversation would be rewritten in full on every message, which for a long
//!   chat means writing megabytes to save a sentence. Per-conversation files
//!   also mean a corrupt one costs one conversation rather than all of them.
//! * **Ids are generated here and never accepted from a caller.** An id becomes
//!   a file name. Taking one from a request means taking a path from a request,
//!   and no amount of escaping afterwards is as safe as never doing it.
//! * **Listing never parses more than it must.** Directory entries carry a
//!   modification time, so the newest can be chosen before anything is opened.
//!   A sidebar showing twenty conversations does not read four hundred.
//!
//! What is stored here is what the user typed. The log deliberately redacts
//! prompts; it would make no sense to redact them there and write them
//! world-readable here, so [`crate::atomic`] writes these owner-only.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic;
use crate::error::StoreError;

/// Bytes of randomness in an id. 128 bits: collisions are not a thing that
/// happens, and no coordination is needed to avoid them.
const ID_BYTES: usize = 16;
/// Hex characters an id has, which is what a file name is checked against.
const ID_LENGTH: usize = ID_BYTES * 2;
/// Conversations a single listing will open.
///
/// A bound rather than a page: the sidebar wants the recent ones, and a user
/// with four hundred conversations is not scrolling to the bottom of them.
const LIST_LIMIT: usize = 200;

const FORMAT_VERSION: u32 = 1;

/// One turn, as it is kept.
///
/// The measurements travel with the message because the panel shows them per
/// message — "82.4 tok/s, 45 tokens, 2.1s" under an answer — and recomputing
/// them later is impossible: the engine reported them once.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: String,
    pub content: String,
    /// A thinking model's reasoning, kept apart from its answer exactly as the
    /// wire protocol keeps them apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Unix seconds.
    #[serde(default)]
    pub at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_second: Option<f64>,
}

/// A conversation and everything in it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    /// Unix seconds.
    pub created_at: u64,
    pub updated_at: u64,
    /// The model that answered, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub messages: Vec<StoredMessage>,
}

/// What a listing returns: enough for the sidebar, and no message bodies.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub model: Option<String>,
    pub message_count: usize,
    /// The opening of the first thing the user said, for the sidebar's second
    /// line. Truncated here rather than in the panel so that a long first
    /// message is not sent in full to draw forty characters of it.
    pub preview: String,
}

/// Characters of the first message kept as a preview.
const PREVIEW_CHARS: usize = 120;

impl Conversation {
    fn summarize(&self) -> ConversationSummary {
        let preview = self
            .messages
            .iter()
            .find(|message| message.role == "user")
            .map(|message| truncate_chars(&message.content, PREVIEW_CHARS))
            .unwrap_or_default();

        ConversationSummary {
            id: self.id.clone(),
            title: self.title.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            model: self.model.clone(),
            message_count: self.messages.len(),
            preview,
        }
    }
}

/// Cut to `limit` characters, never mid-character.
///
/// By characters and not bytes: slicing a `String` at a byte offset that lands
/// inside a multi-byte character panics, and a preview is exactly where
/// non-ASCII text turns up.
fn truncate_chars(text: &str, limit: usize) -> String {
    let mut out: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        out.push('…');
    }
    out
}

/// The file on disk.
#[derive(Serialize, Deserialize)]
struct ConversationFile {
    version: u32,
    #[serde(flatten)]
    conversation: Conversation,
}

/// Conversations under one directory.
#[derive(Clone, Debug)]
pub struct ConversationStore {
    directory: PathBuf,
}

impl ConversationStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    /// A fresh id, in the only shape this store will ever accept back.
    ///
    /// Random rather than sequential for the same reason completion ids are:
    /// two runs producing the same ids would make a user's own files ambiguous
    /// after a restore.
    pub fn new_id() -> String {
        let mut bytes = [0_u8; ID_BYTES];
        if getrandom::fill(&mut bytes).is_err() {
            // Entropy is unavailable. Fall back to the clock, which is weaker
            // but still unique on one machine over the life of a process - and
            // failing to start a conversation because the OS would not give
            // sixteen random bytes would be a worse answer.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default();
            bytes[..16].copy_from_slice(&nanos.to_le_bytes());
        }
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Whether `id` is one this store could have produced.
    ///
    /// The gate between a request and the filesystem. Length and alphabet only:
    /// a value that passes cannot contain a separator, a `..`, a null or a
    /// drive letter, so joining it to the directory cannot leave it.
    fn is_well_formed(id: &str) -> bool {
        id.len() == ID_LENGTH && id.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    fn path_for(&self, id: &str) -> Result<PathBuf, StoreError> {
        if !Self::is_well_formed(id) {
            return Err(StoreError::MalformedId { id: id.to_owned() });
        }
        Ok(self.directory.join(format!("{id}.json")))
    }

    /// Read one conversation.
    pub fn get(&self, id: &str) -> Result<Conversation, StoreError> {
        let path = self.path_for(id)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::NoSuchConversation { id: id.to_owned() });
            }
            Err(err) => return Err(StoreError::io("reading a conversation", err)),
        };
        parse(&path, &bytes)
    }

    /// Write one conversation, replacing what was there.
    ///
    /// The whole document each time. A conversation is text, and the largest
    /// realistic one is smaller than the buffers this process already holds per
    /// request; appending in place would trade that for a file format that can
    /// be torn, which is the thing [`crate::atomic`] exists to prevent.
    pub fn save(&self, conversation: &Conversation) -> Result<(), StoreError> {
        let path = self.path_for(&conversation.id)?;
        let file = ConversationFile {
            version: FORMAT_VERSION,
            conversation: conversation.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&file)
            .map_err(|err| StoreError::io("encoding a conversation", std::io::Error::other(err)))?;
        bytes.push(b'\n');
        atomic::write_private(&path, &bytes)
    }

    /// Forget one conversation.
    pub fn delete(&self, id: &str) -> Result<(), StoreError> {
        let path = self.path_for(id)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(StoreError::NoSuchConversation { id: id.to_owned() })
            }
            Err(err) => Err(StoreError::io("deleting a conversation", err)),
        }
    }

    /// Summaries of the most recently touched conversations, newest first.
    ///
    /// A conversation whose file will not parse is skipped rather than fatal.
    /// One damaged file must not make the sidebar empty — that would turn a
    /// problem with one conversation into the appearance of having lost all of
    /// them.
    pub fn list(&self) -> Result<Vec<ConversationSummary>, StoreError> {
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            // A user who has never had a conversation has none, which is a true
            // answer rather than a failure.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(StoreError::io("listing conversations", err)),
        };

        // Sorted by modification time before anything is opened, so the bound
        // below is a bound on *work* and not merely on the answer.
        let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
                    && path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .is_some_and(Self::is_well_formed)
            })
            .map(|path| {
                let modified = path
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                (modified, path)
            })
            .collect();
        candidates.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
        candidates.truncate(LIST_LIMIT);

        let mut summaries: Vec<ConversationSummary> = candidates
            .into_iter()
            .filter_map(|(_, path)| {
                let bytes = std::fs::read(&path).ok()?;
                parse(&path, &bytes).ok().map(|c| c.summarize())
            })
            .collect();

        // Re-sorted on the recorded time rather than left on the file system's:
        // a restore from backup rewrites every mtime at once, and the order the
        // user remembers is the one in the file.
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.updated_at));
        Ok(summaries)
    }
}

fn parse(path: &Path, bytes: &[u8]) -> Result<Conversation, StoreError> {
    let file: ConversationFile =
        serde_json::from_slice(bytes).map_err(|err| StoreError::Unreadable {
            what: "a conversation",
            path: path.to_path_buf(),
            reason: err.to_string(),
        })?;
    Ok(file.conversation)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> (PathBuf, ConversationStore) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("hermes-conv-{tag}-{unique}"));
        (dir.clone(), ConversationStore::new(dir))
    }

    fn conversation(id: &str, title: &str, updated_at: u64) -> Conversation {
        Conversation {
            id: id.to_owned(),
            title: title.to_owned(),
            created_at: 1,
            updated_at,
            model: Some("mock-model@4k".into()),
            messages: vec![
                StoredMessage {
                    role: "user".into(),
                    content: "how does a CPU inference engine work?".into(),
                    at: 1,
                    ..StoredMessage::default()
                },
                StoredMessage {
                    role: "assistant".into(),
                    content: "It runs the model on your processor.".into(),
                    at: 2,
                    completion_tokens: Some(45),
                    tokens_per_second: Some(82.4),
                    ..StoredMessage::default()
                },
            ],
        }
    }

    #[test]
    fn a_conversation_survives_a_round_trip_intact() {
        let (dir, store) = store("roundtrip");
        let id = ConversationStore::new_id();
        let original = conversation(&id, "CPU inference explained", 10);

        store.save(&original).expect("save");
        let read = store.get(&id).expect("get");
        assert_eq!(read, original);
        // The per-message measurements are the half a naive round trip loses.
        assert_eq!(read.messages[1].tokens_per_second, Some(82.4));
        assert_eq!(read.messages[1].completion_tokens, Some(45));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ids_are_generated_and_are_the_only_shape_accepted() {
        let (dir, store) = store("ids");
        let id = ConversationStore::new_id();
        assert_eq!(id.len(), ID_LENGTH);
        assert!(ConversationStore::is_well_formed(&id));

        // Everything a request could put in the id position, and none of it
        // reaches the filesystem.
        for attempt in [
            "../../etc/passwd",
            "..",
            "/etc/passwd",
            "a/b",
            "",
            "not-hex-at-all-not-hex-at-all-nn",
            "0011223344556677889900aabbccddee1", // one too long
        ] {
            let err = store.get(attempt).expect_err(attempt);
            assert!(
                matches!(err, StoreError::MalformedId { .. }),
                "{attempt} produced {err:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn two_generated_ids_are_not_the_same() {
        assert_ne!(ConversationStore::new_id(), ConversationStore::new_id());
    }

    #[test]
    fn a_missing_conversation_is_not_found_rather_than_malformed() {
        // The two are different answers and the difference matters: one is the
        // caller's mistake, the other is simply gone.
        let (dir, store) = store("missing");
        let id = ConversationStore::new_id();
        let err = store.get(&id).expect_err("no such conversation");
        assert!(matches!(err, StoreError::NoSuchConversation { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn listing_is_newest_first_by_the_recorded_time() {
        let (dir, store) = store("order");
        for (id, title, updated) in [
            (ConversationStore::new_id(), "oldest", 10_u64),
            (ConversationStore::new_id(), "newest", 30),
            (ConversationStore::new_id(), "middle", 20),
        ] {
            store
                .save(&conversation(&id, title, updated))
                .expect("save");
        }

        let listed = store.list().expect("list");
        let titles: Vec<&str> = listed.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["newest", "middle", "oldest"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_summary_carries_no_message_bodies() {
        let (dir, store) = store("summary");
        let id = ConversationStore::new_id();
        store.save(&conversation(&id, "titled", 5)).expect("save");

        let listed = store.list().expect("list");
        let summary = &listed[0];
        assert_eq!(summary.message_count, 2);
        assert_eq!(summary.title, "titled");
        // The preview is the user's opening line, not the assistant's answer.
        assert!(summary.preview.starts_with("how does a CPU"));

        let encoded = serde_json::to_string(summary).expect("encode");
        assert!(
            !encoded.contains("It runs the model"),
            "a summary must not carry the transcript: {encoded}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_preview_never_splits_a_character() {
        // Slicing a String at a byte offset inside a multi-byte character
        // panics, and a preview is exactly where such text turns up.
        let text: String = "é".repeat(PREVIEW_CHARS + 40);
        let preview = truncate_chars(&text, PREVIEW_CHARS);
        assert_eq!(
            preview.chars().count(),
            PREVIEW_CHARS + 1,
            "plus the ellipsis"
        );
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn one_damaged_file_does_not_empty_the_sidebar() {
        // Otherwise a problem with one conversation looks like having lost all
        // of them.
        let (dir, store) = store("damaged");
        let good = ConversationStore::new_id();
        store.save(&conversation(&good, "intact", 9)).expect("save");
        let damaged = dir.join(format!("{}.json", ConversationStore::new_id()));
        std::fs::write(&damaged, b"{ this is not json").expect("write");

        let listed = store.list().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "intact");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_directory_that_does_not_exist_lists_nothing_rather_than_failing() {
        let (_dir, store) = store("empty");
        assert!(store.list().expect("list").is_empty());
    }

    #[test]
    fn deleting_forgets_it_and_says_so_the_second_time() {
        let (dir, store) = store("delete");
        let id = ConversationStore::new_id();
        store.save(&conversation(&id, "doomed", 1)).expect("save");

        store.delete(&id).expect("delete");
        assert!(matches!(
            store.get(&id).expect_err("gone"),
            StoreError::NoSuchConversation { .. }
        ));
        assert!(matches!(
            store.delete(&id).expect_err("gone twice"),
            StoreError::NoSuchConversation { .. }
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stray_file_in_the_directory_is_ignored() {
        // The directory belongs to the user; an editor's backup file or a
        // `.DS_Store` must not become a conversation.
        let (dir, store) = store("stray");
        let id = ConversationStore::new_id();
        store.save(&conversation(&id, "real", 1)).expect("save");
        std::fs::write(dir.join("notes.txt"), b"hello").expect("write");
        std::fs::write(dir.join("wrong-name.json"), b"{}").expect("write");

        let listed = store.list().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "real");
        std::fs::remove_dir_all(&dir).ok();
    }
}
