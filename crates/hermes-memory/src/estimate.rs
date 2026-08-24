//! RAM estimation and admission control.
//!
//! Spec section 7 makes this a headline feature, and section 19 adds a rule
//! that shapes the whole design: *do not promise that a model will run merely
//! because its weights fit into RAM.* The KV cache and the runtime's own
//! buffers are frequently larger than the weights at a long context, so an
//! estimate that ignores them is worse than no estimate at all.
//!
//! ## What is exact and what is measured
//!
//! Two of the four terms are computed exactly from metadata:
//!
//! * **Weights** — summed from the real tensor shapes and their ggml block
//!   geometry. Not derived from the file size, and not from the quantization
//!   label, which is only ever an approximation of a mixed-type file.
//! * **KV cache** — computed per layer, because real models are not uniform.
//!   Two cases from actual files verified this:
//!   LFM2-1.2B writes `head_count_kv` as a 16-element array
//!   `[0,0,8,0,0,8,0,0,8,0,8,0,8,0,8,0]` — only six of its sixteen layers have
//!   attention at all, so assuming the first non-zero value applies throughout
//!   would overstate its cache by a factor of 2.67. Gemma-3-1B declares
//!   `key_length = 256` while `embedding_length / head_count` is 288, so
//!   deriving the head dimension instead of reading it would overstate by 12.5%.
//!
//! The other two cannot be derived from metadata at all, so they are
//! **measured, versioned and labelled** rather than invented: compute buffers
//! and runtime overhead use coefficients that the benchmark harness fits from
//! observed peak RSS. Until a model has been measured, the shipped conservative
//! defaults are used and the estimate is reported as
//! [`Confidence::Coarse`] — the UI says so rather than implying precision the
//! numbers do not have.
//!
//! ## What the budget is
//!
//! `MemAvailable`, never `MemTotal`, and **swap is never headroom**. Decode
//! touches essentially every weight once per token, so a model that fits only
//! by swapping would page continuously — the "heavy swapping" section 7
//! forbids. Free swap is reported to the user as context and excluded from the
//! arithmetic.

use hermes_core::units::Bytes;
use hermes_core::{GgmlType, Remedy, RemedyAction, SettingsSection};
use hermes_system_info::MemorySnapshot;
use serde::{Deserialize, Serialize};

use hermes_core::RuntimeParams;

/// Whether a load should be allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Fits with the safety margin intact.
    Safe,
    /// Fits, but inside the margin. It will probably work; another application
    /// growing could push it into an OOM kill.
    Tight,
    /// Does not fit. The load is refused, with remedies.
    Insufficient,
}

impl Verdict {
    pub const fn is_admissible(self) -> bool {
        matches!(self, Self::Safe | Self::Tight)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Safe => "SAFE",
            Self::Tight => "TIGHT",
            Self::Insufficient => "INSUFFICIENT",
        }
    }
}

/// How much to trust the numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Weights and KV cache are exact; compute and overhead come from
    /// measurements of this model on this machine.
    Measured,
    /// Weights and KV cache are exact; compute and overhead come from shipped
    /// defaults that have not been calibrated for this model.
    Coarse,
    /// Some metadata needed for an exact figure was missing. The total is a
    /// lower bound, and [`Estimate::missing`] says what was absent.
    Partial,
}

/// Coefficients for the terms that cannot be derived from metadata.
///
/// Deliberately data rather than constants buried in the arithmetic: the
/// benchmark harness fits them from observed peak RSS per
/// (architecture, quantization, context, batch) and stores them, so they can
/// improve without changing this code.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComputeModel {
    /// Multiplier on `n_ubatch * embedding_length * 4` for activations.
    pub activation_factor: f64,
    /// Multiplier on `n_ubatch * max(embedding, ffn) * 4` for engine scratch.
    pub scratch_factor: f64,
    /// Baseline resident set of the engine process before a model is loaded.
    pub engine_baseline: Bytes,
    /// Resident set of the gateway and UI processes.
    pub host_overhead: Bytes,
    /// Whether these came from measurement or are the shipped defaults.
    pub measured: bool,
}

impl Default for ComputeModel {
    /// Conservative shipped defaults, used until a measurement replaces them.
    ///
    /// Erring high is deliberate: an overestimate refuses a load that would
    /// have worked, which the user can override; an underestimate invites the
    /// OOM killer, which they cannot.
    fn default() -> Self {
        Self {
            activation_factor: 8.0,
            scratch_factor: 4.0,
            engine_baseline: Bytes::from_mib(64),
            // The chosen desktop shell is Electron, which is not cheap. Counting
            // it here means the budget reflects the machine as the user will
            // actually be running it, rather than an idealised headless box.
            host_overhead: Bytes::from_mib(450),
            measured: false,
        }
    }
}

impl ComputeModel {
    /// A model with no host overhead, for the headless daemon.
    pub fn headless() -> Self {
        Self {
            host_overhead: Bytes::from_mib(48),
            ..Self::default()
        }
    }
}

/// What a load may spend: what the machine has free, plus what it is about to
/// release.
///
/// The second term exists for exactly one caller. A model swap is estimated
/// while the outgoing model is still resident, because the engine is stopped
/// only once the new load has been admitted — a refusal must never cost the
/// user the model they already had. Without a credit the swap is judged against
/// memory the outgoing engine is about to hand back, and refused for a shortage
/// that will not exist by the time it matters.
///
/// The credit is deliberately narrow. It is the *anonymous* resident set, not
/// the whole of it: the engine mmaps the model file, so most of its RSS is
/// file-backed page cache the kernel already counts inside `MemAvailable`, and
/// crediting that would count the weights twice. Being wrong optimistically
/// here ends in an OOM kill, which is the one direction the memory probe and
/// the disk probe both refuse to be wrong in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// What the machine reported.
    pub snapshot: MemorySnapshot,
    /// What a process this load replaces is about to give back. Zero for every
    /// load that replaces nothing, which is most of them.
    pub reclaimable: Bytes,
}

