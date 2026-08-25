//! What one benchmark run is, on disk.
//!
//! The shape is chosen for two readers. A person wants to know what this
//! machine does with this model; a later calibration pass wants the residual
//! between what the estimator predicted and what the engine actually took. Both
//! are served by recording *observations plus the exact conditions*, and
//! neither is served by recording a conclusion.
//!
//! Nothing here can hold text a user wrote. The prompts are generated from a
//! fixed pattern, and the fields below have nowhere to put one — the same
//! structural guarantee the metrics module relies on, for the same reason.

use hermes_core::{RuntimeParams, units::Bytes};
use hermes_memory::Confidence;
use serde::{Deserialize, Serialize};

/// The format version of a saved run.
pub const FORMAT_VERSION: u32 = 1;

/// One benchmark, as saved.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchmarkRun {
    pub id: String,
    /// Seconds since the Unix epoch.
    pub at_unix: u64,
    pub machine: MachineFingerprint,
    pub engine: EngineFingerprint,
    pub model: ModelFingerprint,
    pub samples: Vec<Sample>,
}

/// The machine a run was taken on.
///
/// Carried with every run because a throughput figure without it is not a
/// measurement of anything: the same model on the same build is several times
/// faster on a processor with AVX2 than on one without. It is also what stops a
/// fit made here from being applied somewhere it does not describe.
///
/// No hostname and no addresses. What identifies the *hardware* is here; what
/// identifies the *machine on a network* is not, because nothing in a benchmark
/// needs it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineFingerprint {
    pub cpu_model: Option<String>,
    pub physical_cores: u32,
    pub logical_cores: u32,
    /// Instruction sets detected, in the crate's own spelling.
    pub isa_features: Vec<String>,
    pub total_memory: Bytes,
    pub os: String,
    pub architecture: String,
}

impl MachineFingerprint {
    /// Read this machine.
    ///
    /// Total memory rather than available: available changes minute by minute
    /// and would make two runs on one machine look like runs on two. Total is
    /// what the hardware *is*, which is what a fingerprint is for.
    ///
    /// A memory probe that fails yields zero here rather than refusing to
    /// produce a fingerprint: a benchmark whose throughput numbers are perfectly
    /// good should not be thrown away because `/proc/meminfo` was unreadable,
    /// and a zero total is visibly not a reading.
    pub fn detect() -> Self {
        let cpu = hermes_system_info::CpuInfo::detect();
        let total =
            hermes_system_info::MemoryProbe::snapshot(&hermes_system_info::SystemMemoryProbe)
                .map(|snapshot| snapshot.total)
                .unwrap_or(Bytes::ZERO);
        Self {
            cpu_model: cpu.model.clone(),
            physical_cores: cpu.physical_cores,
            logical_cores: cpu.logical_cores,
            isa_features: cpu
                .features
                .iter()
                .map(|feature| feature.cpuinfo_flag().to_owned())
                .collect(),
            total_memory: total,
            os: std::env::consts::OS.to_owned(),
            architecture: cpu.architecture.to_owned(),
        }
    }

    /// Whether two fingerprints describe hardware a result may travel between.
    ///
    /// Deliberately strict. A coefficient fitted on four cores without AVX
    /// describes four cores without AVX; letting it stand in for anything else
    /// is how a calibration file becomes a lie about a machine it never saw.
    pub fn matches(&self, other: &Self) -> bool {
        self.cpu_model == other.cpu_model
            && self.physical_cores == other.physical_cores
            && self.logical_cores == other.logical_cores
            && self.isa_features == other.isa_features
            && self.architecture == other.architecture
            && self.os == other.os
    }
}

/// The engine a run was taken with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineFingerprint {
    /// The backend's own id, so a future engine's numbers are never confused
    /// with llama.cpp's.
    pub backend: String,
    /// The pinned build, when the backend has one.
    pub build: Option<String>,
    /// The ggml CPU variant the engine was expected to dispatch to.
    pub ggml_variant: Option<String>,
}

impl EngineFingerprint {
    pub fn matches(&self, other: &Self) -> bool {
        self.backend == other.backend && self.build == other.build
    }
}

/// The model a run was taken against.
///
/// Identified by what it *is*, never by where it lives: two people with the
/// same model in different directories should be able to compare results, and
/// a path in a shared file is a leak besides.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFingerprint {
    pub id: String,
    pub architecture: String,
    pub quantization: String,
    pub parameters: Option<u64>,
}

/// Which workload a sample came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    /// A prompt the engine has never seen, so every token is prefilled.
    ///
    /// Each repetition is given a distinct opening, because an identical prompt
    /// would be served from the prefix cache and the second repetition would
    /// report a prefill speed that is really a cache hit.
    ColdPrefill,
    /// The same prompt again, measuring what prefix reuse saves.
    ///
    /// On a CPU this is the single largest performance feature there is, and
    /// until now it was observed in passing rather than measured.
    CachedPrefill,
    /// A short prompt and a fixed output budget, measuring generation.
    Decode,
}

