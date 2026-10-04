//! What the gateway will admit to having.
//!
//! In this milestone the catalog holds the one model the backend has resident.
//! It is a separate type anyway, because `/v1/models` is answered from *our*
//! record rather than by asking the engine: the engine knows a file path and a
//! per-slot context, while a client needs a stable id, the effective context,
//! and the model's real ceiling. Those are ours to state.

use std::sync::Arc;

use lightweight_api::models::{HermesModelInfo, ModelRow, ModelState};
use lightweight_catalog::InstalledModel;
use lightweight_catalog::alias::{self, ModelSelector};
use lightweight_core::{InstanceId, ModelId};
use tokio::sync::RwLock;

/// A model the gateway can serve right now.
#[derive(Clone, Debug)]
pub struct ResidentModel {
    pub id: ModelId,
    /// The alias the user gave this model in the catalog, if any.
    ///
    /// Carried here so a request is matched without touching the catalog, and
    /// kept current by [`Catalog::adopt_alias`] when the user changes it —
    /// which never reloads the engine, because nothing the engine knows about
    /// has changed.
    pub alias: Option<String>,
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
    /// What the engine is **actually** running with.
    ///
    /// `n_ctx` above is the one number every endpoint advertises and is kept
    /// where it is; this is the whole set, which the engine may have adjusted.
    /// Recorded because a benchmark that did not know the batch size it ran at
    /// would be a measurement of unknown conditions, and because "the effective
    /// values, never the requested ones" has been the rule since M2.
    pub effective: lightweight_core::RuntimeParams,
}

impl ResidentModel {
    /// The id every OpenAI-shaped response names this model by.
    ///
    /// The alias when the user chose one, so a client that discovered `Coder`
    /// in `/v1/models` and asked for `Coder` is answered as `Coder` on every
    /// chunk, and never sees the file-derived id unless nobody named the
    /// model. Without an alias it is the id it has always been, context suffix
    /// included.
    pub fn public_id(&self) -> String {
        self.alias.clone().unwrap_or_else(|| self.id.to_string())
    }

    /// Whether this is the model `record` describes.
    ///
    /// By catalog id, or by file: a model served from the command line is
    /// named after its file stem rather than its catalog slug, and is still
    /// the model the catalog holds at that path.
    pub fn is_record(&self, record: &InstalledModel) -> bool {
        self.id.slug() == record.id || self.model_path == record.path.display().to_string()
    }

