//! Turning a fit into the estimator's coefficients — and refusing to.
//!
//! `fit.rs` measures and records. This decides whether a recorded fit is worth
//! believing, which is a different question and deliberately a separate one:
//! the benchmark cannot know how far its own numbers should travel.
//!
//! **Every rule here is a reason to ignore a fit, never a reason to invent
//! one.** A fit that does not match the machine, the engine build and the
//! bucket exactly is not approximated — `Calibration::find` already refuses it,
//! and the reasons are in that function's own doc comment. A fit that does
//! match may still be too thin to use, and then the shipped defaults stand,
//! which is the outcome the estimator has always had.
//!
//! **What a fit may change, and what it may not.** A residual is a peak minus
//! the exactly-computed terms that peak contains - `Prediction::exact_within`,
//! which is weights plus KV cache on Linux and Windows and the KV cache alone
//! against a macOS footprint - and `peak_rss` is the *engine* process's. So a
//! residual describes the engine's compute buffers and its baseline. It
//! therefore never touches [`ComputeModel::host_overhead`], which is the
//! desktop shell: no benchmark in this workspace has ever observed that
//! process, and calibrating a term nothing measured is exactly the confident
//! wrong number this design refuses. The exact half is never touched at all.
//!
//! **What the measurements decided.** Two of the rules below carry a number,
//! and neither was argued into place. A four-value batch-size sweep and a
//! three-value context sweep on this machine (2026-08-25, SmolLM2-135M on
//! llama.cpp b10590) found that the residual is *flat* in `n_ctx` — 0.48 MiB
//! across a fourfold range, because the context's cost is the KV cache, which
//! is the exact half — and that it moves with `n_ubatch` by only 6 MiB across
//! an eightfold range, in a curve rather than a line. [`MIN_UBATCH_VALUES`] and
//! [`MIN_R_SQUARED`] are where those findings live, each with its numbers in
//! its own doc comment — and each says how far its number actually follows from
//! them, which is further for the first than for the second.
//!
//! The outcome on this machine is that no fit is trustworthy and the shipped
//! guesses stand. That is the contingency M10-PLAN section 4.1 named rather
//! than a failure, and it is not the "defaults are already close" of section
//! 4.2: they over-estimate by 1.4× to 2.9×, in the direction [`ComputeModel`]'s
//! own doc comment says to err. What the refusal is worth here is that it stops
//! a false [`lightweight_memory::Confidence::Measured`] — the compute term's shape is
//! wrong for this engine, and a fit spent against it would be a measured-looking
//! number built on it.
//!
//! **Why the two compute coefficients move together.** The estimator's compute
//! term is `vocab*ub*4 + activation*ub*embd*4 + scratch*ub*max(embd,ffn)*4`.
//! A fit provides one slope, and `fit.rs` explains why peak RSS cannot separate
//! the last two: they are collinear. So the slope fixes their *sum* and their
//! shipped 8:4 ratio is carried over unchanged. The ratio is inherited, not
//! measured, and nothing here pretends otherwise.

use lightweight_core::RuntimeParams;
use lightweight_core::units::Bytes;
use lightweight_gguf::ModelMetadata;
use lightweight_memory::ComputeModel;

use crate::fit::{BucketKey, Calibration, Fit};
use crate::record::{EngineFingerprint, MachineFingerprint};

/// Fewest *distinct* batch sizes a fit may rest on.
///
/// Two points define a line exactly, with no evidence that the line is the
/// right shape; the third is the first one that can disagree with it.
///
/// It counts batch sizes rather than observations, and the difference is not
/// pedantic: the fit this machine had on disk before this rule existed rested
/// on twelve observations at **two** batch sizes, and twelve is comfortably
/// more than three. Repetitions and scenarios multiply the observations without
/// adding a single point the line can be wrong about.
pub const MIN_UBATCH_VALUES: usize = 3;

