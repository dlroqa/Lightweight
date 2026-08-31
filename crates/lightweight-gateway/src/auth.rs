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

use lightweight_store::ApiKeyRecord;
use std::net::IpAddr;

/// How requests are authenticated.
///
/// `Debug` is implemented by hand, and that is not a style choice. This type
/// is reachable from `GatewayState`'s `Debug`, so a derived one would put the
/// API key into any log line written as `?state` — the same accident
/// [`lightweight_core::Private`] exists to prevent for prompts. The guarantee is
/// structural here for the same reason: a rule to remember is not a control.
#[derive(Clone, PartialEq, Eq)]
pub enum AuthPolicy {
    /// Any bearer value is accepted, and so is none at all. Never 401.
    Disabled,
    /// A key is required. A request is admitted if it presents the static key
    /// (from `--api-key` or the environment), if there is one, or any of the
    /// named keys from the store. All comparisons are constant time.
    Required {
        /// The single static key, if one was configured out of band.
        static_key: Option<String>,
        /// The named keys from the store, each carrying its own limit. Only the
        /// hash of each travels here; the plaintext never does.
        named: Vec<ApiKeyRecord>,
    },
}

impl std::fmt::Debug for AuthPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("Disabled"),
            // Neither the static key nor even its length: both are hints about
            // the secret, and nothing above this line has any use for them. The
            // named records hold only hashes, but their count is said so a log
            // is not silent about whether keys exist at all.
            Self::Required { named, .. } => f
                .debug_struct("Required")
                .field("static_key", &"<redacted>")
                .field("named_keys", &named.len())
                .finish(),
        }
    }
}

