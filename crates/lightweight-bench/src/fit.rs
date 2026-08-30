//! Turning observations into coefficients — and refusing to turn them into
//! more coefficients than they support.
//!
//! The estimator computes four terms. Two of them, weights and the KV cache,
//! are exact: they come from tensor shapes and per-layer ggml geometry, and
//! nothing here may touch them. The other two, compute buffers and overhead,
//! cannot be derived from metadata at all and ship as conservative guesses. The
//! residual between an observed peak RSS and the exact half is what those two
//! are between them getting wrong, and that residual is what this module fits.
//!
//! **What the data supports, and what it does not.** The estimator's compute
//! term is `vocab*ub*4 + activation*ub*embd*4 + scratch*ub*max(embd,ffn)*4` —
//! two free coefficients, both proportional to `n_ubatch`. From peak RSS alone
//! they are collinear: no number of samples at one ubatch can separate them,
//! and samples across several ubatch values determine only their *sum*. So this
//! module fits a slope and an intercept, and says so, rather than reporting two
//! coefficients of which one is invented. Deciding how to spend that slope is
//! the calibration milestone's business, not this one's.
//!
//! This module measures and records; it does not decide. Whether a recorded fit
//! is worth believing is `apply.rs`'s question, and the two are kept apart
//! because a benchmark cannot know how far its own numbers should travel. What
//! is recorded here is therefore raw: every observation is kept, including the
//! ones the trust rules will refuse to spend.

use std::collections::BTreeMap;
use std::path::Path;

use lightweight_core::units::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::BenchError;
use crate::record::{BenchmarkRun, EngineFingerprint, MachineFingerprint};

pub const FORMAT_VERSION: u32 = 1;

/// The fitted file.
///
/// Several fits coexist, keyed by the machine and engine they describe, so
/// moving a data directory between machines accumulates coverage instead of
/// overwriting it — and so a fit taken on one processor is never silently
/// applied to another.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Calibration {
    #[serde(default)]
    pub fits: Vec<Fit>,
    /// Anything a newer build wrote that this one does not understand.
    ///
    /// Preserved across a write for the reason `SettingsStore` preserves it: an
    /// older binary running once must not delete a newer one's work.
    #[serde(flatten, default)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

/// One machine, one engine build, one model bucket.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fit {
    pub at_unix: u64,
    pub machine: MachineFingerprint,
    pub engine: EngineFingerprint,
    pub bucket: BucketKey,
    /// Peak RSS beyond weights and KV cache, per unit of `n_ubatch`.
    ///
    /// `None` when the run held `n_ubatch` fixed: one point cannot have a
    /// slope, and reporting one anyway would be the whole failure this crate
    /// exists to avoid.
    pub compute_bytes_per_ubatch: Option<f64>,
    /// Peak RSS beyond weights, KV cache and compute. The regression's
    /// intercept, and `None` under the same condition.
    pub overhead_bytes: Option<f64>,
    /// The largest residual actually seen.
    ///
    /// Always available, even from a single point, and always a safe floor: an
    /// estimator that budgeted this much for compute and overhead together
    /// would have been right about every sample here.
    pub max_residual_bytes: u64,
    /// How much of the spread across batch sizes the fitted line accounts for.
    ///
    /// `None` under the same condition as the two terms above. Recorded rather
    /// than judged here: what counts as good enough is policy, and policy lives
    /// in `apply.rs`. Note that two distinct batch sizes always score 1.0 — a
    /// line through two points passes through both — so this number only starts
    /// carrying information at the third, which is why `apply` counts the batch
    /// sizes before it reads this.
    ///
    /// Absent from files written before this field existed, and `None` there.
    #[serde(default)]
    pub r_squared: Option<f64>,
    /// The observations behind the numbers above, kept so a later pass can fit
    /// them differently without re-running the benchmark.
    pub points: Vec<ResidualPoint>,
}

/// What identifies a bucket: the model's geometry and the knobs that change it.
///
/// The machine is deliberately not part of this — it is on the [`Fit`] — because
/// these terms describe the *model*, and a later pass may reasonably decide they
/// travel between machines where the host terms never do.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BucketKey {
    pub architecture: String,
    pub quantization: String,
    pub n_ctx: u32,
    pub n_parallel: u32,
    pub cache_type_k: String,
    pub cache_type_v: String,
}

