//! RAM estimation and load admission control.
//!
//! See [`estimate`] for the reasoning behind the arithmetic, and
//! [`estimator::Estimator`] for the arithmetic itself.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod estimate;
pub mod estimator;

pub use estimate::{Budget, ComputeModel, Confidence, Estimate, Verdict};
pub use estimator::Estimator;
/// Re-exported from `hermes-core`: the estimator and the inference backend must
/// agree on exactly these parameters, so they share one definition.
pub use hermes_core::RuntimeParams;
