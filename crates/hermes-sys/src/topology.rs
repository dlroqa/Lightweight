//! CPU model and physical core count.
//!
//! Every field is optional and nothing here returns an error: the caller's
//! existing fallback is the logical core count, which is a safe answer
//! everywhere. A probe that failed loudly would turn a cosmetic gap into a
//! refusal to start.

/// What the platform can say about the processor.
#[derive(Clone, Debug, Default)]
pub struct RawTopology {
    pub model: Option<String>,
    pub physical_cores: Option<u32>,
    /// Cores in the fastest performance class, where the platform distinguishes
    /// them. On Apple Silicon this is what decides the thread count: counting
    /// efficiency cores as inference cores measurably costs throughput.
    pub performance_cores: Option<u32>,
}

#[cfg(target_os = "macos")]
mod platform {
    use super::RawTopology;
    use crate::sysctl;

    pub(super) fn read() -> RawTopology {
        let physical_cores = sysctl::u64_by_name(c"hw.physicalcpu", "sysctlbyname(hw.physicalcpu)")
            .ok()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0);

        // `hw.perflevel0` is the fastest class on a machine that has classes at
        // all; it is simply absent on Intel, where `None` is the honest answer
        // and the physical count above is already right.
        let performance_cores = sysctl::u64_by_name(
            c"hw.perflevel0.physicalcpu",
            "sysctlbyname(hw.perflevel0.physicalcpu)",
        )
        .ok()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0);

        let model = sysctl::string_by_name(
            c"machdep.cpu.brand_string",
            "sysctlbyname(machdep.cpu.brand_string)",
        )
        .ok()
        .filter(|s| !s.is_empty());

        RawTopology {
            model,
            physical_cores,
            performance_cores,
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::RawTopology;
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore,
        SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };

    /// Count `RelationProcessorCore` records: one per physical core.
    ///
    /// Two calls, as the API requires: the first reports how many bytes the
    /// answer needs, the second fills them. The records are variable-length, so
    /// the walk advances by each record's own `Size` rather than by a struct
    /// size that would be wrong the moment a record carried more groups.
    fn physical_cores() -> Option<u32> {
        let mut bytes: u32 = 0;
        // SAFETY: a null buffer with a live length is the documented way to ask
        // this API for its required size; it writes only to `bytes`.
        #[allow(unsafe_code)]
        let ok = unsafe {
            GetLogicalProcessorInformationEx(
                RelationProcessorCore,
                std::ptr::null_mut(),
                &raw mut bytes,
            )
        };
        // Success here would mean "no data needed", which this relationship
        // never reports; anything but the buffer-too-small error is a failure.
        if ok != 0
            || std::io::Error::last_os_error().raw_os_error()
                != Some(i32::try_from(ERROR_INSUFFICIENT_BUFFER).ok()?)
        {
            return None;
        }

        let len = usize::try_from(bytes).ok()?;
        if len == 0 {
            return None;
        }
        // A `u64` buffer rather than `u8`: the records contain pointers and
        // 64-bit masks, and this is the cheapest way to guarantee the alignment
        // the API's own casts assume.
        let mut buffer = vec![0u64; len.div_ceil(std::mem::size_of::<u64>())];
        // SAFETY: `buffer` holds at least `bytes` bytes and `bytes` says so, so
        // the call writes within it.
        #[allow(unsafe_code)]
        let ok = unsafe {
            GetLogicalProcessorInformationEx(
                RelationProcessorCore,
                buffer.as_mut_ptr().cast(),
                &raw mut bytes,
            )
        };
        if ok == 0 {
            return None;
        }

        let mut offset = 0usize;
        let mut cores = 0u32;
        let filled = usize::try_from(bytes).ok()?.min(len);
        let base: *const u8 = buffer.as_ptr().cast();
        while offset + std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() <= filled {
            // SAFETY: `offset` is within `filled`, which is within the buffer,
            // and the loop condition leaves room for a whole record header. The
            // buffer is `u64`-aligned, which satisfies this struct.
            #[allow(unsafe_code)]
            let record = unsafe {
                &*base
                    .add(offset)
                    .cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()
            };
            let size = usize::try_from(record.Size).ok()?;
            // A zero or absurd size would spin this loop forever on malformed
            // data; refusing to answer is better than hanging a startup probe.
            if size == 0 || offset + size > filled {
                return None;
            }
            if record.Relationship == RelationProcessorCore {
                cores = cores.saturating_add(1);
            }
            offset += size;
        }

        (cores > 0).then_some(cores)
    }

    /// NUL-terminated UTF-16, as every `*W` entry point wants.
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The processor name, as the firmware reported it to Windows.
    fn model() -> Option<String> {
        let key = wide(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0");
        let value = wide("ProcessorNameString");

        let mut bytes: u32 = 0;
        // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call.
        // A null data pointer with a live length asks for the required size.
        #[allow(unsafe_code)]
        let rc = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut bytes,
            )
        };
        if rc != 0 || bytes == 0 {
            return None;
        }

        let units = usize::try_from(bytes)
            .ok()?
            .div_ceil(std::mem::size_of::<u16>());
        let mut buffer = vec![0u16; units];
        // SAFETY: `buffer` holds `bytes` bytes and `bytes` says so.
        #[allow(unsafe_code)]
        let rc = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &raw mut bytes,
            )
        };
        if rc != 0 {
            return None;
        }

        // The length counts the NUL terminator; the string stops at the first one.
        let end = buffer
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(buffer.len());
        let text = String::from_utf16_lossy(&buffer[..end]).trim().to_owned();
        (!text.is_empty()).then_some(text)
    }

    pub(super) fn read() -> RawTopology {
        RawTopology {
            model: model(),
            physical_cores: physical_cores(),
            // Windows does report efficiency classes through this same API, but
            // reading one correctly means trusting `EfficiencyClass` orderings
            // that vary by vendor, and no machine here can check that. `None`
            // leaves the existing physical-core answer standing, which is right
            // on every non-hybrid part and conservative on the rest.
            performance_cores: None,
        }
    }
}

/// Read what the platform knows about this processor.
pub fn read() -> RawTopology {
    platform::read()
}
