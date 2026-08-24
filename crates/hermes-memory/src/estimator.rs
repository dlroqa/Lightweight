//! The estimator itself.

use hermes_core::units::Bytes;
use hermes_gguf::ModelMetadata;
use hermes_system_info::{MemoryProbe, MemorySnapshot};

use crate::estimate::{Budget, ComputeModel, Confidence, Estimate, Verdict};
use hermes_core::{GgmlType, RuntimeParams};

/// Minimum headroom above the estimate for a [`Verdict::Safe`].
const MIN_MARGIN: Bytes = Bytes(512 * 1024 * 1024);
/// Headroom as a fraction of the budget, when that is larger than the minimum.
const MARGIN_FRACTION: f64 = 0.15;
/// Below this, reducing the context is not worth offering as a remedy.
const MIN_USEFUL_CONTEXT: u32 = 512;

/// Computes memory estimates and admission verdicts.
#[derive(Clone, Debug, Default)]
pub struct Estimator {
    compute_model: ComputeModel,
}

impl Estimator {
    pub fn new(compute_model: ComputeModel) -> Self {
        Self { compute_model }
    }

    /// An estimator for the headless daemon, with no desktop shell counted.
    pub fn headless() -> Self {
        Self::new(ComputeModel::headless())
    }

    pub const fn compute_model(&self) -> &ComputeModel {
        &self.compute_model
    }

    /// Estimate against a memory reading taken now.
    pub fn estimate_now(
        &self,
        metadata: &ModelMetadata,
        params: RuntimeParams,
        probe: &dyn MemoryProbe,
    ) -> Result<Estimate, hermes_system_info::MemoryError> {
        let snapshot = probe.snapshot()?;
        Ok(self.estimate(metadata, params, snapshot))
    }

    /// Estimate against a specific memory reading.
    ///
    /// Delegates to [`Estimator::estimate_against`] with nothing to reclaim,
    /// which is every caller except a model swap.
    pub fn estimate(
        &self,
        metadata: &ModelMetadata,
        params: RuntimeParams,
        snapshot: MemorySnapshot,
    ) -> Estimate {
        self.estimate_against(metadata, params, Budget::of(snapshot))
    }

    /// Estimate against a budget that may include memory about to be released.
    pub fn estimate_against(
        &self,
        metadata: &ModelMetadata,
        params: RuntimeParams,
        budget_for: Budget,
    ) -> Estimate {
        let snapshot = budget_for.snapshot;
        let mut missing = Vec::new();

        let weights = match metadata.weight_bytes {
            Some(bytes) => Bytes(bytes),
            None => {
                missing.push("weight size (a tensor uses an unrecognised ggml type)".to_owned());
                Bytes::ZERO
            }
        };

        let kv = self.kv_cache_bytes(metadata, params, &mut missing);
        let compute = self.compute_bytes(metadata, params, &mut missing);
        let overhead = self
            .compute_model
            .engine_baseline
            .saturating_add(self.compute_model.host_overhead);

        let total = weights
            .saturating_add(kv.bytes)
            .saturating_add(compute)
            .saturating_add(overhead);

        let budget = budget_for.spendable();
        let margin = margin_for(budget);

        let verdict = if total.saturating_add(margin) <= budget {
            Verdict::Safe
        } else if total <= budget {
            Verdict::Tight
        } else {
            Verdict::Insufficient
        };

        let confidence = if !missing.is_empty() {
            Confidence::Partial
        } else if self.compute_model.measured {
            Confidence::Measured
        } else {
            Confidence::Coarse
        };

        let fixed = weights
            .saturating_add(compute)
            .saturating_add(overhead)
            .saturating_add(margin);
        // Clamped to what the model was actually trained for. Memory might
        // allow more, but suggesting a context beyond the model's own
        // `context_length` would be advice the engine refuses to take.
        let max_context_that_fits = solve_max_context(budget, fixed, kv.bytes_per_token)
            .map(|fits| match metadata.context_length {
                Some(model_max) => fits.min(u32::try_from(model_max).unwrap_or(u32::MAX)),
                None => fits,
            })
            .filter(|&fits| fits >= MIN_USEFUL_CONTEXT);

        Estimate {
            weights,
            kv_cache: kv.bytes,
            compute,
            overhead,
            total,
            budget,
            reclaimable: budget_for.reclaimable,
            margin,
            verdict,
            confidence,
            kv_bytes_per_token: kv.bytes_per_token,
            max_context_that_fits,
            missing,
            snapshot,
            params,
        }
    }