    pub fn to_row(&self) -> ModelRow {
        ModelRow::new(
            self.public_id(),
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
    /// Read through the same [`ModelSelector`] the catalog uses, in this order:
    ///
    /// 1. nothing, or `default`: whatever is loaded, which is this model. It
    ///    is resolved per request rather than stored, so a swap moves it.
    /// 2. the model's alias, ignoring case.
    /// 3. the id exactly, or a match on the part before our `@context` suffix.
    ///
    /// The suffix tolerance is deliberate and asymmetric: the suffix is *our*
    /// invention and changes whenever the context does, so a client holding
    /// `model@8k` while we now serve `model@4k` is our doing, not a mistake —
    /// while a different base name is the user naming a model we do not have,
    /// which they need to be told about.
    pub fn matches(&self, requested: &str) -> bool {
        let ModelSelector::Named(requested) = ModelSelector::parse(Some(requested)) else {
            return true;
        };
        if self
            .alias
            .as_deref()
            .is_some_and(|held| alias::same_name(held, requested))
        {
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

    /// Take up `record`'s alias if it describes the resident model.
    ///
    /// Called whenever an alias is set, changed or cleared. Only the name
    /// changes: the instance, the context and the slots are the same engine
    /// they were a moment ago. Returns whether the resident model was the one.
    pub async fn adopt_alias(&self, record: &InstalledModel) -> bool {
        let mut resident = self.resident.write().await;
        match resident.as_mut() {
            Some(model) if model.is_record(record) => {
                model.alias.clone_from(&record.alias);
                true
            }
            _ => false,
        }
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
            alias: None,
            instance: InstanceId::new(),
            n_ctx: 8192,
            architecture: "lfm2".into(),
            param_count: Some(1_170_000_000),
            quantization: Some("Q4_K_M".into()),
            model_max_context_length: Some(128_000),
            ram_verdict: Some("safe".into()),
            backend: Some("llamacpp-process".into()),
            model_path: "/models/lfm2.gguf".into(),
            effective: lightweight_core::RuntimeParams::default().with_context(8192),
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

    #[test]
    fn default_is_the_resident_model_whatever_it_is_called() {
        assert!(model().matches("default"));
        assert!(model().matches("  default  "));
        let named = ResidentModel {
            alias: Some("Fast".into()),
            ..model()
        };
        assert!(named.matches("default"));
        assert!(named.matches(""));
    }

    #[test]
    fn the_alias_matches_in_any_casing_and_the_id_still_does() {
        let named = ResidentModel {
            alias: Some("Fast".into()),
            ..model()
        };
        assert!(named.matches("Fast"));
        assert!(named.matches("fast"));
        assert!(named.matches(" FAST "));
        assert!(named.matches("lfm2-1.2b-q4_k_m@8k"));
        assert!(named.matches("lfm2-1.2b-q4_k_m"));
        assert!(!named.matches("Coder"));
        // An alias is a whole name, not a prefix.
        assert!(!named.matches("Fast@8k"));
    }

    #[test]
    fn the_public_id_is_the_alias_when_there_is_one() {
        assert_eq!(model().public_id(), "lfm2-1.2b-q4_k_m@8k");
        let named = ResidentModel {
            alias: Some("Fast".into()),
            ..model()
        };
        assert_eq!(named.public_id(), "Fast");
        assert_eq!(named.to_row().id, "Fast");
        // The real context is still advertised; only the name changed.
        assert_eq!(named.to_row().context_length, 8192);
    }

    #[tokio::test]
    async fn an_alias_change_reaches_the_resident_model_and_no_other() {
        let catalog = Catalog::with_resident(model());
        let mut record: InstalledModel = serde_json::from_value(serde_json::json!({
            "id": "lfm2-1.2b-q4_k_m", "name": "LFM2", "path": "/elsewhere/lfm2.gguf",
            "bytes": 1, "sha256": "aa", "integrity": "imported",
            "source": {"kind": "import", "original_path": "/elsewhere/lfm2.gguf"},
            "architecture": "lfm2", "supported": true, "added_at": 0
        }))
        .expect("record");
        record.alias = Some("Fast".into());
        assert!(catalog.adopt_alias(&record).await);
        assert_eq!(catalog.rows().await[0].id, "Fast");

        record.alias = None;
        assert!(catalog.adopt_alias(&record).await);
        assert_eq!(catalog.rows().await[0].id, "lfm2-1.2b-q4_k_m@8k");

        // A different model's alias leaves the resident one alone.
        record.id = "something-else".into();
        record.alias = Some("Coder".into());
        assert!(!catalog.adopt_alias(&record).await);
        assert_eq!(catalog.rows().await[0].id, "lfm2-1.2b-q4_k_m@8k");
    }

    #[tokio::test]
    async fn a_model_served_from_its_file_takes_the_alias_recorded_for_that_file() {
        // `hermes serve <file>` names the model after its file stem, which is
        // not the catalog slug; the path is what they share.
        let catalog = Catalog::with_resident(ResidentModel {
            id: ModelId::with_context("LFM2-1.2B-Q4_K_M", 8192),
            ..model()
        });
        let record: InstalledModel = serde_json::from_value(serde_json::json!({
            "id": "lfm2-1.2b-q4_k_m-2", "alias": "Fast", "name": "LFM2",
            "path": "/models/lfm2.gguf", "bytes": 1, "sha256": "aa",
            "integrity": "imported",
            "source": {"kind": "import", "original_path": "/models/lfm2.gguf"},
            "architecture": "lfm2", "supported": true, "added_at": 0
        }))
        .expect("record");
        assert!(catalog.adopt_alias(&record).await);
        assert_eq!(catalog.rows().await[0].id, "Fast");
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