/// One observation: what an ubatch of this size actually cost beyond the exact
/// terms.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResidualPoint {
    pub n_ubatch: u32,
    pub residual_bytes: u64,
    pub peak_rss: Bytes,
}

impl Calibration {
    /// Read a calibration file, or an empty one when none has been written.
    ///
    /// A file that will not parse is an error rather than a silent reset, the
    /// same rule `SettingsStore` follows: treating unreadable calibration as
    /// "no calibration" answers a corrupt file by overwriting it.
    pub fn load(path: &Path) -> Result<Self, BenchError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(BenchError::Read {
                    detail: err.to_string(),
                });
            }
        };
        let file: CalibrationFile =
            serde_json::from_slice(&bytes).map_err(|err| BenchError::Read {
                detail: err.to_string(),
            })?;
        Ok(file.calibration)
    }

    /// Write it, atomically and owner-only.
    pub fn save(&self, path: &Path) -> Result<(), BenchError> {
        if let Some(parent) = path.parent() {
            lightweight_store::atomic::create_private_dir(parent).map_err(|source| {
                BenchError::Write {
                    path: parent.display().to_string(),
                    source,
                }
            })?;
        }
        let file = CalibrationFile {
            version: FORMAT_VERSION,
            calibration: self.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&file).map_err(|err| BenchError::Encode {
            detail: err.to_string(),
        })?;
        bytes.push(b'\n');
        lightweight_store::atomic::write_private(path, &bytes).map_err(|source| BenchError::Write {
            path: path.display().to_string(),
            source,
        })
    }

    /// The fit describing this machine, this engine and this bucket.
    ///
    /// A mismatch is a miss, never an approximation. A coefficient fitted on
    /// four cores without AVX describes four cores without AVX, and letting it
    /// stand in for a machine it never saw is how a calibration file becomes a
    /// confident wrong number — the one thing the estimator refuses to be.
    pub fn find(
        &self,
        machine: &MachineFingerprint,
        engine: &EngineFingerprint,
        bucket: &BucketKey,
    ) -> Option<&Fit> {
        self.fits.iter().find(|fit| {
            fit.machine.matches(machine) && fit.engine.matches(engine) && &fit.bucket == bucket
        })
    }

    /// Add a fit, replacing any earlier one for the same machine, engine and
    /// bucket.
    pub fn insert(&mut self, fit: Fit) {
        self.fits.retain(|existing| {
            !(existing.machine.matches(&fit.machine)
                && existing.engine.matches(&fit.engine)
                && existing.bucket == fit.bucket)
        });
        self.fits.push(fit);
    }
}

#[derive(Serialize, Deserialize)]
struct CalibrationFile {
    version: u32,
    #[serde(flatten)]
    calibration: Calibration,
}

/// Fit every bucket a run covers.
///
/// Samples without a prediction are skipped: without the exact half there is no
/// residual to compute, only a peak RSS with nothing to compare it to.
pub fn fit_run(run: &BenchmarkRun) -> Vec<Fit> {
    let mut buckets: BTreeMap<BucketKey, Vec<ResidualPoint>> = BTreeMap::new();

    for sample in &run.samples {
        let (Some(prediction), Some(peak)) = (sample.predicted, sample.peak_rss) else {
            continue;
        };
        // The exact term this peak actually contains, which is not the same on
        // every platform: a macOS footprint excludes the mapped weights, and
        // subtracting them anyway underflowed and silently threw the sample
        // away - which is why `hermes bench --fit` could fit nothing there.
        let exact = prediction.exact_within(sample.peak_kind, sample.params);
        let Some(residual) = peak.get().checked_sub(exact) else {
            // The engine used less than the exactly-computed half. That is not
            // a residual, it is a sign the weights were not all resident - a
            // partially paged mmap, most likely - and fitting compute buffers
            // to it would produce a negative coefficient.
            continue;
        };
        let key = BucketKey {
            architecture: run.model.architecture.clone(),
            quantization: run.model.quantization.clone(),
            n_ctx: sample.params.n_ctx,
            n_parallel: sample.params.n_parallel,
            cache_type_k: sample.params.cache_type_k.name().to_owned(),
            cache_type_v: sample.params.cache_type_v.name().to_owned(),
        };
        buckets.entry(key).or_default().push(ResidualPoint {
            n_ubatch: sample.params.n_ubatch,
            residual_bytes: residual,
            peak_rss: peak,
        });
    }

    buckets
        .into_iter()
        .map(|(bucket, points)| {
            let line = regress(&points);
            Fit {
                at_unix: run.at_unix,
                machine: run.machine.clone(),
                engine: run.engine.clone(),
                bucket,
                compute_bytes_per_ubatch: line.map(|line| line.slope),
                overhead_bytes: line.map(|line| line.intercept),
                r_squared: line.map(|line| line.r_squared),
                max_residual_bytes: points
                    .iter()
                    .map(|point| point.residual_bytes)
                    .max()
                    .unwrap_or_default(),
                points,
            }
        })
        .collect()
}