    /// KV cache size, summed per layer.
    ///
    /// Per layer rather than `2 * layers * ...` because real models are not
    /// uniform: LFM2 gives whole layers zero KV heads, and a uniform formula
    /// would bill for attention those layers do not have.
    ///
    /// A declared `sliding_window` is deliberately **not** applied as a
    /// discount. The metadata says a window exists but not which layers use it,
    /// and inferring that from the architecture name is exactly the hard-coding
    /// spec section 6 forbids. Section 7's rule settles the direction to be
    /// wrong in: never promise a model will run. So the estimate is an upper
    /// bound, and a windowed model simply uses less than predicted.
    fn kv_cache_bytes(
        &self,
        metadata: &ModelMetadata,
        params: RuntimeParams,
        missing: &mut Vec<String>,
    ) -> KvCache {
        let Some(layers) = metadata.block_count else {
            missing.push("block_count".to_owned());
            return KvCache::UNKNOWN;
        };
        let (Some(head_dim_k), Some(head_dim_v)) = (metadata.head_dim_k(), metadata.head_dim_v())
        else {
            missing.push("attention head dimension".to_owned());
            return KvCache::UNKNOWN;
        };
        if metadata.head_count_kv.is_none() {
            missing.push("attention.head_count_kv".to_owned());
            return KvCache::UNKNOWN;
        }

        let n_ctx = u64::from(params.n_ctx);
        let n_parallel = u64::from(params.n_parallel.max(1));

        // The whole sum is fallible, so it is written as one fallible pass
        // rather than as a loop with a fallback in it. The two halves of this
        // arithmetic - the byte total and the marginal cost per token - have to
        // agree about a type this build cannot size, and the only way to
        // guarantee that is to leave no fallback for either of them to reach
        // for. `?` on the same geometry both halves use is that guarantee.
        let accumulate = || -> Result<KvCache, String> {
            let bpe_k = bytes_per_element(params.cache_type_k)?;
            let bpe_v = bytes_per_element(params.cache_type_v)?;

            let mut bytes: u64 = 0;
            let mut per_token: f64 = 0.0;

            for layer in 0..layers {
                // A layer past the end of a per-layer array is unknown. Falling
                // back to layer zero's count would be a guess; billing nothing
                // would understate. Record it and stop.
                let kv_heads = metadata
                    .kv_heads_for_layer(layer)
                    .ok_or_else(|| format!("attention.head_count_kv for layer {layer}"))?;
                if kv_heads == 0 {
                    // Genuinely no attention in this layer, as in LFM2's
                    // short-convolution blocks. Not missing data - zero cost.
                    continue;
                }

                let k_elements = n_ctx
                    .saturating_mul(kv_heads)
                    .saturating_mul(head_dim_k)
                    .saturating_mul(n_parallel);
                let v_elements = n_ctx
                    .saturating_mul(kv_heads)
                    .saturating_mul(head_dim_v)
                    .saturating_mul(n_parallel);

                // Block-aware, using the same ggml geometry table as the weight
                // arithmetic. A quantized cache is not a whole number of bytes
                // per element: q8_0 is 34 bytes per 32.
                bytes = bytes.saturating_add(sized(params.cache_type_k, k_elements)?);
                bytes = bytes.saturating_add(sized(params.cache_type_v, v_elements)?);

                let heads_times_parallel = (kv_heads.saturating_mul(n_parallel)) as f64;
                per_token += heads_times_parallel
                    * ((head_dim_k as f64) * bpe_k + (head_dim_v as f64) * bpe_v);
            }

            Ok(KvCache {
                bytes: Bytes(bytes),
                bytes_per_token: per_token.ceil() as u64,
            })
        };

        match accumulate() {
            Ok(kv_cache) => kv_cache,
            Err(reason) => {
                missing.push(reason);
                KvCache::UNKNOWN
            }
        }
    }

    /// Logits, activations and engine scratch.
    ///
    /// Scales with `n_ubatch`, the physical batch, not with `n_batch`. The
    /// coefficients are the calibrated part of the model; see [`ComputeModel`].
    fn compute_bytes(
        &self,
        metadata: &ModelMetadata,
        params: RuntimeParams,
        missing: &mut Vec<String>,
    ) -> Bytes {
        let ubatch = f64::from(params.n_ubatch.max(1));

        let logits = match metadata.vocab_size {
            Some(vocab) => (vocab as f64) * ubatch * 4.0,
            None => {
                missing.push("vocab_size".to_owned());
                0.0
            }
        };

        let embedding = metadata.embedding_length.unwrap_or(0) as f64;
        let ffn = metadata.feed_forward_length.unwrap_or(0) as f64;
        if embedding == 0.0 {
            missing.push("embedding_length".to_owned());
        }

        let activations = self.compute_model.activation_factor * ubatch * embedding * 4.0;
        let scratch = self.compute_model.scratch_factor * ubatch * embedding.max(ffn) * 4.0;

        Bytes((logits + activations + scratch).max(0.0) as u64)
    }
}

/// Average bytes per element of a KV cache side.
///
/// In bytes rather than bits because that is what the per-token arithmetic
/// wants; dividing at each use is what let the two halves of the sum drift
/// apart in the first place.
fn bytes_per_element(kind: GgmlType) -> Result<f64, String> {
    kind.bits_per_element()
        .map(|bits| bits / 8.0)
        .ok_or_else(|| unsizeable(kind))
}

