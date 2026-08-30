//! What one process is costing, on the two platforms without `/proc`.
//!
//! The engine is a child process, and reading *its* resident set rather than
//! ours is the reason the process boundary pays for itself twice: the estimator
//! can be calibrated against what the engine actually used, and a swap can be
//! credited with what the outgoing engine is about to release. On Linux that is
//! two files in `/proc/<pid>`, parsed in the backend beside the tests that pin
//! the parsing. Here it is one system call each.
//!
//! **Every figure is normalised to the units `/proc` publishes**, because the
//! consumers are shared: `ResourceSnapshot` carries bytes, and `CpuTicks` is
//! documented as kernel clock ticks at `USER_HZ` — 100 per second. macOS
//! reports processor time in nanoseconds and Windows in 100-nanosecond
//! intervals, so both are converted here rather than at the reader, where a
//! second unit under the same field name would quietly wrong every ratio drawn
//! from it.

use crate::error::ProbeError;

/// One process's cost, in the units `/proc/<pid>/status` and `stat` use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawProcess {
    /// Resident set, in bytes.
    pub rss: u64,
    /// High-water mark, in bytes. Zero when the platform keeps none.
    pub peak_rss: u64,
    /// Whether `peak_rss` is a peak *footprint* rather than a peak resident
    /// set - which is to say, whether it excludes clean file-backed pages.
    ///
    /// A bare `bool` because this crate is FFI and nothing else: naming the two
    /// kinds is `lightweight-inference`'s job, and a type from there would put the
    /// workspace's unsafe crate on top of its inference crate.
    pub peak_is_footprint: bool,
    /// The part of the resident set that is not clean file-backed pages, in
    /// bytes — what the process genuinely hands back when it exits.
    ///
    /// `None` where the platform publishes no such figure, which is the same
    /// answer Linux gives on a kernel too old for `RssAnon`: a swap credits
    /// nothing rather than crediting a guess.
    pub anon_rss: Option<u64>,
    /// User time, in ticks of 1/100 s.
    pub user_ticks: u64,
    /// System time, in ticks of 1/100 s.
    pub system_ticks: u64,
}

