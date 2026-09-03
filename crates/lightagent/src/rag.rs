//! `lightagent rag` — index documents and search them, and the `rag.search`
//! tool wired into a run.
//!
//! Retrieval is per-profile: the active profile's index lives at
//! `<profile>/rag/index.jsonl`. `add` chunks and embeds a file (or every file in
//! a directory), `search` returns the best passages, `list` shows what is
//! indexed, and `clear` empties it. The same store, opened read-only, backs the
//! `rag.search` tool a chat or served run is given when the index is non-empty.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lightagent_core::{Config, ConfigStore, LightagentPaths, ProfileStore};
use lightagent_rag::{HashingEmbedder, RagSearch, RagStore, index_path};
use lightagent_tools::Tool;

/// The `rag.search` tool for a run, or `None` when nothing is indexed.
pub(crate) fn rag_tool(profile_dir: &Path, config: &Config) -> Option<Arc<dyn Tool>> {
    let store = RagStore::open(index_path(profile_dir)).ok()?;
    if store.is_empty() {
        return None;
    }
    Some(Arc::new(RagSearch::new(Arc::new(store), config.rag.top_k)))
}

/// Resolve the active profile's index path and the loaded config.
fn active_index() -> Result<(PathBuf, Config), String> {
    let paths = LightagentPaths::resolve().map_err(|error| error.to_string())?;
    let config = ConfigStore::at(&paths)
        .load()
        .map_err(|error| error.to_string())?;
    let store = ProfileStore::new(paths.root());
    let active = store
        .active()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "no active profile — run `lightagent init` first".to_owned())?;
    let dir = store.handle(&active).dir().to_path_buf();
    Ok((index_path(&dir), config))
}

/// `rag add <path>` — index a file, or every file in a directory.
pub fn add(path: PathBuf, source: Option<String>, json: bool) -> Result<(), String> {
    let (index, config) = active_index()?;
    let mut store = RagStore::open(&index).map_err(|error| error.to_string())?;
    let embedder = HashingEmbedder;

    let mut targets = Vec::new();
    if path.is_dir() {
        let entries = std::fs::read_dir(&path).map_err(|error| error.to_string())?;
        for entry in entries.flatten() {
            if entry.path().is_file() {
                targets.push(entry.path());
            }
        }
        targets.sort();
    } else {
        targets.push(path.clone());
    }

    let mut total = 0;
    let mut indexed = Vec::new();
    for target in targets {
        let text = match std::fs::read_to_string(&target) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("· skipping {}: {error}", target.display());
                continue;
            }
        };
        let name = source.clone().unwrap_or_else(|| {
            target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| target.display().to_string())
        });
        let added = store
            .add(
                &name,
                &text,
                &embedder,
                config.rag.max_chunk_chars,
                config.rag.chunk_overlap_chars,
            )
            .map_err(|error| error.to_string())?;
        total += added;
        indexed.push((name, added));
    }

    if json {
        let value = serde_json::json!({
            "indexed": indexed.iter().map(|(n, c)| serde_json::json!({ "source": n, "chunks": c })).collect::<Vec<_>>(),
            "total_chunks": total,
        });
        println!("{value:#}");
    } else {
        for (name, added) in &indexed {
            println!("indexed {name} ({added} chunks)");
        }
        println!("{total} chunk(s) added.");
    }
    Ok(())
}

/// `rag search <query>` — the best passages for a query.
pub fn search(query: String, top_k: Option<usize>, json: bool) -> Result<(), String> {
    let (index, config) = active_index()?;
    let store = RagStore::open(&index).map_err(|error| error.to_string())?;
    let k = top_k.unwrap_or(config.rag.top_k).max(1);
    let hits = store.search(&query, &HashingEmbedder, k);

    if json {
        let value = serde_json::json!({
            "query": query,
            "hits": hits.iter().map(|hit| serde_json::json!({
                "source": hit.source, "score": hit.score, "text": hit.text,
            })).collect::<Vec<_>>(),
        });
        println!("{value:#}");
        return Ok(());
    }
    if hits.is_empty() {
        println!("No relevant passages found.");
        return Ok(());
    }
    for (rank, hit) in hits.iter().enumerate() {
        println!("[{}] {} (score {:.2})", rank + 1, hit.source, hit.score);
        println!("{}\n", hit.text);
    }
    Ok(())
}

/// `rag list` — the indexed sources and their chunk counts.
pub fn list(json: bool) -> Result<(), String> {
    let (index, _) = active_index()?;
    let store = RagStore::open(&index).map_err(|error| error.to_string())?;
    let sources = store.sources();
    if json {
        let value = serde_json::json!({
            "sources": sources.iter().map(|(n, c)| serde_json::json!({ "source": n, "chunks": c })).collect::<Vec<_>>(),
            "total_chunks": store.len(),
        });
        println!("{value:#}");
        return Ok(());
    }
    if sources.is_empty() {
        println!("Nothing indexed. Add documents with `lightagent rag add <path>`.");
        return Ok(());
    }
    for (name, count) in sources {
        println!("{name}  ({count} chunks)");
    }
    Ok(())
}

/// `rag clear` — empty the index.
pub fn clear(json: bool) -> Result<(), String> {
    let (index, _) = active_index()?;
    let mut store = RagStore::open(&index).map_err(|error| error.to_string())?;
    store.clear().map_err(|error| error.to_string())?;
    if json {
        println!("{}", serde_json::json!({ "cleared": true }));
    } else {
        println!("Index cleared.");
    }
    Ok(())
}