/// How much of the spread a fitted line must account for.
///
/// **Measurement-informed conservative policy, not a derived constant, and the
/// difference is worth being exact about.** What the measurement establishes is
/// a lower bound: a four-value sweep on this machine (run `18cf2bfb07298264`,
/// SmolLM2-135M on llama.cpp b10590) scored **0.79** with its ratchet
/// controlled for, and that shortfall is not noise — the slope between
/// consecutive batch sizes was 32,600 then 26,378 then 2,949 bytes per ubatch
/// across 64 → 128 → 256 → 512. An elevenfold disagreement between segments is
/// a curve, and a straight line through it describes something the fit format
/// cannot describe. So 0.79 must fail. **Nothing here derives 0.95**; any bar
/// above 0.79 would have refused this sweep.
///
/// It is set high within that freedom because of the asymmetry in what passing
/// buys: permission to lower a memory budget below the shipped guess, where
/// being wrong invites the OOM killer rather than a refusal the user can
/// override. A machine whose residual really is affine will score far nearer
/// 1.0 than 0.95 and pass without argument. A second machine scoring between
/// 0.79 and 0.95 is the case this number was chosen without evidence about, and
/// is a reason to revisit it with that machine's data.
pub const MIN_R_SQUARED: f64 = 0.95;

/// Why a fit was not used.
///
/// Returned rather than logged and swallowed, because every one of these is
/// something a person running `hermes bench` would want to be told.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Untrusted {
    /// No fit for this machine, this engine build and this bucket.
    NoFit,
    /// Fewer than [`MIN_UBATCH_VALUES`] distinct batch sizes.
    TooFewUbatchValues { have: usize },
    /// One ubatch value, so the regression declined to report a slope.
    NoSlope,
    /// The line accounts for less than [`MIN_R_SQUARED`] of the spread, so the
    /// residual is not the affine function of `n_ubatch` the format assumes.
    NotAffine { r_squared: f64 },
    /// The fit predates this field and cannot be judged against
    /// [`MIN_R_SQUARED`]. Refitting the run it came from produces one.
    NoGoodnessOfFit,
    /// A negative slope or intercept: a measurement artefact, not a
    /// coefficient. It happens when the weights were not all resident.
    NegativeTerm,
    /// The model's own geometry is missing, so the slope cannot be spent.
    ShapeUnknown,
    /// The measured slope is below the logits term alone, which the shipped
    /// model computes from the vocabulary rather than fitting.
    ///
    /// This was written expecting to catch a run that measured the wrong thing.
    /// On this machine it fires on runs that measured correctly, and the reason
    /// is worth recording where the guard is: against llama.cpp b10590 the
    /// *whole* measured slope is about 13,100 bytes per ubatch, while the
    /// shipped logits term alone claims 196,608 — fifteen times more — because
    /// `Estimator::compute_bytes` scales logits by `n_ubatch` and the engine
    /// appears to size that buffer by the number of tokens it is actually asked
    /// for, which during prefill is one. Until the compute term's *shape* is
    /// revisited against a second engine, this guard refuses every honest fit
    /// on this one. That is the conservative direction — the shipped guess
    /// stands and it over-estimates — so it stays, and the shape is recorded as
    /// a deferral rather than fixed here on one machine's evidence.
    SlopeBelowLogits,
}

impl Untrusted {
    /// A sentence naming what was wrong, for a log line or a CLI note.
    pub fn reason(self) -> String {
        match self {
            Self::NoFit => "no measurement for this machine, engine and settings".to_owned(),
            Self::TooFewUbatchValues { have } => format!(
                "the run swept {have} batch size(s); {MIN_UBATCH_VALUES} are needed before a line \
                 through them is believed"
            ),
            Self::NoSlope => {
                "the run held the batch size fixed, so it has no slope to spend".to_owned()
            }
            Self::NotAffine { r_squared } => format!(
                "the fitted line accounts for {:.0}% of the spread across batch sizes, below the \
                 {:.0}% a measurement must reach before it replaces the shipped guess",
                r_squared * 100.0,
                MIN_R_SQUARED * 100.0
            ),
            Self::NoGoodnessOfFit => {
                "the fit was recorded before goodness of fit was, so it cannot be judged; \
                 re-run `hermes bench --fit`"
                    .to_owned()
            }
            Self::NegativeTerm => {
                "the fit is negative, which means the weights were not all resident".to_owned()
            }
            Self::ShapeUnknown => {
                "the model header did not carry the vocabulary and embedding sizes".to_owned()
            }
            Self::SlopeBelowLogits => {
                "the measured slope is below the exactly-computed logits term".to_owned()
            }
        }
    }
}

