//! A durable set of remembered facts for one profile.
//!
//! Memories are short notes the agent writes during a run and recalls in later
//! ones. They persist as JSONL under the profile's owner-only `memory/`
//! directory. Recall reuses the lexical retriever from `lightagent-rag`: each
//! memory carries a feature-hashed vector, so `search` ranks by cosine
//! similarity, while `recent` orders by write time for the prompt snapshot.

use std::io;
use std::path::{Path, PathBuf};

use lightagent_core::RunId;
use lightagent_rag::{Embedder, cosine};
use serde::{Deserialize, Serialize};

/// One remembered fact.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Memory {
    /// A unique id (used to `forget` it).
    pub id: String,
    /// The remembered text.
    pub text: String,
    /// A coarse kind (`fact`, `preference`, …); free-form, `fact` by default.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Optional tags for grouping.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Unix seconds when it was written.
    pub created_at: u64,
    /// The recall vector; skipped from any public rendering.
    #[serde(default)]
    vector: Vec<f32>,
}

fn default_kind() -> String {
    "fact".to_owned()
}

/// A profile's persisted memories.
pub struct MemoryStore {
    path: PathBuf,
    memories: Vec<Memory>,
}

impl MemoryStore {
    /// Open the store at `path`, loading it when present, empty when not.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let memories = match std::fs::read_to_string(&path) {
            Ok(text) => text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| serde_json::from_str::<Memory>(line).ok())
                .collect(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        Ok(Self { path, memories })
    }

    /// Write a memory, returning its id.
    pub fn write(
        &mut self,
        text: &str,
        kind: &str,
        tags: Vec<String>,
        embedder: &dyn Embedder,
        created_at: u64,
    ) -> io::Result<String> {
        let id = RunId::new().as_str().to_owned();
        let kind = if kind.trim().is_empty() {
            default_kind()
        } else {
            kind.trim().to_owned()
        };
        self.memories.push(Memory {
            id: id.clone(),
            text: text.trim().to_owned(),
            kind,
            tags,
            created_at,
            vector: embedder.embed(text),
        });
        self.persist()?;
        Ok(id)
    }

    /// The `k` memories most relevant to `query`, best first (positive only).
    pub fn search(&self, query: &str, embedder: &dyn Embedder, k: usize) -> Vec<&Memory> {
        let embedded = embedder.embed(query);
        let mut scored: Vec<(f32, &Memory)> = self
            .memories
            .iter()
            .map(|memory| (cosine(&embedded, &memory.vector), memory))
            .filter(|(score, _)| *score > 0.0)
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(k)
            .map(|(_, memory)| memory)
            .collect()
    }

    /// The `n` most recently written memories, newest first.
    pub fn recent(&self, n: usize) -> Vec<&Memory> {
        let mut all: Vec<&Memory> = self.memories.iter().collect();
        all.sort_by_key(|memory| std::cmp::Reverse(memory.created_at));
        all.truncate(n);
        all
    }

    /// Every memory, in write order.
    pub fn all(&self) -> &[Memory] {
        &self.memories
    }

    /// Remove the memory with `id`; returns whether one was removed.
    pub fn forget(&mut self, id: &str) -> io::Result<bool> {
        let before = self.memories.len();
        self.memories.retain(|memory| memory.id != id);
        let removed = self.memories.len() != before;
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    /// Drop every memory.
    pub fn clear(&mut self) -> io::Result<()> {
        self.memories.clear();
        self.persist()
    }

    /// The number of memories.
    pub fn len(&self) -> usize {
        self.memories.len()
    }

    /// Whether there are no memories.
    pub fn is_empty(&self) -> bool {
        self.memories.is_empty()
    }

    /// The prompt snapshot: the `n` most recent memories as a compact list, or an
    /// empty string when there are none.
    pub fn recent_catalog(&self, n: usize) -> String {
        let recent = self.recent(n);
        if recent.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "# What you remember\nDurable notes from earlier sessions. Use the `memory.search` \
             tool to recall more, and `memory.write` to remember something new.\n\n",
        );
        for memory in recent {
            out.push_str(&format!("- {}\n", memory.text));
        }
        out
    }

    fn persist(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        for memory in &self.memories {
            let line = serde_json::to_string(memory)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            out.push_str(&line);
            out.push('\n');
        }
        std::fs::write(&self.path, out)
    }
}

/// The default memory file under a profile's `memory/` directory.
pub fn memory_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("memory").join("memories.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightagent_rag::HashingEmbedder;

    fn scratch() -> PathBuf {
        std::env::temp_dir().join(format!(
            "lightagent-mem-{}-{}/memories.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    #[test]
    fn write_recall_recent_and_forget() {
        let path = scratch();
        let embedder = HashingEmbedder;
        let id = {
            let mut store = MemoryStore::open(&path).unwrap();
            store
                .write(
                    "The user prefers Rust and terse code.",
                    "preference",
                    vec![],
                    &embedder,
                    100,
                )
                .unwrap();
            store
                .write(
                    "The deploy script lives at ops/deploy.sh.",
                    "fact",
                    vec![],
                    &embedder,
                    200,
                )
                .unwrap()
        };
        // Reopen from disk.
        let mut store = MemoryStore::open(&path).unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(store.recent(1)[0].created_at, 200, "newest first");
        let hits = store.search("where is the deploy script", &embedder, 5);
        assert!(!hits.is_empty());
        assert!(hits[0].text.contains("deploy"), "relevant memory recalled");
        assert!(store.recent_catalog(5).contains("What you remember"));

        assert!(store.forget(&id).unwrap());
        assert_eq!(store.len(), 1);
        assert!(!store.forget("nope").unwrap());
        store.clear().unwrap();
        assert!(store.is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
