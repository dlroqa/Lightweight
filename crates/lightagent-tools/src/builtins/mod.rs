//! The built-in tools.
//!
//! [`DateTimeNow`] is the minimal read-only tool that proves the whole loop;
//! [`AgentDelegate`] starts a bounded worker run under another profile through
//! the same core loop; [`WebFetch`] and [`WebSearch`] reach the network for
//! read-only data under a guard, when web access is enabled; [`FsRead`],
//! [`FsList`], [`FsWrite`] and [`TerminalRun`] work within a confined workspace,
//! when filesystem/terminal access is enabled.

pub mod datetime;
pub mod delegate;
pub mod fs;
pub mod terminal;
pub mod web;

pub use datetime::DateTimeNow;
pub use delegate::AgentDelegate;
pub use fs::{FsList, FsRead, FsWrite};
pub use terminal::TerminalRun;
pub use web::{WebFetch, WebSearch};
