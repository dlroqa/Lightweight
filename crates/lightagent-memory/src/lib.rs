//! Durable per-profile agent memory for the Lightagent runtime.
//!
//! Memory is what a profile carries between sessions: short facts the agent
//! writes with `memory.write` and recalls with `memory.search`, plus a snapshot
//! of the most recent ones injected into the system prompt so recall is not
//! wholly the model's responsibility. Storage and recall reuse the dependency-free
//! lexical retriever from `lightagent-rag`, so memory adds no new dependency and
//! ranks by the same feature-hashed cosine as document search.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod store;
pub mod tool;

pub use store::{Memory, MemoryStore, memory_path};
pub use tool::{MemorySearch, MemoryWrite};
