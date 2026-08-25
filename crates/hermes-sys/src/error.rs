//! One error shape for every probe here.

/// A platform call that failed, and the name of the call.
///
/// The name is carried rather than formatted into a message because the caller
/// puts it in `MemoryError::Read { source_path, .. }`, whose whole point is to
/// name the thing that could not be read. `/proc/meminfo` is a path on Linux;
/// `GlobalMemoryStatusEx` is the equivalent fact on Windows.
#[derive(Debug)]
pub struct ProbeError {
    pub api: &'static str,
    pub source: std::io::Error,
}

impl ProbeError {
    pub(crate) fn last(api: &'static str) -> Self {
        Self {
            api,
            source: std::io::Error::last_os_error(),
        }
    }

    /// Only macOS builds construct this: its `sysctl` wrappers turn an
    /// unexpected result length into `EINVAL` themselves, where Windows always
    /// has a real last-error to report.
    #[cfg(target_os = "macos")]
    pub(crate) fn from_raw(api: &'static str, code: i32) -> Self {
        Self {
            api,
            source: std::io::Error::from_raw_os_error(code),
        }
    }

    /// A Win32 status that is already an error code rather than a flag telling
    /// the caller to go and ask `GetLastError`.
    #[cfg(windows)]
    pub(crate) fn from_win32(api: &'static str, code: u32) -> Self {
        Self {
            api,
            source: i32::try_from(code).map_or_else(
                |_| std::io::Error::other(format!("win32 error {code}")),
                std::io::Error::from_raw_os_error,
            ),
        }
    }
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed: {}", self.api, self.source)
    }
}

impl std::error::Error for ProbeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}
