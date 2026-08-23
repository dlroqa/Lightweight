//! `GET /v1/models`.
//!
//! Optional to the client and load-bearing anyway. Hermes scans the rows for a
//! context length and sizes every prompt to what it finds, so this endpoint
//! decides whether conversations fit.
//!
//! The scan is *recursive over nested dictionaries*, matches these keys
//! case-insensitively, and takes the **first** one it finds
//! (`agent/model_metadata.py:1119-1132`):
//!
//! ```text
//! context_length  context_window  context_size  max_context_length
//! max_position_embeddings  max_model_len  max_input_tokens
//! max_sequence_length  max_seq_len  n_ctx_train  n_ctx  ctx_size
//! ```
//!
//! Two consequences shape the row below.
//!
//! **We advertise the effective context, not the model's ceiling.** A model
//! that supports 128K but is loaded at 8192 must report 8192, or the client
//! sizes a prompt to 128K and every request overflows.
//!
//! **The ceiling lives under a key the scanner does not recognize.** It is
//! genuinely useful — the UI offers it as a context preset — so it is reported
//! as `hermes.model_max_context_length`, deliberately *not* one of the twelve
//! names above. `max_context_length` would have been read as the effective
//! value and undone the point.

use serde::{Deserialize, Serialize};

/// One row of `GET /v1/models`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelRow {
    /// Our catalog id, context suffix included: `lfm2-1.2b-q4_k_m@8k`.
    ///
    /// The suffix is not decoration. The client caches a context length under
    /// `f"{model}@{base_url}"`, so serving the same model at a different
    /// context under the same name leaves a stale entry behind. Encoding the
    /// context in the id makes a change produce a new cache key instead.
    pub id: String,
    pub object: String,
    pub created: u64,
    pub owned_by: String,
    /// The **effective** context, repeated under every spelling a client might
    /// look for. One number, several names: they can never disagree.
    pub context_length: u32,
    pub n_ctx: u32,
    pub max_tokens: u32,
    pub max_output_tokens: u32,
    /// Everything the OpenAI schema has no room for.
    pub hermes: HermesModelInfo,
}

/// Our own namespace on a model row.
///
/// Kept under one key so nothing here can be mistaken for a standard field —
/// and, specifically, so the model's true ceiling cannot be read as its
/// effective context.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HermesModelInfo {
    pub architecture: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantization: Option<String>,
    /// The largest context this model's metadata declares.
    ///
    /// Named to stay outside the client's recognized key set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_max_context_length: Option<u64>,
    pub state: ModelState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram_verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
}

/// Whether a model can serve a request right now.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    /// Loaded and serving.
    Ready,
    /// Being made resident.
    Loading,
    /// Known to the catalog but not loaded.
    #[default]
    Available,
}

impl ModelRow {
    pub fn new(id: impl Into<String>, n_ctx: u32, hermes: HermesModelInfo) -> Self {
        Self {
            id: id.into(),
            object: "model".to_owned(),
            created: crate::unix_now(),
            owned_by: "hermes-gateway".to_owned(),
            context_length: n_ctx,
            n_ctx,
            max_tokens: n_ctx,
            max_output_tokens: n_ctx,
            hermes,
        }
    }
}

/// The `GET /v1/models` body.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelList {
    pub object: String,
    pub data: Vec<ModelRow>,
}

impl ModelList {
    pub fn new(data: Vec<ModelRow>) -> Self {
        Self {
            object: "list".to_owned(),
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// The keys the client scans for, transcribed from
    /// `agent/model_metadata.py:655-668`.
    const CONTEXT_KEYS: &[&str] = &[
        "context_length",
        "context_window",
        "context_size",
        "max_context_length",
        "max_position_embeddings",
        "max_model_len",
        "max_input_tokens",
        "max_sequence_length",
        "max_seq_len",
        "n_ctx_train",
        "n_ctx",
        "ctx_size",
    ];

    /// Walk a row the way the client's scanner does: every nested object, in
    /// order, first recognized key wins.
    fn first_context_key(value: &Value) -> Option<(String, u64)> {
        let object = value.as_object()?;
        for (key, value) in object {
            if CONTEXT_KEYS.contains(&key.to_lowercase().as_str())
                && let Some(number) = value.as_u64()
            {
                return Some((key.clone(), number));
            }
        }
        for value in object.values() {
            if let Some(found) = first_context_key(value) {
                return Some(found);
            }
        }
        None
    }

    fn row() -> Value {
        let row = ModelRow::new(
            "lfm2-1.2b-q4_k_m@8k",
            8192,
            HermesModelInfo {
                architecture: "lfm2".into(),
                param_count: Some(1_170_000_000),
                quantization: Some("Q4_K_M".into()),
                model_max_context_length: Some(128_000),
                state: ModelState::Ready,
                ram_verdict: Some("safe".into()),
                backend: Some("llamacpp-process".into()),
            },
        );
        serde_json::to_value(row).expect("serialize")
    }

    #[test]
    fn the_scanner_finds_the_effective_context_first() {
        // The whole point of the row's shape: a model that supports 128000 but
        // is running at 8192 must report 8192, or every prompt is sized to a
        // window that does not exist.
        let (key, value) = first_context_key(&row()).expect("a context key");
        assert_eq!(value, 8192, "the scanner found {key} = {value}");
    }

    #[test]
    fn the_models_true_ceiling_is_not_mistakable_for_the_effective_context() {
        let row = row();
        assert_eq!(row["hermes"]["model_max_context_length"], 128_000);
        // `max_context_length` *is* in the scanner's key set, so the extra
        // `model_` prefix is what keeps the ceiling from being read as the
        // effective value.
        assert!(!CONTEXT_KEYS.contains(&"model_max_context_length"));
    }

    #[test]
    fn every_context_field_agrees() {
        // Several names, one number. Disagreement here is a silent overflow.
        let row = row();
        for key in ["context_length", "n_ctx", "max_tokens", "max_output_tokens"] {
            assert_eq!(row[key], 8192, "{key} disagrees");
        }
    }

    #[test]
    fn a_row_is_shaped_like_an_openai_model() {
        let row = row();
        assert_eq!(row["object"], "model");
        assert_eq!(row["id"], "lfm2-1.2b-q4_k_m@8k");
        assert_eq!(row["owned_by"], "hermes-gateway");
        assert!(row["created"].is_number());
    }

    #[test]
    fn the_list_is_an_openai_list() {
        let list = ModelList::new(vec![ModelRow::new(
            "m@4k",
            4096,
            HermesModelInfo::default(),
        )]);
        let json = serde_json::to_value(list).expect("serialize");
        assert_eq!(json["object"], "list");
        assert_eq!(json["data"][0]["id"], "m@4k");
    }
}
