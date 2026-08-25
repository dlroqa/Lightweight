//! Free space on the volume holding a path, on Windows.
//!
//! Linux and macOS reach `statvfs` through `rustix`, which wraps it safely
//! upstream, so this module is Windows-only.

use crate::ProbeError;

/// A volume's capacity, in bytes.
#[derive(Clone, Copy, Debug)]
pub struct RawDiskSpace {
    pub total: u64,
    /// What this caller may actually use. On a volume with quotas this is less
    /// than `free`, and it is the number a pre-flight check must spend.
    pub available: u64,
    pub free: u64,
}

/// NUL-terminated UTF-16, as every `*W` entry point wants.
fn wide(value: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    value
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Read free space for the volume containing `path`.
pub fn space_for(path: &std::path::Path) -> Result<RawDiskSpace, ProbeError> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide_path = wide(path);
    let mut available: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;

    // SAFETY: `wide_path` is NUL-terminated UTF-16 that outlives the call, and
    // the three out-parameters are live `u64`s. The call writes only to them.
    #[allow(unsafe_code)]
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide_path.as_ptr(),
            &raw mut available,
            &raw mut total,
            &raw mut free,
        )
    };
    if ok == 0 {
        return Err(ProbeError::last("GetDiskFreeSpaceExW"));
    }

    // Windows reports bytes directly, so unlike `statvfs` there is no block size
    // to multiply by and no zero-unit case to defend against.
    Ok(RawDiskSpace {
        total,
        available,
        free,
    })
}
