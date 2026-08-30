//! A llama.cpp backend that runs the engine as a supervised child process.
#![deny(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod acquire;
pub mod backend;
pub mod manifest;
pub mod supervisor;
pub mod tls;
pub mod upstream;
