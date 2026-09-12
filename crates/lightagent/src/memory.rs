//! `lightagent memory` — write, recall and manage durable memories, and the
//! `memory.write`/`memory.search` tools and per-request recall wired into a run.
//!
//! Memory is per-profile: the active profile's memories live at
//! `<profile>/memory/memories.jsonl`. The CLI edits them directly; a run is given
//! the tools over the same file and, unless disabled, a small relevant-memory
//! selection is added to each request.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use lightagent_core::{Config, ConfigStore, LightagentPaths, ProfileStore};
use lightagent_memory::{
    MemoryReflect, MemorySearch, MemorySource, MemoryStore, MemoryWrite, SessionLookup,
    memory_path, retain,
};
use lightagent_rag::HashingEmbedder;
use lightagent_store::{Session, SessionId, SessionStore};
use lightagent_tools::Tool;

/// Durable recall and source lookup tools for a run.
pub(crate) fn memory_tools(profile_dir: &Path, config: &Config) -> Vec<Arc<dyn Tool>> {
    let path = memory_path(profile_dir);
    let search = MemorySearch::new(path.clone(), config.memory.top_k);
    let search = match crate::rag::semantic_embedder(config) {
        Some(semantic) => search.with_semantic(semantic),
        None => search,
    };
    vec![
        Arc::new(MemoryWrite::new(path.clone())),
        Arc::new(search),
        Arc::new(MemoryReflect::new(path)),
        Arc::new(SessionLookup::new(SessionStore::new(
            profile_dir.join("sessions"),
        ))),
    ]
}

/// Retain only explicit, durable user statements. The source points back to
/// the saved transcript when one exists; unclear statements remain there.
pub(crate) fn capture(
    profile_dir: &Path,
    config: &Config,
    message: &str,
    source: Option<MemorySource>,
) -> Result<usize, String> {
    if !config.memory.auto_capture {
        return Ok(0);
    }
    let mut store = MemoryStore::open(memory_path(profile_dir)).map_err(|e| e.to_string())?;
    retain(&mut store, message, source, now_secs()).map_err(|e| e.to_string())
}

/// Select a small set of memories for this request's prompt.
pub(crate) async fn relevant_catalog(
    profile_dir: &Path,
    config: &Config,
    query: &str,
) -> Result<String, String> {
    if config.memory.inject_recent == 0 {
        return Ok(String::new());
    }
    let store = MemoryStore::open(memory_path(profile_dir)).map_err(|error| error.to_string())?;
    let count = config.memory.inject_recent.min(config.memory.top_k).min(3);
    let semantic = crate::rag::semantic_embedder(config);
    let relevant = store
        .relevant_catalog_hybrid(query, count, 600, semantic.as_deref())
        .await;
    if !relevant.is_empty() {
        return Ok(relevant);
    }
    let fallback = store
        .recent(store.len())
        .into_iter()
        .find(|memory| memory.kind == "preference");
    Ok(fallback
        .map(|memory| {
            format!(
                "Recent durable memory [{}]: {}",
                memory.id,
                memory.text.chars().take(350).collect::<String>()
            )
        })
        .unwrap_or_default())
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

fn saved_session(raw_id: &str) -> Result<Session, String> {
    let paths = LightagentPaths::resolve().map_err(|error| error.to_string())?;
    let profiles = ProfileStore::new(paths.root());
    let active = profiles
        .active()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "no active profile".to_owned())?;
    let id = SessionId::parse(raw_id).map_err(|error| error.to_string())?;
    SessionStore::at_profile(&profiles.handle(&active))
        .load(&id)
        .map_err(|error| error.to_string())
}

/// Show user statements that look durable; nothing is saved until `promote`.
pub fn candidates(session_id: String, json: bool) -> Result<(), String> {
    let session = saved_session(&session_id)?;
    let candidates: Vec<_> = session
        .messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            if message.role != "user" {
                return false;
            }
            let text = message.content.to_lowercase();
            [
                "prefer",
                "remember",
                "we decided",
                "always",
                "never",
                "my ",
                "our project",
                "uses ",
            ]
            .iter()
            .any(|marker| text.contains(marker))
        })
        .map(|(index, message)| (index + 1, message.content.as_str()))
        .collect();
    if json {
        println!(
            "{}",
            serde_json::json!({"session_id": session_id, "candidates": candidates.iter().map(|(index, text)| serde_json::json!({"message": index, "text": text})).collect::<Vec<_>>()})
        );
    } else if candidates.is_empty() {
        println!("No suggested facts. Any user message can still be promoted by its number.");
    } else {
        for (index, text) in candidates {
            println!("{index}: {}", text.replace('\n', " "));
        }
    }
    Ok(())
}

