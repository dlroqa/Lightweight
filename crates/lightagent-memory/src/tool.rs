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
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use lightagent_core::{RiskClass, Scope, ToolOutcome};
use lightagent_rag::{HashingEmbedder, SemanticEmbedder};
use lightagent_store::{SessionId, SessionStore};
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
    semantic: Option<Arc<dyn SemanticEmbedder>>,
}

/// Query the structured working knowledge derived from the memory bank.
pub struct MemoryReflect {
    definition: ToolDefinition,
    path: PathBuf,
}

impl MemoryReflect {
    pub const NAME: &'static str = "memory.reflect";

    pub fn new(path: PathBuf) -> Self {
        Self {
            definition: ToolDefinition::new(
                Self::NAME,
                "Read a structured knowledge page of established preferences, decisions, conventions and fixes.",
                json!({
                    "type": "object",
                    "properties": {
                        "topic": { "type": "string", "description": "Optional topic or category to focus on." }
                    },
                    "additionalProperties": false
                }),
                RiskClass::Observe,
                vec![Scope::new("memory:read")],
            ),
            path,
        }
    }
}

#[async_trait]
impl Tool for MemoryReflect {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, args: &Value, _ctx: &ToolCtx) -> ToolOutcome {
        let topic = args.get("topic").and_then(Value::as_str);
        let store = match MemoryStore::open(&self.path) {
            Ok(store) => store,
            Err(error) => return ToolOutcome::error(format!("could not open memory: {error}")),
        };
        ToolOutcome::ok(store.knowledge_page(topic, 8_000))
    }
}

/// Read an exact saved session message or bounded tool result by reference.
pub struct SessionLookup {
    definition: ToolDefinition,
    sessions: SessionStore,
}

impl SessionLookup {
    pub const NAME: &'static str = "session.lookup";

    pub fn new(sessions: SessionStore) -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "The saved session id from a memory source." },
                "message": { "type": "integer", "minimum": 1, "description": "One-based message number." },
                "tool_call_id": { "type": "string", "description": "A saved tool-call id." },
                "offset": { "type": "integer", "minimum": 0, "description": "Character offset for a long message." }
            },
            "required": ["session_id"],
            "additionalProperties": false
        });
        Self {
            definition: ToolDefinition::new(
                Self::NAME,
                "Read a cited message or tool result from a saved session in this profile.",
                parameters,
                RiskClass::Observe,
                vec![Scope::new("sessions:read")],
            ),
            sessions,
        }
    }
}

#[derive(Deserialize)]
struct LookupArgs {
    session_id: String,
    message: Option<usize>,
    tool_call_id: Option<String>,
    #[serde(default)]
    offset: usize,
}

#[async_trait]
impl Tool for SessionLookup {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, args: &Value, _ctx: &ToolCtx) -> ToolOutcome {
        let Ok(args) = serde_json::from_value::<LookupArgs>(args.clone()) else {
            return ToolOutcome::error("could not read session.lookup arguments");
        };
        let Ok(id) = SessionId::parse(&args.session_id) else {
            return ToolOutcome::error("invalid session id");
        };
        let Ok(session) = self.sessions.load(&id) else {
            return ToolOutcome::error("saved session was not found or could not be read");
        };
        let (label, content) = match (args.message, args.tool_call_id) {
            (Some(number), None) if number > 0 => match session.messages.get(number - 1) {
                Some(message) => (
                    format!("message {number} ({})", message.role),
                    message.content.as_str(),
                ),
                None => return ToolOutcome::error("message number is outside this session"),
            },
            (None, Some(call_id)) => match session
                .runs
                .iter()
                .flat_map(|run| &run.tools)
                .find(|tool| tool.id == call_id)
            {
                Some(tool) => (
                    format!("tool {} ({})", call_id, tool.tool),
                    tool.result_excerpt.as_str(),
                ),
                None => return ToolOutcome::error("tool call was not found in this session"),
            },
            _ => return ToolOutcome::error("specify exactly one of message or tool_call_id"),
        };
        let total = content.chars().count();
        let start = args.offset.min(total);
        let end = start.saturating_add(2_000).min(total);
        let excerpt: String = content.chars().skip(start).take(end - start).collect();
        ToolOutcome::ok(format!(
            "session {} {label}, chars {start}..{end}/{total}:\n{excerpt}",
            id.as_str()
        ))
    }
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
            semantic: None,
        }
    }

    pub fn with_semantic(mut self, semantic: Arc<dyn SemanticEmbedder>) -> Self {
        self.semantic = Some(semantic);
        self
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
        let hits = store
            .search_hybrid(&args.query, &HashingEmbedder, self.semantic.as_deref(), k)
            .await;
        if hits.is_empty() {
            return ToolOutcome::ok("No relevant memories.");
        }
        let mut out = String::new();
        for memory in hits {
            let source = memory
                .source
                .as_ref()
                .map(|source| {
                    format!(
                        " from session {} message {}",
                        source.session_id, source.message_index
                    )
                })
                .unwrap_or_default();
            out.push_str(&format!(
                "- [{}] ({}) {}{source}\n",
                memory.id, memory.kind, memory.text
            ));
        }
        ToolOutcome::ok(out.trim_end().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightagent_store::{Session, StoredMessage};
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
    async fn reflect_tool_returns_grouped_knowledge() {
        let path = scratch();
        MemoryStore::open(&path)
            .unwrap()
            .write(
                "I prefer concise answers",
                "preference",
                vec![],
                &HashingEmbedder,
                1,
            )
            .unwrap();
        let result = MemoryReflect::new(path.clone())
            .call(
                &json!({"topic":"preference"}),
                &ToolCtx::new(CancellationToken::new()),
            )
            .await;
        assert!(!result.is_error);
        assert!(result.content.contains("## Preferences"));
        assert!(result.content.contains("I prefer concise answers"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn session_lookup_reads_a_promoted_memory_source() {
        let path = scratch();
        let sessions = SessionStore::new(path.parent().unwrap().join("sessions"));
        let mut session = Session::new("default", "chat");
        session.push_message(StoredMessage::new("user", "I prefer concise answers."));
        sessions.save(&session).unwrap();
        let tool = SessionLookup::new(sessions);
        let ctx = ToolCtx::new(CancellationToken::new());
        let result = tool
            .call(
                &json!({
                    "session_id": session.id.as_str(), "message": 1
                }),
                &ctx,
            )
            .await;
        assert!(!result.is_error);
        assert!(result.content.contains("I prefer concise answers."));
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
