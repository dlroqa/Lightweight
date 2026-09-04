//! Scoped bearer authentication.
//!
//! The API draws the same single distinction the inference gateway does:
//! loopback is trusted, anything else needs a key. An [`AuthConfig::open`] admits
//! every request (the default for a loopback bind and for tests); a keyed config
//! requires `Authorization: Bearer <key>` **and** that the key carries the
//! [`Scope`] the route needs, so a token minted for reading runs cannot start
//! them. Refusals are the two the plan calls for: `401` for a missing or wrong
//! key, `403` for a valid key that lacks the scope.

use std::collections::HashSet;

use axum::http::{HeaderMap, StatusCode};

/// A capability a route requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    RunsRead,
    RunsWrite,
    SessionsRead,
    SessionsWrite,
    ToolsRead,
    ApprovalsWrite,
    /// Grants every scope.
    Admin,
}

/// The API's authentication policy.
#[derive(Clone, Debug)]
pub struct AuthConfig {
    key: Option<String>,
    scopes: HashSet<Scope>,
}

impl AuthConfig {
    /// Admit every request (loopback, or a test). No key, no scope checks.
    pub fn open() -> Self {
        Self {
            key: None,
            scopes: HashSet::new(),
        }
    }

    /// Require `key`, granting exactly `scopes` (add [`Scope::Admin`] for all).
    pub fn keyed(key: impl Into<String>, scopes: impl IntoIterator<Item = Scope>) -> Self {
        Self {
            key: Some(key.into()),
            scopes: scopes.into_iter().collect(),
        }
    }

    /// Whether this policy requires a key.
    pub fn requires_key(&self) -> bool {
        self.key.is_some()
    }

    /// Authorize a request carrying `headers` for a route needing `required`.
    pub fn authorize(
        &self,
        headers: &HeaderMap,
        required: Scope,
    ) -> Result<(), (StatusCode, String)> {
        let Some(expected) = &self.key else {
            return Ok(()); // open policy: loopback is the boundary
        };
        let provided = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                // The auth scheme is case-insensitive per RFC 7235, and clients
                // vary — accept `Bearer`, `bearer`, `BEARER`.
                let (scheme, token) = value.split_once(' ')?;
                scheme
                    .eq_ignore_ascii_case("bearer")
                    .then_some(token.trim())
            });
        match provided {
            // Constant-time compare, so a key sharing a prefix with the real one
            // is not rejected measurably slower than one differing at the first
            // byte — the same protection the inference gateway gives its keys.
            Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {
                if self.scopes.contains(&Scope::Admin) || self.scopes.contains(&required) {
                    Ok(())
                } else {
                    Err((
                        StatusCode::FORBIDDEN,
                        format!("the key lacks the required scope {required:?}"),
                    ))
                }
            }
            _ => Err((
                StatusCode::UNAUTHORIZED,
                "a valid `Authorization: Bearer <key>` is required".to_owned(),
            )),
        }
    }
}

/// Compare two byte strings without leaking their contents through timing.
///
/// Reproduced from the inference gateway's own `constant_time_eq` rather than
/// imported — this crate depends on no `lightweight-*` crate — so the API's key
/// check has the same protection the gateway's does. The length difference is
/// unavoidable and harmless; what matters is that a key sharing a prefix with the
/// real one does not take measurably longer to reject than one that differs at
/// the first byte.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Ok(value) = format!("Bearer {token}").parse() {
            headers.insert("authorization", value);
        }
        headers
    }

    #[test]
    fn an_open_policy_admits_everything() {
        let auth = AuthConfig::open();
        assert!(auth.authorize(&HeaderMap::new(), Scope::RunsWrite).is_ok());
    }

    #[test]
    fn a_keyed_policy_needs_the_key() {
        let auth = AuthConfig::keyed("secret", [Scope::RunsRead]);
        let (status, _) = auth
            .authorize(&HeaderMap::new(), Scope::RunsRead)
            .unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = auth
            .authorize(&headers_with("wrong"), Scope::RunsRead)
            .unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            auth.authorize(&headers_with("secret"), Scope::RunsRead)
                .is_ok()
        );
    }

    #[test]
    fn a_valid_key_without_the_scope_is_forbidden() {
        let auth = AuthConfig::keyed("secret", [Scope::RunsRead]);
        let (status, _) = auth
            .authorize(&headers_with("secret"), Scope::RunsWrite)
            .unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn admin_grants_every_scope() {
        let auth = AuthConfig::keyed("secret", [Scope::Admin]);
        assert!(
            auth.authorize(&headers_with("secret"), Scope::SessionsWrite)
                .is_ok()
        );
    }

    #[test]
    fn the_bearer_scheme_is_case_insensitive() {
        let auth = AuthConfig::keyed("secret", [Scope::RunsRead]);
        for scheme in ["Bearer", "bearer", "BEARER"] {
            let mut headers = HeaderMap::new();
            if let Ok(value) = format!("{scheme} secret").parse() {
                headers.insert("authorization", value);
            }
            assert!(
                auth.authorize(&headers, Scope::RunsRead).is_ok(),
                "scheme {scheme:?} should be accepted"
            );
        }
    }

    #[test]
    fn constant_time_eq_matches_only_equal_slices() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreu")); // differs at the last byte
        assert!(!constant_time_eq(b"secret", b"xecret")); // differs at the first byte
        assert!(!constant_time_eq(b"secret", b"secre")); // shorter
        assert!(!constant_time_eq(b"secret", b"secrets")); // longer
        assert!(constant_time_eq(b"", b""));
    }
}
