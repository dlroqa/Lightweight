//! Runtime parameters that determine a model's memory footprint.

use hermes_core::GgmlType;
use serde::{Deserialize, Serialize};

/// The knobs that change how much memory a loaded model occupies.
///
/// Sampling parameters (temperature, top-p and the rest) are not here: they
/// affect what the model produces, not what it costs to hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeParams {
    /// Context window in tokens. The dominant term in KV cache size.
    pub n_ctx: u32,
    /// Logical batch size for prompt processing.
    pub n_batch: u32,
    /// Physical batch size. Compute buffers scale with this, not `n_batch`.
    pub n_ubatch: u32,
    /// Concurrent sequences. Multiplies the KV cache.
    pub n_parallel: u32,
    /// Element type for the K cache.
    ///
    /// Defaults to f16 and is passed to the engine explicitly rather than left
    /// to its default, so the estimate and the reality cannot drift apart.
    pub cache_type_k: GgmlType,
    /// Element type for the V cache.
    pub cache_type_v: GgmlType,
}

impl Default for RuntimeParams {
    fn default() -> Self {
        Self {
            // 4096 rather than the model's advertised maximum. A 128K context
            // on a 3B model is tens of gigabytes of KV cache, so defaulting to
            // the ceiling would make almost every model look unloadable.
            n_ctx: 4096,
            n_batch: 2048,
            n_ubatch: 512,
            n_parallel: 1,
            cache_type_k: GgmlType::F16,
            cache_type_v: GgmlType::F16,
        }
    }
}

impl RuntimeParams {
    pub fn with_context(mut self, n_ctx: u32) -> Self {
        self.n_ctx = n_ctx;
        self
    }

    pub fn with_kv_cache_type(mut self, cache_type: GgmlType) -> Self {
        self.cache_type_k = cache_type;
        self.cache_type_v = cache_type;
        self
    }

    /// Context presets from spec section 8.
    pub const CONTEXT_PRESETS: &'static [u32] = &[2048, 4096, 8192, 16384, 32768, 65536, 131072];

    /// The presets that make sense for a model with `model_max` context.
    ///
    /// Spec section 8: only show context sizes the model actually supports.
    /// The model's own maximum is included even when it is not a round number,
    /// because refusing to offer a model's full context would be strange.
    pub fn context_presets_for(model_max: Option<u64>) -> Vec<u32> {
        let Some(model_max) = model_max else {
            return Self::CONTEXT_PRESETS.to_vec();
        };
        let ceiling = u32::try_from(model_max).unwrap_or(u32::MAX);
        let mut presets: Vec<u32> = Self::CONTEXT_PRESETS
            .iter()
            .copied()
            .filter(|&preset| preset <= ceiling)
            .collect();
        if !presets.contains(&ceiling) && ceiling >= 512 {
            presets.push(ceiling);
        }
        presets.sort_unstable();
        presets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_a_modest_context_not_the_model_maximum() {
        // Defaulting to a 128K ceiling would make most models report as
        // unloadable on an ordinary machine.
        assert_eq!(RuntimeParams::default().n_ctx, 4096);
    }

    #[test]
    fn defaults_to_an_f16_kv_cache() {
        // Stated explicitly rather than inherited from the engine, so the
        // estimate and the engine cannot disagree.
        let params = RuntimeParams::default();
        assert_eq!(params.cache_type_k, GgmlType::F16);
        assert_eq!(params.cache_type_v, GgmlType::F16);
    }

    #[test]
    fn presets_are_capped_at_the_models_real_maximum() {
        // Section 8: only offer context sizes the model supports.
        let presets = RuntimeParams::context_presets_for(Some(8192));
        assert_eq!(presets, vec![2048, 4096, 8192]);
    }

    #[test]
    fn a_models_own_odd_maximum_is_offered_too() {
        // Qwen3 advertises 40960, and LFM2 128000 - neither is a power of two,
        // and both should still be reachable.
        let presets = RuntimeParams::context_presets_for(Some(40960));
        assert!(presets.contains(&40960));
        assert!(presets.contains(&32768));
        assert!(!presets.contains(&65536));

        assert!(RuntimeParams::context_presets_for(Some(128_000)).contains(&128_000));
    }

    #[test]
    fn an_unknown_maximum_offers_every_preset() {
        assert_eq!(
            RuntimeParams::context_presets_for(None),
            RuntimeParams::CONTEXT_PRESETS.to_vec()
        );
    }

    #[test]
    fn presets_are_sorted_and_unique() {
        let presets = RuntimeParams::context_presets_for(Some(32768));
        assert!(presets.windows(2).all(|w| w[0] < w[1]), "{presets:?}");
    }
}
