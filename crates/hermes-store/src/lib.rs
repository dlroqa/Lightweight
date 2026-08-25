//! What the user accumulates: their conversations and their settings.
//!
//! Separate from `hermes-catalog`, which keeps what the *machine* accumulates.
//! The two look alike — a directory, atomic writes, a JSON document — and are
//! different in the way that matters: a model can be downloaded again, and a
//! conversation cannot.
//!
//! That difference is why writes here are owner-only. The gateway redacts
//! prompts from its log by default; writing the same words to a world-readable
//! file would make that redaction decorative.
//!
//! Neither store locks. A single gateway plus the panel it serves is the shape
//! this is built for, which is the same caveat the model catalog carries: every
//! write is atomic, so a file is never half-written, but two processes writing
//! the same conversation at once would have one of them win.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

/// Atomic, owner-only writes.
///
/// Public because a third copy of it was the alternative. `hermes-catalog` has
/// its own, written first and without the permission hardening; this one added
/// that, and the module doc there records why the duplication was accepted at
/// the time. A crate that wants the same guarantees now reuses these rather
/// than making the same decision a third time.
pub mod atomic;
pub mod conversations;
pub mod error;
pub mod settings;

pub use conversations::{Conversation, ConversationStore, ConversationSummary, StoredMessage};
pub use error::StoreError;
pub use settings::{GatewaySettings, Settings, SettingsStore};
