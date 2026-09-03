//! A persisted set of chunks, searched by hybrid lexical + semantic similarity.
//!
//! Each chunk carries a lexical vector (the dependency-free feature hash, always)
//! and, when a semantic embedder is configured, a model embedding. Search ranks
//! by both and fuses the two rankings with Reciprocal Rank Fusion (RRF), which
//! combines lists on rank rather than raw score — so the very different scales of
//! a bag-of-words cosine and a model cosine mix cleanly, and a record missing one
//! signal (an old index with no semantic vector, or a chunk with no shared words)
//! still ranks on the other. With no semantic embedder it is pure lexical, as
//! before. The index is JSONL under the profile's owner-only `rag/` directory.

use std::cmp::Ordering;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::chunk::chunk;
use crate::embed::{DIM, Embedder, SemanticEmbedder, cosine};

/// RRF's rank damping constant; 60 is the value from the original paper.
const RRF_K: f32 = 60.0;

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    source: String,
    chunk: usize,
    text: String,
    /// The lexical (feature-hash) vector; always present.
    vector: Vec<f32>,
    /// The semantic (model) vector, when one was computed at index time.
    #[serde(default)]
    semantic: Option<Vec<f32>>,
}

/// One search result.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub score: f32,
    pub source: String,
    pub text: String,
}

/// An on-disk hybrid vector index for one profile.
pub struct RagStore {
    path: PathBuf,
    records: Vec<Record>,
}

impl RagStore {
    /// Open the index at `path`, loading it when present (lexical vectors of a
    /// stale dimension are skipped) and starting empty when it is not.
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

    /// Index `text` under `source`, replacing any earlier chunks for it. When a
    /// `semantic` embedder is given, each chunk also gets a model embedding.
    /// Returns the number of chunks stored.
    pub async fn add(
        &mut self,
        source: &str,
        text: &str,
        lexical: &dyn Embedder,
        semantic: Option<&dyn SemanticEmbedder>,
        max_chars: usize,
        overlap: usize,
    ) -> Result<usize, String> {
        self.records.retain(|record| record.source != source);
        let chunks = chunk(text, max_chars, overlap);
        if chunks.is_empty() {
            self.persist().map_err(|error| error.to_string())?;
            return Ok(0);
        }
        // One batched semantic call for the whole document, index-aligned to
        // `chunks`; a failure degrades to lexical-only rather than aborting.
        let semantic_vectors: Option<Vec<Vec<f32>>> = match semantic {
            Some(embedder) => match embedder.embed(&chunks).await {
                Ok(vectors) if vectors.len() == chunks.len() => Some(vectors),
                _ => None,
            },
            None => None,
        };

        let mut added = 0;
        for (index, piece) in chunks.iter().enumerate() {
            let vector = lexical.embed(piece);
            let semantic = semantic_vectors
                .as_ref()
                .and_then(|all| all.get(index).cloned());
            if vector.iter().all(|value| *value == 0.0) && semantic.is_none() {
                continue; // nothing to match on
            }
            self.records.push(Record {
                source: source.to_owned(),
                chunk: index,
                text: piece.clone(),
                vector,
                semantic,
            });
            added += 1;
        }
        self.persist().map_err(|error| error.to_string())?;
        Ok(added)
    }

