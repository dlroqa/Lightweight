//! What one process is costing, off Linux.
//!
//! **This is deliberately not the whole story, and the split is the point.**
//! On Linux the engine's resident set is read from `/proc/<pid>/status` and
//! `/proc/<pid>/stat` by `hermes-backend-llamacpp`, next to the parsers whose
//! tests pin every field against captured samples. Moving that here would move
//! working, tested code for symmetry alone. What was missing is the other two
//! platforms, which have no `/proc` to read and one system call each instead -
//! so that is what this adds, and Linux is left exactly as it was.
//!
//! Until M10 there was no reading at all off Linux, and the consequences were
//! quiet rather than loud: `hermes bench --fit` skipped every sample because
//! none had a peak, so `calibration.json` could never be written on a Mac or a
//! Windows machine; the engine RSS gauges reported nothing; and a model swap
//! credited nothing back.

use hermes_core::units::Bytes;

/// What a process is costing, in the units `/proc` publishes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessUsage {
    pub rss: Bytes,
    /// High-water mark. Zero where the platform keeps none.
    pub peak_rss: Bytes,
    /// Whether that mark excludes clean file-backed pages, which is what macOS
    /// publishes and what Linux and Windows do not. Carried rather than
    /// inferred: the caller records it, and a recording outlives the machine.
    pub peak_is_footprint: bool,
    /// The part that is not clean file-backed pages — what the process really
    /// hands back. `None` where the platform publishes no such figure.
    pub anon_rss: Option<Bytes>,
    /// Processor time, in kernel clock ticks of 1/100 s, as `/proc` reports it.
    pub user_ticks: u64,
    pub system_ticks: u64,
}

/// Why a process could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("could not read process {pid}: {detail}")]
    Read { pid: u32, detail: String },
    #[error("reading another process is not implemented on {platform}")]
    UnsupportedPlatform { platform: &'static str },
}

impl ProcessError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Read { .. } => "process_probe_failed",
            Self::UnsupportedPlatform { .. } => "process_probe_unsupported",
        }
    }
}

/// Read one process by pid.
///
/// Never called on Linux, where the backend reads `/proc` directly; it answers
/// `UnsupportedPlatform` there rather than pretending to a second
/// implementation of a thing that already works.
pub fn usage(pid: u32) -> Result<ProcessUsage, ProcessError> {
    platform::usage(pid)
}

#[cfg(any(target_os = "macos", windows))]
mod platform {
    use super::{Bytes, ProcessError, ProcessUsage};

    pub(super) fn usage(pid: u32) -> Result<ProcessUsage, ProcessError> {
        let raw = hermes_sys::process::read(pid).map_err(|error| ProcessError::Read {
            pid,
            detail: error.to_string(),
        })?;
        Ok(ProcessUsage {
            rss: Bytes(raw.rss),
            peak_rss: Bytes(raw.peak_rss),
            peak_is_footprint: raw.peak_is_footprint,
            anon_rss: raw.anon_rss.map(Bytes),
            user_ticks: raw.user_ticks,
            system_ticks: raw.system_ticks,
        })
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::{ProcessError, ProcessUsage};

    pub(super) fn usage(_pid: u32) -> Result<ProcessUsage, ProcessError> {
        Err(ProcessError::UnsupportedPlatform {
            platform: std::env::consts::OS,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This process, read through the platform's own call.
    ///
    /// Relationships rather than magnitudes, for the reason the memory probe's
    /// own live test gives: a runner's numbers are whatever the hypervisor gave
    /// it, and pinning one here would fit the suite to one machine.
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn a_live_process_reports_a_resident_set_and_a_peak() {
        let me = usage(std::process::id()).expect("read this process");
        assert!(me.rss > Bytes::ZERO, "a process with no memory: {me:?}");
        let anon = me
            .anon_rss
            .expect("both platforms publish a private figure");
        assert!(anon > Bytes::ZERO, "no private memory at all: {me:?}");

        // The peak is compared against the quantity it is a peak *of*, which is
        // the whole reason `peak_is_footprint` is carried. Asserting
        // `peak >= rss` here failed on macOS against a perfectly good reading -
        // 1.64 MiB of peak footprint under 2.53 MiB of resident set - because
        // the difference is the clean file-backed pages a footprint excludes,
        // starting with this test binary itself.
        if me.peak_is_footprint {
            assert!(
                me.peak_rss >= anon,
                "a peak footprint below the current footprint: {me:?}"
            );
        } else {
            assert!(
                me.peak_rss >= me.rss,
                "a peak resident set below the current one: {me:?}"
            );
        }
    }

    /// A pid that cannot exist is an error, not a zeroed reading.
    ///
    /// Zero bytes resident is what a caller would read as "the engine is using
    /// nothing", which is a very different claim from "it could not be read".
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn a_process_that_is_not_there_is_an_error_rather_than_zero() {
        let err = usage(u32::MAX - 1).expect_err("no such process");
        assert_eq!(err.code(), "process_probe_failed");
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    #[test]
    fn linux_says_this_is_not_its_reader() {
        // The `/proc` reading lives in the backend beside the tests that pin
        // its parsing; this module exists for the platforms without one.
        let err = usage(std::process::id()).expect_err("linux reads /proc instead");
        assert_eq!(err.code(), "process_probe_unsupported");
    }
}
