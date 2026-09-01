//! Minimal `Deserialize` structs for the inbound chunk subset.
//!
//! The gateway's own chunk types (`lightweight-api::stream`) are built for
//! *emission* — Serialize-only, and in a crate this adapter must not depend on.
//! Parsing them back would be both a coupling and a misuse, so the inbound
//! shape is declared here from scratch, carrying only the fields the adapter
//! reads: the streamed delta, the finish reason, the usage totals, and the
//! terminal error chunk.

use serde::Deserialize;

/// One `chat.completion.chunk`, or a usage/error chunk sharing the shape.
#[derive(Debug, Default, Deserialize)]
pub struct ChatChunk {
    /// Empty on the usage chunk, and only there.
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<UsageBody>,
    /// Set only on a stream that failed after headers were sent.
    #[serde(default)]
    pub error: Option<ErrorBody>,
}

/// One choice inside a chunk.
#[derive(Debug, Default, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub delta: Delta,
    /// `null` until the final content chunk.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The `delta` object of a streamed choice.
#[derive(Debug, Default, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    /// The engine emits `reasoning_content`; some builds use `reasoning`.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

impl Delta {
    /// The reasoning fragment under whichever spelling is present.
    pub fn reasoning_text(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .or(self.reasoning.as_deref())
    }
}

/// A tool-call fragment inside a delta.
#[derive(Debug, Default, Deserialize)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: u32,
    /// Present on the first delta of a call and never again.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Default, Deserialize)]
pub struct FunctionDelta {
    /// Sent whole, once.
    #[serde(default)]
    pub name: Option<String>,
    /// Concatenated across deltas, in order.
    #[serde(default)]
    pub arguments: Option<String>,
}

/// The token accounting carried by the usage chunk.
#[derive(Debug, Default, Deserialize)]
pub struct UsageBody {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

/// The body of a terminal error chunk.
#[derive(Debug, Default, Deserialize)]
pub struct ErrorBody {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub r#type: Option<String>,
}

/// The `GET /v1/models` response envelope.
#[derive(Debug, Default, Deserialize)]
pub struct ModelsResponse {
    #[serde(default)]
    pub data: Vec<ModelEntry>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ModelEntry {
    #[serde(default)]
    pub id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_chunk_parses_with_empty_content() {
        let chunk: ChatChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
        )
        .expect("parse");
        assert_eq!(chunk.choices[0].delta.role.as_deref(), Some("assistant"));
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some(""));
    }

    #[test]
    fn a_usage_chunk_has_empty_choices() {
        let chunk: ChatChunk = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":36,"completion_tokens":3,"total_tokens":39}}"#,
        )
        .expect("parse");
        assert!(chunk.choices.is_empty());
        let usage = chunk.usage.expect("usage");
        assert_eq!(usage.total_tokens, 39);
    }

    #[test]
    fn an_error_chunk_carries_its_message() {
        let chunk: ChatChunk =
            serde_json::from_str(r#"{"choices":[],"error":{"message":"engine died"}}"#)
                .expect("parse");
        assert_eq!(chunk.error.expect("error").message, "engine died");
    }

    #[test]
    fn reasoning_reads_either_spelling() {
        let a: Delta = serde_json::from_str(r#"{"reasoning_content":"x"}"#).expect("parse");
        let b: Delta = serde_json::from_str(r#"{"reasoning":"y"}"#).expect("parse");
        assert_eq!(a.reasoning_text(), Some("x"));
        assert_eq!(b.reasoning_text(), Some("y"));
    }

    #[test]
    fn models_response_parses() {
        let models: ModelsResponse =
            serde_json::from_str(r#"{"object":"list","data":[{"id":"m@8k"},{"id":"n@4k"}]}"#)
                .expect("parse");
        assert_eq!(models.data.len(), 2);
        assert_eq!(models.data[0].id, "m@8k");
    }
}
