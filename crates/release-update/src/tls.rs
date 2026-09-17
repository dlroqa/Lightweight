//! One-time installation of the rustls crypto provider.
//!
//! Reproduced from `lightweight-download::tls` and
//! `lightagent-provider-lightweight::tls` rather than imported: this crate is
//! shared by both CLI families and depends on neither. `rustls` here is built
//! with **no default provider** — its usual `aws-lc-rs` needs CMake, which is
//! unavailable on the target — so `ring` is installed explicitly.
//!
//! It is not optional: `reqwest::Client::builder().build()` **panics** (it does
//! not return an error) when no provider has been installed. So
//! [`ensure_provider`] runs before any client is built.

use std::sync::OnceLock;

/// Install the `ring` provider, once per process. Idempotent and thread-safe.
pub(crate) fn ensure_provider() {
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
}
