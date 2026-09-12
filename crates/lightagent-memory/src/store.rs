//! A durable set of remembered facts for one profile.
//!
//! Memories are short notes the agent writes during a run and recalls in later
//! ones. They persist as JSONL under the profile's owner-only `memory/`
//! directory. Recall reuses the lexical retriever from `lightagent-rag`: each
//! memory carries a feature-hashed vector, so `search` ranks by cosine
//! similarity, while `recent` orders by write time for the prompt snapshot.

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};

use lightagent_core::RunId;
use lightagent_core::paths;
use lightagent_rag::{Embedder, SemanticEmbedder, cosine};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    /// The reviewed session message this fact came from, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<MemorySource>,
    /// The recall vector; skipped from any public rendering.
    #[serde(default)]
    vector: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySource {
    pub session_id: String,
    /// One-based index in the saved session transcript.
    pub message_index: usize,
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
                .enumerate()
                .filter(|(_, line)| !line.trim().is_empty())
                .map(|(index, line)| {
                    serde_json::from_str::<Memory>(line).map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("{}:{}: {error}", path.display(), index + 1),
                        )
                    })
                })
                .collect::<io::Result<Vec<_>>>()?,
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
        self.write_sourced(text, kind, tags, None, embedder, created_at)
    }

    /// Save an approved fact with a pointer back to the exact session message.
    pub fn write_sourced(
        &mut self,
        text: &str,
        kind: &str,
        tags: Vec<String>,
        source: Option<MemorySource>,
        embedder: &dyn Embedder,
        created_at: u64,
    ) -> io::Result<String> {
        if text.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "memory text is empty",
            ));
        }
        let id = RunId::new().as_str().to_owned();
        let kind = if kind.trim().is_empty() {
            default_kind()
        } else {
            kind.trim().to_owned()
        };
        let text = text.trim().to_owned();
        self.mutate(|memories| {
            if let Some(existing) = memories.iter_mut().find(|memory| memory.text == text) {
                let mut changed = false;
                if existing.source.is_none() && source.is_some() {
                    existing.source = source.clone();
                    changed = true;
                }
                if existing.kind == "fact" && kind != "fact" {
                    existing.kind = kind.clone();
                    changed = true;
                }
                for tag in &tags {
                    if !existing.tags.contains(tag) {
                        existing.tags.push(tag.clone());
                        changed = true;
                    }
                }
                if changed {
                    existing.updated_at = Some(created_at);
                }
                return (existing.id.clone(), changed);
            }
            memories.push(Memory {
                id: id.clone(),
                text: text.clone(),
                kind,
                tags,
                created_at,
                updated_at: None,
                source,
                vector: embedder.embed(&text),
            });
            (id, true)
        })
    }

    /// Correct an outdated fact without changing its identity or provenance.
    pub fn update(
        &mut self,
        id: &str,
        text: &str,
        embedder: &dyn Embedder,
        now: u64,
    ) -> io::Result<bool> {
        let text = text.trim();
        if text.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "memory text is empty",
            ));
        }
        self.mutate(|memories| {
            let Some(memory) = memories.iter_mut().find(|memory| memory.id == id) else {
                return (false, false);
            };
            memory.text = text.to_owned();
            memory.vector = embedder.embed(text);
            memory.updated_at = Some(now);
            (true, true)
        })
    }

    /// The `k` memories most relevant to `query`, best first (positive only).
    pub fn search(&self, query: &str, embedder: &dyn Embedder, k: usize) -> Vec<&Memory> {
        if query.trim().is_empty() || k == 0 {
            return Vec::new();
        }
        let embedded = embedder.embed(query);
        let mut terms = lexical_terms(query);
        terms.retain(|word| {
            !matches!(
                word.as_str(),
                "the"
                    | "and"
                    | "for"
                    | "from"
                    | "with"
                    | "where"
                    | "what"
                    | "that"
                    | "this"
                    | "have"
            )
        });
        terms.sort();
        terms.dedup();
        let corpus_size = self.memories.len() as f32;
        let documents: Vec<_> = self
            .memories
            .iter()
            .map(|memory| {
                lexical_terms(&format!(
                    "{} {} {}",
                    memory.text,
                    memory.kind,
                    memory.tags.join(" ")
                ))
            })
            .collect();
        let average_length =
            (documents.iter().map(Vec::len).sum::<usize>() as f32 / corpus_size.max(1.0)).max(1.0);
        let frequencies: Vec<_> = terms
            .iter()
            .map(|term| {
                documents
                    .iter()
                    .filter(|document| document.contains(term))
                    .count() as f32
            })
            .collect();
        let mut scored: Vec<(f32, &Memory)> = self
            .memories
            .iter()
            .zip(documents.iter())
            .map(|(memory, document)| {
                let lexical = terms
                    .iter()
                    .zip(frequencies.iter())
                    .map(|(term, matching)| {
                        let frequency = document.iter().filter(|word| *word == term).count() as f32;
                        if frequency == 0.0 {
                            return 0.0;
                        }
                        let idf = (1.0 + (corpus_size - *matching + 0.5) / (*matching + 0.5)).ln();
                        let length = document.len() as f32 / average_length;
                        idf * frequency * 2.2 / (frequency + 1.2 * (0.25 + 0.75 * length))
                    })
                    .sum::<f32>();
                let phrase = if memory.text.to_lowercase().contains(&query.to_lowercase()) {
                    1.0
                } else {
                    0.0
                };
                let score = if lexical > 0.0 || phrase > 0.0 {
                    lexical + phrase + cosine(&embedded, &memory.vector).max(0.0) * 0.25
                } else {
                    0.0
                };
                (score, memory)
            })
            .filter(|(score, _)| *score > 0.0)
            .collect();
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    b.1.updated_at
                        .unwrap_or(b.1.created_at)
                        .cmp(&a.1.updated_at.unwrap_or(a.1.created_at))
                })
        });
        scored
            .into_iter()
            .take(k)
            .map(|(_, memory)| memory)
            .collect()
    }

    /// Fuse lexical and optional semantic rankings. A failed embeddings service
    /// leaves the offline lexical path usable.
    pub async fn search_hybrid(
        &self,
        query: &str,
        embedder: &dyn Embedder,
        semantic: Option<&dyn SemanticEmbedder>,
        k: usize,
    ) -> Vec<&Memory> {
        let lexical = self.search(query, embedder, self.memories.len());
        let Some(semantic) = semantic else {
            return lexical.into_iter().take(k).collect();
        };
        if query.trim().is_empty() || self.memories.is_empty() {
            return Vec::new();
        }
        let mut input = Vec::with_capacity(self.memories.len() + 1);
        input.push(query.to_owned());
        input.extend(self.memories.iter().map(|memory| memory.text.clone()));
        let Ok(vectors) = semantic.embed(&input).await else {
            return lexical.into_iter().take(k).collect();
        };
        if vectors.len() != input.len() {
            return lexical.into_iter().take(k).collect();
        }
        let mut scores = vec![0.0_f32; self.memories.len()];
        for (rank, memory) in lexical.iter().enumerate() {
            if let Some(index) = self.memories.iter().position(|item| item.id == memory.id) {
                scores[index] += 1.0 / (60 + rank) as f32;
            }
        }
        let mut semantic_ranked: Vec<_> = vectors[1..]
            .iter()
            .enumerate()
            .map(|(index, vector)| (index, cosine(&vectors[0], vector)))
            .filter(|(_, score)| *score > 0.0)
            .collect();
        semantic_ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (rank, (index, _)) in semantic_ranked.into_iter().enumerate() {
            scores[index] += 2.0 / (60 + rank) as f32;
        }
        let mut ranked: Vec<_> = self
            .memories
            .iter()
            .enumerate()
            .filter(|(index, _)| scores[*index] > 0.0)
            .collect();
        ranked.sort_by(|a, b| {
            scores[b.0]
                .partial_cmp(&scores[a.0])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        ranked
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
        self.mutate(|memories| {
            let before = memories.len();
            memories.retain(|memory| memory.id != id);
            let removed = memories.len() != before;
            (removed, removed)
        })
    }

    /// Drop every memory.
    pub fn clear(&mut self) -> io::Result<()> {
        self.mutate(|memories| {
            memories.clear();
            ((), true)
        })
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

    /// Query-aware prompt notes, bounded so a small model is not crowded out.
    pub fn relevant_catalog(&self, query: &str, n: usize, max_chars: usize) -> String {
        let hits = self.search(query, &lightagent_rag::HashingEmbedder, n);
        Self::catalog_from_hits(hits, max_chars)
    }

    pub async fn relevant_catalog_hybrid(
        &self,
        query: &str,
        n: usize,
        max_chars: usize,
        semantic: Option<&dyn SemanticEmbedder>,
    ) -> String {
        let hits = self
            .search_hybrid(query, &lightagent_rag::HashingEmbedder, semantic, n)
            .await;
        Self::catalog_from_hits(hits, max_chars)
    }

    fn catalog_from_hits(hits: Vec<&Memory>, max_chars: usize) -> String {
        let mut out = String::from("Relevant durable memories (use memory.search for detail):\n");
        for memory in hits {
            let line = format!("- [{}] {}\n", memory.id, memory.text.replace('\n', " "));
            if out.len() + line.len() > max_chars {
                break;
            }
            out.push_str(&line);
        }
        if out.lines().count() == 1 {
            String::new()
        } else {
            out
        }
    }

    fn mutate<T>(&mut self, change: impl FnOnce(&mut Vec<Memory>) -> (T, bool)) -> io::Result<T> {
        if let Some(parent) = self.path.parent() {
            paths::create_private_dir(parent).map_err(io::Error::other)?;
        }
        let lock_path = self.path.with_extension("lock");
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let lock = options.open(lock_path)?;
        lock.lock()?;
        let mut fresh = Self::open(&self.path)?;
        let (result, changed) = change(&mut fresh.memories);
        if changed {
            fresh.persist()?;
        }
        self.memories = fresh.memories;
        Ok(result)
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
        paths::write_private(&self.path, out.as_bytes())
    }
}

fn lexical_terms(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| word.len() >= 2)
        .map(str::to_lowercase)
        .collect()
}

/// The default memory file under a profile's `memory/` directory.
pub fn memory_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("memory").join("memories.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightagent_rag::HashingEmbedder;

    struct MockSemantic;

    #[async_trait::async_trait]
    impl SemanticEmbedder for MockSemantic {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts
                .iter()
                .map(|text| {
                    if text.contains("automobile") || text.contains("car") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect())
        }
    }

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

    #[test]
    fn stale_writers_merge_and_source_survives_reopen() {
        let path = scratch();
        let embedder = HashingEmbedder;
        let mut first = MemoryStore::open(&path).unwrap();
        let mut second = MemoryStore::open(&path).unwrap();
        first
            .write("Rust is preferred", "preference", vec![], &embedder, 1)
            .unwrap();
        let source = MemorySource {
            session_id: "abc".into(),
            message_index: 3,
        };
        let id = second
            .write_sourced(
                "Deploy with ops/deploy.sh",
                "fact",
                vec!["deployment".into()],
                Some(source.clone()),
                &embedder,
                2,
            )
            .unwrap();
        let mut reopened = MemoryStore::open(&path).unwrap();
        assert_eq!(reopened.len(), 2);
        assert_eq!(reopened.search("deployment", &embedder, 1)[0].id, id);
        assert_eq!(reopened.all()[1].source, Some(source));
        assert!(
            reopened
                .update(&id, "Deploy with ops/release.sh", &embedder, 3)
                .unwrap()
        );
        assert!(
            MemoryStore::open(&path).unwrap().all()[1]
                .text
                .contains("release.sh")
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn malformed_record_is_reported_without_rewriting_the_file() {
        let path = scratch();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json\n").unwrap();
        let error = match MemoryStore::open(&path) {
            Ok(_) => panic!("corrupt record must not be ignored"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains(":1:"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json\n");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn semantic_recall_finds_different_wording_when_enabled() {
        let path = scratch();
        let mut store = MemoryStore::open(&path).unwrap();
        store
            .write(
                "The car is in the garage",
                "fact",
                vec![],
                &HashingEmbedder,
                1,
            )
            .unwrap();
        store
            .write(
                "Bananas are on the table",
                "fact",
                vec![],
                &HashingEmbedder,
                2,
            )
            .unwrap();
        assert!(store.search("automobile", &HashingEmbedder, 1).is_empty());
        let hits = store
            .search_hybrid("automobile", &HashingEmbedder, Some(&MockSemantic), 1)
            .await;
        assert!(hits[0].text.contains("car"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
