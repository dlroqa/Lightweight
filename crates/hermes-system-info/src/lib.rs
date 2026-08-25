//! Facts about the machine we are running on, and where we are allowed to
//! write.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Production code must never panic (spec section 27). A test, however, reports
// failure *by* panicking, so the deny above would otherwise force every
// assertion helper into needless error plumbing.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod cpu;
pub mod disk;
pub mod load;
pub mod memory;
pub mod network;
pub mod paths;

pub use cpu::{CpuInfo, IsaFeature};
pub use disk::{DiskError, DiskSpace, space_for};
pub use load::{CpuTimes, LoadError, cpu_times};
pub use memory::{
    FailingMemoryProbe, FixedMemoryProbe, MemoryError, MemoryProbe, MemorySnapshot,
    SystemMemoryProbe,
};
pub use network::{NetworkError, reachable_addresses};
pub use paths::{DataPaths, PathsError};
