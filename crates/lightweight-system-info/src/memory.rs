//! System memory probing.
//!
//! The RAM estimator compares what a model needs against what the machine has
//! spare. Which number it compares against is the whole ball game, so it is
//! worth being explicit:
//!
//! * **`MemAvailable`, not `MemTotal`.** Total memory includes everything other
//!   processes are already using. A verdict computed against it would cheerfully
//!   approve a load that cannot possibly fit.
//! * **Swap is never counted as headroom.** Spec section 7 says the application
//!   must never intentionally cause heavy swapping, and decode touches
//!   essentially every weight once per token — a model that "fits" only by
//!   swapping would page continuously. Swap is reported to the user as context
//!   and excluded from the budget.
//!
//! `MemAvailable` specifically, rather than `MemFree`, because the kernel's own
//! estimate accounts for reclaimable page cache and is what it would actually
//! hand out. On this machine `MemFree` reads roughly 400 MB while
//! `MemAvailable` reads about 2.3 GB.

use serde::{Deserialize, Serialize};

use lightweight_core::Bytes;
use lightweight_core::{Actionable, ErrorKind, Remedy, RemedyAction};

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("could not read {source_path}: {source}")]
    Read {
        source_path: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("{source_path} did not contain the expected field {field:?}")]
    MissingField {
        source_path: &'static str,
        field: &'static str,
    },

    #[error("memory probing is not implemented for this platform ({platform})")]
    UnsupportedPlatform { platform: &'static str },
}

impl Actionable for MemoryError {
    fn code(&self) -> &'static str {
        match self {
            Self::Read { .. } => "memory_probe_failed",
            Self::MissingField { .. } => "memory_probe_incomplete",
            Self::UnsupportedPlatform { .. } => "memory_probe_unsupported",
        }
    }

    fn kind(&self) -> ErrorKind {
        ErrorKind::Internal
    }

    /// What can actually be done, per failure.
    ///
    /// These once all pointed at "the available-memory override in settings",
    /// a setting that has never existed. Naming a real escape hatch matters
    /// more here than anywhere: without a reading there is no verdict at all,
    /// so the user is not choosing to overrule a refusal - they are choosing
    /// whether to proceed with the check unmade. `--force` says exactly that,
    /// per load, and is recorded.
    ///
    /// An override was considered and rejected: it would let a user feed the
    /// estimator a number nobody measured, which is the confident wrong verdict
    /// the platform stub above refuses to produce.
    fn remedies(&self) -> Vec<Remedy> {
        match self {
            Self::Read { source_path, .. } => vec![Remedy::new(
                format!(
                    "{source_path} could not be read, so no memory verdict is possible; \
                     load with --force to proceed without one"
                ),
                RemedyAction::ForceLoad,
            )],
            Self::MissingField { source_path, field } => vec![Remedy::new(
                format!(
                    "{source_path} did not report {field}, so no memory verdict is possible; \
                     load with --force to proceed without one"
                ),
                RemedyAction::ForceLoad,
            )],
            Self::UnsupportedPlatform { platform } => vec![Remedy::new(
                format!(
                    "memory probing is not implemented on {platform}; \
                     every load must be forced until it is"
                ),
                RemedyAction::ForceLoad,
            )],
        }
    }
}

/// A point-in-time reading of system memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySnapshot {
    pub total: Bytes,
    /// The kernel's estimate of what could be allocated without swapping.
    /// This is the budget the estimator spends.
    pub available: Bytes,
    /// Genuinely unused memory. Reported for the dashboard; not the budget,
    /// because it ignores reclaimable cache and reads far too pessimistically.
    pub free: Bytes,
    pub swap_total: Bytes,
    pub swap_free: Bytes,
}

impl MemorySnapshot {
    /// Memory currently in use by everything on the machine.
    pub fn used(&self) -> Bytes {
        self.total.saturating_sub(self.available)
    }

    /// Swap currently in use.
    ///
    /// Surfaced because non-zero swap usage means the machine is already under
    /// memory pressure, and a load that looks merely tight is likelier to end
    /// in an OOM kill than the numbers alone suggest.
    pub fn swap_used(&self) -> Bytes {
        self.swap_total.saturating_sub(self.swap_free)
    }

