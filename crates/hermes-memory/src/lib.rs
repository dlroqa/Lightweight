//! RAM estimation and load admission control.
//!
//! See [`estimate`] for the reasoning behind the arithmetic, and
//! [`estimator::Estimator`] for the arithmetic itself.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod estimate;
pub mod estimator;
pub mod params;

pub use estimate::{ComputeModel, Confidence, Estimate, Verdict};
pub use estimator::Estimator;
pub use params::RuntimeParams;
