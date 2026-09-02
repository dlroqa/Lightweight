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
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim);
        match provided {
            Some(token) if token == expected => {
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
}