/// Save exactly one user-selected message, optionally edited, with provenance.
pub fn promote(
    session_id: String,
    message_number: usize,
    text: Option<String>,
    kind: Option<String>,
    tags: Vec<String>,
    json: bool,
) -> Result<(), String> {
    let session = saved_session(&session_id)?;
    let message = session
        .messages
        .get(message_number.saturating_sub(1))
        .filter(|message| message_number > 0 && message.role == "user")
        .ok_or_else(|| "choose a user message number from the saved session".to_owned())?;
    let fact = text.as_deref().unwrap_or(&message.content);
    let (path, _) = active_memory()?;
    let mut store = MemoryStore::open(&path).map_err(|error| error.to_string())?;
    let id = store
        .write_sourced(
            fact,
            kind.as_deref().unwrap_or("fact"),
            tags,
            Some(MemorySource {
                session_id,
                message_index: message_number,
            }),
            &HashingEmbedder,
            now_secs(),
        )
        .map_err(|error| error.to_string())?;
    if json {
        println!("{}", serde_json::json!({"id": id}));
    } else {
        println!("remembered ({id})");
    }
    Ok(())
}

pub fn update(id: String, text: String, json: bool) -> Result<(), String> {
    let (path, _) = active_memory()?;
    let mut store = MemoryStore::open(&path).map_err(|error| error.to_string())?;
    let updated = store
        .update(&id, &text, &HashingEmbedder, now_secs())
        .map_err(|error| error.to_string())?;
    if json {
        println!("{}", serde_json::json!({"updated": updated}));
    } else if updated {
        println!("updated {id}");
    } else {
        println!("no memory with id {id}");
    }
    Ok(())
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
                "id": m.id, "kind": m.kind, "tags": m.tags, "created_at": m.created_at,
                "updated_at": m.updated_at, "source": m.source, "text": m.text,
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
        let source = memory
            .source
            .as_ref()
            .map(|source| format!("  from {}#{}", source.session_id, source.message_index))
            .unwrap_or_default();
        println!("{}  ({})  {}{source}", memory.id, memory.kind, memory.text);
    }
    Ok(())
}

/// Render the structured working knowledge derived from the bank.
pub fn reflect(topic: Option<String>, json: bool) -> Result<(), String> {
    let (path, _) = active_memory()?;
    let store = MemoryStore::open(&path).map_err(|e| e.to_string())?;
    let page = store.knowledge_page(topic.as_deref(), 32_000);
    if json {
        println!(
            "{}",
            serde_json::json!({ "topic": topic, "knowledge": page })
        );
    } else {
        println!("{page}");
    }
    Ok(())
}

/// `memory search <query>` — the most relevant memories.
pub async fn search(query: String, top_k: Option<usize>, json: bool) -> Result<(), String> {
    let (path, config) = active_memory()?;
    let store = MemoryStore::open(&path).map_err(|error| error.to_string())?;
    let k = top_k.unwrap_or(config.memory.top_k).max(1);
    let semantic = crate::rag::semantic_embedder(&config);
    let hits = store
        .search_hybrid(&query, &HashingEmbedder, semantic.as_deref(), k)
        .await;
    if json {
        let value = serde_json::json!({
            "query": query,
            "memories": hits.iter().map(|m| serde_json::json!({
                "id": m.id, "kind": m.kind, "source": m.source, "text": m.text,
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
        let source = memory
            .source
            .as_ref()
            .map(|source| format!(" [from {}#{}]", source.session_id, source.message_index))
            .unwrap_or_default();
        println!("({}) {}{source}", memory.kind, memory.text);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn automatic_fact_is_recalled_in_a_later_session() {
        let dir = std::env::temp_dir().join(format!(
            "lightagent-memory-cycle-{}",
            lightagent_core::RunId::new().as_str()
        ));
        let config = Config::default();
        capture(
            &dir,
            &config,
            "I prefer concise answers.",
            Some(MemorySource {
                session_id: "first-session".to_owned(),
                message_index: 1,
            }),
        )
        .unwrap();
        let recalled = relevant_catalog(&dir, &config, "How long should your answers be?")
            .await
            .unwrap();
        assert!(recalled.contains("I prefer concise answers"));
        let page = MemoryStore::open(memory_path(&dir))
            .unwrap()
            .knowledge_page(Some("preference"), 8_000);
        assert!(page.contains("## Preferences"));
        assert!(page.contains("first-session#1"));

        let mut disabled = config;
        disabled.memory.auto_capture = false;
        assert_eq!(
            capture(&dir, &disabled, "We decided to use SQLite.", None).unwrap(),
            0
        );
        assert_eq!(MemoryStore::open(memory_path(&dir)).unwrap().len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }
}
