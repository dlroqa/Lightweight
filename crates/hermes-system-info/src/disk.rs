//! Free space on the filesystem holding a given path.
//!
//! M6a deferred this deliberately, and the reason is worth restating because it
//! is the reason this module looks the way it does: `statvfs` is a libc call,
//! calling it directly needs `unsafe`, and every crate in this workspace except
//! the process supervisor sets `forbid(unsafe_code)`. Weakening that to save a
//! failed download was the wrong trade, so a full disk was reported from
//! `ENOSPC` as it happened instead.
//!
//! `rustix` settles it rather than trading it away: the syscall is wrapped
//! safely upstream, the crate has no build script, and on Linux it issues the
//! syscall through its own `linux_raw` backend — so nothing here needs `unsafe`
//! and the dependency policy is untouched.
//!
//! Two numbers matter and they are not the same number:
//!
//! * **`available` is the budget.** `f_bavail` excludes the blocks reserved for
//!   root — 5% of an ext4 filesystem by default — which an unprivileged process
//!   cannot spend no matter what the free count says.
//! * **`free` is context.** `f_bfree` includes that reserve. It is reported so a
//!   disk that looks full to us but not to `df` is explicable, and it is never
//!   what a decision is made against.
//!
//! The same asymmetry [`crate::memory`] applies to `MemAvailable` against
//! `MemFree`, for the same reason: being wrong in the pessimistic direction
//! refuses a download that would have fitted, and being wrong in the optimistic
//! direction fills the disk.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use hermes_core::{Actionable, Bytes, ErrorKind, Remedy, RemedyAction, SettingsSection};

#[derive(Debug, thiserror::Error)]
pub enum DiskError {
    #[error("could not read filesystem information for {path}: {source}")]
    Query {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("disk probing is not implemented for this platform ({platform})")]
    UnsupportedPlatform { platform: &'static str },
}

impl Actionable for DiskError {
    fn code(&self) -> &'static str {
        match self {
            Self::Query { .. } => "disk_probe_failed",
            Self::UnsupportedPlatform { .. } => "disk_probe_unsupported",
        }
    }

    fn kind(&self) -> ErrorKind {
        ErrorKind::Internal
    }

    fn remedies(&self) -> Vec<Remedy> {
        vec![Remedy::new(
            "Check free space yourself (`df -h`) before downloading a model",
            RemedyAction::OpenSettings {
                section: SettingsSection::Storage,
            },
        )]
    }
}

/// Space on one filesystem, read at one instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskSpace {
    pub total: Bytes,
    /// What an unprivileged process can actually write. This is the budget.
    pub available: Bytes,
    /// Unused blocks including the root reserve. Context, never the budget.
    pub free: Bytes,
}

impl DiskSpace {
    /// Space in use, as `total - free`.
    ///
    /// Computed from `free` rather than `available` so it matches what `df`
    /// prints. Using the budget here would count the root reserve as consumed
    /// and report a fuller disk than the operator sees anywhere else.
    pub fn used(&self) -> Bytes {
        self.total.saturating_sub(self.free)
    }

    /// Fraction of the filesystem in use, 0.0 to 1.0.
    pub fn pressure(&self) -> f64 {
        self.used().fraction_of(self.total)
    }

    /// Whether `wanted` bytes would fit in the spendable budget.
    ///
    /// The question a download asks before it starts. Deliberately compared
    /// against `available`: a transfer that fits only in the root reserve does
    /// not fit.
    pub fn fits(&self, wanted: Bytes) -> bool {
        self.available >= wanted
    }
}

/// Space on the filesystem holding `path`.
///
/// `path` need not be a directory, but it must exist — `statvfs` resolves it,
/// and a path that is not there yet produces `ENOENT` rather than the numbers
/// for its eventual parent. A caller asking about a directory it is about to
/// create should ask about the nearest existing ancestor.
pub fn space_for(path: &Path) -> Result<DiskSpace, DiskError> {
    platform::space_for(path)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform {
    use super::{Bytes, DiskError, DiskSpace, Path};

    pub(super) fn space_for(path: &Path) -> Result<DiskSpace, DiskError> {
        let stats = rustix::fs::statvfs(path).map_err(|errno| DiskError::Query {
            path: path.to_path_buf(),
            source: std::io::Error::from_raw_os_error(errno.raw_os_error()),
        })?;

        // POSIX says block counts are in units of `f_frsize`. Some filesystems
        // report it as zero, in which case `f_bsize` is the only unit on offer;
        // if both are zero there is no scale to multiply by, and every figure
        // would come out as zero — which reads as a full disk rather than as an
        // unanswerable question.
        let unit = match (stats.f_frsize, stats.f_bsize) {
            (0, 0) => {
                return Err(DiskError::Query {
                    path: path.to_path_buf(),
                    source: std::io::Error::other(
                        "the filesystem reported a block size of zero, so its \
                         free-space counts have no scale",
                    ),
                });
            }
            (0, bsize) => bsize,
            (frsize, _) => frsize,
        };

        // Saturating rather than wrapping: a corrupt or synthetic filesystem
        // reporting an absurd block count must not wrap around into a small
        // number that looks like a plausible answer.
        Ok(DiskSpace {
            total: Bytes(stats.f_blocks.saturating_mul(unit)),
            available: Bytes(stats.f_bavail.saturating_mul(unit)),
            free: Bytes(stats.f_bfree.saturating_mul(unit)),
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::{DiskError, DiskSpace, Path};

    /// Not implemented off Unix.
    ///
    /// An error rather than a guess, for the reason [`crate::memory`] gives:
    /// "nothing to report" and "I did not look" are opposite answers, and a
    /// pre-flight check that silently approves every download is worse than no
    /// pre-flight check at all. Windows needs `GetDiskFreeSpaceEx`, which
    /// belongs to the cross-platform milestone where it can be verified.
    pub(super) fn space_for(_path: &Path) -> Result<DiskSpace, DiskError> {
        Err(DiskError::UnsupportedPlatform {
            platform: std::env::consts::OS,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_figures_are_consistent() {
        let space = DiskSpace {
            total: Bytes::from_gib(100),
            available: Bytes::from_gib(20),
            free: Bytes::from_gib(25),
        };
        // `used` is computed from `free`, so it matches `df` rather than
        // counting the root reserve as consumed.
        assert_eq!(space.used(), Bytes::from_gib(75));
        assert!((space.pressure() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn a_download_that_fits_only_in_the_root_reserve_does_not_fit() {
        // The whole reason `available` and `free` are kept apart.
        let space = DiskSpace {
            total: Bytes::from_gib(100),
            available: Bytes::from_gib(2),
            free: Bytes::from_gib(7),
        };
        assert!(space.fits(Bytes::from_gib(2)));
        assert!(!space.fits(Bytes::from_gib(5)));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn probing_this_machine_returns_plausible_numbers() {
        let space = space_for(Path::new(".")).expect("probe the working directory");
        assert!(space.total.get() > 0, "a mounted filesystem has a size");
        assert!(space.free <= space.total);
        assert!(
            space.available <= space.free,
            "the spendable budget cannot exceed the free count it is drawn from"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_path_that_does_not_exist_is_an_error_not_a_zero() {
        // A zero would read as a full disk and refuse every download.
        let missing = Path::new("/nonexistent-path-for-hermes-disk-probe-test");
        assert!(space_for(missing).is_err());
    }
}
