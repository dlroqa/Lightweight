//! How busy the processor has been.
//!
//! Separate from [`crate::cpu`] on purpose. That module describes what the
//! machine *is* — cores, instruction sets, the ggml variant it will load — and
//! every answer it gives is true whenever it is asked. This module reports a
//! *rate*, and a rate cannot be read at an instant: `/proc/stat` publishes
//! monotonically increasing counters, and a single reading of a counter says
//! nothing about how fast it is climbing.
//!
//! So nothing here computes a percentage from one sample, and nothing here
//! keeps a previous sample in order to pretend it can. A caller takes two
//! readings and asks for the utilization between them, which is the same
//! discipline the gateway's metrics already impose on the panel's charts: the
//! server publishes counters, the client differences them, and the number that
//! results means something exact — "between these two instants" — rather than
//! "recently".
//!
//! The alternative was a background sampler holding a rolling average. It was
//! rejected for what it would have to invent: a sampling interval nobody asked
//! for, a first reading that is either absent or a lie, and state to own and
//! test for the benefit of one tile on one screen.

use serde::{Deserialize, Serialize};

use hermes_core::{Actionable, ErrorKind, Remedy, RemedyAction, SettingsSection};

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("could not read {source_path}: {source}")]
    Read {
        source_path: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("{source_path} had no aggregate `cpu` line")]
    NoAggregateLine { source_path: &'static str },

    #[error("processor time accounting is not implemented for this platform ({platform})")]
    UnsupportedPlatform { platform: &'static str },
}

impl Actionable for LoadError {
    fn code(&self) -> &'static str {
        match self {
            Self::Read { .. } => "cpu_times_probe_failed",
            Self::NoAggregateLine { .. } => "cpu_times_probe_incomplete",
            Self::UnsupportedPlatform { .. } => "cpu_times_probe_unsupported",
        }
    }

    fn kind(&self) -> ErrorKind {
        ErrorKind::Internal
    }

    fn remedies(&self) -> Vec<Remedy> {
        vec![Remedy::new(
            "Read processor load from your own tools (`top`, `htop`); it is \
             reported for display only and no decision depends on it",
            RemedyAction::OpenSettings {
                section: SettingsSection::Logging,
            },
        )]
    }
}

/// Cumulative processor time since boot, summed over every core.
///
/// Both figures are in kernel clock ticks — jiffies, `USER_HZ`, normally 100
/// per second. They are published unconverted because every use is a ratio of
/// two readings, and a ratio does not care about the unit. Converting to
/// seconds here would divide by a constant this crate would have to guess at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuTimes {
    /// Every accounted tick: busy and idle together.
    pub total: u64,
    /// Ticks spent idle, including time blocked on I/O.
    ///
    /// `iowait` is counted as idle because the processor genuinely was not
    /// executing during it. A machine stalled on a slow disk is not busy, and
    /// reporting it as busy would send someone hunting for a CPU problem that
    /// is really a storage one.
    pub idle: u64,
}

impl CpuTimes {
    /// Ticks spent doing work.
    pub const fn busy(&self) -> u64 {
        self.total.saturating_sub(self.idle)
    }

    /// Fraction of the interval between `earlier` and `self` spent busy.
    ///
    /// `None` rather than zero in all three cases where the question has no
    /// answer, because a dashboard showing 0% for "I cannot tell" is how an
    /// overloaded machine comes to look idle:
    ///
    /// * **No time passed.** Two readings from the same tick have nothing
    ///   between them to apportion.
    /// * **The counters went backwards.** They are monotonic while the kernel
    ///   is up; across a suspend, a CPU going offline, or two samples taken from
    ///   different containers, they need not be. That is not a measurement.
    /// * **`earlier` is not earlier.** Caller error, reported rather than
    ///   silently reordered.
    pub fn utilization_since(&self, earlier: &Self) -> Option<f64> {
        let elapsed = self.total.checked_sub(earlier.total)?;
        let busy = self.busy().checked_sub(earlier.busy())?;
        if elapsed == 0 {
            return None;
        }
        // Clamped because `busy` and `total` are separate counters that a
        // mid-read kernel update can leave momentarily inconsistent; a
        // utilization above 1.0 is not a number worth reporting as such.
        Some((busy as f64 / elapsed as f64).clamp(0.0, 1.0))
    }
}

