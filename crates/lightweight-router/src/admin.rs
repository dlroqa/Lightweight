//! Who may change the router's settings: the operator at this machine, and
//! nobody else.
//!
//! The router's client key authorizes *inference* and the read-only admin
//! view; it is the key every agent holds, so it cannot also authorize a
//! change of settings. Writes need a separate **admin token**:
//!
//! * 32 random bytes, minted at every start and never logged. It is written,
//!   owner-only, next to the configuration (`router.json.admin-token`), where
//!   `hermes router admin-token` reads it back for the operator to paste. It
//!   is removed when the router stops, and a restart mints a new one.
//! * A router with any listener off loopback has no admin token at all, so a
//!   remote router's settings — and its key — can never be changed from
//!   another machine. Those keep the environment variable.
//!
//! Each write is also checked as a browser request: the `Host` must be a
//! loopback name on a port this router bound (a DNS-rebinding page arrives
//! under its own name), the `Origin` must be this router's own, and a body
//! must be JSON. The token header is not one a page can send cross-origin
//! without a preflight, and the router answers no preflight.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::http::{HeaderMap, StatusCode, header};
use sha2::{Digest, Sha256};

/// The header carrying the admin token.
pub const TOKEN_HEADER: &str = "x-lightweight-admin-token";
/// The largest settings body accepted.
pub const MAX_BODY_BYTES: usize = 16 * 1024;
/// The loopback names a browser may address this router by.
const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];

/// A refused admin request: a status, a stable code, and a sentence. Never a
/// header's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: &'static str,
}

const fn refuse(status: StatusCode, code: &'static str, message: &'static str) -> Refusal {
    Refusal {
        status,
        code,
        message,
    }
}

/// What this router accepts as an admin request.
pub struct AdminAccess {
    digest: [u8; 32],
    ports: Vec<u16>,
}

impl AdminAccess {
    /// Admin access for a router bound to `bound`, or why there is none.
    pub fn new(token: &str, bound: &[SocketAddr]) -> Result<Self, &'static str> {
        loopback_only(bound)?;
        if token.len() < 32 {
            return Err("the admin token is too short");
        }
        Ok(Self {
            digest: digest(token),
            ports: bound.iter().map(SocketAddr::port).collect(),
        })
    }

    /// Check one write. `has_body` is whether the request carries a body that
    /// must be JSON.
    pub fn check(&self, headers: &HeaderMap, has_body: bool) -> Result<(), Refusal> {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(str::to_ascii_lowercase)
            .ok_or(refuse(
                StatusCode::FORBIDDEN,
                "admin_host_refused",
                "settings can be changed only through a loopback address of this router",
            ))?;
        if !self.is_own_loopback_host(&host) {
            return Err(refuse(
                StatusCode::FORBIDDEN,
                "admin_host_refused",
                "settings can be changed only through a loopback address of this router",
            ));
        }
        let origin = headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .map(str::to_ascii_lowercase);
        if origin.as_deref() != Some(format!("http://{host}").as_str()) {
            return Err(refuse(
                StatusCode::FORBIDDEN,
                "cross_origin_refused",
                "settings can be changed only from the panel this router serves",
            ));
        }
        let presented = headers
            .get(TOKEN_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(refuse(
                StatusCode::UNAUTHORIZED,
                "admin_token_required",
                "changing settings needs the router's admin token: run `hermes router admin-token`",
            ))?;
        if !constant_time_eq(&digest(presented), &self.digest) {
            return Err(refuse(
                StatusCode::UNAUTHORIZED,
                "admin_token_invalid",
                "the admin token is not this router's; a restarted router has a new one",
            ));
        }
        if has_body {
            let json = headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"));
            if !json {
                return Err(refuse(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "unsupported_media_type",
                    "the body must be application/json",
                ));
            }
        }
        Ok(())
    }

    fn is_own_loopback_host(&self, host: &str) -> bool {
        let (name, port) = match host.rsplit_once(':') {
            // `[::1]` alone has colons but no port after its bracket.
            Some((name, port)) if !port.ends_with(']') => (name, port.parse::<u16>().ok()),
            _ => (host, Some(80)),
        };
        LOOPBACK_HOSTS.contains(&name) && port.is_some_and(|port| self.ports.contains(&port))
    }
}

/// Whether a router bound to `bound` may have admin access at all: only when
/// every listener is on loopback.
pub fn loopback_only(bound: &[SocketAddr]) -> Result<(), &'static str> {
    if bound.is_empty() || bound.iter().any(|address| !address.ip().is_loopback()) {
        return Err(
            "a listener is not on loopback, so settings cannot be changed through the \
             router: set them in its configuration and environment on its own machine",
        );
    }
    Ok(())
}