    /// Fraction of total memory in use, 0.0 to 1.0.
    pub fn pressure(&self) -> f64 {
        self.used().fraction_of(self.total)
    }
}

/// A source of memory readings.
///
/// A trait so tests can inject fixed numbers. Every verdict the estimator
/// produces depends on this reading, and a test that depended on the real
/// machine's free memory would pass or fail according to what else was running.
pub trait MemoryProbe: Send + Sync {
    fn snapshot(&self) -> Result<MemorySnapshot, MemoryError>;
}

/// Reads the running system's memory.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemMemoryProbe;

impl MemoryProbe for SystemMemoryProbe {
    fn snapshot(&self) -> Result<MemorySnapshot, MemoryError> {
        platform::snapshot()
    }
}

/// Returns whatever it was constructed with.
#[derive(Clone, Copy, Debug)]
pub struct FixedMemoryProbe(pub MemorySnapshot);

impl FixedMemoryProbe {
    /// A machine with `available` free out of `total`, and no swap.
    pub const fn with_available(total: Bytes, available: Bytes) -> Self {
        Self(MemorySnapshot {
            total,
            available,
            free: available,
            swap_total: Bytes::ZERO,
            swap_free: Bytes::ZERO,
        })
    }
}

impl MemoryProbe for FixedMemoryProbe {
    fn snapshot(&self) -> Result<MemorySnapshot, MemoryError> {
        Ok(self.0)
    }
}

/// Fails every time, the way an unreadable `/proc/meminfo` or an unimplemented
/// platform does.
///
/// It exists so the "forced past a failed probe" path is provable on the
/// machine the suite actually runs on. Before this, that path could only be
/// reached on a platform with no probe at all - which is to say it was never
/// tested anywhere, which is how it came to be unreachable in the first place.
#[derive(Clone, Copy, Debug, Default)]
pub struct FailingMemoryProbe;

impl MemoryProbe for FailingMemoryProbe {
    fn snapshot(&self) -> Result<MemorySnapshot, MemoryError> {
        Err(MemoryError::Read {
            source_path: "/proc/meminfo",
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        })
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{Bytes, MemoryError, MemorySnapshot};

    const MEMINFO: &str = "/proc/meminfo";

    pub(super) fn snapshot() -> Result<MemorySnapshot, MemoryError> {
        let contents = std::fs::read_to_string(MEMINFO).map_err(|source| MemoryError::Read {
            source_path: MEMINFO,
            source,
        })?;
        parse_meminfo(&contents)
    }

    /// Parse `/proc/meminfo`.
    ///
    /// Split out from the read so it can be tested against captured files from
    /// several kernels without depending on the machine running the tests.
    pub(super) fn parse_meminfo(contents: &str) -> Result<MemorySnapshot, MemoryError> {
        let field = |name: &'static str| -> Option<Bytes> {
            contents.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                if key.trim() != name {
                    return None;
                }
                // Values are "<number> kB". The unit has been kibibytes since
                // the field was introduced, despite the lowercase spelling.
                let kib: u64 = value.split_whitespace().next()?.parse().ok()?;
                Some(Bytes::from_kib(kib))
            })
        };

        let require = |name: &'static str| {
            field(name).ok_or(MemoryError::MissingField {
                source_path: MEMINFO,
                field: name,
            })
        };

        Ok(MemorySnapshot {
            total: require("MemTotal")?,
            // MemAvailable has been present since Linux 3.14. On anything
            // older, fall back to MemFree - pessimistic, but never optimistic,
            // which is the correct direction to be wrong in.
            available: field("MemAvailable").or_else(|| field("MemFree")).ok_or(
                MemoryError::MissingField {
                    source_path: MEMINFO,
                    field: "MemAvailable",
                },
            )?,
            free: require("MemFree")?,
            swap_total: field("SwapTotal").unwrap_or(Bytes::ZERO),
            swap_free: field("SwapFree").unwrap_or(Bytes::ZERO),
        })
    }
}

#[cfg(any(target_os = "macos", windows))]
mod platform {
    use super::{Bytes, MemoryError, MemorySnapshot};

