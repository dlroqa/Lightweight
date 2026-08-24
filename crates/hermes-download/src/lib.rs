//! Resumable, digest-verified HTTP downloads.
//!
//! Two things in this workspace fetch large files over the network and then
//! trust them: the engine installer, which downloads a binary and executes it,
//! and the model catalog, which downloads weights and loads them. They need the
//! same guarantees — resume, streamed hashing, refusal to leave an unverified
//! file where a later run could pick it up — so they share one implementation
//! rather than each keeping a copy that drifts.
//!
//! The crate deliberately knows nothing about engines or models. It takes a
//! URL, a destination, an optional digest and a progress callback, and reports
//! what it wrote.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod error;
mod fetch;
mod tls;

pub use error::DownloadError;
pub use fetch::{Fetch, Fetched, ProgressSink, fetch, hash_file, partial_path};
pub use tls::ensure_provider;

/// An HTTP client with the crypto provider installed and a user agent set.
///
/// Every caller in the workspace builds its client through here, because
/// `reqwest::Client::builder().build()` **panics** rather than erroring when no
/// rustls provider has been installed — see [`ensure_provider`].
pub fn client(user_agent: &str) -> Result<reqwest::Client, DownloadError> {
    ensure_provider();
    reqwest::Client::builder()
        .user_agent(user_agent.to_owned())
        .build()
        .map_err(|err| DownloadError::Failed {
            what: "an HTTP client".to_owned(),
            reason: err.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_can_be_built_without_taking_the_process_down() {
        // The regression this guards: reqwest panics when no crypto provider
        // is installed, so any path that built a client without calling
        // `ensure_provider` first would abort the process rather than return.
        assert!(client("hermes-test").is_ok());
    }
}