/// A fresh admin token: 32 bytes of OS entropy, hex encoded.
pub fn generate_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| format!("no OS entropy: {err}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Where the admin token of the router started with `config` is written.
pub fn token_path(config: &Path) -> PathBuf {
    let mut name = config
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_else(|| "router.json".into());
    name.push(".admin-token");
    config.with_file_name(name)
}

/// Write the token readable by this user alone, replacing any left by a
/// router that did not stop cleanly.
pub fn write_token(path: &Path, token: &str) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    std::io::Write::write_all(&mut file, token.as_bytes())?;
    file.sync_all()
}

/// Remove the token file, if it is still this router's.
pub fn remove_token(path: &Path, token: &str) {
    if std::fs::read_to_string(path).is_ok_and(|held| held.trim() == token) {
        let _ = std::fs::remove_file(path);
    }
}

fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";

    fn access() -> AdminAccess {
        AdminAccess::new(TOKEN, &["127.0.0.1:18500".parse().unwrap()]).unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn good() -> Vec<(&'static str, &'static str)> {
        vec![
            ("host", "127.0.0.1:18500"),
            ("origin", "http://127.0.0.1:18500"),
            (TOKEN_HEADER, TOKEN),
            ("content-type", "application/json"),
        ]
    }

    fn with(replace: &str, value: Option<&'static str>) -> HeaderMap {
        let pairs: Vec<_> = good()
            .into_iter()
            .filter(|(name, _)| *name != replace)
            .chain(value.map(|value| {
                let name: &'static str = Box::leak(replace.to_owned().into_boxed_str());
                (name, value)
            }))
            .collect();
        headers(&pairs)
    }

    #[test]
    fn a_same_origin_loopback_request_with_the_token_is_admitted() {
        assert_eq!(access().check(&headers(&good()), true), Ok(()));
        let named = headers(&[
            ("host", "localhost:18500"),
            ("origin", "http://localhost:18500"),
            (TOKEN_HEADER, TOKEN),
        ]);
        assert_eq!(access().check(&named, false), Ok(()));
    }

    #[test]
    fn a_router_listening_off_loopback_has_no_admin_access() {
        assert!(AdminAccess::new(TOKEN, &["0.0.0.0:18500".parse().unwrap()]).is_err());
        assert!(
            AdminAccess::new(
                TOKEN,
                &[
                    "127.0.0.1:18500".parse().unwrap(),
                    "192.0.2.5:18500".parse().unwrap()
                ]
            )
            .is_err()
        );
        assert!(AdminAccess::new(TOKEN, &[]).is_err());
    }

    #[test]
    fn a_rebinding_or_foreign_host_is_refused() {
        for host in [
            "attacker.example:18500",
            "127.0.0.1:9999",
            "127.0.0.1.nip.io:18500",
            "localhost",
        ] {
            let refusal = access().check(&with("host", Some(host)), true).unwrap_err();
            assert_eq!(refusal.code, "admin_host_refused", "{host}");
        }
        let refusal = access().check(&with("host", None), true).unwrap_err();
        assert_eq!(refusal.code, "admin_host_refused");
    }

    #[test]
    fn a_missing_foreign_or_opaque_origin_is_refused() {
        for origin in [
            None,
            Some("http://attacker.example"),
            Some("null"),
            Some("http://localhost:18500"),
            Some("https://127.0.0.1:18500"),
        ] {
            let refusal = access().check(&with("origin", origin), true).unwrap_err();
            assert_eq!(refusal.code, "cross_origin_refused", "{origin:?}");
            assert_eq!(refusal.status, StatusCode::FORBIDDEN);
        }
    }

    #[test]
    fn the_admin_token_is_required_and_must_match() {
        let refusal = access().check(&with(TOKEN_HEADER, None), true).unwrap_err();
        assert_eq!(refusal.code, "admin_token_required");
        let refusal = access()
            .check(&with(TOKEN_HEADER, Some("not-the-token")), true)
            .unwrap_err();
        assert_eq!(refusal.code, "admin_token_invalid");
        assert_eq!(refusal.status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn the_inference_key_is_not_an_admin_token() {
        let mut map = with(TOKEN_HEADER, None);
        map.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer client-key"),
        );
        assert_eq!(
            access().check(&map, true).unwrap_err().code,
            "admin_token_required"
        );
    }

    #[test]
    fn a_body_must_be_json() {
        for kind in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
        ] {
            let refusal = access()
                .check(&with("content-type", kind), true)
                .unwrap_err();
            assert_eq!(refusal.code, "unsupported_media_type", "{kind:?}");
        }
        assert_eq!(
            access().check(
                &with("content-type", Some("application/json; charset=utf-8")),
                true
            ),
            Ok(())
        );
        assert_eq!(access().check(&with("content-type", None), false), Ok(()));
    }

    #[test]
    fn tokens_are_long_random_and_never_repeat() {
        let first = generate_token().unwrap();
        let second = generate_token().unwrap();
        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
    }

    #[test]
    fn the_token_file_sits_beside_the_configuration() {
        assert_eq!(
            token_path(Path::new("/etc/lightweight/router.json")),
            Path::new("/etc/lightweight/router.json.admin-token")
        );
    }
}