/// A compute model that came from a measurement, and what it cost to make safe.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Calibrated {
    /// The model to hand [`lightweight_memory::Estimator::new`].
    pub compute_model: ComputeModel,
    /// When the fit behind it was taken.
    pub fit_at_unix: u64,
    /// How much the intercept had to be raised so that no observation in the
    /// fit sits above the line. Zero for a fit whose own points are all under
    /// it, which is most of them.
    pub floor_raised_by: Bytes,
}

/// The bucket a load falls into.
///
/// Built exactly as `fit_run` builds it, from the same two sources: the model's
/// own labels and the runtime parameters. Anywhere these two could disagree is
/// a load silently priced against another load's measurement.
pub fn bucket_for(metadata: &ModelMetadata, params: RuntimeParams) -> BucketKey {
    BucketKey {
        architecture: metadata.architecture.clone(),
        quantization: metadata.quantization_label(),
        n_ctx: params.n_ctx,
        n_parallel: params.n_parallel,
        cache_type_k: params.cache_type_k.name().to_owned(),
        cache_type_v: params.cache_type_v.name().to_owned(),
    }
}

/// The compute model to use for this load, if a trustworthy fit describes it.
///
/// `base` is the model that would otherwise be used — `ComputeModel::default()`
/// or `headless()` — and it is where `host_overhead` and the coefficients'
/// ratio come from. Passing it in rather than reaching for a default means the
/// headless daemon and the desktop shell stay as different as they already are.
pub fn compute_model_for(
    calibration: &Calibration,
    machine: &MachineFingerprint,
    engine: &EngineFingerprint,
    metadata: &ModelMetadata,
    params: RuntimeParams,
    base: ComputeModel,
) -> Result<Calibrated, Untrusted> {
    let bucket = bucket_for(metadata, params);
    let fit = calibration
        .find(machine, engine, &bucket)
        .ok_or(Untrusted::NoFit)?;
    apply(fit, metadata, base)
}

/// Spend one fit's slope and intercept on a compute model.
///
/// Separate from the lookup so that the trust rules can be tested against a fit
/// directly, without a calibration file and a fingerprint to match.
pub fn apply(
    fit: &Fit,
    metadata: &ModelMetadata,
    base: ComputeModel,
) -> Result<Calibrated, Untrusted> {
    // Counted first, and before the goodness of fit is read, because two batch
    // sizes always score a perfect 1.0 — a line through two points passes
    // through both — and a rule that cannot fail is not a rule.
    let ubatch_values: std::collections::BTreeSet<u32> =
        fit.points.iter().map(|point| point.n_ubatch).collect();
    if ubatch_values.len() < MIN_UBATCH_VALUES {
        return Err(Untrusted::TooFewUbatchValues {
            have: ubatch_values.len(),
        });
    }
    let (Some(slope), Some(intercept)) = (fit.compute_bytes_per_ubatch, fit.overhead_bytes) else {
        return Err(Untrusted::NoSlope);
    };
    match fit.r_squared {
        None => return Err(Untrusted::NoGoodnessOfFit),
        Some(r_squared) if r_squared < MIN_R_SQUARED => {
            return Err(Untrusted::NotAffine { r_squared });
        }
        Some(_) => {}
    }
    if slope < 0.0 || intercept < 0.0 {
        return Err(Untrusted::NegativeTerm);
    }

    // The same three numbers `Estimator::compute_bytes` reads, and the same
    // arithmetic, per unit of ubatch.
    let (Some(vocab), Some(embedding)) = (metadata.vocab_size, metadata.embedding_length) else {
        return Err(Untrusted::ShapeUnknown);
    };
    let vocab = vocab as f64;
    let embedding = embedding as f64;
    let ffn = metadata.feed_forward_length.unwrap_or(0) as f64;
    if embedding == 0.0 {
        return Err(Untrusted::ShapeUnknown);
    }

    let logits_per_ubatch = vocab * 4.0;
    let shape_per_ubatch =
        base.activation_factor * embedding * 4.0 + base.scratch_factor * embedding.max(ffn) * 4.0;
    if shape_per_ubatch <= 0.0 {
        return Err(Untrusted::ShapeUnknown);
    }
    let fitted = slope - logits_per_ubatch;
    if fitted < 0.0 {
        return Err(Untrusted::SlopeBelowLogits);
    }
    let scale = fitted / shape_per_ubatch;

    // The floor. Least squares puts a line *through* its points, so about half
    // of them sit above it; budgeting the line alone would under-estimate every
    // one of those. Raising the intercept by the largest shortfall keeps the
    // measured slope and makes the model right about every sample it was fitted
    // from - which is the property `Fit::max_residual_bytes` was recorded for.
    let shortfall = fit
        .points
        .iter()
        .map(|point| point.residual_bytes as f64 - (slope * f64::from(point.n_ubatch) + intercept))
        .fold(0.0_f64, f64::max);

    Ok(Calibrated {
        compute_model: ComputeModel {
            activation_factor: base.activation_factor * scale,
            scratch_factor: base.scratch_factor * scale,
            engine_baseline: bytes(intercept + shortfall),
            // Never fitted: no benchmark has ever measured the shell.
            host_overhead: base.host_overhead,
            measured: true,
        },
        fit_at_unix: fit.at_unix,
        floor_raised_by: bytes(shortfall),
    })
}

