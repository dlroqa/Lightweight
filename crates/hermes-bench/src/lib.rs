//! What this machine actually does with a model.
//!
//! Everything else in this workspace either computes a number from metadata or
//! reports one the engine handed it. This crate is the only place that
//! *measures*, and that puts three obligations on it.
//!
//! **It measures the path a user's request takes.** The scenarios drive the
//! same [`InferenceBackend`] the gateway drives, through the same engine, with
//! the same runtime parameters. A harness that talked to the engine some other
//! way would report a number nobody can obtain by using the product.
//!
//! **It records what it measured, not what it concluded.** A run is raw
//! observations plus the exact parameters they were taken at, and the
//! prediction the estimator made for those same parameters. Nothing here fits
//! coefficients, decides a default, or rewrites a setting. The residual is left
//! in the file for a later pass to fit, and until then the estimator's shipped
//! defaults stand.
//!
//! **It records where it measured.** A tokens-per-second figure is a fact about
//! one machine and one engine build, and is meaningless without both. So every
//! run carries a machine and an engine fingerprint, and nothing in this
//! repository ever quotes a speed as a property of the software.
//!
//! What a run may not contain is the same list the metrics module refuses:
//! no prompt, no completion, no filesystem path, no hostname. The prompts are
//! generated here from a fixed pattern, so there is never any user text to
//! leak in the first place.

#![forbid(unsafe_code)]

pub mod error;
pub mod fit;
pub mod record;
pub mod runner;
pub mod store;

pub use error::BenchError;
pub use record::{
    BenchmarkRun, EngineFingerprint, MachineFingerprint, ModelFingerprint, Sample, Scenario,
};
pub use runner::{RunPlan, Runner};
pub use store::BenchmarkStore;
