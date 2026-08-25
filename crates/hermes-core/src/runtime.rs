//! Runtime parameters for a loaded model.
//!
//! These live in `hermes-core` rather than beside either of their users
//! because both need them and neither should depend on the other: the RAM
//! estimator computes a footprint from them, and the inference backend turns
//! them into engine arguments. A shared vocabulary is the point - an estimate
//! computed for one set of parameters and a model loaded with another would be
//! silently meaningless.

use serde::{Deserialize, Serialize};

use crate::ggml::GgmlType;

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
    /// Inference threads. `None` means "use the machine's physical core count",
    /// which is the section 9 default; hyperthread siblings share execution
    /// units that matrix multiplication already saturates.
    pub threads: Option<u32>,
    /// Threads for prompt processing. `None` means "the same as `threads`",
    /// which is the engine's own default, so leaving it unset changes nothing.
    ///
    /// Worth having as a separate knob because the two phases are not the same
    /// kind of work: prefill is compute-bound matrix multiplication that can use
    /// every hardware thread, while decode is memory-bound and usually stops
    /// scaling at the physical core count. Which way that cuts depends on the
    /// machine, which is why there is no default here derived from any one of
    /// them.
    pub threads_batch: Option<u32>,
    /// How hard idle engine threads spin waiting for work, 0 to 100.
    ///
    /// `None` leaves the engine's default of 50. Polling trades processor time
    /// for latency, which is a good trade on an idle machine and a bad one on a
    /// machine already fully committed — so it is exposed and left alone.
    pub poll: Option<u8>,
    /// Smallest chunk the engine will try to reuse from the KV cache by
    /// shifting, rather than only matching an exact prefix.
    ///
    /// `None` leaves the engine's default of 0, which is off. Turning it on
    /// helps exactly the shape an agent loop produces — a stable system prompt
    /// with a changed message in the middle — but it reuses cache entries by
    /// moving them, which trades a little output fidelity. No estimate judges
    /// output fidelity, so this is a choice and not a default.
    pub cache_reuse: Option<u32>,
    /// The processors the engine's threads may run on, as an inclusive range.
    ///
    /// `None` lets the scheduler place them, which is right on nearly every
    /// machine. It earns its place on the ones where it is not: hybrid cores of
    /// unequal speed, or a box where the engine should be kept off the cores
    /// something else needs.
    pub cpu_range: Option<(u32, u32)>,
    /// Whether the placement above is strict.
    pub cpu_strict: bool,
    /// How the weights are brought into memory.
    ///
    /// `None` leaves the engine's `auto`, which memory-maps them. Anything that
    /// *locks* them changes what the memory budget may credit, which is why
    /// this is a typed choice rather than a passthrough string — see the
    /// admission path in `hermes-gateway`.
    pub load_mode: Option<LoadMode>,
}

/// How the engine brings a model's weights into memory.
///
/// The names are the engine's own, read from `--help` at the pinned build
/// rather than transcribed from documentation. `--mmap`, `--no-mmap` and
/// `--mlock` are all deprecated in favour of this one flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadMode {
    /// Memory-map, unless a device cannot.
    Auto,
    /// No special handling.
    None,
    /// Memory-map.
    Mmap,
    /// Keep the weights in RAM rather than letting them be swapped or
    /// compressed.
    Mlock,
    /// Both.
    MmapMlock,
}

impl LoadMode {
    /// Every mode the pinned engine accepts, in its own spelling.
    pub const ALL: [Self; 5] = [
        Self::Auto,
        Self::None,
        Self::Mmap,
        Self::Mlock,
        Self::MmapMlock,
    ];

    /// The value the engine's `--load-mode` expects.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Mmap => "mmap",
            Self::Mlock => "mlock",
            Self::MmapMlock => "mmap+mlock",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == name)
    }

    /// Whether this mode pins the weights in memory.
    ///
    /// The distinction the memory budget turns on: locked weights are not
    /// reclaimable page cache, so they neither come out of `MemAvailable` for
    /// free nor return to it quietly, and an overshoot is an OOM kill rather
    /// than paging.
    pub const fn locks_weights(self) -> bool {
        matches!(self, Self::Mlock | Self::MmapMlock)
    }
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
            threads: None,
            // Every one of these is `None`, and that is the whole point: absent
            // means "whatever the engine does by default", so adding them
            // changed no existing deployment's behaviour by a single flag.
            threads_batch: None,
            poll: None,
            cache_reuse: None,
            cpu_range: None,
            cpu_strict: false,
            load_mode: None,
        }
    }
}

impl RuntimeParams {
    pub fn with_context(mut self, n_ctx: u32) -> Self {
        self.n_ctx = n_ctx;
        self
    }

    pub fn with_threads(mut self, threads: u32) -> Self {
        self.threads = Some(threads);
        self
    }

    pub fn with_kv_cache_type(mut self, cache_type: GgmlType) -> Self {
        self.cache_type_k = cache_type;
        self.cache_type_v = cache_type;
        self
    }

    /// The physical batch, and the logical batch raised to hold it.
    ///
    /// The engine refuses `n_ubatch > n_batch`, and finding that out from a
    /// failed launch several minutes into a benchmark is a poor way to learn
    /// it. Raising rather than refusing, because a caller asking for a larger
    /// physical batch means it.
    pub fn with_ubatch(mut self, n_ubatch: u32) -> Self {
        self.n_ubatch = n_ubatch.max(1);
        self.n_batch = self.n_batch.max(self.n_ubatch);
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
