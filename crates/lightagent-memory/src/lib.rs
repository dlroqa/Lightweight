//! Durable per-profile agent memory for the Lightagent runtime.
//!
//! Memory is what a profile carries between sessions: short facts the agent
//! writes with `memory.write` and recalls with `memory.search`. A small relevant
//! catalog is injected per request; cited session messages remain available via
//! `session.lookup`. Offline ranking is lexical, with optional semantic fusion
//! when an embeddings endpoint is configured.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod learning;
pub mod store;
pub mod tool;

pub use learning::{Candidate, candidates, retain};
pub use store::{Memory, MemorySource, MemoryStore, memory_path};
pub use tool::{MemoryReflect, MemorySearch, MemoryWrite, SessionLookup};
