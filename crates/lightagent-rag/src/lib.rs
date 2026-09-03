//! Dependency-free lexical retrieval for the Lightagent runtime.
//!
//! A small RAG stack that needs no model and no new dependency: [`chunk`] splits
//! a document, [`HashingEmbedder`] turns text into a comparable vector by feature
//! hashing, [`RagStore`] persists the embedded chunks and searches them by
//! cosine similarity, and [`RagSearch`] exposes that as a `rag.search` tool.
//!
//! The retrieval is lexical (it matches shared words), not semantic (shared
//! meaning): a genuine embedding model would need either a new dependency or an
//! embeddings endpoint on the inference engine, both outside this additive build.
//! The [`Embedder`] trait is the seam where a semantic backend would slot in
//! without changing the store, the tool, or the on-disk format's shape.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod chunk;
pub mod embed;
pub mod store;
pub mod tool;

pub use chunk::chunk;
pub use embed::{DIM, Embedder, HashingEmbedder, SemanticEmbedder, cosine};
pub use store::{Hit, RagStore, index_path};
pub use tool::RagSearch;
