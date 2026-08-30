//! Physical memory, as the platform reports it.
//!
//! The numbers are raw bytes; naming them and deciding which one the estimator
//! may spend is `lightweight-system-info`'s job, not this crate's.

use crate::ProbeError;

/// A platform memory reading, in bytes.
#[derive(Clone, Copy, Debug)]
pub struct RawMemory {
    pub total: u64,
    pub available: u64,
    pub free: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{ProbeError, RawMemory};
    use crate::sysctl;

    /// Ask the Mach kernel for its page-level accounting.
    fn vm_statistics() -> Result<libc::vm_statistics64, ProbeError> {
        // SAFETY: all-zero is a valid `vm_statistics64`; the call overwrites it.
        #[allow(unsafe_code)]
        let mut stat: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: `mach_host_self` returns a host port needing no rights for a
        // statistics read. The out-buffer is a `vm_statistics64` and `count` is
        // that struct's size in 32-bit words - the pairing `HOST_VM_INFO64`
        // requires - so the kernel writes exactly within it.
        //
        // `mach_host_self` is deprecated in `libc` in favour of the `mach2`
        // crate. Taking a whole new dependency for one symbol is the trade the
        // workspace dependency policy exists to refuse, and the call is stable
        // Darwin ABI regardless of which crate declares it. If `libc` ever
        // removes it, `mach2` is the migration and this is the only call site.
        #[allow(unsafe_code, deprecated)]
        let rc = unsafe {
            libc::host_statistics64(
                libc::mach_host_self(),
                libc::HOST_VM_INFO64,
                std::ptr::from_mut(&mut stat).cast(),
                &mut count,
            )
        };
        if rc != libc::KERN_SUCCESS {
            // A Mach kern_return_t is not an errno; reporting it as one would
            // print a misleading message, so the failure is reported as a plain
            // "other" error carrying the actual code.
            return Err(ProbeError {
                api: "host_statistics64",
                source: std::io::Error::other(format!("kern_return_t {rc}")),
            });
        }
        Ok(stat)
    }

    pub(super) fn read() -> Result<RawMemory, ProbeError> {
        let total = sysctl::u64_by_name(c"hw.memsize", "sysctlbyname(hw.memsize)")?;
        let stat = vm_statistics()?;
        let page = sysctl::page_size();

        // Activity Monitor's own decomposition of "memory used", and the reason
        // each term is where it is:
        //
        // * `internal - purgeable` is app memory. Purgeable pages are ones the
        //   kernel may drop under pressure, so they are headroom, not usage.
        // * `wire_count` cannot be paged out at all.
        // * `compressor_page_count` is memory the compressor is *occupying*.
        //   Counting compressed pages as free is the mistake that would let a
        //   model be approved onto a machine already under pressure.
        //
        // External (file-backed) pages are deliberately absent: they are
        // reclaimable, which is exactly the argument `MemAvailable` makes on
        // Linux and the reason this crate's caller prefers it to `MemFree`.
        let used_pages = u64::from(stat.internal_page_count)
            .saturating_sub(u64::from(stat.purgeable_count))
            .saturating_add(u64::from(stat.wire_count))
            .saturating_add(u64::from(stat.compressor_page_count));
        let used = used_pages.saturating_mul(page);

        // Speculative pages are excluded from `free`: they are read-ahead the
        // kernel has already decided to keep, and counting them would report
        // more genuinely-unused memory than exists.
        let free = u64::from(stat.free_count)
            .saturating_sub(u64::from(stat.speculative_count))
            .saturating_mul(page);

        let swap: libc::xsw_usage =
            sysctl::struct_by_name(c"vm.swapusage", "sysctlbyname(vm.swapusage)")?;

        Ok(RawMemory {
            total,
            available: total.saturating_sub(used),
            free,
            swap_total: swap.xsu_total,
            swap_free: swap.xsu_avail,
        })
    }
}

#[cfg(windows)]
mod platform {
    use super::{ProbeError, RawMemory};
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    pub(super) fn read() -> Result<RawMemory, ProbeError> {
        let mut status = MEMORYSTATUSEX {
            dwLength: u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).unwrap_or(0),
            ..unsafe_zeroed()
        };
        // SAFETY: `status` is a live, correctly-sized `MEMORYSTATUSEX` whose
        // `dwLength` has been set to its own size, which is the contract this
        // call documents. It writes only within that struct.
        #[allow(unsafe_code)]
        let ok = unsafe { GlobalMemoryStatusEx(&raw mut status) };
        if ok == 0 {
            return Err(ProbeError::last("GlobalMemoryStatusEx"));
        }

        // Windows publishes one figure for unused physical memory, and it
        // includes the standby list. There is no documented call for "free and
        // zeroed" short of performance counters, so `free` and `available` are
        // the same number here rather than a Linux distinction invented for a
        // platform that does not draw it.
        //
        // The page file is reported as a *commit limit* that already includes
        // physical memory, so the page file's own size is what remains after
        // subtracting it. Its free portion cannot be derived the same way:
        // subtracting available physical from available commit produced
        // `swap_free` **larger than `swap_total`** on a real runner - 3.37 GiB
        // free out of 3.09 GiB - because the two availability figures are
        // accounted separately and the standby list belongs to both.
        //
        // Derived through *usage* instead, which is the one quantity commit
        // accounting states consistently: what is committed beyond what is
        // resident is what the page file is holding. Saturating, so a machine
        // whose numbers still disagree reports an empty page file rather than
        // an impossible one - and never a free figure above the total, which
        // `MemorySnapshot::swap_used` would turn into a wrong number on screen.
        //
        // Both remain approximations, which is acceptable for exactly one
        // reason: swap is never counted as headroom, so these reach the
        // dashboard and no decision.
        let swap_total = status.ullTotalPageFile.saturating_sub(status.ullTotalPhys);
        let committed = status
            .ullTotalPageFile
            .saturating_sub(status.ullAvailPageFile);
        let resident = status.ullTotalPhys.saturating_sub(status.ullAvailPhys);
        let swap_used = committed.saturating_sub(resident).min(swap_total);
        Ok(RawMemory {
            total: status.ullTotalPhys,
            available: status.ullAvailPhys,
            free: status.ullAvailPhys,
            swap_total,
            swap_free: swap_total.saturating_sub(swap_used),
        })
    }

    /// All-zero `MEMORYSTATUSEX`, for the struct-update above.
    fn unsafe_zeroed() -> MEMORYSTATUSEX {
        // SAFETY: `MEMORYSTATUSEX` is a plain C struct of integers, for which
        // all-zero is a valid value. Every field is overwritten by the call.
        #[allow(unsafe_code)]
        unsafe {
            std::mem::zeroed()
        }
    }
}

/// Read the machine's memory.
pub fn read() -> Result<RawMemory, ProbeError> {
    platform::read()
}