/// A straight line through the residuals, and how well it describes them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineFit {
    /// Bytes of residual per unit of `n_ubatch`.
    pub slope: f64,
    /// Residual at `n_ubatch` zero.
    pub intercept: f64,
    /// The fraction of the spread across batch sizes the line accounts for.
    pub r_squared: f64,
}

/// Least squares of residual against `n_ubatch`, **one point per batch size**.
///
/// *The samples inside a bucket are not independent observations.* `peak_rss`
/// is a high-water mark and every sample in a bucket is read from one engine
/// process, so the readings only ever climb. Measured on this machine
/// (2026-08-25, runs `18cf2bfb07298264` and `18cf2c1c92bd4be4`): a bucket's six
/// samples spread 17.5 MiB from the first to the last, and that spread was
/// within 0.1 MiB of the same value in all eight buckets swept — four batch
/// sizes and three contexts — while the entire effect of an eightfold change in
/// `n_ubatch` was 6 MiB. Regressing the raw samples therefore fits the order
/// they happened to be taken in far more than the quantity of interest: R² came
/// out at 0.10 over a four-value sweep whose slope is otherwise stable to
/// within 1.4% however it is aggregated.
///
/// So each batch size contributes one point, its **largest** residual. That is
/// both the independent observation and the right one to budget: the peak a
/// configuration reached is what an estimator has to cover, and the mean over a
/// ratchet is a number no run ever recorded.
///
/// `None` unless at least two *distinct* ubatch values were measured. Repeating
/// one ubatch a hundred times narrows the noise on a single point and says
/// nothing whatever about the slope through it.
fn regress(points: &[ResidualPoint]) -> Option<LineFit> {
    let mut peaks: BTreeMap<u32, u64> = BTreeMap::new();
    for point in points {
        let peak = peaks.entry(point.n_ubatch).or_default();
        *peak = (*peak).max(point.residual_bytes);
    }
    if peaks.len() < 2 {
        return None;
    }

    let n = peaks.len() as f64;
    let mean_x = peaks.keys().map(|ubatch| f64::from(*ubatch)).sum::<f64>() / n;
    let mean_y = peaks.values().map(|bytes| *bytes as f64).sum::<f64>() / n;
    let mut covariance = 0.0;
    let mut variance = 0.0;
    for (ubatch, residual) in &peaks {
        let dx = f64::from(*ubatch) - mean_x;
        covariance += dx * (*residual as f64 - mean_y);
        variance += dx * dx;
    }
    if variance == 0.0 {
        return None;
    }
    let slope = covariance / variance;
    let intercept = mean_y - slope * mean_x;

    let residual_sum: f64 = peaks
        .iter()
        .map(|(ubatch, bytes)| {
            let error = *bytes as f64 - (slope * f64::from(*ubatch) + intercept);
            error * error
        })
        .sum();
    let total_sum: f64 = peaks
        .values()
        .map(|bytes| {
            let deviation = *bytes as f64 - mean_y;
            deviation * deviation
        })
        .sum();
    // Every point identical is a flat line that describes them perfectly. The
    // usual ratio is 0/0 there, and calling that "explains nothing" would refuse
    // the one case where the line is exactly right. A zero slope is refused
    // later and for its own reason.
    let r_squared = if total_sum == 0.0 {
        1.0
    } else {
        1.0 - residual_sum / total_sum
    };

    Some(LineFit {
        slope,
        intercept,
        r_squared,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{ModelFingerprint, Prediction, Sample, Scenario};
    use lightweight_core::RuntimeParams;
    use lightweight_inference::PeakKind;
    use lightweight_memory::Confidence;

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

    /// A sample whose peak RSS is exactly `exact + intercept + slope*ubatch`.
    fn sample(n_ubatch: u32, exact: u64, intercept: u64, slope: u64) -> Sample {
        let params = RuntimeParams {
            n_ubatch,
            ..RuntimeParams::default()
        };
        Sample {
            scenario: Scenario::Decode,
            params,
            threads: 4,
            repetition: 0,
            prompt_tokens: 10,
            cached_tokens: 0,
            prefilled_tokens: 10,
            generated_tokens: 10,
            prefill_ms: Some(100.0),
            decode_ms: Some(1_000.0),
            time_to_first_token_ms: Some(120),
            wall_ms: 1_200,
            engine_ticks: Some(100),
            machine_ticks: Some(400),
            rss: Some(Bytes(exact)),
            peak_rss: Some(Bytes(exact + intercept + slope * u64::from(n_ubatch))),
            peak_kind: PeakKind::ResidentSet,
            predicted: Some(Prediction {
                weights: Bytes(exact),
                kv_cache: Bytes(0),
                compute: Bytes::from_mib(64),
                overhead: Bytes::from_mib(48),
                total: Bytes(exact + Bytes::from_mib(112).get()),
                confidence: Confidence::Coarse,
            }),
            busy_slots_per_decode: None,
        }
    }

    fn run(samples: Vec<Sample>) -> BenchmarkRun {
        BenchmarkRun {
            id: "0000000000000001".to_owned(),
            at_unix: 1_700_000_000,
            machine: machine(),
            engine: engine(),
            model: ModelFingerprint {
                id: "a-model".to_owned(),
                architecture: "llama".to_owned(),
                quantization: "Q4_K_M".to_owned(),
                parameters: Some(135_000_000),
            },
            samples,
        }
    }

    /// A footprint peak is fitted against the term it actually contains.
    ///
    /// The defect this exists for is silent: subtracting weights a macOS
    /// footprint never counted underflows, `fit_run` skips the sample, and
    /// `hermes bench --fit` reports a run with no fits in it and no reason why.
    #[test]
    fn a_footprint_peak_is_not_asked_to_contain_the_weights() {
        let weights = Bytes::from_mib(700).get();
        let kv = Bytes::from_mib(200).get();
        let residual = Bytes::from_mib(150).get();

        // What macOS would record: the peak holds the KV cache and the
        // residual, and not the mapped weights.
        let mut sample = sample(512, weights, 0, 0);
        sample.peak_kind = PeakKind::Footprint;
        sample.peak_rss = Some(Bytes(kv + residual));
        sample.predicted = Some(Prediction {
            weights: Bytes(weights),
            kv_cache: Bytes(kv),
            compute: Bytes::ZERO,
            overhead: Bytes::ZERO,
            total: Bytes(weights + kv),
            confidence: Confidence::Coarse,
        });

        let fits = fit_run(&run(vec![sample]));
        assert_eq!(fits.len(), 1, "the sample must not be thrown away");
        assert_eq!(
            fits[0].max_residual_bytes, residual,
            "the residual must be the peak minus the KV cache alone"
        );
    }

    /// Unless the load locked its weights, which puts them in the footprint.
    #[test]
    fn a_locked_load_puts_the_weights_back_inside_a_footprint() {
        let weights = Bytes::from_mib(700).get();
        let kv = Bytes::from_mib(200).get();
        let residual = Bytes::from_mib(150).get();

        let mut sample = sample(512, weights, 0, 0);
        sample.peak_kind = PeakKind::Footprint;
        sample.params.load_mode = Some(lightweight_core::LoadMode::MmapMlock);
        // Locked weights are wired and charged to the process, so they are in
        // the peak - and the residual is what is left after all of it.
        sample.peak_rss = Some(Bytes(weights + kv + residual));
        sample.predicted = Some(Prediction {
            weights: Bytes(weights),
            kv_cache: Bytes(kv),
            compute: Bytes::ZERO,
            overhead: Bytes::ZERO,
            total: Bytes(weights + kv),
            confidence: Confidence::Coarse,
        });

        let fits = fit_run(&run(vec![sample]));
        assert_eq!(fits.len(), 1);
        assert_eq!(fits[0].max_residual_bytes, residual);
    }

    /// A run recorded before the field existed is what it always was.
    #[test]
    fn a_run_from_before_this_field_reads_as_a_resident_set_peak() {
        // Every recording made until now was taken on Linux, where the peak is
        // `VmHWM`. Defaulting to anything else would silently re-interpret
        // every benchmark already on disk.
        let stored = serde_json::json!({
            "scenario": "decode",
            "params": lightweight_core::RuntimeParams::default(),
            "threads": 4,
            "repetition": 0,
            "prompt_tokens": 10,
            "cached_tokens": 0,
            "prefilled_tokens": 10,
            "generated_tokens": 10,
            "wall_ms": 1_200,
            "peak_rss": 1_000,
        });
        let sample: Sample = serde_json::from_value(stored).expect("an older sample still parses");
        assert_eq!(sample.peak_kind, PeakKind::ResidentSet);
    }

    #[test]
    fn a_sweep_over_ubatch_recovers_the_slope_and_the_intercept() {
        let exact = Bytes::from_mib(500).get();
        let intercept = Bytes::from_mib(300).get();
        let slope = 4_096;
        let fits = fit_run(&run(vec![
            sample(128, exact, intercept, slope),
            sample(256, exact, intercept, slope),
            sample(512, exact, intercept, slope),
        ]));

        assert_eq!(fits.len(), 1);
        let fit = &fits[0];
        let fitted_slope = fit.compute_bytes_per_ubatch.expect("a slope");
        let fitted_intercept = fit.overhead_bytes.expect("an intercept");
        assert!(
            (fitted_slope - slope as f64).abs() < 1.0,
            "slope was {fitted_slope}"
        );
        assert!(
            (fitted_intercept - intercept as f64).abs() < 1.0,
            "intercept was {fitted_intercept}"
        );
    }

    #[test]
    fn a_bucket_is_fitted_at_its_peak_rather_than_through_its_climb() {
        // The measurement behind this: `peak_rss` only ever climbs within one
        // engine process, so a bucket's samples are one point read at several
        // moments. On this machine that climb was 17.5 MiB per bucket while the
        // whole ubatch effect across 64..512 was 6 MiB - fitting the raw
        // samples fits the order they were taken in.
        //
        // Here each batch size is measured three times, climbing by 10 MiB, and
        // the true relationship is 4096 bytes per ubatch on top of 300 MiB. The
        // line must come from the peaks, so the climb changes the intercept and
        // leaves the slope alone.
        let exact = Bytes::from_mib(500).get();
        let base = Bytes::from_mib(300).get();
        let climb = Bytes::from_mib(10).get();
        let mut samples = Vec::new();
        for ubatch in [128_u32, 256, 512] {
            for step in 0..3 {
                samples.push(sample(ubatch, exact, base + step * climb, 4_096));
            }
        }
        let fits = fit_run(&run(samples));

        let fit = &fits[0];
        let slope = fit.compute_bytes_per_ubatch.expect("a slope");
        let intercept = fit.overhead_bytes.expect("an intercept");
        assert!((slope - 4_096.0).abs() < 1.0, "slope was {slope}");
        assert!(
            (intercept - (base + 2 * climb) as f64).abs() < 1.0,
            "the intercept must sit at the peak of the climb, not its mean; it was {intercept}"
        );
        assert_eq!(
            fit.r_squared,
            Some(1.0),
            "three peaks exactly on a line describe it perfectly"
        );
        assert_eq!(fit.points.len(), 9, "every observation is still recorded");
    }

    #[test]
    fn a_bend_across_batch_sizes_is_recorded_as_a_worse_fit() {
        // The shape this machine measured: most of the growth between the small
        // batch sizes and almost none between the large ones. The numbers are
        // recorded; refusing to spend them is `apply`'s decision, not this
        // module's.
        let exact = Bytes::from_mib(500).get();
        let fits = fit_run(&run(vec![
            sample(64, exact, Bytes::from_mib(300).get(), 0),
            sample(128, exact, Bytes::from_mib(302).get(), 0),
            sample(256, exact, Bytes::from_mib(305).get(), 0),
            sample(512, exact, Bytes::from_mib(306).get(), 0),
        ]));

        let r_squared = fits[0].r_squared.expect("a goodness of fit");
        assert!(
            (0.7..0.9).contains(&r_squared),
            "a bend this shape scored {r_squared} on this machine's own data, which was 0.79"
        );
        assert!(r_squared < crate::apply::MIN_R_SQUARED, "and it is refused");
    }

    #[test]
    fn one_ubatch_value_yields_no_slope_however_many_times_it_was_repeated() {
        // The failure this guards: a hundred repetitions at one ubatch look
        // like a hundred samples and are one point. A line through one point is
        // whatever you want it to be.
        let exact = Bytes::from_mib(500).get();
        let samples = (0..100)
            .map(|_| sample(512, exact, Bytes::from_mib(300).get(), 4_096))
            .collect();
        let fits = fit_run(&run(samples));

        let fit = &fits[0];
        assert_eq!(fit.compute_bytes_per_ubatch, None);
        assert_eq!(fit.overhead_bytes, None);
        // What one point does support is a floor, and that is still recorded.
        assert_eq!(
            fit.max_residual_bytes,
            Bytes::from_mib(300).get() + 4_096 * 512
        );
        assert_eq!(fit.points.len(), 100);
    }

    #[test]
    fn a_peak_below_the_exact_terms_is_discarded_rather_than_fitted_negative() {
        let exact = Bytes::from_mib(500).get();
        let mut low = sample(512, exact, 0, 0);
        low.peak_rss = Some(Bytes::from_mib(100));
        assert!(fit_run(&run(vec![low])).is_empty());
    }

    #[test]
    fn a_fit_is_not_found_for_a_machine_it_never_saw() {
        let fits = fit_run(&run(vec![
            sample(
                128,
                Bytes::from_mib(500).get(),
                Bytes::from_mib(300).get(),
                4_096,
            ),
            sample(
                512,
                Bytes::from_mib(500).get(),
                Bytes::from_mib(300).get(),
                4_096,
            ),
        ]));
        let mut calibration = Calibration::default();
        let bucket = fits[0].bucket.clone();
        calibration.insert(fits[0].clone());

        assert!(calibration.find(&machine(), &engine(), &bucket).is_some());

        let mut elsewhere = machine();
        elsewhere.isa_features.push("avx2".to_owned());
        assert!(
            calibration.find(&elsewhere, &engine(), &bucket).is_none(),
            "a fit from a machine without AVX2 must not describe one with it"
        );

        let mut newer = engine();
        newer.build = Some("b99999".to_owned());
        assert!(
            calibration.find(&machine(), &newer, &bucket).is_none(),
            "a fit is a fit of one engine build"
        );

        // The variant was recorded and not compared until an audit asked why.
        // The machine fingerprint pins the ISA features it is derived from, so
        // this is usually implied - but "usually implied" is not what the rest
        // of this type promises, and the mapping from features to a variant
        // belongs to the build doing the mapping.
        let mut dispatched_elsewhere = engine();
        dispatched_elsewhere.ggml_variant = Some("avx2".to_owned());
        assert!(
            calibration
                .find(&machine(), &dispatched_elsewhere, &bucket)
                .is_none(),
            "a fit describes the code the engine actually dispatched to"
        );

        let mut unstated = engine();
        unstated.ggml_variant = None;
        assert!(
            calibration.find(&machine(), &unstated, &bucket).is_none(),
            "an engine that did not say which variant it ran is not a match for one that did"
        );
    }

    #[test]
    fn a_newer_builds_keys_survive_an_older_builds_write() {
        let directory = std::env::temp_dir().join(format!(
            "hermes-fit-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&directory).expect("scratch");
        let path = directory.join("calibration.json");

        std::fs::write(
            &path,
            br#"{"version":1,"fits":[],"something_a_later_build_added":{"keep":"me"}}"#,
        )
        .expect("seed");

        let calibration = Calibration::load(&path).expect("load");
        assert!(
            calibration
                .unknown
                .contains_key("something_a_later_build_added")
        );
        calibration.save(&path).expect("save");

        let written = std::fs::read_to_string(&path).expect("read back");
        assert!(
            written.contains("something_a_later_build_added"),
            "{written}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }
}