    /// The same reading, from the platform's own C API.
    ///
    /// What each field means is argued in `lightweight-sys`, next to the call that
    /// produces it; the short version is that `available` is what this machine
    /// could hand out without swapping, which is what the estimator spends, and
    /// that it is derived the same way Activity Monitor derives it on macOS.
    pub(super) fn snapshot() -> Result<MemorySnapshot, MemoryError> {
        let raw = lightweight_sys::memory::read().map_err(|error| MemoryError::Read {
            source_path: error.api,
            source: error.source,
        })?;
        Ok(MemorySnapshot {
            total: Bytes(raw.total),
            available: Bytes(raw.available),
            free: Bytes(raw.free),
            swap_total: Bytes(raw.swap_total),
            swap_free: Bytes(raw.swap_free),
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::{MemoryError, MemorySnapshot};

    /// Not implemented on this platform.
    ///
    /// Deliberately an error rather than a guess. Every admission decision
    /// rests on this number, and an invented one would produce confident,
    /// wrong verdicts - the exact failure mode spec section 7 warns against.
    /// Linux, macOS and Windows are implemented; a fourth platform gets this
    /// until someone can verify a probe on it.
    pub(super) fn snapshot() -> Result<MemorySnapshot, MemoryError> {
        Err(MemoryError::UnsupportedPlatform {
            platform: std::env::consts::OS,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from the development machine.
    /// A real `/proc/meminfo`, so only the Linux parser tests below want it.
    #[cfg(target_os = "linux")]
    const SAMPLE: &str = "\
MemTotal:        8033316 kB
MemFree:          447752 kB
MemAvailable:    2394112 kB
Buffers:           31240 kB
Cached:          2405512 kB
SwapCached:        84120 kB
SwapTotal:       4194300 kB
SwapFree:        2400508 kB
";

    #[test]
    fn every_variant_offers_a_remedy_that_names_something_real() {
        // These all used to be one sentence pointing at an available-memory
        // override in settings, which has never existed. A remedy naming a
        // control the product does not have is worse than no remedy: it sends
        // the user looking for a page that will not be there.
        let failures = [
            MemoryError::Read {
                source_path: "/proc/meminfo",
                source: std::io::Error::other("permission denied"),
            },
            MemoryError::MissingField {
                source_path: "/proc/meminfo",
                field: "MemTotal",
            },
            MemoryError::UnsupportedPlatform {
                platform: "freebsd",
            },
        ];

        let mut labels = Vec::new();
        for failure in &failures {
            let remedies = failure.remedies();
            assert_eq!(remedies.len(), 1, "{failure} offered {remedies:?}");
            let label = remedies[0].label.clone();
            assert!(
                !label.contains("settings"),
                "{failure} still points at a settings page: {label}"
            );
            assert_eq!(remedies[0].action, RemedyAction::ForceLoad);
            labels.push(label);
        }

        labels.sort();
        labels.dedup();
        assert_eq!(
            labels.len(),
            failures.len(),
            "each failure must say what it could not read, not share one sentence"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_a_real_meminfo() {
        let snapshot = platform::parse_meminfo(SAMPLE).expect("parse");
        assert_eq!(snapshot.total, Bytes::from_kib(8_033_316));
        assert_eq!(snapshot.available, Bytes::from_kib(2_394_112));
        assert_eq!(snapshot.free, Bytes::from_kib(447_752));
        assert_eq!(snapshot.swap_total, Bytes::from_kib(4_194_300));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn available_is_far_larger_than_free() {
        // The reason MemAvailable is the budget: MemFree ignores reclaimable
        // page cache and here reads five times too low. Budgeting from it would
        // refuse loads that would have fitted comfortably.
        let snapshot = platform::parse_meminfo(SAMPLE).expect("parse");
        assert!(snapshot.available.get() > snapshot.free.get() * 4);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn falls_back_to_free_when_available_is_absent() {
        // Pre-3.14 kernels. Being pessimistic is acceptable; being optimistic
        // would approve loads that cannot fit.
        let old = "MemTotal: 8033316 kB\nMemFree: 447752 kB\n";
        let snapshot = platform::parse_meminfo(old).expect("parse");
        assert_eq!(snapshot.available, snapshot.free);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_meminfo_without_memtotal_is_an_error_not_a_zero() {
        // A zero total would make every percentage NaN and every verdict
        // meaningless.
        assert!(platform::parse_meminfo("Nonsense: 1 kB\n").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_swap_fields_are_treated_as_no_swap() {
        // Swap is never counted as headroom, so its absence changes nothing.
        let no_swap = "MemTotal: 100 kB\nMemFree: 50 kB\nMemAvailable: 60 kB\n";
        let snapshot = platform::parse_meminfo(no_swap).expect("parse");
        assert_eq!(snapshot.swap_total, Bytes::ZERO);
        assert_eq!(snapshot.swap_used(), Bytes::ZERO);
    }

    #[test]
    fn derived_figures_are_consistent() {
        let snapshot = MemorySnapshot {
            total: Bytes::from_gib(8),
            available: Bytes::from_gib(2),
            free: Bytes::from_mib(400),
            swap_total: Bytes::from_gib(4),
            swap_free: Bytes::from_gib(2),
        };
        assert_eq!(snapshot.used(), Bytes::from_gib(6));
        assert_eq!(snapshot.swap_used(), Bytes::from_gib(2));
        assert!((snapshot.pressure() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn the_fixed_probe_reports_exactly_what_it_was_given() {
        // This is what makes every estimator test deterministic.
        let probe = FixedMemoryProbe::with_available(Bytes::from_gib(16), Bytes::from_gib(9));
        let snapshot = probe.snapshot().expect("fixed probe never fails");
        assert_eq!(snapshot.available, Bytes::from_gib(9));
        assert_eq!(snapshot.total, Bytes::from_gib(16));
    }

    /// The one test that proves a platform probe actually works.
    ///
    /// No longer Linux-only: it is what a macOS or Windows CI runner executes
    /// to show that `lightweight-sys` returned a real reading rather than compiling
    /// and returning nonsense. The assertions are deliberately relationships
    /// rather than magnitudes - a runner's memory is whatever the hypervisor
    /// gave it, and pinning a number here would be fitting the suite to one
    /// machine, which is the thing this project refuses everywhere else.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn the_real_probe_returns_plausible_numbers() {
        let snapshot = SystemMemoryProbe.snapshot().expect("probe this machine");
        assert!(
            snapshot.total.get() > 0,
            "a machine with no memory: {snapshot:?}"
        );
        assert!(snapshot.available <= snapshot.total, "{snapshot:?}");
        assert!(snapshot.free <= snapshot.total, "{snapshot:?}");
        assert!(snapshot.swap_free <= snapshot.swap_total, "{snapshot:?}");
        // `used` and `pressure` are what the dashboard and the estimator read;
        // a probe that satisfied the bounds above but produced a nonsensical
        // pressure would still be broken.
        assert!(snapshot.used() <= snapshot.total, "{snapshot:?}");
        let pressure = snapshot.pressure();
        assert!(
            (0.0..=1.0).contains(&pressure),
            "pressure {pressure} from {snapshot:?}"
        );
    }

    /// The probe the platform actually has, end to end through `CpuInfo`.
    ///
    /// `default_threads` feeds `--threads` to a real engine, so a topology
    /// probe that returned zero or something wilder than the machine has would
    /// misconfigure every load on that platform.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn the_real_topology_probe_agrees_with_itself() {
        let cpu = crate::cpu::CpuInfo::detect();
        assert!(cpu.logical_cores >= 1, "{cpu:?}");
        assert!(cpu.physical_cores >= 1, "{cpu:?}");
        assert!(
            cpu.physical_cores <= cpu.logical_cores,
            "more physical cores than hardware threads: {cpu:?}"
        );
        if let Some(performance) = cpu.performance_cores {
            assert!(performance >= 1, "{cpu:?}");
            assert!(
                performance <= cpu.physical_cores,
                "more performance cores than cores: {cpu:?}"
            );
        }
        assert!(cpu.default_threads() >= 1, "{cpu:?}");
        assert!(cpu.default_threads() <= cpu.logical_cores, "{cpu:?}");
    }
}
