//! `lightagent memory` — write, recall and manage durable memories, and the
//! `memory.write`/`memory.search` tools and prompt snapshot wired into a run.
//!
//! Memory is per-profile: the active profile's memories live at
//! `<profile>/memory/memories.jsonl`. The CLI edits them directly; a run is given
//! the two tools over the same file and, unless disabled, a snapshot of the most
//! recent memories appended to the system prompt.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use lightagent_core::{Config, ConfigStore, LightagentPaths, ProfileStore};
use lightagent_memory::{MemorySearch, MemoryStore, MemoryWrite, memory_path};
use lightagent_rag::HashingEmbedder;
use lightagent_tools::Tool;

/// The memory tools for a run: `memory.write` and `memory.search`.
pub(crate) fn memory_tools(profile_dir: &Path, config: &Config) -> Vec<Arc<dyn Tool>> {
    let path = memory_path(profile_dir);
    vec![
        Arc::new(MemoryWrite::new(path.clone())),
        Arc::new(MemorySearch::new(path, config.memory.top_k)),
    ]
}

/// The recent-memory snapshot for the system prompt, or empty.
pub(crate) fn recent_catalog(profile_dir: &Path, config: &Config) -> String {
    if config.memory.inject_recent == 0 {
        return String::new();
    }
    match MemoryStore::open(memory_path(profile_dir)) {
        Ok(store) => store.recent_catalog(config.memory.inject_recent),
        Err(_) => String::new(),
    }
}

fn active_memory() -> Result<(PathBuf, Config), String> {
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
    Ok((memory_path(&dir), config))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// `memory add <text>` — remember a fact.
pub fn add(
    text: String,
    kind: Option<String>,
    tags: Vec<String>,
    json: bool,
) -> Result<(), String> {
    let (path, _) = active_memory()?;
    let mut store = MemoryStore::open(&path).map_err(|error| error.to_string())?;
    let id = store
        .write(
            &text,
            kind.as_deref().unwrap_or("fact"),
            tags,
            &HashingEmbedder,
            now_secs(),
        )
        .map_err(|error| error.to_string())?;
    if json {
        println!("{}", serde_json::json!({ "id": id }));
    } else {
        println!("remembered ({id})");
    }
    Ok(())
}

/// `memory list` — every memory.
pub fn list(json: bool) -> Result<(), String> {
    let (path, _) = active_memory()?;
    let store = MemoryStore::open(&path).map_err(|error| error.to_string())?;
    if json {
        let value = serde_json::json!({
            "memories": store.all().iter().map(|m| serde_json::json!({
                "id": m.id, "kind": m.kind, "tags": m.tags, "created_at": m.created_at, "text": m.text,
            })).collect::<Vec<_>>(),
        });
        println!("{value:#}");
        return Ok(());
    }
    if store.is_empty() {
        println!("No memories. Add one with `lightagent memory add <text>`.");
        return Ok(());
    }
    for memory in store.all() {
        println!("{}  ({})  {}", memory.id, memory.kind, memory.text);
    }
    Ok(())
}

/// `memory search <query>` — the most relevant memories.
pub fn search(query: String, top_k: Option<usize>, json: bool) -> Result<(), String> {
    let (path, config) = active_memory()?;
    let store = MemoryStore::open(&path).map_err(|error| error.to_string())?;
    let k = top_k.unwrap_or(config.memory.top_k).max(1);
    let hits = store.search(&query, &HashingEmbedder, k);
    if json {
        let value = serde_json::json!({
            "query": query,
            "memories": hits.iter().map(|m| serde_json::json!({
                "id": m.id, "kind": m.kind, "text": m.text,
            })).collect::<Vec<_>>(),
        });
        println!("{value:#}");
        return Ok(());
    }
    if hits.is_empty() {
        println!("No relevant memories.");
        return Ok(());
    }
    for memory in hits {
        println!("({}) {}", memory.kind, memory.text);
    }
    Ok(())
}

/// `memory forget <id>` — remove one memory.
pub fn forget(id: String, json: bool) -> Result<(), String> {
    let (path, _) = active_memory()?;
    let mut store = MemoryStore::open(&path).map_err(|error| error.to_string())?;
    let removed = store.forget(&id).map_err(|error| error.to_string())?;
    if json {
        println!("{}", serde_json::json!({ "removed": removed }));
    } else if removed {
        println!("forgot {id}");
    } else {
        println!("no memory with id {id}");
    }
    Ok(())
}

/// `memory clear` — forget everything.
pub fn clear(json: bool) -> Result<(), String> {
    let (path, _) = active_memory()?;
    let mut store = MemoryStore::open(&path).map_err(|error| error.to_string())?;
    store.clear().map_err(|error| error.to_string())?;
    if json {
        println!("{}", serde_json::json!({ "cleared": true }));
    } else {
        println!("Memory cleared.");
    }
    Ok(())
}
