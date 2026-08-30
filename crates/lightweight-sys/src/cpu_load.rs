//! System-wide processor time, on the two platforms without `/proc/stat`.
//!
//! The Linux gateway reads `/proc/stat` directly in `hermes-system-info`, where
//! the parsing lives beside the tests that pin it. Here it is one system call
//! each. macOS and Windows both publish a running total of processor time
//! summed over every core, which is exactly the shape `/proc/stat`'s aggregate
//! `cpu` line has, so the number lands at the same reader with the same meaning.
//!
//! **Every figure is normalised to the units `/proc/stat` publishes** — kernel
//! clock ticks at `USER_HZ`, 100 per second, summed over all cores — because
//! the consumer is shared. The panel differences two readings for a utilization
//! percentage, and it also divides the machine's total by the core count to
//! turn the engine's own tick counter into a count of cores in use. That second
//! ratio only cancels its unit if both sides are the same tick, so a second
//! unit hiding under this field name would quietly wrong the "cores in use"
//! figure on every non-Linux machine. macOS already reports at that rate;
//! Windows reports 100-nanosecond intervals and is converted here.

use crate::error::ProbeError;

/// Cumulative processor time since boot, summed over every core, in ticks of
/// 1/100 s — the same units and the same shape as [`/proc/stat`'s aggregate
/// line][crate], so a caller differences two readings exactly as it does there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawCpuLoad {
    /// Every accounted tick: busy and idle together.
    pub total: u64,
    /// Ticks spent idle.
    pub idle: u64,
}

/// Read the machine's cumulative processor-time counters.
pub fn read() -> Result<RawCpuLoad, ProbeError> {
    platform::read()
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{ProbeError, RawCpuLoad};

    pub(super) fn read() -> Result<RawCpuLoad, ProbeError> {
        // SAFETY: all-zero is a valid `host_cpu_load_info`; the call overwrites
        // it.
        #[allow(unsafe_code)]
        let mut info: libc::host_cpu_load_info = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        // SAFETY: `mach_host_self` returns a host port needing no rights for a
        // statistics read. The out-buffer is a `host_cpu_load_info` and `count`
        // is that struct's size in 32-bit words - the pairing
        // `HOST_CPU_LOAD_INFO` requires - so the kernel writes exactly within
        // it.
        //
        // `mach_host_self` is deprecated in `libc` in favour of the `mach2`
        // crate, and `crate::memory` carries the argument for why this
        // workspace keeps using it rather than taking that dependency for one
        // stable-ABI symbol.
        #[allow(unsafe_code, deprecated)]
        let rc = unsafe {
            libc::host_statistics(
                libc::mach_host_self(),
                libc::HOST_CPU_LOAD_INFO,
                std::ptr::from_mut(&mut info).cast(),
                &mut count,
            )
        };
        if rc != libc::KERN_SUCCESS {
            // A Mach `kern_return_t` is not an errno; reporting it as one would
            // print a misleading message, so it is carried as a plain "other"
            // error the same way `crate::memory` does.
            return Err(ProbeError {
                api: "host_statistics(HOST_CPU_LOAD_INFO)",
                source: std::io::Error::other(format!("kern_return_t {rc}")),
            });
        }

        // The four states are user, system, idle and nice. `host_statistics`
        // returns them already summed over every processor and already at the
        // 100 Hz Mach tick, so no conversion and no per-core sum is needed - the
        // number is in the units `/proc/stat` publishes, straight out of the
        // kernel. `nice` is a distinct state here (unlike Linux, where it is
        // folded into `user`), so it is added into the total rather than
        // dropped, or a busy interval of niced work would read as idle.
        let ticks = info.cpu_ticks;
        let total = ticks
            .iter()
            .copied()
            .fold(0_u64, |acc, tick| acc.saturating_add(u64::from(tick)));
        let idle = u64::from(ticks[libc::CPU_STATE_IDLE as usize]);
        Ok(RawCpuLoad { total, idle })
    }
}

#[cfg(windows)]
mod platform {
    use super::{ProbeError, RawCpuLoad};
    use windows_sys::Win32::Foundation::FILETIME;
    // `GetSystemTimes` is `processthreadsapi.h`, which windows-sys maps to the
    // same `Threading` module `crate::process` already reads `GetProcessTimes`
    // from - not `SystemInformation`, despite the "system" in its name.
    use windows_sys::Win32::System::Threading::GetSystemTimes;

    /// 100-nanosecond intervals in one 1/100 s tick — the same conversion
    /// `crate::process` applies, for the same reason: the reader is shared and
    /// counts in ticks.
    const INTERVALS_PER_TICK: u64 = 100_000;

    pub(super) fn read() -> Result<RawCpuLoad, ProbeError> {
        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: all three `FILETIME`s are live and distinct, and the call
        // writes only within them. Its three figures are already summed over
        // every processor, which is the aggregation this probe wants.
        #[allow(unsafe_code)]
        let ok = unsafe { GetSystemTimes(&raw mut idle, &raw mut kernel, &raw mut user) };
        if ok == 0 {
            return Err(ProbeError::last("GetSystemTimes"));
        }

        // `kernel` already includes idle time - documented, and the reason the
        // total is `kernel + user` rather than a sum of all three. Idle is then
        // reported separately, which is what the utilization difference reads.
        let idle = filetime_intervals(idle);
        let kernel = filetime_intervals(kernel);
        let user = filetime_intervals(user);
        Ok(RawCpuLoad {
            total: kernel.saturating_add(user) / INTERVALS_PER_TICK,
            idle: idle / INTERVALS_PER_TICK,
        })
    }

    /// A `FILETIME`'s two halves as the one 64-bit count of 100 ns intervals
    /// they are. Not read through a pointer cast: the struct is a split 64-bit
    /// value and is not guaranteed to be aligned for one.
    const fn filetime_intervals(time: FILETIME) -> u64 {
        ((time.dwHighDateTime as u64) << 32) | (time.dwLowDateTime as u64)
    }
}
