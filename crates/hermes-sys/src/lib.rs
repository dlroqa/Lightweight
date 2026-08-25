//! The workspace's only platform FFI.
//!
//! `hermes-system-info` is `#![forbid(unsafe_code)]`, and that is a property
//! worth keeping: it is the crate every admission decision reads its numbers
//! from. On Linux it keeps that promise for free, because `/proc` is text and
//! `rustix` wraps `statvfs` safely upstream. macOS and Windows publish the same
//! numbers only through C APIs, so the `unsafe` has to live somewhere.
//!
//! It lives here, and only here. Every call is a single `#[allow(unsafe_code)]`
//! block with a `SAFETY:` note above it, in the same shape
//! `hermes-backend-llamacpp` already uses for its two `pre_exec` calls. The
//! rule this crate exists to preserve: a reader auditing the workspace's unsafe
//! surface has two files to read, not fifteen.
//!
//! Nothing here compiles on Linux.

#![deny(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[cfg(any(target_os = "macos", windows))]
mod error;
#[cfg(any(target_os = "macos", windows))]
pub use error::ProbeError;

#[cfg(target_os = "macos")]
mod sysctl;

#[cfg(any(target_os = "macos", windows))]
pub mod memory;

#[cfg(any(target_os = "macos", windows))]
pub mod topology;

#[cfg(windows)]
pub mod disk;

#[cfg(any(target_os = "macos", windows))]
pub mod net;

#[cfg(any(target_os = "macos", windows))]
pub mod process;
