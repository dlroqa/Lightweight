//! One-time installation of the rustls crypto provider.
//!
//! This is reproduced from `lightweight-download::tls` rather than imported: the
//! adapter must not depend on any `lightweight-*` crate. The fact it encodes is
//! the same one that file records — `rustls` here is built with **no default
//! provider** (its usual `aws-lc-rs` needs CMake, which is unavailable on the
//! target), so `ring` is installed explicitly.
//!
//! The catch that makes this non-optional: `reqwest::Client::builder().build()`
//! **panics** — it does not return an error — when no provider has been
//! installed, even for a plain-HTTP client. So [`ensure_provider`] runs before
//! any client is built, including in [`LightweightProvider::new`].
//!
//! [`LightweightProvider::new`]: crate::LightweightProvider::new

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
