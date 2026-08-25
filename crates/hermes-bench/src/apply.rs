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
//! **What a fit may change, and what it may not.** `Prediction::exact()` is
//! weights plus KV cache and `peak_rss` is the *engine* process's, so a
//! residual describes the engine's compute buffers and its baseline. It
//! therefore never touches [`ComputeModel::host_overhead`], which is the
//! desktop shell: no benchmark in this workspace has ever observed that
//! process, and calibrating a term nothing measured is exactly the confident
//! wrong number this design refuses. The exact half is never touched at all.
//!
//! **Why the two compute coefficients move together.** The estimator's compute
//! term is `vocab*ub*4 + activation*ub*embd*4 + scratch*ub*max(embd,ffn)*4`.
//! A fit provides one slope, and `fit.rs` explains why peak RSS cannot separate
//! the last two: they are collinear. So the slope fixes their *sum* and their
//! shipped 8:4 ratio is carried over unchanged. The ratio is inherited, not
//! measured, and nothing here pretends otherwise.

use hermes_core::RuntimeParams;
use hermes_core::units::Bytes;
use hermes_gguf::ModelMetadata;
use hermes_memory::ComputeModel;

use crate::fit::{BucketKey, Calibration, Fit};
use crate::record::{EngineFingerprint, MachineFingerprint};

/// Fewest observations a fit may rest on.
///
/// Two points define a line exactly, with no evidence that the line is the
/// right shape; the third is the first one that can disagree with it.
pub const MIN_POINTS: usize = 3;

/// Why a fit was not used.
///
/// Returned rather than logged and swallowed, because every one of these is
/// something a person running `hermes bench` would want to be told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Untrusted {
    /// No fit for this machine, this engine build and this bucket.
    NoFit,
    /// Fewer than [`MIN_POINTS`] observations.
    TooFewPoints { have: usize },
    /// One ubatch value, so the regression declined to report a slope.
    NoSlope,
    /// A negative slope or intercept: a measurement artefact, not a
    /// coefficient. It happens when the weights were not all resident.
    NegativeTerm,
    /// The model's own geometry is missing, so the slope cannot be spent.
    ShapeUnknown,
    /// The measured slope is below the logits term alone, which is computed
    /// from the vocabulary rather than fitted. The run did not measure what it
    /// thought it did.
    SlopeBelowLogits,
}

impl Untrusted {
    /// A sentence naming what was wrong, for a log line or a CLI note.
    pub fn reason(self) -> String {
        match self {
            Self::NoFit => "no measurement for this machine, engine and settings".to_owned(),
            Self::TooFewPoints { have } => format!(
                "only {have} observation(s); {MIN_POINTS} are needed before a fit is believed"
            ),
            Self::NoSlope => {
                "the run held the batch size fixed, so it has no slope to spend".to_owned()
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
    /// The model to hand [`hermes_memory::Estimator::new`].
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
    if fit.points.len() < MIN_POINTS {
        return Err(Untrusted::TooFewPoints {
            have: fit.points.len(),
        });
    }
    let (Some(slope), Some(intercept)) = (fit.compute_bytes_per_ubatch, fit.overhead_bytes) else {
        return Err(Untrusted::NoSlope);
    };
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
pub fn engine_fingerprint(backend: &dyn hermes_inference::InferenceBackend) -> EngineFingerprint {
    EngineFingerprint {
        backend: backend.id().to_string(),
        // Stated by the backend rather than guessed by the caller.
        build: backend.capabilities().build,
        ggml_variant: Some(
            hermes_system_info::CpuInfo::detect()
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
) -> (hermes_memory::Estimator, Outcome) {
    let calibration = match Calibration::load(calibration_path) {
        Ok(calibration) => calibration,
        Err(err) => {
            return (
                hermes_memory::Estimator::new(base),
                Outcome::Unreadable(err.to_string()),
            );
        }
    };
    let machine = MachineFingerprint::detect();
    match compute_model_for(&calibration, &machine, engine, metadata, params, base) {
        Ok(calibrated) => (
            hermes_memory::Estimator::new(calibrated.compute_model),
            Outcome::Applied(calibrated),
        ),
        Err(untrusted) => (
            hermes_memory::Estimator::new(base),
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
    use hermes_gguf::{QuantMix, TokenizerMeta};
    use hermes_memory::Estimator;

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
            hermes_system_info::MemorySnapshot {
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
            Err(Untrusted::TooFewPoints { have: 2 })
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
