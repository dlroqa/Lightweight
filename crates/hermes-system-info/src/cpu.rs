//! CPU topology and instruction-set detection.
//!
//! Spec section 9 asks for a thread control that knows the difference between
//! physical and logical cores and defaults to physical rather than consuming
//! every hyperthread. Section 10 asks that AVX, AVX2, AVX-512, FMA and NEON are
//! detected, that there is a portable fallback, and that the application never
//! fails merely because an advanced instruction is missing.
//!
//! The fallback requirement is already satisfied by the engine: the shipped
//! llama.cpp build compiles every CPU variant as a separate shared object and
//! picks one at runtime by score. Measured on the development machine — an
//! Intel Pentium Silver J5005 with no AVX at all — `libggml-cpu-sse42.so`
//! scores 5, `libggml-cpu-x64.so` scores 1, and every AVX-and-above variant
//! scores 0. So detection here is for *display and diagnosis*: to tell the user
//! which variant they are getting and why their throughput is what it is, and
//! to turn a SIGILL into an explanation rather than a crash.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// A CPU instruction-set extension relevant to inference throughput.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsaFeature {
    Sse42,
    Avx,
    Avx2,
    Fma,
    F16c,
    Avx512f,
    Avx512Bw,
    Avx512Vnni,
    AvxVnni,
    // AMX is deliberately absent: detecting it requires a nightly-only
    // intrinsic, and the dependency policy rules out nightly. It matters only
    // on Sapphire Rapids server parts, which are not this product's target.
    Neon,
    Dotprod,
    Sve,
}

impl IsaFeature {
    /// The name as it appears in `/proc/cpuinfo` flags, where one exists.
    pub const fn cpuinfo_flag(self) -> &'static str {
        match self {
            Self::Sse42 => "sse4_2",
            Self::Avx => "avx",
            Self::Avx2 => "avx2",
            Self::Fma => "fma",
            Self::F16c => "f16c",
            Self::Avx512f => "avx512f",
            Self::Avx512Bw => "avx512bw",
            Self::Avx512Vnni => "avx512_vnni",
            Self::AvxVnni => "avx_vnni",
            Self::Neon => "asimd",
            Self::Dotprod => "asimddp",
            Self::Sve => "sve",
        }
    }

    /// Display name.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sse42 => "SSE4.2",
            Self::Avx => "AVX",
            Self::Avx2 => "AVX2",
            Self::Fma => "FMA",
            Self::F16c => "F16C",
            Self::Avx512f => "AVX-512F",
            Self::Avx512Bw => "AVX-512BW",
            Self::Avx512Vnni => "AVX-512 VNNI",
            Self::AvxVnni => "AVX VNNI",
            Self::Neon => "NEON",
            Self::Dotprod => "DotProd",
            Self::Sve => "SVE",
        }
    }
}

/// What we know about this machine's processor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuInfo {
    /// Marketing name, when the platform exposes one.
    pub model: Option<String>,
    /// Architecture as Rust names it: `x86_64`, `aarch64`, ...
    pub architecture: &'static str,
    /// Independent execution cores.
    pub physical_cores: u32,
    /// Hardware threads, including SMT siblings.
    pub logical_cores: u32,
    /// Detected instruction-set extensions, sorted.
    pub features: Vec<IsaFeature>,
}

impl CpuInfo {
    /// Probe the current machine.
    pub fn detect() -> Self {
        let logical_cores = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1);

        let (model, physical_from_os) = platform::topology();

