//! `sysctl` readers, shared by the macOS probes.

use crate::ProbeError;

/// Read a scalar `sysctl` by name.
///
/// Widths differ between keys - `hw.memsize` is 64-bit, `hw.physicalcpu` is 32 -
/// so the length the kernel reports back decides how it is read, rather than a
/// per-key constant that could drift out of step with the kernel.
pub(crate) fn u64_by_name(name: &std::ffi::CStr, api: &'static str) -> Result<u64, ProbeError> {
    let mut value: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: `name` is a NUL-terminated C string by construction. `value` is 8
    // bytes and `len` says so, so the kernel writes at most that much; it
    // updates `len` with what it actually wrote, checked below. The last two
    // arguments are null/0, the documented form for a read.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::from_mut(&mut value).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(ProbeError::last(api));
    }
    match len {
        8 => Ok(value),
        // A 32-bit key leaves the high half of `value` untouched, which is zero
        // here; masking is what makes that explicit rather than lucky.
        4 => Ok(value & u64::from(u32::MAX)),
        _ => Err(ProbeError::from_raw(api, libc::EINVAL)),
    }
}

/// Read a fixed-size `sysctl` struct by name.
pub(crate) fn struct_by_name<T>(name: &std::ffi::CStr, api: &'static str) -> Result<T, ProbeError> {
    // SAFETY: `T` here is only ever a `libc` C struct of scalars, for which
    // all-zero is a valid value; it is overwritten by the call below.
    #[allow(unsafe_code)]
    let mut value: T = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<T>();
    // SAFETY: as `u64_by_name`, with a buffer of exactly `size_of::<T>()` bytes
    // and `len` set to match.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::from_mut(&mut value).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(ProbeError::last(api));
    }
    if len != std::mem::size_of::<T>() {
        return Err(ProbeError::from_raw(api, libc::EINVAL));
    }
    Ok(value)
}

/// Read a string `sysctl` by name.
///
/// Two calls: the first asks how long the value is, the second reads it. Sizing
/// from the kernel's own answer rather than a fixed buffer is what keeps a
/// longer-than-expected CPU brand string from being silently truncated.
pub(crate) fn string_by_name(
    name: &std::ffi::CStr,
    api: &'static str,
) -> Result<String, ProbeError> {
    let mut len = 0usize;
    // SAFETY: a null out-buffer with a live `len` is the documented way to ask
    // for the size of a `sysctl` value; nothing is written to the buffer.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(ProbeError::last(api));
    }
    if len == 0 {
        return Err(ProbeError::from_raw(api, libc::EINVAL));
    }

    let mut buffer = vec![0u8; len];
    // SAFETY: `buffer` holds exactly `len` bytes and `len` says so, so the
    // kernel writes within it.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(ProbeError::last(api));
    }

    // The value is NUL-terminated and `len` counts the terminator.
    buffer.truncate(len);
    while buffer.last() == Some(&0) {
        buffer.pop();
    }
    String::from_utf8(buffer).map_err(|_| ProbeError::from_raw(api, libc::EILSEQ))
}

/// The machine's page size.
pub(crate) fn page_size() -> u64 {
    // SAFETY: `sysconf` takes an int and returns a long; no pointers involved.
    // A negative return means "no definite limit", which cannot happen for page
    // size on any Darwin this runs on - the fallback keeps the arithmetic sane
    // rather than describing a real machine.
    #[allow(unsafe_code)]
    let raw = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(raw).unwrap_or(4096)
}