impl Scenario {
    pub const ALL: [Self; 3] = [Self::ColdPrefill, Self::CachedPrefill, Self::Decode];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ColdPrefill => "cold_prefill",
            Self::CachedPrefill => "cached_prefill",
            Self::Decode => "decode",
        }
    }
}

/// One measured generation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub scenario: Scenario,
    /// Exactly what the engine was running with, so a later reader never has to
    /// infer the conditions from the numbers.
    pub params: RuntimeParams,
    pub threads: u32,
    pub repetition: u32,
    /// The prompt's length, as the tokenizer counted it before generating.
    ///
    /// Never what was asked for: the sizer aims at a target and records what it
    /// hit.
    pub prompt_tokens: u32,
    /// Tokens the engine served from its prefix cache.
    pub cached_tokens: u32,
    /// Tokens the engine actually evaluated.
    ///
    /// Separate from `prompt_tokens`, and the distinction is not pedantry: on a
    /// fully cached prompt the engine reports having evaluated one token, and
    /// reading that as the prompt's length would report a 256-token prompt as a
    /// 1-token one — and any rate computed from it as 256 times too slow.
    pub prefilled_tokens: u32,
    pub generated_tokens: u32,
    /// The engine's own prefill and decode times, when it reported them.
    pub prefill_ms: Option<f64>,
    pub decode_ms: Option<f64>,
    /// Measured here, because only this side can see it.
    pub time_to_first_token_ms: Option<u64>,
    pub wall_ms: u64,
    /// Processor ticks the engine was charged for during this sample, and the
    /// ticks the whole machine was charged for over the same interval.
    ///
    /// Kept as the two raw counters rather than as a ratio: the ratio is
    /// [`Sample::cores_used`], and a reader who wants a different one is not
    /// stuck with ours.
    pub engine_ticks: Option<u64>,
    pub machine_ticks: Option<u64>,
    pub rss: Option<Bytes>,
    /// The high-water mark, which is what an OOM kill would have been measured
    /// against and what a calibration pass fits.
    pub peak_rss: Option<Bytes>,
    /// What the estimator predicted for these exact parameters.
    ///
    /// `weights` and `kv_cache` are exact; the residual between `peak_rss` and
    /// their sum is what `compute` and `overhead` are guessing at, and is the
    /// whole reason this is recorded beside the observation.
    pub predicted: Option<Prediction>,
}

impl Sample {
    /// Mean cores the engine kept busy during this sample.
    ///
    /// A ratio of two tick counts. `/proc/stat`'s total is summed over every
    /// core, so dividing it by the core count gives what one core could have
    /// spent over the same interval — which makes this a count of cores with
    /// the tick rate cancelled out rather than assumed.
    ///
    /// `None` when either counter is missing or the interval is empty. Never
    /// zero, which would claim an engine idled through a generation it served.
    pub fn cores_used(&self, logical_cores: u32) -> Option<f64> {
        let engine = self.engine_ticks?;
        let machine = self.machine_ticks?;
        if machine == 0 || logical_cores == 0 {
            return None;
        }
        Some(engine as f64 / (machine as f64 / f64::from(logical_cores)))
    }

    /// Prompt tokens processed per second, from the engine's own timing.
    ///
    /// Counts only tokens that were actually prefilled: a cached prefix was not
    /// processed, and including it would report a prefill rate that climbs with
    /// cache hits rather than with speed.
    pub fn prefill_tokens_per_second(&self) -> Option<f64> {
        let ms = self.prefill_ms?;
        // The engine's own count of what it evaluated. A cached prefix was not
        // processed, and including it would report a prefill rate that climbs
        // with cache hits rather than with speed.
        (ms > 0.0 && self.prefilled_tokens > 0)
            .then(|| f64::from(self.prefilled_tokens) * 1000.0 / ms)
    }

    /// Tokens generated per second, from the engine's own timing.
    ///
    /// `None` for a scenario that generated a single token: one token against a
    /// sub-millisecond timing produces a rate in the millions, which is an
    /// artefact of the measurement rather than a property of the machine.
    pub fn decode_tokens_per_second(&self) -> Option<f64> {
        let ms = self.decode_ms?;
        (ms > 0.0 && self.generated_tokens > 1)
            .then(|| f64::from(self.generated_tokens) * 1000.0 / ms)
    }
}

/// What the estimator said this load would cost, taken at the same moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prediction {
    pub weights: Bytes,
    pub kv_cache: Bytes,
    pub compute: Bytes,
    pub overhead: Bytes,
    pub total: Bytes,
    /// The estimator's own word for how much of this was measured.
    ///
    /// The estimator's type rather than a string: a benchmark whose confidence
    /// no longer parses is a benchmark that cannot be compared with a live
    /// estimate, and a rename should break here rather than quietly widen the
    /// set of values a later pass has to guess at.
    pub confidence: Confidence,
}

impl Prediction {
    /// The part of the prediction that was computed exactly.
    ///
    /// Weights come from tensor shapes and the KV cache from per-layer
    /// geometry; neither is fitted, and a calibration pass must not try to
    /// re-fit them.
    pub fn exact(&self) -> u64 {
        self.weights.get().saturating_add(self.kv_cache.get())
    }
}