/// Read the processor's cumulative time counters.
pub fn cpu_times() -> Result<CpuTimes, LoadError> {
    platform::cpu_times()
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{CpuTimes, LoadError};

    const STAT: &str = "/proc/stat";

    pub(super) fn cpu_times() -> Result<CpuTimes, LoadError> {
        let contents = std::fs::read_to_string(STAT).map_err(|source| LoadError::Read {
            source_path: STAT,
            source,
        })?;
        parse_stat(&contents)
    }

    /// Parse the aggregate `cpu` line of `/proc/stat`.
    ///
    /// ```text
    /// cpu  199 0 152 92402 84 0 5 0 0 0
    /// ```
    ///
    /// The fields are user, nice, system, idle, iowait, irq, softirq, steal,
    /// guest, guest_nice. Only the first eight are summed: `guest` is already
    /// included in `user` and `guest_nice` in `nice`, so adding them would
    /// count virtualised time twice and depress every utilization figure on a
    /// guest. Kernels also append fields over time, which is why the sum is
    /// taken over a fixed prefix rather than over "all of them".
    ///
    /// The per-core `cpu0`, `cpu1`, ... lines are ignored. The aggregate is
    /// already the sum of them, and taking both would invite reporting a
    /// four-core machine as 400% busy.
    pub(super) fn parse_stat(contents: &str) -> Result<CpuTimes, LoadError> {
        /// user, nice, system, idle, iowait, irq, softirq, steal.
        const ACCOUNTED_FIELDS: usize = 8;
        /// Position of `idle` among them.
        const IDLE: usize = 3;
        /// Position of `iowait`.
        const IOWAIT: usize = 4;

        let line = contents
            .lines()
            // The aggregate line is `cpu` followed by spaces; `cpu0` is a core.
            .find(|line| {
                line.strip_prefix("cpu")
                    .is_some_and(|rest| rest.starts_with(' '))
            })
            .ok_or(LoadError::NoAggregateLine { source_path: STAT })?;

        let fields: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .take(ACCOUNTED_FIELDS)
            // A field that will not parse is treated as absent rather than as
            // zero, so a truncated line fails the length check below instead of
            // yielding a plausible-looking total.
            .map_while(|field| field.parse::<u64>().ok())
            .collect();

        // Every kernel since 2.6.11 publishes all eight. Fewer means the line
        // is not what this parser was written against, and inventing the
        // missing ones would produce a confident wrong percentage.
        if fields.len() < ACCOUNTED_FIELDS {
            return Err(LoadError::NoAggregateLine { source_path: STAT });
        }

        Ok(CpuTimes {
            total: fields.iter().copied().fold(0_u64, u64::saturating_add),
            idle: fields[IDLE].saturating_add(fields[IOWAIT]),
        })
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use super::{CpuTimes, LoadError};

    /// Not implemented off Linux.
    ///
    /// An error rather than a guess, for the reason [`crate::memory`] gives.
    /// macOS needs `host_processor_info` and Windows `GetSystemTimes`; both
    /// belong to the cross-platform milestone where they can be verified on
    /// those systems.
    pub(super) fn cpu_times() -> Result<CpuTimes, LoadError> {
        Err(LoadError::UnsupportedPlatform {
            platform: std::env::consts::OS,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from the development machine, trimmed to the lines that matter.
    #[cfg(target_os = "linux")]
    const SAMPLE: &str = "\
cpu  75341 1204 28017 4820113 9942 0 1533 0 0 0
cpu0 18921 301 7004 1205028 2485 0 383 0 0 0
cpu1 18803 299 6981 1205031 2486 0 381 0 0 0
intr 12345678 0 0
ctxt 98765432
btime 1755900000
";

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_the_aggregate_line_and_ignores_the_cores() {
        let times = platform::parse_stat(SAMPLE).expect("parse");
        // 75341 + 1204 + 28017 + 4820113 + 9942 + 0 + 1533 + 0
        assert_eq!(times.total, 4_936_150);
        // idle + iowait, not idle alone.
        assert_eq!(times.idle, 4_830_055);
        assert_eq!(times.busy(), 106_095);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn guest_time_is_not_counted_twice() {
        // `guest` is already inside `user`. A kernel reporting a large guest
        // figure must not change the total, or every utilization reading on a
        // virtual machine would come out too low.
        let without = "cpu 100 0 0 100 0 0 0 0 0 0\n";
        let with_guest = "cpu 100 0 0 100 0 0 0 0 90 5\n";
        let a = platform::parse_stat(without).expect("parse");
        let b = platform::parse_stat(with_guest).expect("parse");
        assert_eq!(a.total, b.total);
        assert_eq!(a.busy(), b.busy());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_per_core_line_is_never_mistaken_for_the_aggregate() {
        // `cpu0` must not match: it would report one core's time as the whole
        // machine's.
        let cores_only = "cpu0 100 0 0 100 0 0 0 0\ncpu1 100 0 0 100 0 0 0 0\n";
        assert!(platform::parse_stat(cores_only).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_truncated_line_is_an_error_not_a_partial_total() {
        // A partial sum would be a smaller total than reality, which inflates
        // every utilization computed against it.
        assert!(platform::parse_stat("cpu 100 0 0\n").is_err());
        assert!(platform::parse_stat("ctxt 500\n").is_err());
    }

    #[test]
    fn utilization_is_the_busy_share_of_the_interval() {
        let earlier = CpuTimes {
            total: 1_000,
            idle: 900,
        };
        // 100 ticks passed, 75 of them busy.
        let later = CpuTimes {
            total: 1_100,
            idle: 925,
        };
        let utilization = later.utilization_since(&earlier).expect("an interval");
        assert!((utilization - 0.75).abs() < 1e-9);
    }

    #[test]
    fn two_readings_from_the_same_instant_have_no_answer() {
        // Not zero. Zero would be a claim that the machine was idle.
        let times = CpuTimes {
            total: 1_000,
            idle: 900,
        };
        assert_eq!(times.utilization_since(&times), None);
    }

    #[test]
    fn counters_going_backwards_have_no_answer() {
        // Suspend, a core going offline, or two samples from different
        // namespaces. None of those is a measurement of anything.
        let earlier = CpuTimes {
            total: 2_000,
            idle: 1_800,
        };
        let later = CpuTimes {
            total: 1_000,
            idle: 900,
        };
        assert_eq!(later.utilization_since(&earlier), None);
    }

    #[test]
    fn a_fully_busy_interval_reads_as_one() {
        let earlier = CpuTimes {
            total: 1_000,
            idle: 500,
        };
        let later = CpuTimes {
            total: 1_100,
            idle: 500,
        };
        assert_eq!(later.utilization_since(&earlier), Some(1.0));
    }

    #[test]
    fn an_inconsistent_pair_is_clamped_rather_than_reported_above_one() {
        // `busy` and `total` are separate counters; a read straddling a kernel
        // update can leave them momentarily disagreeing.
        // 10 ticks pass, but `idle` also falls by 5, so the busy delta is 15 -
        // more than the whole interval. Without the clamp this reports 150%.
        let earlier = CpuTimes {
            total: 1_000,
            idle: 900,
        };
        let later = CpuTimes {
            total: 1_010,
            idle: 895,
        };
        let utilization = later.utilization_since(&earlier).expect("an interval");
        assert_eq!(
            utilization, 1.0,
            "an inconsistent pair must saturate at fully busy, not exceed it"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_real_probe_returns_plausible_counters() {
        let times = cpu_times().expect("probe this machine");
        assert!(times.total > 0, "a running machine has accounted time");
        assert!(times.idle <= times.total);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn two_real_readings_are_ordered() {
        // The property every caller depends on: counters climb, so a later
        // reading can always be differenced against an earlier one.
        let first = cpu_times().expect("first reading");
        let second = cpu_times().expect("second reading");
        assert!(second.total >= first.total);
        assert!(second.busy() >= first.busy());
    }
}