/// Exact bytes for `elements` values, refusing a type this build cannot size.
///
/// The refusal is the point. A type with no geometry billed as zero would make
/// the estimate *smaller* than the truth, which is the one direction section 7
/// forbids being wrong in.
fn sized(kind: GgmlType, elements: u64) -> Result<u64, String> {
    kind.bytes_for_elements(elements)
        .ok_or_else(|| unsizeable(kind))
}

fn unsizeable(kind: GgmlType) -> String {
    format!("block geometry for KV cache type {kind}")
}

/// KV cache size plus its marginal cost per token of context.
struct KvCache {
    bytes: Bytes,
    /// Bytes each extra token of context adds. Zero when unknown, which
    /// suppresses the "reduce the context" remedy rather than dividing by zero.
    bytes_per_token: u64,
}

impl KvCache {
    const UNKNOWN: Self = Self {
        bytes: Bytes::ZERO,
        bytes_per_token: 0,
    };
}

/// Headroom required above the estimate for a `Safe` verdict.
///
/// A fixed floor plus a proportional part: on a small machine 512 MiB is a
/// meaningful cushion, and on a large one a flat 512 MiB would be far too
/// little to absorb another application growing.
fn margin_for(budget: Bytes) -> Bytes {
    let proportional = Bytes((budget.get() as f64 * MARGIN_FRACTION) as u64);
    if proportional > MIN_MARGIN {
        proportional
    } else {
        MIN_MARGIN
    }
}

/// Largest context whose KV cache still fits alongside the fixed costs.
///
/// Returns `None` when the fixed costs alone exceed the budget — in that case
/// no context is small enough and offering "reduce the context" would send the
/// user down a road that cannot work.
fn solve_max_context(budget: Bytes, fixed: Bytes, kv_bytes_per_token: u64) -> Option<u32> {
    if kv_bytes_per_token == 0 {
        return None;
    }
    let spare = budget.get().checked_sub(fixed.get())?;
    let tokens = spare.checked_div(kv_bytes_per_token)?;
    let tokens = u32::try_from(tokens).unwrap_or(u32::MAX);
    (tokens >= MIN_USEFUL_CONTEXT).then_some(tokens)
}

#[cfg(test)]
mod tests_support {
    use super::*;
    use hermes_gguf::{QuantMix, TokenizerMeta};
    use hermes_system_info::FixedMemoryProbe;

    /// A model with the geometry the arithmetic cares about and nothing else.
    ///
    /// Built directly rather than through a fixture file so the expected values
    /// can be worked out by hand and asserted exactly.
    pub(super) fn model(
        layers: u64,
        heads: u64,
        kv_heads: Vec<u64>,
        head_dim: u64,
    ) -> ModelMetadata {
        ModelMetadata {
            architecture: "test".to_owned(),
            supported: true,
            name: None,
            context_length: Some(131_072),
            block_count: Some(layers),
            embedding_length: Some(heads * head_dim),
            feed_forward_length: Some(heads * head_dim * 4),
            head_count: Some(heads),
            head_count_kv: Some(kv_heads),
            key_length: None,
            value_length: None,
            sliding_window: None,
            rope_freq_base: None,
            vocab_size: Some(32_000),
            tokenizer: TokenizerMeta::default(),
            file_type: None,
            quantization: QuantMix::default(),
            tensor_count: 0,
            param_count: Some(0),
            weight_bytes: Some(0),
            gguf_version: 3,
            alignment: 32,
            missing: Vec::new(),
        }
    }

    /// An estimator with every non-derivable term zeroed, so a test can assert
    /// the exact KV figure without the calibrated coefficients in the way.
    pub(super) fn bare_estimator() -> Estimator {
        Estimator::new(ComputeModel {
            activation_factor: 0.0,
            scratch_factor: 0.0,
            engine_baseline: Bytes::ZERO,
            host_overhead: Bytes::ZERO,
            measured: true,
        })
    }

