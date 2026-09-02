//! The built-in tools.
//!
//! [`DateTimeNow`] is the minimal read-only tool that proves the whole loop;
//! [`AgentDelegate`] starts a bounded worker run under another profile through
//! the same core loop.

pub mod datetime;
pub mod delegate;

pub use datetime::DateTimeNow;
pub use delegate::AgentDelegate;
