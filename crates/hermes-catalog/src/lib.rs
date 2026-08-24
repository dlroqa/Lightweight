//! The model catalog: what this machine has, and how it got it.
//!
//! Separate from the gateway on purpose. `hermes-gateway` is the HTTP surface,
//! and importing, listing and removing a model must work with no server
//! running — `hermes models list` is a question about this machine, not about a
//! process. The gateway's own `catalog` module is a different thing with a
//! similar name: it tracks the one model that is *resident in the engine right
//! now*, which is a runtime fact, not a stored one.
//!
//! What this crate is careful about:
//!
//! * **A catalog write can never half-happen.** See [`store`].
//! * **How much was promised about a file's bytes is recorded per model**, and
//!   never rounded up to "verified". See [`record::Integrity`].
//! * **A record outlives its file.** A model whose file has been moved away
//!   comes back as absent, not as deleted, because an unmounted drive is not a
//!   reason to forget what a user installed.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod error;
pub mod hf;
pub mod install;
pub mod manifest;
pub mod record;
pub mod store;

pub use error::CatalogError;
pub use install::{AddModel, InstallProgress, Installer, Plan, Scanned, read_header};
pub use manifest::CatalogModel;
pub use record::{InstalledModel, Integrity, Source};
pub use store::CatalogStore;