    /// The `k` best matches for `query`. Lexical always; when `semantic` is given
    /// and reachable, lexical and semantic rankings are fused with RRF.
    pub async fn search(
        &self,
        query: &str,
        lexical: &dyn Embedder,
        semantic: Option<&dyn SemanticEmbedder>,
        k: usize,
    ) -> Vec<Hit> {
        let lexical_query = lexical.embed(query);
        let lexical_ranked = ranked(
            self.records
                .iter()
                .enumerate()
                .map(|(index, record)| (index, cosine(&lexical_query, &record.vector))),
        );

        let semantic_ranked = match semantic {
            Some(embedder) => match embedder
                .embed(std::slice::from_ref(&query.to_owned()))
                .await
            {
                Ok(vectors) => vectors.into_iter().next().map(|query_vector| {
                    ranked(
                        self.records
                            .iter()
                            .enumerate()
                            .filter_map(|(index, record)| {
                                record
                                    .semantic
                                    .as_ref()
                                    .map(|vector| (index, cosine(&query_vector, vector)))
                            }),
                    )
                }),
                Err(_) => None, // a failed embed degrades to lexical-only
            },
            None => None,
        };

        let fused = match semantic_ranked {
            Some(semantic_ranked) => fuse_rrf(&[lexical_ranked, semantic_ranked]),
            None => lexical_ranked,
        };

        fused
            .into_iter()
            .take(k)
            .map(|(index, score)| Hit {
                score,
                source: self.records[index].source.clone(),
                text: self.records[index].text.clone(),
            })
            .collect()
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

    /// Whether any indexed chunk carries a semantic vector.
    pub fn has_semantic(&self) -> bool {
        self.records.iter().any(|record| record.semantic.is_some())
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

/// Sort `(index, score)` pairs by descending score, dropping non-positive ones.
fn ranked(scored: impl Iterator<Item = (usize, f32)>) -> Vec<(usize, f32)> {
    let mut ranked: Vec<(usize, f32)> = scored.filter(|(_, score)| *score > 0.0).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    ranked
}

/// Reciprocal Rank Fusion: a record's fused score is the sum over the ranked
/// lists of `1 / (RRF_K + rank)`, so appearing high in either list helps and
/// appearing in both helps most. Returns `(index, fused_score)` best first.
fn fuse_rrf(lists: &[Vec<(usize, f32)>]) -> Vec<(usize, f32)> {
    use std::collections::HashMap;
    let mut fused: HashMap<usize, f32> = HashMap::new();
    for list in lists {
        for (rank, (index, _score)) in list.iter().enumerate() {
            *fused.entry(*index).or_insert(0.0) += 1.0 / (RRF_K + rank as f32);
        }
    }
    let mut fused: Vec<(usize, f32)> = fused.into_iter().collect();
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    fused
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

    /// A deterministic offline semantic embedder: a fixed-dim vector keyed on the
    /// presence of a few concept words, so "related" texts share direction
    /// without matching literal tokens (which the lexical half already covers).
    struct FakeSemantic;

    #[async_trait::async_trait]
    impl SemanticEmbedder for FakeSemantic {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            let concepts = ["runtime", "fruit", "database"];
            Ok(texts
                .iter()
                .map(|text| {
                    let lower = text.to_lowercase();
                    // Map synonyms to the same concept axis.
                    let async_like = lower.contains("async")
                        || lower.contains("concurren")
                        || lower.contains("runtime")
                        || lower.contains("tokio");
                    concepts
                        .iter()
                        .enumerate()
                        .map(|(i, c)| {
                            if (i == 0 && async_like) || lower.contains(c) {
                                1.0
                            } else {
                                0.0
                            }
                        })
                        .collect()
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn lexical_only_still_works() {
        let path = scratch_index();
        let embedder = HashingEmbedder;
        let mut store = RagStore::open(&path).unwrap();
        store
            .add(
                "a.md",
                "Tokio is an async runtime for Rust.",
                &embedder,
                None,
                500,
                50,
            )
            .await
            .unwrap();
        assert!(!store.has_semantic());
        let hits = store.search("async runtime", &embedder, None, 3).await;
        assert_eq!(hits[0].source, "a.md");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn hybrid_recalls_a_synonym_the_lexical_half_would_miss() {
        let path = scratch_index();
        let lexical = HashingEmbedder;
        let semantic = FakeSemantic;
        let mut store = RagStore::open(&path).unwrap();
        store
            .add(
                "rust.md",
                "This service uses concurrent tasks for high throughput.",
                &lexical,
                Some(&semantic),
                500,
                50,
            )
            .await
            .unwrap();
        store
            .add(
                "fruit.md",
                "Bananas are a tropical fruit.",
                &lexical,
                Some(&semantic),
                500,
                50,
            )
            .await
            .unwrap();
        assert!(store.has_semantic());

        // The query shares no salient words with the rust doc ("async runtime" vs
        // "concurrent tasks"), so lexical alone would not surface it; the semantic
        // concept axis does, and RRF puts it first.
        let hits = store
            .search("async runtime", &lexical, Some(&semantic), 2)
            .await;
        assert!(!hits.is_empty());
        assert_eq!(hits[0].source, "rust.md", "semantic recall wins via RRF");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
