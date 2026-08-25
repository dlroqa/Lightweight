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
//! Nothing here is read by the estimator yet, deliberately. M8 measures and
//! records; the pass that consumes a fit also has to decide when a fit is
//! trustworthy, and that decision is not one a benchmark can make for itself.

use std::collections::BTreeMap;
use std::path::Path;

use hermes_core::units::Bytes;
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
            hermes_store::atomic::create_private_dir(parent).map_err(|source| {
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
        hermes_store::atomic::write_private(path, &bytes).map_err(|source| BenchError::Write {
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
        let Some(residual) = peak.get().checked_sub(prediction.exact()) else {
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
            let (slope, intercept) = regress(&points);
            Fit {
                at_unix: run.at_unix,
                machine: run.machine.clone(),
                engine: run.engine.clone(),
                bucket,
                compute_bytes_per_ubatch: slope,
                overhead_bytes: intercept,
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

/// Least squares of residual against `n_ubatch`.
///
/// `None` for both terms unless at least two *distinct* ubatch values were
/// measured. Repeating one ubatch a hundred times narrows the noise on a single
/// point and says nothing whatever about the slope through it.
fn regress(points: &[ResidualPoint]) -> (Option<f64>, Option<f64>) {
    let distinct: std::collections::BTreeSet<u32> =
        points.iter().map(|point| point.n_ubatch).collect();
    if distinct.len() < 2 {
        return (None, None);
    }

    let n = points.len() as f64;
    let mean_x = points.iter().map(|p| f64::from(p.n_ubatch)).sum::<f64>() / n;
    let mean_y = points.iter().map(|p| p.residual_bytes as f64).sum::<f64>() / n;
    let mut covariance = 0.0;
    let mut variance = 0.0;
    for point in points {
        let dx = f64::from(point.n_ubatch) - mean_x;
        covariance += dx * (point.residual_bytes as f64 - mean_y);
        variance += dx * dx;
    }
    if variance == 0.0 {
        return (None, None);
    }
    let slope = covariance / variance;
    (Some(slope), Some(mean_y - slope * mean_x))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{ModelFingerprint, Prediction, Sample, Scenario};
    use hermes_core::RuntimeParams;
    use hermes_memory::Confidence;

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