        Self {
            model,
            architecture: std::env::consts::ARCH,
            // Fall back to the logical count rather than to 1: over-reporting
            // cores costs some oversubscription, but under-reporting to a
            // single core would make the default thread count useless on every
            // machine where the probe fails.
            physical_cores: physical_from_os.unwrap_or(logical_cores).max(1),
            logical_cores: logical_cores.max(1),
            features: detect_features(),
        }
    }

    pub fn has(&self, feature: IsaFeature) -> bool {
        self.features.contains(&feature)
    }

    /// Whether this CPU has any AVX-family support.
    ///
    /// Worth surfacing on its own: its absence is the single biggest predictor
    /// of low throughput, and users deserve to be told rather than left to
    /// wonder why generation is slow.
    pub fn has_avx_family(&self) -> bool {
        self.has(IsaFeature::Avx) || self.has(IsaFeature::Avx2) || self.has(IsaFeature::Avx512f)
    }

    /// Default thread count for inference.
    ///
    /// Physical cores, per spec section 9. Hyperthread siblings share execution
    /// units, and matrix multiplication already saturates those, so counting
    /// them typically costs throughput rather than adding it. Capped at 1 below.
    pub fn default_threads(&self) -> u32 {
        self.physical_cores.max(1)
    }

    /// Thread counts to offer in the UI: `Auto` plus the sensible explicit
    /// values for this machine, never exceeding its logical core count.
    pub fn thread_choices(&self) -> Vec<u32> {
        let mut choices: BTreeSet<u32> = [1, 2, 4, 6, 8, 12, 16, 24, 32]
            .into_iter()
            .filter(|&n| n <= self.logical_cores)
            .collect();
        choices.insert(self.default_threads());
        choices.insert(self.logical_cores);
        choices.into_iter().collect()
    }

    /// The ggml CPU variant this machine is expected to load.
    ///
    /// Advisory: the engine decides for itself by scoring each variant at
    /// startup, and the truth is reported by the backend once it is running.
    /// This exists so the UI can say something accurate before that happens.
    pub fn expected_ggml_variant(&self) -> &'static str {
        if self.architecture == "aarch64" {
            return if self.has(IsaFeature::Sve) {
                "armv8.2_3"
            } else if self.has(IsaFeature::Dotprod) {
                "armv8.2_1"
            } else {
                "armv8.0_1"
            };
        }
        if self.has(IsaFeature::Avx512f) {
            "skylakex"
        } else if self.has(IsaFeature::Avx2) && self.has(IsaFeature::Fma) {
            "haswell"
        } else if self.has(IsaFeature::Avx) {
            "sandybridge"
        } else if self.has(IsaFeature::Sse42) {
            "sse42"
        } else {
            "x64"
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn detect_features() -> Vec<IsaFeature> {
    // `is_x86_feature_detected!` compiles to a runtime CPUID check, so a binary
    // built anywhere reports what the machine running it actually has. That is
    // the property section 10 needs: one artifact, correct everywhere.
    let mut features = Vec::new();
    let mut check = |enabled: bool, feature: IsaFeature| {
        if enabled {
            features.push(feature);
        }
    };
    check(is_x86_feature_detected!("sse4.2"), IsaFeature::Sse42);
    check(is_x86_feature_detected!("avx"), IsaFeature::Avx);
    check(is_x86_feature_detected!("avx2"), IsaFeature::Avx2);
    check(is_x86_feature_detected!("fma"), IsaFeature::Fma);
    check(is_x86_feature_detected!("f16c"), IsaFeature::F16c);
    check(is_x86_feature_detected!("avx512f"), IsaFeature::Avx512f);
    check(is_x86_feature_detected!("avx512bw"), IsaFeature::Avx512Bw);
    check(
        is_x86_feature_detected!("avx512vnni"),
        IsaFeature::Avx512Vnni,
    );
    check(is_x86_feature_detected!("avxvnni"), IsaFeature::AvxVnni);
    features.sort_unstable();
    features
}

#[cfg(target_arch = "aarch64")]
fn detect_features() -> Vec<IsaFeature> {
    let mut features = Vec::new();
    // NEON is architecturally guaranteed on aarch64, so it is reported
    // unconditionally rather than probed.
    features.push(IsaFeature::Neon);
    if std::arch::is_aarch64_feature_detected!("dotprod") {
        features.push(IsaFeature::Dotprod);
    }
    if std::arch::is_aarch64_feature_detected!("sve") {
        features.push(IsaFeature::Sve);
    }
    features.sort_unstable();
    features
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn detect_features() -> Vec<IsaFeature> {
    // An architecture we have no detection for reports nothing, which is
    // honest. The engine's own runtime dispatch still picks a working variant,
    // so this costs display detail and not function.
    Vec::new()
}

#[cfg(target_os = "linux")]
mod platform {
    /// Read the model name and physical core count from `/proc/cpuinfo`.
    ///
    /// Physical cores are counted as distinct `(physical id, core id)` pairs,
    /// which is what distinguishes real cores from SMT siblings. Machines
    /// without those fields (many VMs, and some ARM kernels) yield `None`, and
    /// the caller falls back to the logical count.
    pub(super) fn topology() -> (Option<String>, Option<u32>) {
        let Ok(contents) = std::fs::read_to_string("/proc/cpuinfo") else {
            return (None, None);
        };

        let mut model = None;
        let mut cores = std::collections::BTreeSet::new();
        let mut physical_id: Option<String> = None;
        let mut core_id: Option<String> = None;

        for line in contents.lines() {
            let Some((key, value)) = line.split_once(':') else {
                // A blank line separates processors; anything pending belongs
                // to the one that just ended.
                if line.trim().is_empty()
                    && let (Some(p), Some(c)) = (physical_id.take(), core_id.take())
                {
                    cores.insert((p, c));
                }
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "model name" if model.is_none() => model = Some(value.to_owned()),
                "physical id" => physical_id = Some(value.to_owned()),
                "core id" => core_id = Some(value.to_owned()),
                _ => {}
            }
        }
        // The final processor block may not be followed by a blank line.
        if let (Some(p), Some(c)) = (physical_id, core_id) {
            cores.insert((p, c));
        }

        let physical = (!cores.is_empty()).then_some(cores.len() as u32);
        (model, physical)
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    /// Topology detection is only implemented for Linux so far.
    ///
    /// Returning `None` rather than a guess means the caller falls back to the
    /// logical core count, which is a safe default everywhere. macOS and
    /// Windows topology is part of the cross-platform milestone; reporting an
    /// unverified number here would be worse than reporting none.
    pub(super) fn topology() -> (Option<String>, Option<u32>) {
        (None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_reports_at_least_one_core() {
        let cpu = CpuInfo::detect();
        assert!(cpu.physical_cores >= 1);
        assert!(cpu.logical_cores >= 1);
        assert!(cpu.logical_cores >= cpu.physical_cores);
    }

    #[test]
    fn default_threads_never_exceeds_the_physical_core_count() {
        // Section 9: default to physical cores rather than every logical thread.
        let cpu = CpuInfo::detect();
        assert_eq!(cpu.default_threads(), cpu.physical_cores);
        assert!(cpu.default_threads() >= 1);
    }

    #[test]
    fn thread_choices_are_sorted_and_bounded_by_the_machine() {
        let cpu = CpuInfo::detect();
        let choices = cpu.thread_choices();
        assert!(!choices.is_empty());
        assert!(
            choices.windows(2).all(|w| w[0] < w[1]),
            "not sorted: {choices:?}"
        );
        assert!(choices.iter().all(|&n| n >= 1 && n <= cpu.logical_cores));
        assert!(choices.contains(&cpu.default_threads()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detected_features_agree_with_proc_cpuinfo() {
        // CPUID and the kernel must not disagree. If they do, one of the two
        // flag names below is wrong, and the UI would be telling the user
        // something false about their machine.
        let Ok(contents) = std::fs::read_to_string("/proc/cpuinfo") else {
            return;
        };
        let Some(flags_line) = contents.lines().find(|l| l.starts_with("flags")) else {
            return;
        };
        let flags: Vec<&str> = flags_line.split_whitespace().collect();

        let cpu = CpuInfo::detect();
        for feature in [
            IsaFeature::Sse42,
            IsaFeature::Avx,
            IsaFeature::Avx2,
            IsaFeature::Fma,
            IsaFeature::F16c,
            IsaFeature::Avx512f,
        ] {
            let in_cpuinfo = flags.contains(&feature.cpuinfo_flag());
            assert_eq!(
                cpu.has(feature),
                in_cpuinfo,
                "{} : CPUID says {}, /proc/cpuinfo says {}",
                feature.label(),
                cpu.has(feature),
                in_cpuinfo
            );
        }
    }

    #[test]
    fn the_expected_variant_is_one_the_engine_actually_ships() {
        // Names must match the `libggml-cpu-<name>.so` files in the release
        // artifact, or the UI would name a variant that does not exist.
        const SHIPPED: &[&str] = &[
            "x64",
            "sse42",
            "sandybridge",
            "ivybridge",
            "piledriver",
            "haswell",
            "skylakex",
            "cannonlake",
            "cascadelake",
            "cooperlake",
            "icelake",
            "zen4",
            "alderlake",
            "sapphirerapids",
            "armv8.0_1",
            "armv8.2_1",
            "armv8.2_3",
        ];
        let variant = CpuInfo::detect().expected_ggml_variant();
        assert!(
            SHIPPED.contains(&variant),
            "{variant} is not a shipped variant"
        );
    }

    #[test]
    fn a_cpu_without_avx_still_maps_to_a_working_variant() {
        // Section 10: never fail merely because an advanced instruction is
        // missing. The development machine is exactly this case.
        let cpu = CpuInfo::detect();
        if !cpu.has_avx_family() && cpu.architecture == "x86_64" {
            assert!(matches!(cpu.expected_ggml_variant(), "sse42" | "x64"));
        }
    }

    #[test]
    fn info_serializes_for_the_dashboard() {
        let json = serde_json::to_value(CpuInfo::detect()).expect("serialize");
        assert!(json["physical_cores"].as_u64().unwrap_or(0) >= 1);
        assert!(json["features"].is_array());
    }
}