    pub(super) fn machine(available_gib: u64) -> MemorySnapshot {
        FixedMemoryProbe::with_available(Bytes::from_gib(64), Bytes::from_gib(available_gib)).0
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;
    use hermes_core::{GgmlType, RemedyAction};

    #[test]
    fn kv_cache_matches_the_hand_computed_figure() {
        // 32 layers, 8 KV heads, head_dim 128, 8192 context, f16 (2 bytes),
        // K and V both:
        //   32 * 8192 * 8 * 128 * 2 bytes * 2 (K and V) = 1,073,741,824
        let metadata = model(32, 32, vec![8], 128);
        let params = RuntimeParams::default().with_context(8192);
        let estimate = bare_estimator().estimate(&metadata, params, machine(32));

        assert_eq!(estimate.kv_cache, Bytes(1_073_741_824));
        assert_eq!(estimate.confidence, Confidence::Measured);
        assert!(estimate.missing.is_empty());
    }

    #[test]
    fn grouped_query_attention_costs_exactly_its_share() {
        // The single largest correction in the whole estimate. 32 query heads
        // against 4 KV heads is an eighth of a multi-head model's cache; using
        // head_count instead of head_count_kv would be an eight-fold error.
        let mha = model(32, 32, vec![32], 128);
        let gqa = model(32, 32, vec![4], 128);
        let params = RuntimeParams::default().with_context(8192);
        let estimator = bare_estimator();

        let mha_kv = estimator.estimate(&mha, params, machine(64)).kv_cache;
        let gqa_kv = estimator.estimate(&gqa, params, machine(64)).kv_cache;
        assert_eq!(gqa_kv.get() * 8, mha_kv.get());
    }

    #[test]
    fn layers_without_attention_cost_nothing() {
        // LFM2-1.2B's real head_count_kv array. Only six of sixteen layers have
        // attention, so a uniform assumption would overstate by 16/6 = 2.67x.
        let lfm2_pattern = vec![0, 0, 8, 0, 0, 8, 0, 0, 8, 0, 8, 0, 8, 0, 8, 0];
        let hybrid = model(16, 32, lfm2_pattern, 64);
        let uniform = model(16, 32, vec![8], 64);
        let params = RuntimeParams::default().with_context(4096);
        let estimator = bare_estimator();

        let hybrid_kv = estimator.estimate(&hybrid, params, machine(32)).kv_cache;
        let uniform_kv = estimator.estimate(&uniform, params, machine(32)).kv_cache;

        // Six attention layers out of sixteen.
        assert_eq!(hybrid_kv.get() * 16, uniform_kv.get() * 6);
    }

    #[test]
    fn an_explicit_head_dimension_is_used_over_the_derived_one() {
        // Gemma-3-1B: embedding 1152 / 4 heads = 288, but key_length is 256.
        // Deriving would overstate the cache by 12.5%.
        let mut declared = model(26, 4, vec![1], 288);
        declared.embedding_length = Some(1152);
        declared.key_length = Some(256);
        declared.value_length = Some(256);

        let derived = model(26, 4, vec![1], 288);
        let params = RuntimeParams::default().with_context(4096);
        let estimator = bare_estimator();

        let declared_kv = estimator.estimate(&declared, params, machine(32)).kv_cache;
        let derived_kv = estimator.estimate(&derived, params, machine(32)).kv_cache;
        assert!(declared_kv < derived_kv);
        assert_eq!(declared_kv.get() * 288, derived_kv.get() * 256);
    }

    #[test]
    fn kv_cache_grows_in_proportion_to_context() {
        let metadata = model(32, 32, vec![8], 128);
        let estimator = bare_estimator();
        let at = |ctx| {
            estimator
                .estimate(
                    &metadata,
                    RuntimeParams::default().with_context(ctx),
                    machine(64),
                )
                .kv_cache
                .get()
        };
        assert_eq!(at(8192), at(4096) * 2);
        assert_eq!(at(16384), at(4096) * 4);
    }

    #[test]
    fn parallel_sequences_multiply_the_cache() {
        let metadata = model(32, 32, vec![8], 128);
        let estimator = bare_estimator();
        let mut params = RuntimeParams::default().with_context(4096);
        let one = estimator.estimate(&metadata, params, machine(64)).kv_cache;
        params.n_parallel = 4;
        let four = estimator.estimate(&metadata, params, machine(64)).kv_cache;
        assert_eq!(four.get(), one.get() * 4);
    }

    #[test]
    fn a_quantized_kv_cache_uses_the_real_block_geometry() {
        // q8_0 is 34 bytes per 32 elements, so 17/32 of f16's cost - not half,
        // which is what rounding to "1 byte per element" would imply. On a
        // multi-gigabyte cache that difference is hundreds of megabytes.
        let metadata = model(32, 32, vec![8], 128);
        let estimator = bare_estimator();
        let f16 = estimator
            .estimate(
                &metadata,
                RuntimeParams::default().with_context(8192),
                machine(64),
            )
            .kv_cache;
        let q8 = estimator
            .estimate(
                &metadata,
                RuntimeParams::default()
                    .with_context(8192)
                    .with_kv_cache_type(GgmlType::Q8_0),
                machine(64),
            )
            .kv_cache;
        assert_eq!(q8.get() * 64, f16.get() * 34);
    }

    #[test]
    fn an_unknown_kv_cache_type_is_partial_not_free() {
        // A type this build cannot size must make the estimate *partial*, not
        // make the cache free. Billing zero and reporting `Coarse` is a
        // confident wrong answer, which is the failure the whole confidence
        // axis exists to prevent.
        let metadata = model(32, 32, vec![8], 128);
        let params = RuntimeParams::default()
            .with_context(8192)
            .with_kv_cache_type(GgmlType::Unknown(9999));
        let estimate = bare_estimator().estimate(&metadata, params, machine(64));

        assert_eq!(estimate.confidence, Confidence::Partial);
        assert_eq!(estimate.kv_cache, Bytes::ZERO);
        // The half that used to disagree: a per-token cost assumed at 16 bits
        // while the total assumed nothing at all.
        assert_eq!(estimate.kv_bytes_per_token, 0);
        assert_eq!(estimate.max_context_that_fits, None);
        assert!(
            estimate
                .missing
                .iter()
                .any(|entry| entry.contains("block geometry")),
            "the estimate must say which term it could not compute: {:?}",
            estimate.missing
        );
    }

    #[test]
    fn the_per_token_figure_and_the_total_never_disagree() {
        // Pins the two halves together for every type rather than for one. The
        // total rounds each layer up to a whole block and the per-token figure
        // cannot, so they agree to within one block per layer per side - and a
        // half that had silently fallen back to a different type would miss
        // that window by orders of magnitude.
        let metadata = model(32, 32, vec![8], 128);
        let n_ctx = 8192_u64;
        for kind in GgmlType::ALL.iter().copied().filter(|kind| kind.is_known()) {
            let estimate = bare_estimator().estimate(
                &metadata,
                RuntimeParams::default()
                    .with_context(n_ctx as u32)
                    .with_kv_cache_type(kind),
                machine(64),
            );
            let projected = estimate.kv_bytes_per_token * n_ctx;
            let slack = 32 * 2 * kind.type_size().expect("a known type has a size");
            assert!(
                projected <= estimate.kv_cache.get() + slack
                    && estimate.kv_cache.get() <= projected + slack,
                "{kind}: per-token projects {projected} against a total of {}",
                estimate.kv_cache.get()
            );
        }
    }

    // ---- the budget ----

    #[test]
    fn a_reclaimable_credit_raises_the_budget_and_nothing_else() {
        // A swap is judged while the outgoing model is still resident. Without
        // the credit it is refused for memory that engine is about to hand
        // back; with it the verdict is the one that will be true a moment
        // later. What must not change is the reading itself: `snapshot` is what
        // the machine said, and stays what the machine said.
        let metadata = model(32, 32, vec![8], 128);
        // 16K of context on this geometry is a 2 GiB KV cache, against a
        // machine with 1 GiB free: refused, and comfortably so.
        let params = RuntimeParams::default().with_context(16_384);
        let snapshot = machine(1);

        let without = bare_estimator().estimate(&metadata, params, snapshot);
        assert_eq!(without.verdict, Verdict::Insufficient);
        assert_eq!(without.reclaimable, Bytes::ZERO);

        let with = bare_estimator().estimate_against(
            &metadata,
            params,
            Budget::of(snapshot).reclaiming(Bytes::from_gib(4)),
        );
        assert_eq!(with.verdict, Verdict::Safe);
        assert_eq!(with.reclaimable, Bytes::from_gib(4));
        assert_eq!(
            with.snapshot.available, without.snapshot.available,
            "the credit must not rewrite what the machine reported"
        );
        assert_eq!(
            with.budget,
            snapshot.available.saturating_add(with.reclaimable)
        );
    }

    #[test]
    fn a_context_search_spends_the_same_budget_the_verdict_does() {
        // The search and the estimate that judges its answer have to agree
        // about how much there is, or the search can pick a context the
        // estimate immediately refuses.
        let metadata = model(32, 32, vec![8], 128);
        let budget = Budget::of(machine(2)).reclaiming(Bytes::from_gib(8));
        let estimator = bare_estimator();

        let chosen = estimator
            .largest_safe_context_against(&metadata, RuntimeParams::default(), budget, None)
            .expect("something fits once the credit is counted");
        let verdict = estimator
            .estimate_against(
                &metadata,
                RuntimeParams::default().with_context(chosen),
                budget,
            )
            .verdict;
        assert_eq!(verdict, Verdict::Safe);
    }

    #[test]
    fn a_tight_verdict_still_offers_no_remedies_and_is_still_admissible() {
        // M7.2 gave `Tight` a warning in the log and a sentence on screen, and
        // deliberately changed no policy. It stays admissible - gating it would
        // demand --force for loads that work today - and it stays outside
        // `remedies`, which is contracted to speak only for refusals.
        let metadata = model(32, 32, vec![8], 128);
        let estimate = bare_estimator().estimate(
            &metadata,
            // Half a gigabyte of KV plus logits against 1 GiB free: it fits,
            // and what is left is inside the safety margin.
            RuntimeParams::default().with_context(4096),
            machine(1),
        );
        assert_eq!(estimate.verdict, Verdict::Tight);
        assert!(estimate.verdict.is_admissible());
        assert!(
            estimate.remedies().is_empty(),
            "remedies speak for refusals only: {:?}",
            estimate.remedies()
        );
    }

    #[test]
    fn the_budget_is_available_memory_not_total() {
        // A verdict computed against total memory would approve loads that
        // cannot possibly fit alongside everything already running.
        let metadata = model(32, 32, vec![8], 128);
        // 1 GiB free on a 64 GiB machine, against a 1 GiB KV cache. Budgeting
        // from `total` would call this comfortably Safe; budgeting from
        // `available` correctly refuses it.
        let snapshot = MemorySnapshot {
            total: Bytes::from_gib(64),
            available: Bytes::from_gib(1),
            free: Bytes::from_mib(512),
            swap_total: Bytes::ZERO,
            swap_free: Bytes::ZERO,
        };
        let estimate = bare_estimator().estimate(
            &metadata,
            RuntimeParams::default().with_context(8192),
            snapshot,
        );
        assert_eq!(estimate.budget, Bytes::from_gib(1));
        assert_eq!(estimate.verdict, Verdict::Insufficient);

        // Same machine, same model, budgeting from total instead: Safe. That is
        // the mistake this guards against.
        let if_we_used_total = bare_estimator().estimate(
            &metadata,
            RuntimeParams::default().with_context(8192),
            MemorySnapshot {
                available: snapshot.total,
                ..snapshot
            },
        );
        assert_eq!(if_we_used_total.verdict, Verdict::Safe);
    }

    #[test]
    fn free_swap_is_never_treated_as_headroom() {
        // Section 7: never intentionally cause heavy swapping. Decode touches
        // every weight once per token, so a model that "fits" into swap would
        // page continuously.
        let metadata = model(32, 32, vec![8], 128);
        let no_swap = MemorySnapshot {
            total: Bytes::from_gib(8),
            available: Bytes::from_gib(1),
            free: Bytes::from_gib(1),
            swap_total: Bytes::ZERO,
            swap_free: Bytes::ZERO,
        };
        let lots_of_swap = MemorySnapshot {
            swap_total: Bytes::from_gib(64),
            swap_free: Bytes::from_gib(64),
            ..no_swap
        };
        let params = RuntimeParams::default().with_context(8192);
        let estimator = bare_estimator();

        let a = estimator.estimate(&metadata, params, no_swap);
        let b = estimator.estimate(&metadata, params, lots_of_swap);
        assert_eq!(a.budget, b.budget);
        assert_eq!(a.verdict, b.verdict);
    }

    #[test]
    fn verdicts_step_through_safe_tight_and_insufficient() {
        let metadata = model(32, 32, vec![8], 128);
        let estimator = bare_estimator();
        let params = RuntimeParams::default().with_context(8192);

        // KV alone is 1 GiB, and the margin on a small budget is 512 MiB.
        let verdict_at = |gib| estimator.estimate(&metadata, params, machine(gib)).verdict;
        assert_eq!(verdict_at(32), Verdict::Safe);
        assert_eq!(verdict_at(1), Verdict::Insufficient);

        // Just over 1 GiB but under 1.5 GiB is admissible without the margin.
        let tight = MemorySnapshot {
            total: Bytes::from_gib(8),
            available: Bytes(1_200_000_000),
            free: Bytes(1_200_000_000),
            swap_total: Bytes::ZERO,
            swap_free: Bytes::ZERO,
        };
        assert_eq!(
            estimator.estimate(&metadata, params, tight).verdict,
            Verdict::Tight
        );
    }

    #[test]
    fn tight_is_admissible_and_insufficient_is_not() {
        assert!(Verdict::Safe.is_admissible());
        assert!(Verdict::Tight.is_admissible());
        assert!(!Verdict::Insufficient.is_admissible());
    }

    // ---- solving for a context that fits ----

    #[test]
    fn the_suggested_context_actually_fits_and_the_next_one_up_does_not() {
        // The remedy has to be true, or it sends the user in a circle.
        let metadata = model(32, 32, vec![8], 128);
        let estimator = bare_estimator();
        let snapshot = machine(2);
        let estimate = estimator.estimate(
            &metadata,
            RuntimeParams::default().with_context(32768),
            snapshot,
        );

        assert_eq!(estimate.verdict, Verdict::Insufficient);
        let suggested = estimate
            .max_context_that_fits
            .expect("a context should fit");

        let retry = estimator.estimate(
            &metadata,
            RuntimeParams::default().with_context(suggested),
            snapshot,
        );
        assert_eq!(
            retry.verdict,
            Verdict::Safe,
            "the suggested context did not fit"
        );

        let over = estimator.estimate(
            &metadata,
            RuntimeParams::default().with_context(suggested.saturating_add(1024)),
            snapshot,
        );
        assert_ne!(
            over.verdict,
            Verdict::Safe,
            "the suggestion was too pessimistic"
        );
    }

    #[test]
    fn no_context_is_suggested_when_the_weights_alone_do_not_fit() {
        // Offering "reduce the context" when the weights cannot fit is
        // busywork the user cannot succeed at.
        let mut metadata = model(32, 32, vec![8], 128);
        metadata.weight_bytes = Some(Bytes::from_gib(20).get());
        let estimate = bare_estimator().estimate(
            &metadata,
            RuntimeParams::default().with_context(8192),
            machine(2),
        );

        assert_eq!(estimate.verdict, Verdict::Insufficient);
        assert_eq!(estimate.max_context_that_fits, None);
        assert!(
            !estimate
                .remedies()
                .iter()
                .any(|r| matches!(r.action, RemedyAction::ReduceContext { .. })),
            "a context reduction was offered even though the weights do not fit"
        );
    }

    #[test]
    fn remedies_are_offered_only_when_the_load_is_refused() {
        let metadata = model(32, 32, vec![8], 128);
        let estimator = bare_estimator();
        // 32 GiB free: fits easily, so there is nothing to suggest.
        let roomy = RuntimeParams::default().with_context(4096);
        assert!(
            estimator
                .estimate(&metadata, roomy, machine(32))
                .remedies()
                .is_empty()
        );

        // 1 GiB free against a 1 GiB cache plus a 512 MiB margin: refused, and
        // the refusal must come with something the user can act on.
        let cramped = RuntimeParams::default().with_context(8192);
        let refused = estimator.estimate(&metadata, cramped, machine(1));
        assert_eq!(refused.verdict, Verdict::Insufficient);
        assert!(!refused.remedies().is_empty());
    }

    #[test]
    fn the_shortfall_is_what_is_actually_needed_to_be_safe() {
        let metadata = model(32, 32, vec![8], 128);
        let estimate = bare_estimator().estimate(
            &metadata,
            RuntimeParams::default().with_context(8192),
            machine(1),
        );
        assert_eq!(
            estimate.shortfall(),
            estimate.total + estimate.margin - estimate.budget
        );
    }

    // ---- incomplete metadata ----

    #[test]
    fn missing_geometry_yields_a_partial_estimate_not_a_confident_one() {
        // Substituting a plausible layer count would produce a confident number
        // from data we do not have.
        let mut metadata = model(32, 32, vec![8], 128);
        metadata.block_count = None;
        let estimate = bare_estimator().estimate(
            &metadata,
            RuntimeParams::default().with_context(8192),
            machine(32),
        );

        assert_eq!(estimate.confidence, Confidence::Partial);
        assert!(estimate.missing.contains(&"block_count".to_owned()));
        assert_eq!(estimate.kv_cache, Bytes::ZERO);
        assert_eq!(estimate.max_context_that_fits, None);
    }

    #[test]
    fn an_unsizeable_weight_total_is_reported_as_partial() {
        let mut metadata = model(32, 32, vec![8], 128);
        metadata.weight_bytes = None;
        let estimate = bare_estimator().estimate(
            &metadata,
            RuntimeParams::default().with_context(4096),
            machine(32),
        );
        assert_eq!(estimate.confidence, Confidence::Partial);
    }

    #[test]
    fn a_per_layer_array_shorter_than_the_layer_count_is_not_extrapolated() {
        // Guessing the missing layers would be a silent invention.
        let metadata = model(32, 32, vec![8, 8, 8], 128);
        let estimate = bare_estimator().estimate(
            &metadata,
            RuntimeParams::default().with_context(4096),
            machine(32),
        );
        assert_eq!(estimate.confidence, Confidence::Partial);
        assert_eq!(estimate.kv_cache, Bytes::ZERO);
    }

    #[test]
    fn uncalibrated_coefficients_are_reported_as_coarse() {
        // The estimate is honest about which parts are measured.
        let metadata = model(32, 32, vec![8], 128);
        let estimate = Estimator::default().estimate(
            &metadata,
            RuntimeParams::default().with_context(4096),
            machine(32),
        );
        assert_eq!(estimate.confidence, Confidence::Coarse);
        assert!(!estimate.compute_model_is_measured());
    }

    #[test]
    fn the_margin_scales_with_the_machine() {
        // A flat 512 MiB is a real cushion on an 8 GB box and nothing at all on
        // a 128 GB one.
        assert_eq!(margin_for(Bytes::from_gib(2)), MIN_MARGIN);
        assert_eq!(
            margin_for(Bytes::from_gib(64)),
            Bytes((Bytes::from_gib(64).get() as f64 * 0.15) as u64)
        );
    }

    #[test]
    fn estimates_serialize_for_the_api_and_the_ui() {
        let metadata = model(32, 32, vec![8], 128);
        let estimate = Estimator::default().estimate(
            &metadata,
            RuntimeParams::default().with_context(4096),
            machine(32),
        );
        let json = serde_json::to_value(&estimate).expect("serialize");
        assert_eq!(json["verdict"], "safe");
        assert!(json["kv_cache"].as_u64().is_some());
        assert!(json["total"].as_u64().is_some());
    }
}

#[cfg(test)]
mod context_ceiling_tests {
    use super::tests_support::*;
    use super::*;

