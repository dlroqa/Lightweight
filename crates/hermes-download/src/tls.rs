//! One-time installation of the rustls crypto provider.
//!
//! `rustls` is built here with **no default provider**, deliberately: its usual
//! default is `aws-lc-rs`, whose build script requires CMake, which cannot be
//! installed on the target machine without sudo. `ring` is selected instead.
//!
//! The catch is that `reqwest::Client::builder().build()` **panics** — not
//! returns an error — when no provider has been installed. That makes this a
//! process-wide precondition rather than a detail of any one component, so
//! every place that builds an HTTP client calls [`ensure_provider`] first.
//!
//! It applies even to plain-HTTP clients. The engine supervisor only ever talks
//! to `127.0.0.1` with no TLS at all, but the builder checks for a provider
//! regardless of the scheme it will end up using.

use std::sync::OnceLock;

/// Install the `ring` provider, once per process.
///
/// Idempotent and safe to call from anywhere, including concurrently.
pub fn ensure_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        // Fails only when a provider is already installed, which satisfies the
        // requirement just as well.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
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