/// Read one process, by pid.
pub fn read(pid: u32) -> Result<RawProcess, ProbeError> {
    platform::read(pid)
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{ProbeError, RawProcess};

    /// Nanoseconds in one 1/100 s tick.
    const NANOS_PER_TICK: u64 = 10_000_000;

    pub(super) fn read(pid: u32) -> Result<RawProcess, ProbeError> {
        // `RUSAGE_INFO_V4` because it is the first flavour carrying
        // `ri_lifetime_max_phys_footprint`, which is the peak this crate exists
        // to report; V2 would give the current figures and no high-water mark.
        let mut info: libc::rusage_info_v4 = unsafe_zeroed();
        // SAFETY: `proc_pid_rusage` writes a `rusage_info_v4` through the
        // pointer when told flavour V4, and `info` is exactly that type, live,
        // and not aliased. The cast is the one the C prototype requires: its
        // buffer parameter is `rusage_info_t`, a `void *`.
        #[allow(unsafe_code)]
        let status = unsafe {
            libc::proc_pid_rusage(
                pid as libc::c_int,
                libc::RUSAGE_INFO_V4,
                // `rusage_info_t` is itself `void *`, and the prototype takes
                // a pointer *to* one - which is how the C header spells "give
                // me the address of your buffer". So this is a pointer to the
                // struct, cast to that type, exactly as the C call sites do it.
                std::ptr::from_mut(&mut info).cast::<libc::rusage_info_t>(),
            )
        };
        if status != 0 {
            return Err(ProbeError::last("proc_pid_rusage"));
        }

        // `ri_phys_footprint` is what Activity Monitor calls Memory: the
        // process's own dirty and compressed pages, without the clean
        // file-backed ones the kernel can drop without asking. That is the same
        // distinction `RssAnon` draws on Linux, and it is drawn for the same
        // caller - the swap credit, which must never count the mapped weights
        // twice.
        //
        // **The peak is that same footprint's high-water mark, and not a peak
        // RSS.** macOS publishes no per-process maximum resident set: the only
        // lifetime maximum in `rusage_info` is this one, and `task_info`'s
        // `resident_size_max` needs `task_for_pid`, which a process may not
        // call on another without an entitlement. The footprint is the right
        // number for what `ResourceSnapshot::peak_rss` is documented to mean -
        // jetsam judges a process by its footprint, so it is what would have
        // killed the engine - but it is **not** comparable with Linux's
        // `VmHWM`, which counts the mapped weights that a footprint excludes.
        //
        // The consequence is real and belongs where somebody will meet it:
        // `lightweight-bench` computes a residual as peak minus (weights + KV
        // cache), and on a mapped model this peak does not contain the weights,
        // so that subtraction underflows and `fit_run` skips the sample. Windows
        // reports a true peak working set and is unaffected. `docs/M10-PLAN.md`
        // carries the rest of the argument.
        Ok(RawProcess {
            rss: info.ri_resident_size,
            peak_rss: info.ri_lifetime_max_phys_footprint,
            peak_is_footprint: true,
            anon_rss: Some(info.ri_phys_footprint),
            user_ticks: info.ri_user_time / NANOS_PER_TICK,
            system_ticks: info.ri_system_time / NANOS_PER_TICK,
        })
    }

    /// All-zero `rusage_info_v4`, which the call overwrites.
    fn unsafe_zeroed() -> libc::rusage_info_v4 {
        // SAFETY: it is a plain C struct of integers and a byte array, for
        // which all-zero is a valid value.
        #[allow(unsafe_code)]
        unsafe {
            std::mem::zeroed()
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{ProbeError, RawProcess};
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    /// 100-nanosecond intervals in one 1/100 s tick.
    const INTERVALS_PER_TICK: u64 = 100_000;

    pub(super) fn read(pid: u32) -> Result<RawProcess, ProbeError> {
        let handle = Handle::open(pid)?;
        let memory = memory_counters(handle.0)?;
        let (user_ticks, system_ticks) = times(handle.0)?;

        Ok(RawProcess {
            rss: memory.WorkingSetSize as u64,
            peak_rss: memory.PeakWorkingSetSize as u64,
            // A true peak working set, mapped files included - the same
            // quantity Linux's `VmHWM` reports.
            peak_is_footprint: false,
            // Private bytes: the process's own committed memory, which excludes
            // the mapped file its weights come from. The same thing `RssAnon`
            // is read for on Linux, and read here for the same caller.
            anon_rss: Some(memory.PagefileUsage as u64),
            user_ticks,
            system_ticks,
        })
    }

    fn memory_counters(handle: HANDLE) -> Result<PROCESS_MEMORY_COUNTERS, ProbeError> {
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS>()).unwrap_or(0),
            ..Default::default()
        };
        // SAFETY: `handle` is a live process handle opened with
        // `PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ`, which is what
        // this call documents as sufficient, and `counters` is a live
        // `PROCESS_MEMORY_COUNTERS` whose `cb` states its own size. The call
        // writes only within it.
        #[allow(unsafe_code)]
        let ok = unsafe { GetProcessMemoryInfo(handle, &raw mut counters, counters.cb) };
        if ok == 0 {
            return Err(ProbeError::last("GetProcessMemoryInfo"));
        }
        Ok(counters)
    }

    fn times(handle: HANDLE) -> Result<(u64, u64), ProbeError> {
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: `handle` is a live process handle with query access, and all
        // four `FILETIME`s are live and distinct. The call writes only within
        // them.
        #[allow(unsafe_code)]
        let ok = unsafe {
            GetProcessTimes(
                handle,
                &raw mut creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        };
        if ok == 0 {
            return Err(ProbeError::last("GetProcessTimes"));
        }
        Ok((
            filetime_ticks(user) / INTERVALS_PER_TICK,
            filetime_ticks(kernel) / INTERVALS_PER_TICK,
        ))
    }

    /// A `FILETIME`'s two halves as the one 64-bit count they are.
    ///
    /// Not read through a union or a pointer cast: the struct is documented as
    /// a split 64-bit value and is not guaranteed to be aligned for one.
    const fn filetime_ticks(time: FILETIME) -> u64 {
        ((time.dwHighDateTime as u64) << 32) | (time.dwLowDateTime as u64)
    }

    /// A process handle that closes itself.
    ///
    /// A leaked handle keeps the *process object* alive after the engine exits,
    /// so a supervisor polling a dead engine would accumulate one per read.
    struct Handle(HANDLE);

    impl Handle {
        fn open(pid: u32) -> Result<Self, ProbeError> {
            // The narrowest rights that answer both calls: querying limited
            // information plus reading the working set. Asking for
            // `PROCESS_ALL_ACCESS` would fail on a process we are allowed to
            // measure but not control.
            // SAFETY: an ordinary Win32 call taking no pointers. A failure
            // returns null, which is checked before the handle is used.
            #[allow(unsafe_code)]
            let handle =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid) };
            if handle.is_null() {
                return Err(ProbeError::last("OpenProcess"));
            }
            Ok(Self(handle))
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a handle this type opened and has not closed,
            // and nothing else holds a copy.
            #[allow(unsafe_code)]
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}
