//! Who may call the gateway.
//!
//! The default is **disabled**, and that is a deliberate, bounded decision
//! rather than laziness: the gateway binds loopback, so the security boundary
//! is the network stack, not a bearer token. Clients rely on this. Hermes
//! *always* sends an `Authorization` header, and when no key is configured it
//! sends the literal string `Bearer no-key-required`
//! (`agent/runtime_provider.py:1144,1226,1408`). Rejecting that — or rejecting
//! a missing header — would break the client for a token that was never a
//! secret.
//!
//! The moment the bind address stops being loopback, that reasoning stops
//! holding, so [`AuthPolicy::for_bind`] forces a key on. Section 23 asks for
//! exactly that: LAN exposure is opt-in and authenticated, and it is not
//! something a user can end up with by accident.

use std::net::IpAddr;

/// How requests are authenticated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthPolicy {
    /// Any bearer value is accepted, and so is none at all. Never 401.
    Disabled,
    /// A key is required and compared in constant time.
    Required { key: String },
}

/// Why a request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthFailure {
    Missing,
    Invalid,
}

impl AuthFailure {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Missing => "an API key is required: send an Authorization: Bearer header",
            Self::Invalid => "the API key is not valid",
        }
    }
}

impl AuthPolicy {
    /// Choose a policy for a bind address.
    ///
    /// Loopback keeps whatever the user configured — including nothing. Any
    /// other address requires a key, and if none was configured the caller is
    /// told rather than being silently exposed.
    pub fn for_bind(address: IpAddr, configured_key: Option<String>) -> Result<Self, NeedsKey> {
        match (address.is_loopback(), configured_key) {
            (_, Some(key)) if !key.is_empty() => Ok(Self::Required { key }),
            (true, _) => Ok(Self::Disabled),
            (false, _) => Err(NeedsKey),
        }
    }

    /// Check one request's `Authorization` header value.
    pub fn check(&self, header: Option<&str>) -> Result<(), AuthFailure> {
        let Self::Required { key } = self else {
            // Disabled accepts everything, including a missing header. This is
            // the branch `Bearer no-key-required` lands in.
            return Ok(());
        };

        let presented = header
            .and_then(|value| {
                // The scheme is case-insensitive per RFC 7235, and clients do
                // vary.
                let (scheme, token) = value.split_once(' ')?;
                scheme
                    .eq_ignore_ascii_case("bearer")
                    .then_some(token.trim())
            })
            .ok_or(AuthFailure::Missing)?;

        if constant_time_eq(presented.as_bytes(), key.as_bytes()) {
            Ok(())
        } else {
            Err(AuthFailure::Invalid)
        }
    }

    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::Required { .. })
    }
}

/// A non-loopback bind was requested with no key configured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("binding to a non-loopback address requires an API key")]
pub struct NeedsKey;

/// Compare two byte strings without leaking their contents through timing.
///
/// The length difference is unavoidable and harmless; what matters is that a
/// key which shares a prefix with the real one does not take measurably longer
/// to reject than one that differs in the first byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

/// Generate a key for a LAN bind.
///
/// 24 bytes from the operating system's entropy source, hex encoded. Used when
/// a user turns on network access and has not chosen a key themselves.
pub fn generate_key() -> Result<String, std::io::Error> {
    let mut bytes = [0_u8; 24];
    getrandom::fill(&mut bytes).map_err(|err| std::io::Error::other(err.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn the_clients_placeholder_key_is_accepted() {
        // Hermes sends this literal string when no key is configured. A 401
        // here breaks every request it makes.
        let policy = AuthPolicy::Disabled;
        assert!(policy.check(Some("Bearer no-key-required")).is_ok());
    }

    #[test]
    fn a_missing_header_is_accepted_when_auth_is_disabled() {
        assert!(AuthPolicy::Disabled.check(None).is_ok());
        assert!(AuthPolicy::Disabled.check(Some("nonsense")).is_ok());
    }

    #[test]
    fn a_required_key_must_match_exactly() {
        let policy = AuthPolicy::Required {
            key: "secret".into(),
        };
        assert!(policy.check(Some("Bearer secret")).is_ok());
        assert_eq!(
            policy.check(Some("Bearer wrong")),
            Err(AuthFailure::Invalid)
        );
        assert_eq!(policy.check(None), Err(AuthFailure::Missing));
        assert_eq!(
            policy.check(Some("no-key-required")),
            Err(AuthFailure::Missing),
            "a header with no bearer scheme is not a key"
        );
    }

    #[test]
    fn the_bearer_scheme_is_matched_case_insensitively() {
        let policy = AuthPolicy::Required {
            key: "secret".into(),
        };
        assert!(policy.check(Some("bearer secret")).is_ok());
        assert!(policy.check(Some("BEARER secret")).is_ok());
    }

    #[test]
    fn a_loopback_bind_needs_no_key_and_a_lan_bind_does() {
        // Section 23: exposure beyond this machine is opt-in and
        // authenticated, and cannot be reached by accident.
        assert_eq!(
            AuthPolicy::for_bind(IpAddr::V4(Ipv4Addr::LOCALHOST), None),
            Ok(AuthPolicy::Disabled)
        );
        assert_eq!(
            AuthPolicy::for_bind(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), None),
            Err(NeedsKey)
        );
        assert!(
            AuthPolicy::for_bind(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), Some("k".into()))
                .is_ok_and(|policy| policy.is_enabled())
        );
    }

    #[test]
    fn an_empty_configured_key_does_not_count_as_a_key() {
        // Otherwise `--api-key ""` would look like authentication while
        // accepting an empty token from anyone.
        assert_eq!(
            AuthPolicy::for_bind(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), Some(String::new())),
            Err(NeedsKey)
        );
    }

    #[test]
    fn comparison_does_not_short_circuit_on_a_shared_prefix() {
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abce"));
        assert!(!constant_time_eq(b"abcd", b"abcde"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn a_generated_key_is_long_and_hexadecimal() {
        let key = generate_key().expect("entropy");
        assert_eq!(key.len(), 48);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(key, generate_key().expect("entropy"));
    }
}