impl Budget {
    /// A budget with nothing to reclaim: the ordinary case.
    pub const fn of(snapshot: MemorySnapshot) -> Self {
        Self {
            snapshot,
            reclaimable: Bytes::ZERO,
        }
    }

    /// Credit memory a process this load replaces is about to release.
    #[must_use]
    pub const fn reclaiming(mut self, reclaimable: Bytes) -> Self {
        self.reclaimable = reclaimable;
        self
    }

    /// What this load may actually spend.
    pub const fn spendable(&self) -> Bytes {
        self.snapshot.available.saturating_add(self.reclaimable)
    }
}

impl From<MemorySnapshot> for Budget {
    fn from(snapshot: MemorySnapshot) -> Self {
        Self::of(snapshot)
    }
}

/// A full accounting of what a load would cost and whether it fits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    /// Exact, from tensor shapes and ggml block geometry.
    pub weights: Bytes,
    /// Exact, computed per layer.
    pub kv_cache: Bytes,
    /// Logits, activations and engine scratch.
    pub compute: Bytes,
    /// Engine baseline plus gateway and UI.
    pub overhead: Bytes,
    /// The sum of the four.
    pub total: Bytes,
    /// What this load may spend: `MemAvailable` plus `reclaimable`. Swap is
    /// excluded from both.
    pub budget: Bytes,
    /// The part of `budget` that is not free yet, because it belongs to a
    /// process this load replaces. Zero for every load that replaces nothing.
    ///
    /// Carried so that `budget` exceeding `snapshot.available` reads as the
    /// deliberate credit it is rather than as a corrupt pair of numbers.
    pub reclaimable: Bytes,
    /// Headroom required above `total` for a [`Verdict::Safe`].
    pub margin: Bytes,
    pub verdict: Verdict,
    pub confidence: Confidence,
    /// KV bytes each additional token of context costs. Used to solve for the
    /// largest context that would fit, and useful on its own in the UI.
    pub kv_bytes_per_token: u64,
    /// Largest context that would fit, when a smaller one would help.
    pub max_context_that_fits: Option<u32>,
    /// Metadata keys that were needed and absent.
    pub missing: Vec<String>,
    /// The memory reading this verdict was computed against.
    pub snapshot: MemorySnapshot,
    /// The parameters this estimate is for.
    pub params: RuntimeParams,
}

impl Estimate {
    /// Whether the calibrated coefficients behind this estimate came from a
    /// measurement rather than the shipped defaults.
    pub const fn compute_model_is_measured(&self) -> bool {
        matches!(self.confidence, Confidence::Measured)
    }

    /// How much more memory would be needed to reach [`Verdict::Safe`].
    pub fn shortfall(&self) -> Bytes {
        self.total
            .saturating_add(self.margin)
            .saturating_sub(self.budget)
    }

    /// Actionable next steps, best first.
    ///
    /// Spec section 27's example is explicit that an error must suggest what to
    /// do, and section 7 wants those suggestions to be specific. Every remedy
    /// here carries the numbers needed to apply it, so the UI can offer a
    /// button rather than advice.
    pub fn remedies(&self) -> Vec<Remedy> {
        if self.verdict.is_admissible() {
            return Vec::new();
        }

        let mut remedies = Vec::new();

        // Reducing context only helps if the weights themselves fit. When they
        // do not, offering it would be busywork that cannot succeed.
        if let Some(context) = self.max_context_that_fits {
            remedies.push(Remedy::new(
                format!("Reduce the context to {context} tokens"),
                RemedyAction::ReduceContext { to_tokens: context },
            ));
        }

        if self.params.cache_type_k == GgmlType::F16 && self.kv_cache > Bytes::from_mib(256) {
            // q8_0 is 34 bytes per 32 elements against f16's 2 bytes per
            // element, so very close to half.
            let saved = Bytes(
                self.kv_cache
                    .get()
                    .saturating_sub(self.kv_cache.get().saturating_mul(34) / 64),
            );
            remedies.push(Remedy::new(
                format!("Quantize the KV cache to q8_0, saving about {saved}"),
                RemedyAction::QuantizeKvCache {
                    cache_type: "q8_0".to_owned(),
                    saves_bytes: saved.get(),
                },
            ));
        }

        remedies.push(Remedy::new(
            format!(
                "Choose a model whose weights are under {}",
                self.budget
                    .saturating_sub(self.margin)
                    .saturating_sub(self.overhead)
            ),
            RemedyAction::UseSmallerModel {
                max_weight_bytes: self
                    .budget
                    .saturating_sub(self.margin)
                    .saturating_sub(self.overhead)
                    .get(),
            },
        ));

        remedies.push(Remedy::new(
            format!(
                "Close other applications to free about {}",
                self.shortfall()
            ),
            RemedyAction::FreeMemory {
                needed_bytes: self.shortfall().get(),
            },
        ));

        if self.confidence == Confidence::Partial {
            remedies.push(Remedy::new(
                "This model's metadata is incomplete, so the estimate is a lower bound",
                RemedyAction::OpenSettings {
                    section: SettingsSection::Models,
                },
            ));
        }

        remedies
    }
}