/// The fingerprint of the engine a backend is driving.
///
/// One definition for the three places that need one - `hermes bench`, the
/// gateway's benchmark endpoint and the load path's calibration lookup -
/// because a fit is looked up by exactly the key a run was recorded with, and
/// two spellings of "the same engine" would mean a machine could never find its
/// own measurement.
pub fn engine_fingerprint(
    backend: &dyn lightweight_inference::InferenceBackend,
) -> EngineFingerprint {
    EngineFingerprint {
        backend: backend.id().to_string(),
        // Stated by the backend rather than guessed by the caller.
        build: backend.capabilities().build,
        ggml_variant: Some(
            lightweight_system_info::CpuInfo::detect()
                .expected_ggml_variant()
                .to_owned(),
        ),
    }
}

/// What happened when a load asked for a calibrated model.
///
/// Three outcomes rather than an `Option`, because "there is no measurement"
/// and "there is one and it cannot be trusted" and "the file is damaged" are
/// three different things to say to a user, and the last one is the only one
/// worth a warning in the log.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// A fit was found and spent.
    Applied(Calibrated),
    /// A fit was found or looked for, and the shipped defaults stand.
    Rejected(Untrusted),
    /// The calibration file exists and could not be read.
    Unreadable(String),
}

impl Outcome {
    /// The fit that was used, if any.
    pub const fn calibrated(&self) -> Option<&Calibrated> {
        match self {
            Self::Applied(calibrated) => Some(calibrated),
            _ => None,
        }
    }
}

/// The estimator to use for one load, calibrated if this machine has earned it.
///
/// **A calibration may never cost somebody their model.** An absent file is the
/// ordinary case, an unparsable one is reported and then ignored, and a fit
/// that fails any rule above leaves the shipped defaults exactly as they were.
/// Every path returns an estimator; none of them returns an error.
pub fn estimator_for(
    calibration_path: &std::path::Path,
    engine: &EngineFingerprint,
    metadata: &ModelMetadata,
    params: RuntimeParams,
    base: ComputeModel,
) -> (lightweight_memory::Estimator, Outcome) {
    let calibration = match Calibration::load(calibration_path) {
        Ok(calibration) => calibration,
        Err(err) => {
            return (
                lightweight_memory::Estimator::new(base),
                Outcome::Unreadable(err.to_string()),
            );
        }
    };
    let machine = MachineFingerprint::detect();
    match compute_model_for(&calibration, &machine, engine, metadata, params, base) {
        Ok(calibrated) => (
            lightweight_memory::Estimator::new(calibrated.compute_model),
            Outcome::Applied(calibrated),
        ),
        Err(untrusted) => (
            lightweight_memory::Estimator::new(base),
            Outcome::Rejected(untrusted),
        ),
    }
}

