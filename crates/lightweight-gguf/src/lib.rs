//! A bounded, panic-free reader for GGUF metadata.
//!
//! The application must be able to answer "what is this model, and will it fit
//! in RAM?" before loading anything, and it must survive being pointed at a
//! truncated download or a file that is not a model at all (spec section 27).
//! So this crate reads the header region only — architecture, geometry,
//! tokenizer, quantization, tensor shapes — and never touches tensor data.
//!
//! Reading is **metadata-driven**, as spec section 6 requires: architecture-
//! scoped keys are built by interpolating `general.architecture` into
//! `{arch}.block_count` and friends, so a new architecture needs no code change
//! to be inspected. There is no per-architecture branch anywhere in this crate.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use lightweight_gguf::GgufFile;
//!
//! let file = GgufFile::open("model.gguf")?;
//! println!("{} tensors, {} bytes of weights",
//!          file.tensors().len(),
//!          file.weight_bytes().unwrap_or(0));
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
// Production code must never panic. Tests report failure *by* panicking, and
// their arithmetic is over literals rather than over attacker-controlled
// lengths, so the denials above are lifted for them.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )
)]

pub mod architecture;
pub mod error;
pub mod metadata;
pub mod reader;
pub mod value;

// The fixture builder is a test helper. It is compiled under the `fixtures`
// feature as well as under `cfg(test)`, so it does not inherit the test-only
// relaxation above and needs its own. Its arithmetic is over literal fixture
// sizes, bounded by `MAX_FIXTURE_DATA_BYTES`, not over untrusted input.
#[cfg(any(test, feature = "fixtures"))]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]
pub mod fixture;

pub use error::GgufError;
pub use metadata::{ModelMetadata, QuantMix, QuantStat, TokenizerMeta};
pub use reader::{GgufFile, ReadLimits, TensorInfo};
pub use value::{ArraySummary, GgufValue, GgufValueType};