    #[test]
    fn the_suggested_context_never_exceeds_the_models_own_maximum() {
        // Plenty of memory, but the model was trained for 4096 tokens. Offering
        // more would be advice the engine refuses to take.
        let mut metadata = model(4, 8, vec![2], 64);
        metadata.context_length = Some(4096);
        let estimate = bare_estimator().estimate(
            &metadata,
            RuntimeParams::default().with_context(2048),
            machine(64),
        );
        assert_eq!(estimate.max_context_that_fits, Some(4096));
    }

    #[test]
    fn a_model_with_no_declared_maximum_is_bounded_only_by_memory() {
        let mut metadata = model(4, 8, vec![2], 64);
        metadata.context_length = None;
        let estimate = bare_estimator().estimate(
            &metadata,
            RuntimeParams::default().with_context(2048),
            machine(2),
        );
        assert!(estimate.max_context_that_fits.is_some());
    }
}

impl Estimator {
    /// The largest context preset that loads with a [`Verdict::Safe`] verdict.
    ///
    /// Exists so the product scales with the machine rather than to a constant.
    /// A fixed default sized for a small laptop would leave a 64 GB workstation
    /// running an 8K context for no reason, and a default sized for the
    /// workstation would refuse to load anything on the laptop. Both ends fall
    /// out of measuring the machine instead.
    ///
    /// Bounded by what the model was trained for, and by `ceiling` when the
    /// caller has its own limit. Returns `None` when not even the smallest
    /// preset fits, which is a refusal the caller should surface with remedies
    /// rather than paper over.
    pub fn largest_safe_context(
        &self,
        metadata: &ModelMetadata,
        base: RuntimeParams,
        snapshot: MemorySnapshot,
        ceiling: Option<u32>,
    ) -> Option<u32> {
        self.largest_safe_context_against(metadata, base, Budget::of(snapshot), ceiling)
    }

