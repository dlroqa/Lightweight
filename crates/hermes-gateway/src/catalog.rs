//! What the gateway will admit to having.
//!
//! In this milestone the catalog holds the one model the backend has resident.
//! It is a separate type anyway, because `/v1/models` is answered from *our*
//! record rather than by asking the engine: the engine knows a file path and a
//! per-slot context, while a client needs a stable id, the effective context,
//! and the model's real ceiling. Those are ours to state.

use std::sync::Arc;

use hermes_api::models::{HermesModelInfo, ModelRow, ModelState};
use hermes_core::{InstanceId, ModelId};
use tokio::sync::RwLock;

/// A model the gateway can serve right now.
#[derive(Clone, Debug)]
pub struct ResidentModel {
    pub id: ModelId,
    pub instance: InstanceId,
    /// The context the model is actually loaded with — the number every
    /// endpoint advertises.
    pub n_ctx: u32,
    pub architecture: String,
    pub param_count: Option<u64>,
    pub quantization: Option<String>,
    /// The largest context the model's own metadata declares.
    pub model_max_context_length: Option<u64>,
    pub ram_verdict: Option<String>,
    pub backend: Option<String>,
    /// Where the weights live. Reported by `/props`, which llama.cpp clients
    /// expect to find a path in.
    pub model_path: String,
}

impl ResidentModel {
    pub fn to_row(&self) -> ModelRow {
        ModelRow::new(
            self.id.to_string(),
            self.n_ctx,
            HermesModelInfo {
                architecture: self.architecture.clone(),
                param_count: self.param_count,
                quantization: self.quantization.clone(),
                model_max_context_length: self.model_max_context_length,
                state: ModelState::Ready,
                ram_verdict: self.ram_verdict.clone(),
                backend: self.backend.clone(),
            },
        )
    }

    /// Whether a client's `model` string names this model.
    ///
    /// Exact match, or a match on the part before our `@context` suffix. The
    /// tolerance is deliberate and asymmetric: the suffix is *our* invention
    /// and changes whenever the context does, so a client holding
    /// `model@8k` while we now serve `model@4k` is our doing, not a mistake —
    /// while a different base name is the user naming a model we do not have,
    /// which they need to be told about.
    pub fn matches(&self, requested: &str) -> bool {
        let requested = requested.trim();
        if requested.is_empty() {
            // No model named at all: there is exactly one, so serve it. The
            // response still reports the real id.
            return true;
        }
        if requested == self.id.as_str() {
            return true;
        }
        ModelId::new(requested).slug() == self.id.slug()
    }
}

/// The models the gateway knows about.
#[derive(Debug, Default)]
pub struct Catalog {
    resident: RwLock<Option<ResidentModel>>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// A catalog with one model already resident.
    pub fn with_resident(model: ResidentModel) -> Self {
        Self {
            resident: RwLock::new(Some(model)),
        }
    }

    pub async fn set_resident(&self, model: Option<ResidentModel>) {
        *self.resident.write().await = model;
    }

    pub async fn resident(&self) -> Option<ResidentModel> {
        self.resident.read().await.clone()
    }

    /// The rows for `GET /v1/models`.
    pub async fn rows(&self) -> Vec<ModelRow> {
        self.resident
            .read()
            .await
            .as_ref()
            .map(|model| vec![model.to_row()])
            .into_iter()
            .flatten()
            .collect()
    }
}

/// A catalog behind an `Arc`, which is how the state holds it.
pub fn shared(model: Option<ResidentModel>) -> Arc<Catalog> {
    Arc::new(match model {
        Some(model) => Catalog::with_resident(model),
        None => Catalog::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> ResidentModel {
        ResidentModel {
            id: ModelId::with_context("lfm2-1.2b-q4_k_m", 8192),
            instance: InstanceId::new(),
            n_ctx: 8192,
            architecture: "lfm2".into(),
            param_count: Some(1_170_000_000),
            quantization: Some("Q4_K_M".into()),
            model_max_context_length: Some(128_000),
            ram_verdict: Some("safe".into()),
            backend: Some("llamacpp-process".into()),
            model_path: "/models/lfm2.gguf".into(),
        }
    }

    #[test]
    fn the_exact_id_matches() {
        assert!(model().matches("lfm2-1.2b-q4_k_m@8k"));
    }

    #[test]
    fn a_stale_context_suffix_still_matches() {
        // Our own naming policy causes this: change the context and the id
        // changes with it, so a client that cached the old one is holding a
        // name we invented. Refusing it would break a conversation over a
        // detail the user never chose.
        assert!(model().matches("lfm2-1.2b-q4_k_m@4k"));
        assert!(model().matches("lfm2-1.2b-q4_k_m"));
    }

    #[test]
    fn a_different_model_does_not_match() {
        // The user naming a model we do not have is a real error, and silently
        // answering with a different one would hide it.
        assert!(!model().matches("llama-3.2-3b@8k"));
        assert!(!model().matches("gpt-4"));
    }

    #[test]
    fn an_absent_model_field_is_served_by_the_only_model() {
        assert!(model().matches(""));
    }

    #[tokio::test]
    async fn an_empty_catalog_lists_nothing() {
        let catalog = Catalog::new();
        assert!(catalog.rows().await.is_empty());
        assert!(catalog.resident().await.is_none());
    }

    #[tokio::test]
    async fn a_resident_model_is_listed_with_its_effective_context() {
        let catalog = Catalog::with_resident(model());
        let rows = catalog.rows().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].context_length, 8192);
        assert_eq!(rows[0].hermes.model_max_context_length, Some(128_000));
    }
}
