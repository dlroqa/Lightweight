//! Log targets for the categories spec section 26 requires.
//!
//! Constants rather than string literals at each call site: the operator-facing
//! filter syntax is `HERMES_LOG=hermes::inference=debug`, and that only works
//! if the target strings are spelled identically everywhere. A typo in a
//! literal produces a target nobody can filter on and nobody notices.

/// Process startup, configuration and privacy mode.
pub const STARTUP: &str = "hermes::startup";

/// CPU backend initialisation, engine discovery, ISA detection.
pub const BACKEND: &str = "hermes::backend";

/// Model loading and unloading.
pub const MODEL: &str = "hermes::model";

/// Inference requests and generation errors. Never carries prompt text.
pub const INFERENCE: &str = "hermes::inference";

/// RAM estimation, admission verdicts and memory pressure warnings.
pub const MEMORY: &str = "hermes::memory";

/// HTTP requests to the OpenAI-compatible and control APIs.
pub const API: &str = "hermes::api";

/// Scheduler queue transitions.
pub const SCHEDULER: &str = "hermes::scheduler";

/// Model downloads and integrity verification.
pub const DOWNLOAD: &str = "hermes::download";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_target_shares_the_filterable_prefix() {
        // `HERMES_LOG=hermes=debug` must reach all of them.
        for target in [
            STARTUP, BACKEND, MODEL, INFERENCE, MEMORY, API, SCHEDULER, DOWNLOAD,
        ] {
            assert!(target.starts_with("hermes::"), "{target} is not filterable");
        }
    }
}
