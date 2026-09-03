//! `rag.search` — retrieve the passages most relevant to a query.
//!
//! [`RiskClass::Observe`](lightagent_core::RiskClass::Observe): it reads the
//! profile's own indexed text and changes nothing. It holds its store (and, for
//! hybrid retrieval, an optional semantic embedder) directly, built by the
//! caller, so it needs no `ToolCtx` injection — the same shape the MCP tools use.

use std::sync::Arc;

use async_trait::async_trait;
use lightagent_core::{RiskClass, Scope, ToolOutcome};
use lightagent_tools::{Tool, ToolCtx, ToolDefinition};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::embed::{HashingEmbedder, SemanticEmbedder};
use crate::store::RagStore;

/// The `rag.search` tool.
pub struct RagSearch {
    definition: ToolDefinition,
    store: Arc<RagStore>,
    semantic: Option<Arc<dyn SemanticEmbedder>>,
    top_k: usize,
}

impl RagSearch {
    pub const NAME: &'static str = "rag.search";

    /// Build the tool over an opened `store`, an optional `semantic` embedder for
    /// hybrid retrieval, defaulting to `top_k` results.
    pub fn new(
        store: Arc<RagStore>,
        semantic: Option<Arc<dyn SemanticEmbedder>>,
        top_k: usize,
    ) -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to look for." },
                "top_k": { "type": "integer", "minimum": 1, "description": "How many passages to return." }
            },
            "required": ["query"],
            "additionalProperties": false,
        });
        Self {
            definition: ToolDefinition::new(
                Self::NAME,
                "Search the indexed documents and return the most relevant passages.",
                parameters,
                RiskClass::Observe,
                vec![Scope::new("rag:search")],
            ),
            store,
            semantic,
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
impl Tool for RagSearch {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, args: &Value, _ctx: &ToolCtx) -> ToolOutcome {
        let Ok(args) = serde_json::from_value::<SearchArgs>(args.clone()) else {
            return ToolOutcome::error("could not read rag.search arguments");
        };
        let k = args.top_k.unwrap_or(self.top_k).clamp(1, 50);
        let hits = self
            .store
            .search(&args.query, &HashingEmbedder, self.semantic.as_deref(), k)
            .await;
        if hits.is_empty() {
            return ToolOutcome::ok("No relevant passages found.");
        }
        let mut out = String::new();
        for (rank, hit) in hits.iter().enumerate() {
            out.push_str(&format!(
                "[{}] {} (score {:.3})\n{}\n\n",
                rank + 1,
                hit.source,
                hit.score,
                hit.text
            ));
        }
        ToolOutcome::ok(out.trim_end().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn tool_is_observe_class() {
        let store = Arc::new(RagStore::open(std::env::temp_dir().join("rag-none.jsonl")).unwrap());
        let tool = RagSearch::new(store, None, 5);
        assert_eq!(tool.definition().risk, RiskClass::Observe);
    }

    #[tokio::test]
    async fn returns_a_message_when_empty() {
        let path = std::env::temp_dir().join(format!("rag-empty-{}.jsonl", std::process::id()));
        let store = Arc::new(RagStore::open(&path).unwrap());
        let tool = RagSearch::new(store, None, 5);
        let out = tool
            .call(
                &json!({ "query": "anything" }),
                &ToolCtx::new(CancellationToken::new()),
            )
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("No relevant"));
    }
}