    /// The same search, against a budget that may include memory about to be
    /// released.
    ///
    /// A swap has to size its window against the memory it will have, not the
    /// memory it has while the outgoing engine is still holding some — the same
    /// argument as [`Budget`], applied to the choice of context rather than to
    /// the verdict on it. The two must agree, or a context this search picked
    /// could be refused by the estimate that follows it.
    pub fn largest_safe_context_against(
        &self,
        metadata: &ModelMetadata,
        base: RuntimeParams,
        budget: Budget,
        ceiling: Option<u32>,
    ) -> Option<u32> {
        let mut presets = RuntimeParams::context_presets_for(metadata.context_length);
        if let Some(ceiling) = ceiling {
            presets.retain(|&preset| preset <= ceiling);
        }
        // Largest first: the answer is the first that fits.
        presets.into_iter().rev().find(|&n_ctx| {
            self.estimate_against(metadata, base.with_context(n_ctx), budget)
                .verdict
                == Verdict::Safe
        })
    }
}

#[cfg(test)]
mod scaling_tests {
    use super::tests_support::*;
    use super::*;

    #[test]
    fn a_bigger_machine_is_offered_a_bigger_context() {
        // The point of measuring rather than defaulting: the same model on a
        // larger machine should use more of it.
        let metadata = model(32, 32, vec![8], 128);
        let estimator = bare_estimator();

        let small = estimator
            .largest_safe_context(&metadata, RuntimeParams::default(), machine(4), None)
            .expect("something should fit on a 4 GiB machine");
        let large = estimator
            .largest_safe_context(&metadata, RuntimeParams::default(), machine(64), None)
            .expect("something should fit on a 64 GiB machine");

        assert!(
            large > small,
            "a 64 GiB machine was offered {large}, no more than the 4 GiB machine's {small}"
        );
    }

