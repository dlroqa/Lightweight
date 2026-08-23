//! `GET /props`.
//!
//! Not an OpenAI endpoint at all — it is llama.cpp's, and the client probes it
//! opportunistically to learn a local server's context
//! (`agent/model_metadata.py`, reading `default_generation_settings.n_ctx`).
//! A 404 is handled, so this exists for one reason: if it answers at all, it
//! must agree with `/v1/models`. Two endpoints reporting different context
//! lengths is worse than one of them being absent.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The shape the client reads, plus what is useful to a human.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PropsBody {
    pub default_generation_settings: GenerationSettings,
    /// Concurrent requests we serve. One today; the scheduler is written so
    /// that raising it is an internal change.
    pub total_slots: u32,
    /// Where the weights live.
    ///
    /// Optional because it is the one field here worth withholding: on a bind
    /// that anyone else can reach, an unauthenticated caller has no business
    /// learning a path on this filesystem. Omitted rather than blanked, so a
    /// client reads "not told" instead of "empty".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_path: Option<String>,
    /// Our build, not the engine's — this is the gateway's endpoint.
    pub build_info: String,
    /// What the engine itself reports, kept under our namespace so the two are
    /// never confused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hermes: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GenerationSettings {
    /// The effective context — the same number `/v1/models` advertises.
    pub n_ctx: u32,
    pub params: GenerationSettingsParams,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GenerationSettingsParams {
    /// `-1` means "no fixed limit"; the gateway clamps per request against the
    /// context that is actually left.
    pub n_predict: i64,
    pub stream: bool,
}

impl PropsBody {
    pub fn new(n_ctx: u32, model_path: impl Into<String>, total_slots: u32) -> Self {
        Self {
            default_generation_settings: GenerationSettings {
                n_ctx,
                params: GenerationSettingsParams {
                    n_predict: -1,
                    stream: true,
                },
            },
            total_slots,
            model_path: Some(model_path.into()),
            build_info: concat!("hermes-gateway-", env!("CARGO_PKG_VERSION")).to_owned(),
            hermes: None,
        }
    }

    /// Drop the filesystem path, for a caller that has not authenticated.
    ///
    /// Everything a client needs in order to size a prompt — the context and
    /// the slot count — still answers, so redacting costs no compatibility.
    /// Refusing the whole endpoint would: clients probe `/props` while
    /// resolving a model's context, and a 401 there degrades that discovery.
    #[must_use]
    pub fn redacted(mut self) -> Self {
        self.model_path = None;
        self
    }

    /// Attach the engine's own `/props`, for diagnostics.
    pub fn with_engine_props(mut self, props: Value) -> Self {
        self.hermes = Some(serde_json::json!({ "engine": props }));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_context_is_where_the_client_looks_for_it() {
        let json =
            serde_json::to_value(PropsBody::new(8192, "/models/m.gguf", 1)).expect("serialize");
        assert_eq!(json["default_generation_settings"]["n_ctx"], 8192);
        assert_eq!(json["total_slots"], 1);
    }

    #[test]
    fn redacting_keeps_the_numbers_and_drops_the_path() {
        let json = serde_json::to_value(PropsBody::new(8192, "/models/m.gguf", 1).redacted())
            .expect("serialize");
        assert_eq!(json["default_generation_settings"]["n_ctx"], 8192);
        assert_eq!(json["total_slots"], 1);
        assert!(
            json.get("model_path").is_none(),
            "the path survived redaction: {json}"
        );
    }

    #[test]
    fn the_engines_own_properties_stay_in_our_namespace() {
        // Useful for diagnosis, and never mistakable for ours: the engine
        // reports its own n_ctx, which is per slot and can differ.
        let props = PropsBody::new(8192, "/models/m.gguf", 1)
            .with_engine_props(serde_json::json!({"total_slots": 4}));
        let json = serde_json::to_value(props).expect("serialize");
        assert_eq!(json["total_slots"], 1);
        assert_eq!(json["hermes"]["engine"]["total_slots"], 4);
    }
}
