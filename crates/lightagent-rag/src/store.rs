//! A persisted set of chunks, searched by hybrid lexical + semantic similarity.
//!
//! Each chunk carries the text needed for BM25, a dependency-free feature-hash
//! vector used for compatibility and deduplication, and, when configured, a model
//! embedding. BM25 and dense results are combined with Reciprocal Rank Fusion
//! (RRF), which mixes rankings rather than incomparable raw score scales. With no
//! semantic embedder retrieval is model-free. The index is JSONL under the
//! profile's owner-only `rag/` directory.

use std::cmp::Ordering;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::chunk::chunk;
use crate::embed::{DIM, Embedder, SemanticEmbedder, cosine, lexical_terms};

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

/// An ephemeral passage to rank without writing it into the profile index.
/// Realtime retrieval uses this for freshly fetched web pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passage {
    pub source: String,
    pub text: String,
}

/// Rank fresh passages with BM25 and, when configured, a bounded semantic pass.
///
/// The sparse first stage considers every passage. The semantic endpoint sees
/// at most `semantic_candidates` passages selected by BM25 plus one leading
/// passage per source, keeping latency and embedding work bounded while retaining
/// a path for synonym-only matches. Query and passages are embedded in one batch.
pub async fn search_passages(
    query: &str,
    passages: &[Passage],
    semantic: Option<&dyn SemanticEmbedder>,
    k: usize,
    semantic_candidates: usize,
) -> Vec<Hit> {
    use std::collections::{HashMap, HashSet};

    let lexical = crate::embed::HashingEmbedder;
    let records: Vec<Record> = passages
        .iter()
        .enumerate()
        .map(|(index, passage)| Record {
            source: passage.source.clone(),
            chunk: index,
            text: passage.text.clone(),
            vector: lexical.embed(&passage.text),
            semantic: None,
        })
        .collect();
    let lexical_ranked = bm25_ranked(&records, query);

    let semantic_ranked = if let Some(embedder) = semantic {
        let cap = semantic_candidates.max(k).min(records.len());
        let mut candidate_indices = Vec::with_capacity(cap);
        let mut seen = HashSet::new();
        // Preserve one route into every high-ranked web source even when its
        // wording shares no token with the query.
        for (index, record) in records.iter().enumerate() {
            if candidate_indices.len() == cap {
                break;
            }
            let first_for_source = records[..index]
                .iter()
                .all(|earlier| earlier.source != record.source);
            if first_for_source && seen.insert(index) {
                candidate_indices.push(index);
            }
        }
        for (index, _) in &lexical_ranked {
            if candidate_indices.len() == cap {
                break;
            }
            if seen.insert(*index) {
                candidate_indices.push(*index);
            }
        }
        for index in 0..records.len() {
            if candidate_indices.len() == cap {
                break;
            }
            if seen.insert(index) {
                candidate_indices.push(index);
            }
        }

        let mut texts = Vec::with_capacity(candidate_indices.len() + 1);
        texts.push(query.to_owned());
        texts.extend(
            candidate_indices
                .iter()
                .map(|index| records[*index].text.clone()),
        );
        match embedder.embed(&texts).await {
            Ok(vectors) if vectors.len() == texts.len() => {
                let query_vector = &vectors[0];
                Some(ranked(candidate_indices.iter().enumerate().map(
                    |(offset, index)| (*index, cosine(query_vector, &vectors[offset + 1])),
                )))
            }
            _ => None,
        }
    } else {
        None
    };

    let ranked = match semantic_ranked {
        Some(semantic_ranked) if !semantic_ranked.is_empty() => {
            fuse_rrf(&[lexical_ranked, semantic_ranked])
        }
        _ if !lexical_ranked.is_empty() => lexical_ranked,
        // Search-engine ordering is the best deterministic fallback when no
        // passage shares a term and the semantic endpoint is absent/down.
        _ => records
            .iter()
            .enumerate()
            .map(|(index, _)| (index, 1.0 / (index + 1) as f32))
            .collect(),
    };

    let mut per_source: HashMap<&str, usize> = HashMap::new();
    let mut selected: Vec<(usize, f32)> = Vec::new();
    for (index, score) in ranked {
        if selected.len() == k {
            break;
        }
        let record = &records[index];
        if per_source.get(record.source.as_str()).copied().unwrap_or(0) >= 2 {
            continue;
        }
        // Suppress boilerplate and mirrored passages without a model reranker.
        if selected
            .iter()
            .any(|(chosen, _)| cosine(&records[*chosen].vector, &record.vector) > 0.92)
        {
            continue;
        }
        *per_source.entry(record.source.as_str()).or_insert(0) += 1;
        selected.push((index, score));
    }

    selected
        .into_iter()
        .map(|(index, score)| Hit {
            score,
            source: records[index].source.clone(),
            text: records[index].text.clone(),
        })
        .collect()
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
        _lexical: &dyn Embedder,
        semantic: Option<&dyn SemanticEmbedder>,
        k: usize,
    ) -> Vec<Hit> {
        // BM25 is the sparse retriever. Unlike a plain bag-of-words cosine it
        // discounts corpus-wide terms and saturates repeated words, which makes
        // exact names and rare facts much more reliable without asking the
        // generator (or another model) to rewrite the query.
        let lexical_ranked = bm25_ranked(&self.records, query);

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
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked
}

