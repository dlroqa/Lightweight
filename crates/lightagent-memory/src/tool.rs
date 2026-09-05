//! `memory.write` and `memory.search` — remember and recall durable facts.
//!
//! Both hold the profile's memory-file path directly and open it per call (like
//! the RAG and MCP tools, needing no `ToolCtx` injection), so a fact written this
//! turn is visible to a search the next. `memory.write` is
//! [`RiskClass::Mutating`](lightagent_core::RiskClass::Mutating) — it changes
//! durable state, so the default policy asks first; `memory.search` is
//! [`RiskClass::Observe`](lightagent_core::RiskClass::Observe).
//!
//! Concurrency note: a write is read-modify-write on the file, so two writes racing
//! in a served deployment are last-writer-wins. Memory writes are infrequent and
//! approval-gated, so this is acceptable; a lock would be the fix if it mattered.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use lightagent_core::{RiskClass, Scope, ToolOutcome};
use lightagent_rag::HashingEmbedder;
use lightagent_tools::{Tool, ToolCtx, ToolDefinition};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::store::MemoryStore;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// The `memory.write` tool.
pub struct MemoryWrite {
    definition: ToolDefinition,
    path: PathBuf,
}

impl MemoryWrite {
    pub const NAME: &'static str = "memory.write";

    pub fn new(path: PathBuf) -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "The fact to remember." },
                "kind": { "type": "string", "description": "A coarse kind, e.g. fact or preference." },
                "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tags." }
            },
            "required": ["text"],
            "additionalProperties": false,
        });
        Self {
            definition: ToolDefinition::new(
                Self::NAME,
                "Remember a durable fact for later sessions.",
                parameters,
                RiskClass::Mutating,
                vec![Scope::new("memory:write")],
            ),
            path,
        }
    }
}

#[derive(Deserialize)]
struct WriteArgs {
    text: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[async_trait]
impl Tool for MemoryWrite {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, args: &Value, _ctx: &ToolCtx) -> ToolOutcome {
        let Ok(args) = serde_json::from_value::<WriteArgs>(args.clone()) else {
            return ToolOutcome::error("could not read memory.write arguments");
        };
        if args.text.trim().is_empty() {
            return ToolOutcome::error("nothing to remember: text is empty");
        }
        let mut store = match MemoryStore::open(&self.path) {
            Ok(store) => store,
            Err(error) => return ToolOutcome::error(format!("could not open memory: {error}")),
        };
        match store.write(
            &args.text,
            args.kind.as_deref().unwrap_or(""),
            args.tags,
            &HashingEmbedder,
            now_secs(),
        ) {
            Ok(id) => ToolOutcome::ok(format!("remembered ({id})")),
            Err(error) => ToolOutcome::error(format!("could not save memory: {error}")),
        }
    }
}

/// The `memory.search` tool.
pub struct MemorySearch {
    definition: ToolDefinition,
    path: PathBuf,
    top_k: usize,
}

impl MemorySearch {
    pub const NAME: &'static str = "memory.search";

    pub fn new(path: PathBuf, top_k: usize) -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to recall." },
                "top_k": { "type": "integer", "minimum": 1, "description": "How many memories to return." }
            },
            "required": ["query"],
            "additionalProperties": false,
        });
        Self {
            definition: ToolDefinition::new(
                Self::NAME,
                "Recall the memories most relevant to a query.",
                parameters,
                RiskClass::Observe,
                vec![Scope::new("memory:read")],
            ),
            path,
            top_k: top_k.max(1),
        }
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    top_k: Option<usize>,
}

#[async_trait]
impl Tool for MemorySearch {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, args: &Value, _ctx: &ToolCtx) -> ToolOutcome {
        let Ok(args) = serde_json::from_value::<SearchArgs>(args.clone()) else {
            return ToolOutcome::error("could not read memory.search arguments");
        };
        let store = match MemoryStore::open(&self.path) {
            Ok(store) => store,
            Err(error) => return ToolOutcome::error(format!("could not open memory: {error}")),
        };
        let k = args.top_k.unwrap_or(self.top_k).clamp(1, 50);
        let hits = store.search(&args.query, &HashingEmbedder, k);
        if hits.is_empty() {
            return ToolOutcome::ok("No relevant memories.");
        }
        let mut out = String::new();
        for memory in hits {
            out.push_str(&format!("- ({}) {}\n", memory.kind, memory.text));
        }
        ToolOutcome::ok(out.trim_end().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn scratch() -> PathBuf {
        std::env::temp_dir().join(format!(
            "lightagent-memtool-{}-{}/memories.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    #[tokio::test]
    async fn write_then_search_via_tools() {
        let path = scratch();
        let ctx = ToolCtx::new(CancellationToken::new());
        let write = MemoryWrite::new(path.clone());
        let out = write
            .call(
                &json!({ "text": "The API key lives in the vault.", "kind": "fact" }),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("remembered"));

        let search = MemorySearch::new(path.clone(), 5);
        let found = search
            .call(&json!({ "query": "where is the api key" }), &ctx)
            .await;
        assert!(!found.is_error);
        assert!(found.content.contains("vault"));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn write_is_mutating_and_search_observes() {
        assert_eq!(
            MemoryWrite::new(scratch()).definition().risk,
            RiskClass::Mutating
        );
        assert_eq!(
            MemorySearch::new(scratch(), 5).definition().risk,
            RiskClass::Observe
        );
    }
}
