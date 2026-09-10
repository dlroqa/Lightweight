//! Indexed and realtime retrieval for the Lightagent runtime.
//!
//! [`chunk`] splits a document at readable boundaries, [`RagStore`] persists its
//! chunks, BM25 provides a fast model-free sparse ranking, and [`RagSearch`]
//! exposes indexed retrieval as a `rag.search` tool.
//! [`RealtimeRag`] composes live web search, guarded concurrent fetches, sparse
//! ranking and an optional bounded semantic pass into one call designed for
//! small quantized generators.
//!
//! Retrieval works offline as lexical BM25. When an OpenAI-compatible embedding
//! endpoint is configured, its dense ranking is fused with BM25 through
//! Reciprocal Rank Fusion. [`HashingEmbedder`] remains the dependency-free vector
//! used for persisted format compatibility and near-duplicate suppression.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod chunk;
pub mod embed;
pub mod realtime;
pub mod store;
pub mod tool;

pub use chunk::chunk;
pub use embed::{DIM, Embedder, HashingEmbedder, SemanticEmbedder, cosine};
pub use realtime::RealtimeRag;
pub use store::{Hit, Passage, RagStore, index_path, search_passages};
pub use tool::RagSearch;