/// Okapi BM25 over the stored chunk text and source name.
fn bm25_ranked(records: &[Record], query: &str) -> Vec<(usize, f32)> {
    use std::collections::{HashMap, HashSet};

    let query_terms: Vec<String> = lexical_terms(query).collect();
    if query_terms.is_empty() || records.is_empty() {
        return Vec::new();
    }

    let documents: Vec<Vec<String>> = records
        .iter()
        .map(|record| lexical_terms(&format!("{} {}", record.source, record.text)).collect())
        .collect();
    let average_length =
        documents.iter().map(Vec::len).sum::<usize>() as f32 / documents.len() as f32;
    if average_length == 0.0 {
        return Vec::new();
    }

    let wanted: HashSet<&str> = query_terms.iter().map(String::as_str).collect();
    let mut document_frequency: HashMap<&str, usize> = HashMap::new();
    for document in &documents {
        let present: HashSet<&str> = document
            .iter()
            .map(String::as_str)
            .filter(|term| wanted.contains(term))
            .collect();
        for term in present {
            *document_frequency.entry(term).or_insert(0) += 1;
        }
    }

    // Robertson's common k1/b values are robust defaults for prose and code.
    const K1: f32 = 1.2;
    const B: f32 = 0.75;
    let corpus_size = documents.len() as f32;
    ranked(documents.iter().enumerate().map(|(index, document)| {
        let mut frequencies: HashMap<&str, usize> = HashMap::new();
        for term in document {
            if wanted.contains(term.as_str()) {
                *frequencies.entry(term.as_str()).or_insert(0) += 1;
            }
        }
        let length_normalization = 1.0 - B + B * document.len() as f32 / average_length;
        let score = query_terms.iter().fold(0.0, |score, term| {
            let frequency = frequencies.get(term.as_str()).copied().unwrap_or(0) as f32;
            if frequency == 0.0 {
                return score;
            }
            let df = document_frequency.get(term.as_str()).copied().unwrap_or(0) as f32;
            let idf = (1.0 + (corpus_size - df + 0.5) / (df + 0.5)).ln();
            score + idf * frequency * (K1 + 1.0) / (frequency + K1 * length_normalization)
        });
        (index, score)
    }))
}

/// Reciprocal Rank Fusion: a record's fused score is the sum over the ranked
/// lists of `1 / (RRF_K + rank)`, so appearing high in either list helps and
/// appearing in both helps most. Returns `(index, fused_score)` best first.
fn fuse_rrf(lists: &[Vec<(usize, f32)>]) -> Vec<(usize, f32)> {
    use std::collections::HashMap;
    let mut fused: HashMap<usize, f32> = HashMap::new();
    for list in lists {
        for (rank, (index, _score)) in list.iter().enumerate() {
            *fused.entry(*index).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
        }
    }
    let mut fused: Vec<(usize, f32)> = fused.into_iter().collect();
    fused.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
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
        use std::sync::atomic::{AtomicU64, Ordering};
        // A process-wide counter makes each call's directory unique regardless of
        // clock resolution: two tests running in parallel must never share a dir,
        // or one test's remove_dir_all(parent) would delete the other's live
        // scratch out from under it. The pid keeps concurrent test binaries apart.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "lightagent-rag-{}-{unique}/index.jsonl",
            std::process::id()
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

    #[tokio::test]
    async fn bm25_prefers_the_rare_exact_fact() {
        let path = scratch_index();
        let lexical = HashingEmbedder;
        let mut store = RagStore::open(&path).unwrap();
        store
            .add(
                "general.md",
                "The guide describes the general release process and the common checklist.",
                &lexical,
                None,
                500,
                50,
            )
            .await
            .unwrap();
        store
            .add(
                "specific.md",
                "Zephyr shipped during the September release window.",
                &lexical,
                None,
                500,
                50,
            )
            .await
            .unwrap();

        let hits = store
            .search("when did Zephyr ship?", &lexical, None, 2)
            .await;
        assert_eq!(hits[0].source, "specific.md");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    struct CountingSemantic(std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl SemanticEmbedder for CountingSemantic {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            self.0
                .store(texts.len(), std::sync::atomic::Ordering::SeqCst);
            Ok(texts.iter().map(|_| vec![1.0]).collect())
        }
    }

    #[tokio::test]
    async fn realtime_semantic_stage_is_batched_and_bounded() {
        let passages: Vec<Passage> = (0..100)
            .map(|index| Passage {
                source: format!("https://source{}.test", index % 5),
                text: format!("topic passage number {index} with distinct evidence"),
            })
            .collect();
        let semantic = CountingSemantic(std::sync::atomic::AtomicUsize::new(0));

        let hits = search_passages("topic", &passages, Some(&semantic), 5, 12).await;

        assert!(!hits.is_empty());
        assert_eq!(
            semantic.0.load(std::sync::atomic::Ordering::SeqCst),
            13,
            "one query plus the bounded candidate pool is sent in one batch"
        );
        assert!(
            hits.iter()
                .map(|hit| hit.source.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len()
                >= 2,
            "the final evidence should not collapse onto one source"
        );
    }
}
