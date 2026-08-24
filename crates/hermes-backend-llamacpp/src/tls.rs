//! One-time installation of the rustls crypto provider.
//!
//! The implementation lives in [`hermes_download`], because installing the
//! provider is a precondition of building *any* HTTP client in this workspace
//! and two `OnceLock`s would each think they had done the job. This module
//! stays as the name the supervisor and the upstream client already call.
//!
//! Why it is needed at all: `rustls` is built here with **no default
//! provider**, deliberately — its usual default is `aws-lc-rs`, whose build
//! script requires CMake, which cannot be installed on the target machine
//! without sudo. `ring` is selected instead, and
//! `reqwest::Client::builder().build()` **panics** rather than erroring when no
//! provider has been installed.
//!
//! It applies even to plain-HTTP clients. The supervisor only ever talks to
//! `127.0.0.1` with no TLS at all, but the builder checks for a provider
//! regardless of the scheme it will end up using.

/// Install the `ring` provider, once per process.
///
/// Idempotent and safe to call from anywhere, including concurrently.
pub fn ensure_provider() {
    hermes_download::ensure_provider();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn building_a_client_after_installing_the_provider_does_not_panic() {
        // This is the regression: reqwest panics rather than erroring when no
        // provider is present, so a client built on a path that skipped the
        // installation would take the process down.
        ensure_provider();
        assert!(reqwest::Client::builder().build().is_ok());
    }

    #[test]
    fn installing_more_than_once_is_harmless() {
        ensure_provider();
        ensure_provider();
        assert!(reqwest::Client::builder().build().is_ok());
    }
}
