//! A persisted set of embedded chunks, searched by cosine similarity.
//!
//! The index is a JSONL file (one record per chunk) under the profile's `rag/`
//! directory, which is itself owner-only, so the indexed text is as protected as
//! the rest of the profile. Adding a source re-indexes it (its old chunks are
//! dropped first), so re-adding an edited file does not leave stale passages.

use std::cmp::Ordering;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::chunk::chunk;
use crate::embed::{DIM, Embedder, cosine};

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    source: String,
    chunk: usize,
    text: String,
    vector: Vec<f32>,
}

/// One search result.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub score: f32,
    pub source: String,
    pub text: String,
}

/// An on-disk vector index for one profile.
pub struct RagStore {
    path: PathBuf,
    records: Vec<Record>,
}

impl RagStore {
    /// Open the index at `path`, loading it when present (records of a stale
    /// dimension are skipped) and starting empty when it is not.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let records = match std::fs::read_to_string(&path) {
            Ok(text) => text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| serde_json::from_str::<Record>(line).ok())
                .filter(|record| record.vector.len() == DIM)
                .collect(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        Ok(Self { path, records })
    }

    /// Index `text` under `source`, replacing any earlier chunks for it. Returns
    /// the number of chunks stored.
    pub fn add(
        &mut self,
        source: &str,
        text: &str,
        embedder: &dyn Embedder,
        max_chars: usize,
        overlap: usize,
    ) -> io::Result<usize> {
        self.records.retain(|record| record.source != source);
        let mut added = 0;
        for (index, piece) in chunk(text, max_chars, overlap).into_iter().enumerate() {
            let vector = embedder.embed(&piece);
            if vector.iter().all(|value| *value == 0.0) {
                continue; // no tokens to match on
            }
            self.records.push(Record {
                source: source.to_owned(),
                chunk: index,
                text: piece,
                vector,
            });
            added += 1;
        }
        self.persist()?;
        Ok(added)
    }

    /// The `k` best matches for `query`, best first, positive scores only.
    pub fn search(&self, query: &str, embedder: &dyn Embedder, k: usize) -> Vec<Hit> {
        let embedded = embedder.embed(query);
        let mut hits: Vec<Hit> = self
            .records
            .iter()
            .map(|record| Hit {
                score: cosine(&embedded, &record.vector),
                source: record.source.clone(),
                text: record.text.clone(),
            })
            .filter(|hit| hit.score > 0.0)
            .collect();
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        hits.truncate(k);
        hits
    }

    /// Each indexed source and its chunk count, sorted by source.
    pub fn sources(&self) -> Vec<(String, usize)> {
        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for record in &self.records {
            *counts.entry(record.source.clone()).or_insert(0) += 1;
        }
        counts.into_iter().collect()
    }

    /// The number of indexed chunks.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Drop every record and persist the empty index.
    pub fn clear(&mut self) -> io::Result<()> {
        self.records.clear();
        self.persist()
    }

    fn persist(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        for record in &self.records {
            let line = serde_json::to_string(record)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            out.push_str(&line);
            out.push('\n');
        }
        std::fs::write(&self.path, out)
    }
}

/// The default index file under a profile's `rag/` directory.
pub fn index_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("rag").join("index.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashingEmbedder;

    fn scratch_index() -> PathBuf {
        std::env::temp_dir().join(format!(
            "lightagent-rag-{}-{}/index.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    #[test]
    fn add_search_and_reindex_round_trip() {
        let path = scratch_index();
        let embedder = HashingEmbedder;
        {
            let mut store = RagStore::open(&path).unwrap();
            store
                .add(
                    "rust.md",
                    "Tokio is an asynchronous runtime for Rust.",
                    &embedder,
                    500,
                    50,
                )
                .unwrap();
            store
                .add(
                    "fruit.md",
                    "Bananas are a yellow tropical fruit.",
                    &embedder,
                    500,
                    50,
                )
                .unwrap();
            let hits = store.search("async rust runtime", &embedder, 3);
            assert!(!hits.is_empty());
            assert_eq!(hits[0].source, "rust.md", "the rust passage should win");
        }
        // Reopen from disk and confirm persistence + re-index behaviour.
        let mut store = RagStore::open(&path).unwrap();
        assert_eq!(store.sources().len(), 2);
        let before = store.len();
        store
            .add(
                "rust.md",
                "Completely different words here now.",
                &embedder,
                500,
                50,
            )
            .unwrap();
        assert_eq!(
            store.sources().len(),
            2,
            "re-adding a source does not duplicate it"
        );
        assert!(store.len() <= before + 1);
        store.clear().unwrap();
        assert!(store.is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
