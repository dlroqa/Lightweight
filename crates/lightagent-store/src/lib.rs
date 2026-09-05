//! Persistent agent sessions.
//!
//! A [`Session`] is one conversation with its run history and per-run tool
//! history, stored as a single owner-private JSON file under a profile's
//! `sessions/` directory. The store mirrors the conventions the sibling
//! `lightweight-store` proved:
//!
//! * **ids are generated, never accepted** — an id becomes a filename, so taking
//!   one from a caller would take a path from a caller; anything not of the
//!   generated shape is [`StoreError::MalformedId`], not a not-found.
//! * **one damaged record costs one record** — a file that will not parse is
//!   skipped in a listing rather than failing it.
//! * **listing bounds its work** — entries are ordered by modification time
//!   before any file is opened, then re-sorted on the recorded `updated_at`, so
//!   a backup restore that rewrites every mtime keeps the order the user knows.
//! * **owner-private** — files are written `0600` under a `0700` directory
//!   through `lightagent-core`'s atomic path.
//! * **history-off refuses writes and still allows reads** — a store built with
//!   history disabled persists nothing but reads back everything already saved.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod error;
mod session;

pub use error::StoreError;
pub use session::{
    RunRecord, Session, SessionId, SessionStore, SessionSummary, StoredMessage, ToolHistoryEntry,
};