/// Who a request turned out to be, once its credentials checked out.
///
/// `None` for the anonymous cases — auth disabled on loopback, or the static
/// key — which carry no per-key identity. `Some` names the store key that
/// matched, so the caller can attribute the request and apply that key's limit.
pub type Caller = Option<ApiKeyRecord>;

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
        Self::for_binds(std::slice::from_ref(&address), configured_key)
    }

    /// Choose one policy for a set of bind addresses.
    ///
    /// The policy is taken from the **most exposed** address in the set: if any
    /// of them is off-loopback, a key is required on all of them. The
    /// alternative — deciding per listener — would let a loopback bind quietly
    /// relax a bind on a LAN or an overlay network, which is precisely the
    /// mistake this type exists to make impossible.
    ///
    /// Note what is *not* consulted: the interface, the network, or which
    /// product assigned the address. Loopback or not is the whole distinction,
    /// which is why a LAN address, a CGNAT address from a mesh VPN and a
    /// unique-local IPv6 address all behave identically.
    pub fn for_binds(
        addresses: &[IpAddr],
        configured_key: Option<String>,
    ) -> Result<Self, NeedsKey> {
        Self::build(addresses, configured_key, Vec::new())
    }

    /// Choose a policy from the bind set, a static key and the stored keys.
    ///
    /// A credential is present when the static key is set and non-empty, or when
    /// at least one named key is stored. An exposed bind with no credential of
    /// either kind is the refusal case, unchanged from when there was only ever
    /// one key.
    pub fn build(
        addresses: &[IpAddr],
        static_key: Option<String>,
        named: Vec<ApiKeyRecord>,
    ) -> Result<Self, NeedsKey> {
        let exposed = addresses.iter().any(|address| !address.is_loopback());
        let static_key = static_key.filter(|key| !key.is_empty());
        let has_credential = static_key.is_some() || !named.is_empty();
        match (has_credential, exposed) {
            (true, _) => Ok(Self::Required { static_key, named }),
            (false, true) => Err(NeedsKey),
            (false, false) => Ok(Self::Disabled),
        }
    }

    /// A policy requiring exactly one static key and no named keys. The shape a
    /// bare `--api-key` produces, and what the tests build.
    pub fn with_static_key(key: String) -> Self {
        Self::Required {
            static_key: Some(key),
            named: Vec::new(),
        }
    }

    /// Check one request's `Authorization` header value.
    pub fn check(&self, header: Option<&str>) -> Result<(), AuthFailure> {
        self.identify(header).map(|_| ())
    }

    /// Check the header and report which key admitted the request.
    ///
    /// The static key is tried first — it is the one a loopback deployment most
    /// often uses — and only then the named keys. A named match returns that
    /// key's record so the caller can attribute the request and meter it; every
    /// other admitted case is anonymous.
    pub fn identify(&self, header: Option<&str>) -> Result<Caller, AuthFailure> {
        let Self::Required { static_key, named } = self else {
            // Disabled accepts everything, including a missing header. This is
            // the branch `Bearer no-key-required` lands in.
            return Ok(None);
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

        if let Some(key) = static_key
            && constant_time_eq(presented.as_bytes(), key.as_bytes())
        {
            return Ok(None);
        }

        lightweight_store::verify_against(named, presented)
            .map(Some)
            .ok_or(AuthFailure::Invalid)
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
///
/// The generator itself now lives in `lightweight-store` beside the key store
/// that is its main caller; this re-export keeps the one existing caller here
/// — [`exposed_without_key`](crate) — unchanged.
pub use lightweight_store::generate_secret as generate_key;

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    // Addresses used below come from the ranges reserved for documentation
    // (RFC 5737, RFC 3849) or from a range's base address, so that no address
    // belonging to a real machine or network is written into this repository.
    /// RFC 5737 documentation range.
    const DOC_A: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));
    /// A second RFC 5737 range.
    const DOC_B: IpAddr = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 4));
    /// The base of RFC 6598 shared address space, which mesh VPNs hand out.
    const CGNAT_BASE: IpAddr = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
    /// The base of the RFC 4193 unique-local IPv6 range.
    const ULA_BASE: IpAddr = IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1));
    /// RFC 1918, which every LAN and most overlays use.
    const PRIVATE_BASE: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));

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
        let policy = AuthPolicy::with_static_key("secret".into());
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
    fn a_named_key_admits_and_identifies_its_caller() {
        // A store-minted key must verify through the policy and hand back its
        // own record, which is what lets the request be attributed and metered.
        let dir = std::env::temp_dir().join(format!(
            "hermes-auth-named-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let store = lightweight_store::ApiKeyStore::new(dir.join("api-keys.json"));
        let (record, full) = store
            .create("harness", lightweight_store::RateLimit::default())
            .expect("create");

        let policy = AuthPolicy::build(
            std::slice::from_ref(&IpAddr::V4(Ipv4Addr::LOCALHOST)),
            None,
            store.list().expect("list"),
        )
        .expect("build");

        let header = format!("Bearer {full}");
        let caller = policy.identify(Some(&header)).expect("identify");
        assert_eq!(caller.map(|r| r.id), Some(record.id));
        assert_eq!(
            policy.identify(Some("Bearer sk-lw-wrong")),
            Err(AuthFailure::Invalid)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_static_key_and_named_keys_coexist() {
        // A deployment can keep its old --api-key while adding named ones; both
        // admit, and the static one stays anonymous.
        let policy = AuthPolicy::Required {
            static_key: Some("legacy".into()),
            named: Vec::new(),
        };
        assert_eq!(policy.identify(Some("Bearer legacy")).expect("ok"), None);
    }

    #[test]
    fn the_bearer_scheme_is_matched_case_insensitively() {
        let policy = AuthPolicy::with_static_key("secret".into());
        assert!(policy.check(Some("bearer secret")).is_ok());
        assert!(policy.check(Some("BEARER secret")).is_ok());
    }

    #[test]
    fn a_loopback_bind_needs_no_key_and_any_other_bind_does() {
        // Section 23: exposure beyond this machine is opt-in and
        // authenticated, and cannot be reached by accident.
        assert_eq!(
            AuthPolicy::for_bind(IpAddr::V4(Ipv4Addr::LOCALHOST), None),
            Ok(AuthPolicy::Disabled)
        );
        assert_eq!(AuthPolicy::for_bind(DOC_A, None), Err(NeedsKey));
        assert!(
            AuthPolicy::for_bind(DOC_A, Some("k".into())).is_ok_and(|policy| policy.is_enabled())
        );
    }

    #[test]
    fn every_network_is_treated_the_same_way() {
        // The property that makes this work on any fabric: a LAN address, the
        // shared range a mesh VPN hands out, a unique-local IPv6 address and an
        // ordinary one are indistinguishable here. Nothing knows which product
        // assigned an address, and nothing should.
        for address in [PRIVATE_BASE, CGNAT_BASE, ULA_BASE, DOC_A, DOC_B] {
            assert_eq!(
                AuthPolicy::for_bind(address, None),
                Err(NeedsKey),
                "{address} was treated as safe without a key"
            );
            assert!(
                AuthPolicy::for_bind(address, Some("k".into()))
                    .is_ok_and(|policy| policy.is_enabled()),
                "{address} did not enable auth with a key"
            );
        }
        for address in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ] {
            assert_eq!(
                AuthPolicy::for_bind(address, None),
                Ok(AuthPolicy::Disabled),
                "{address} should not have demanded a key"
            );
        }
    }

    #[test]
    fn one_exposed_bind_makes_the_whole_set_require_a_key() {
        // A machine on an overlay usually holds several addresses at once.
        // Deciding per listener would let a loopback bind quietly relax an
        // exposed one.
        let mixed = [IpAddr::V4(Ipv4Addr::LOCALHOST), CGNAT_BASE];
        assert_eq!(AuthPolicy::for_binds(&mixed, None), Err(NeedsKey));
        assert!(
            AuthPolicy::for_binds(&mixed, Some("k".into())).is_ok_and(|policy| policy.is_enabled())
        );

        let local = [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ];
        assert_eq!(
            AuthPolicy::for_binds(&local, None),
            Ok(AuthPolicy::Disabled)
        );
    }

    #[test]
    fn the_key_never_appears_in_debug_output() {
        // `GatewayState`'s Debug prints its config, which holds this policy, so
        // a derived Debug here would write the key into any `?state` log line.
        let policy = AuthPolicy::with_static_key("super-secret-key-value".into());
        let rendered = format!("{policy:?}");
        assert!(!rendered.contains("super-secret-key-value"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        assert_eq!(format!("{:?}", AuthPolicy::Disabled), "Disabled");
    }

    #[test]
    fn an_empty_configured_key_does_not_count_as_a_key() {
        // Otherwise `--api-key ""` would look like authentication while
        // accepting an empty token from anyone.
        assert_eq!(
            AuthPolicy::for_bind(PRIVATE_BASE, Some(String::new())),
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
