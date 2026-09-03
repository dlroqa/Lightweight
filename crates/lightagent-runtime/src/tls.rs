//! One-time installation of the rustls crypto provider.
//!
//! Reproduced from `lightagent-provider-lightweight::tls` (which reproduced it
//! from `lightweight-download::tls`) rather than imported, so this crate keeps
//! its own dependency footprint and gains no edge into the provider adapter.
//! `rustls` here is built with **no default provider** — its usual `aws-lc-rs`
//! needs CMake, which is unavailable on the target — so `ring` is installed
//! explicitly.
//!
//! It is not optional: `reqwest::Client::builder().build()` **panics** (it does
//! not return an error) when no provider has been installed, even for a plain
//! HTTP client. So [`ensure_provider`] runs before any client is built.

use std::sync::OnceLock;

/// Install the `ring` provider, once per process. Idempotent and thread-safe.
pub fn ensure_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        // Fails only when a provider is already installed, which satisfies the
        // precondition just as well.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_builds_after_the_provider_is_installed() {
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