    #[test]
    fn the_chosen_context_actually_loads_safely() {
        // The recommendation has to be true, or it is worse than no default.
        let metadata = model(32, 32, vec![8], 128);
        let estimator = bare_estimator();
        for available in [2, 4, 8, 16, 64] {
            let snapshot = machine(available);
            let Some(chosen) =
                estimator.largest_safe_context(&metadata, RuntimeParams::default(), snapshot, None)
            else {
                continue;
            };
            let verdict = estimator
                .estimate(
                    &metadata,
                    RuntimeParams::default().with_context(chosen),
                    snapshot,
                )
                .verdict;
            assert_eq!(
                verdict,
                Verdict::Safe,
                "{chosen} tokens was not safe at {available} GiB"
            );
        }
    }

    #[test]
    fn the_model_maximum_is_never_exceeded() {
        // However much memory there is, the engine will not accept more than
        // the model was trained for.
        let mut metadata = model(4, 8, vec![2], 64);
        metadata.context_length = Some(8192);
        let chosen = bare_estimator()
            .largest_safe_context(&metadata, RuntimeParams::default(), machine(256), None)
            .expect("a tiny model on a huge machine");
        assert_eq!(chosen, 8192);
    }

    #[test]
    fn a_caller_supplied_ceiling_is_respected() {
        let metadata = model(4, 8, vec![2], 64);
        let chosen = bare_estimator()
            .largest_safe_context(
                &metadata,
                RuntimeParams::default(),
                machine(256),
                Some(8192),
            )
            .expect("something fits");
        assert!(chosen <= 8192);
    }

    #[test]
    fn nothing_is_recommended_when_nothing_fits() {
        // Better to refuse with remedies than to suggest a context that will
        // be killed by the OOM killer.
        let mut metadata = model(32, 32, vec![8], 128);
        metadata.weight_bytes = Some(Bytes::from_gib(40).get());
        assert_eq!(
            bare_estimator().largest_safe_context(
                &metadata,
                RuntimeParams::default(),
                machine(2),
                None
            ),
            None
        );
    }
}