/// Round a fitted quantity to bytes, never below zero.
fn bytes(value: f64) -> Bytes {
    if !value.is_finite() || value <= 0.0 {
        return Bytes::ZERO;
    }
    Bytes(value.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fit::ResidualPoint;
    use lightweight_gguf::{QuantMix, TokenizerMeta};
    use lightweight_memory::Estimator;

    /// Numbers chosen so the compute term can be worked out by hand.
    fn metadata() -> ModelMetadata {
        ModelMetadata {
            architecture: "llama".to_owned(),
            supported: true,
            name: None,
            context_length: Some(4_096),
            block_count: Some(4),
            embedding_length: Some(1_000),
            feed_forward_length: Some(2_000),
            head_count: Some(8),
            head_count_kv: Some(vec![8]),
            key_length: None,
            value_length: None,
            sliding_window: None,
            rope_freq_base: None,
            vocab_size: Some(10_000),
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

    fn machine() -> MachineFingerprint {
        MachineFingerprint {
            cpu_model: Some("A Processor".to_owned()),
            physical_cores: 4,
            logical_cores: 4,
            isa_features: vec!["sse4.2".to_owned()],
            total_memory: Bytes::from_mib(8_192),
            os: "linux".to_owned(),
            architecture: "x86_64".to_owned(),
        }
    }

    fn engine() -> EngineFingerprint {
        EngineFingerprint {
            backend: "llama.cpp".to_owned(),
            build: Some("b10590".to_owned()),
            ggml_variant: Some("sse42".to_owned()),
        }
    }

    fn params() -> RuntimeParams {
        RuntimeParams {
            n_ubatch: 512,
            ..RuntimeParams::default()
        }
    }

    /// A fit whose points sit exactly on `slope * ubatch + intercept`.
    fn fit_on_the_line(slope: f64, intercept: f64, ubatches: &[u32]) -> Fit {
        let points: Vec<ResidualPoint> = ubatches
            .iter()
            .map(|&n_ubatch| ResidualPoint {
                n_ubatch,
                residual_bytes: (slope * f64::from(n_ubatch) + intercept) as u64,
                peak_rss: Bytes(0),
            })
            .collect();
        Fit {
            at_unix: 1_700_000_000,
            machine: machine(),
            engine: engine(),
            bucket: bucket_for(&metadata(), params()),
            compute_bytes_per_ubatch: Some(slope),
            overhead_bytes: Some(intercept),
            // Every point is on the line by construction, so the line accounts
            // for all of the spread. A test that wants the linearity rule to
            // fire lowers this itself.
            r_squared: Some(1.0),
            max_residual_bytes: points
                .iter()
                .map(|point| point.residual_bytes)
                .max()
                .unwrap_or_default(),
            points,
        }
    }

    /// What the estimator budgets for the terms a fit may change.
    ///
    /// Taken through the estimator rather than recomputed here: the point of a
    /// calibration is what the estimator ends up saying, and a test that did
    /// the arithmetic itself would pass even if nothing reached it.
    fn compute_and_engine_baseline(model: ComputeModel, n_ubatch: u32) -> u64 {
        let estimator = Estimator::new(model);
        let estimate = estimator.estimate(
            &metadata(),
            RuntimeParams {
                n_ubatch,
                ..params()
            },
            lightweight_system_info::MemorySnapshot {
                total: Bytes::from_mib(16_384),
                free: Bytes::from_mib(8_192),
                available: Bytes::from_mib(8_192),
                swap_total: Bytes::ZERO,
                swap_free: Bytes::ZERO,
            },
        );
        // The engine's share of overhead is what a fit sets; the host's is not,
        // so it is subtracted back off before comparing against a residual.
        estimate.compute.get() + estimate.overhead.get() - model.host_overhead.get()
    }

    #[test]
    fn a_spent_fit_makes_the_estimator_reproduce_the_measured_line() {
        let (slope, intercept) = (40_000.0, 60_000_000.0);
        let calibrated = apply(
            &fit_on_the_line(slope, intercept, &[128, 256, 512]),
            &metadata(),
            ComputeModel::headless(),
        )
        .expect("a three-point fit across three ubatch values is usable");

        // The estimator's own arithmetic, at two batch sizes, must differ by
        // exactly the slope that was measured between them.
        let at_256 = compute_and_engine_baseline(calibrated.compute_model, 256);
        let at_512 = compute_and_engine_baseline(calibrated.compute_model, 512);
        let measured = (at_512 - at_256) as f64;
        assert!(
            (measured - slope * 256.0).abs() < 4_096.0,
            "the estimator's slope is {measured}, the fit's is {}",
            slope * 256.0
        );
        assert_eq!(calibrated.floor_raised_by, Bytes::ZERO);
        assert!(calibrated.compute_model.measured);
        assert_eq!(calibrated.fit_at_unix, 1_700_000_000);
    }

    #[test]
    fn no_observation_in_a_fit_is_left_above_the_model_it_produces() {
        // Least squares puts a line through its points, so some sit above it.
        // Budgeting the line alone would under-estimate exactly those loads,
        // which is the one direction this whole design refuses to be wrong in.
        let mut fit = fit_on_the_line(40_000.0, 60_000_000.0, &[128, 256, 512]);
        let over = fit.points[1].residual_bytes + 25_000_000;
        fit.points[1].residual_bytes = over;
        fit.max_residual_bytes = fit
            .points
            .iter()
            .map(|point| point.residual_bytes)
            .max()
            .unwrap_or_default();

        let calibrated =
            apply(&fit, &metadata(), ComputeModel::headless()).expect("still a usable fit");

        assert_eq!(calibrated.floor_raised_by, Bytes(25_000_000));
        for point in &fit.points {
            let budgeted = compute_and_engine_baseline(calibrated.compute_model, point.n_ubatch);
            assert!(
                budgeted >= point.residual_bytes,
                "a load at ubatch {} would be budgeted {budgeted} against an observed {}",
                point.n_ubatch,
                point.residual_bytes
            );
        }
    }

    #[test]
    fn the_desktop_shells_overhead_is_never_calibrated() {
        // No benchmark in this workspace has ever observed the shell: peak RSS
        // is the engine's. A fit that moved this number would be inventing it.
        for base in [ComputeModel::default(), ComputeModel::headless()] {
            let calibrated = apply(
                &fit_on_the_line(40_000.0, 60_000_000.0, &[128, 256, 512]),
                &metadata(),
                base,
            )
            .expect("usable");
            assert_eq!(calibrated.compute_model.host_overhead, base.host_overhead);
        }
    }

    #[test]
    fn a_run_that_held_the_batch_size_fixed_is_not_a_calibration() {
        let mut fit = fit_on_the_line(40_000.0, 60_000_000.0, &[512, 512, 512]);
        // What `regress` reports for a single distinct ubatch.
        fit.compute_bytes_per_ubatch = None;
        fit.overhead_bytes = None;
        // Refused for the batch-size count rather than for the missing slope:
        // both are true, and the count is the one that names what to do about
        // it. Three repetitions of one batch size is one point.
        assert_eq!(
            apply(&fit, &metadata(), ComputeModel::headless()),
            Err(Untrusted::TooFewUbatchValues { have: 1 })
        );
    }

    #[test]
    fn a_fit_with_enough_batch_sizes_and_no_slope_is_still_refused() {
        // `regress` cannot produce this, so it is a file that was edited or
        // damaged. The guard stays because "believe a slope that is not there"
        // is the one outcome that must not be reachable from a bad file.
        let mut fit = fit_on_the_line(40_000.0, 60_000_000.0, &[128, 256, 512]);
        fit.compute_bytes_per_ubatch = None;
        assert_eq!(
            apply(&fit, &metadata(), ComputeModel::headless()),
            Err(Untrusted::NoSlope)
        );
    }

    #[test]
    fn two_points_are_a_line_rather_than_a_measurement() {
        let fit = fit_on_the_line(40_000.0, 60_000_000.0, &[256, 512]);
        assert_eq!(
            apply(&fit, &metadata(), ComputeModel::headless()),
            Err(Untrusted::TooFewUbatchValues { have: 2 })
        );
    }

    #[test]
    fn repetitions_do_not_turn_two_batch_sizes_into_three_points() {
        // The defect this exists for, and it shipped: the rule counted
        // observations, so the twelve-point two-value fit this machine had on
        // disk sailed through a check that exists to stop exactly that. Six
        // repetitions of each of two batch sizes is still two points.
        let fit = fit_on_the_line(40_000.0, 60_000_000.0, &[128, 128, 128, 512, 512, 512]);
        assert_eq!(fit.points.len(), 6);
        assert_eq!(
            apply(&fit, &metadata(), ComputeModel::headless()),
            Err(Untrusted::TooFewUbatchValues { have: 2 })
        );
    }

    #[test]
    fn a_curve_through_three_batch_sizes_is_not_spent_as_a_line() {
        // Modelled on what this machine actually measured: the segment slopes
        // across 64 -> 128 -> 256 -> 512 were 32,600 then 26,378 then 2,949
        // bytes per ubatch, which is a curve. A line through it scored 0.79.
        let mut fit = fit_on_the_line(40_000.0, 60_000_000.0, &[64, 128, 256, 512]);
        fit.r_squared = Some(0.79);
        assert_eq!(
            apply(&fit, &metadata(), ComputeModel::headless()),
            Err(Untrusted::NotAffine { r_squared: 0.79 })
        );
    }

    #[test]
    fn a_fit_recorded_before_goodness_of_fit_was_is_refused_rather_than_assumed_good() {
        let mut fit = fit_on_the_line(40_000.0, 60_000_000.0, &[128, 256, 512]);
        fit.r_squared = None;
        assert_eq!(
            apply(&fit, &metadata(), ComputeModel::headless()),
            Err(Untrusted::NoGoodnessOfFit)
        );
    }

    #[test]
    fn a_negative_fit_is_a_paging_artefact_rather_than_a_coefficient() {
        let mut fit = fit_on_the_line(40_000.0, 60_000_000.0, &[128, 256, 512]);
        fit.compute_bytes_per_ubatch = Some(-1.0);
        assert_eq!(
            apply(&fit, &metadata(), ComputeModel::headless()),
            Err(Untrusted::NegativeTerm)
        );
        let mut fit = fit_on_the_line(40_000.0, 60_000_000.0, &[128, 256, 512]);
        fit.overhead_bytes = Some(-1.0);
        assert_eq!(
            apply(&fit, &metadata(), ComputeModel::headless()),
            Err(Untrusted::NegativeTerm)
        );
    }

    #[test]
    fn a_slope_below_the_exactly_computed_logits_term_is_refused() {
        // The logits are `vocab * ubatch * 4` and are not fitted. A slope under
        // that describes a run that did not measure what it thought it did.
        let logits_per_ubatch = 10_000.0 * 4.0;
        let fit = fit_on_the_line(logits_per_ubatch - 1.0, 60_000_000.0, &[128, 256, 512]);
        assert_eq!(
            apply(&fit, &metadata(), ComputeModel::headless()),
            Err(Untrusted::SlopeBelowLogits)
        );
    }

    #[test]
    fn a_model_whose_header_is_missing_its_shape_cannot_spend_a_slope() {
        let mut metadata = metadata();
        metadata.vocab_size = None;
        assert_eq!(
            apply(
                &fit_on_the_line(40_000.0, 60_000_000.0, &[128, 256, 512]),
                &metadata,
                ComputeModel::headless()
            ),
            Err(Untrusted::ShapeUnknown)
        );
    }

    #[test]
    fn a_fit_taken_on_another_machine_is_never_spent_here() {
        let mut calibration = Calibration::default();
        calibration.insert(fit_on_the_line(40_000.0, 60_000_000.0, &[128, 256, 512]));

        // The same fit, looked up from the machine it was taken on.
        assert!(
            compute_model_for(
                &calibration,
                &machine(),
                &engine(),
                &metadata(),
                params(),
                ComputeModel::headless(),
            )
            .is_ok()
        );

        let mut elsewhere = machine();
        elsewhere.physical_cores = 16;
        assert_eq!(
            compute_model_for(
                &calibration,
                &elsewhere,
                &engine(),
                &metadata(),
                params(),
                ComputeModel::headless(),
            ),
            Err(Untrusted::NoFit)
        );

        // And the same machine at a context the fit never covered.
        let other_context = RuntimeParams {
            n_ctx: params().n_ctx * 2,
            ..params()
        };
        assert_eq!(
            compute_model_for(
                &calibration,
                &machine(),
                &engine(),
                &metadata(),
                other_context,
                ComputeModel::headless(),
            ),
            Err(Untrusted::NoFit)
        );
    }

    #[test]
    fn an_empty_calibration_leaves_the_shipped_defaults_standing() {
        assert_eq!(
            compute_model_for(
                &Calibration::default(),
                &machine(),
                &engine(),
                &metadata(),
                params(),
                ComputeModel::headless(),
            ),
            Err(Untrusted::NoFit)
        );
    }
}
