//! Serving the control panel's own files.
//!
//! This exists so that no CORS layer ever has to. A browser refuses a request
//! to an origin other than the page's own, and the usual answer is to add
//! permissive headers to the API — which is a decision about who may call this
//! gateway, taken in order to solve a problem about where a file is served
//! from. Serving the panel from the gateway makes the question not arise: the
//! page and the API share an origin, so there is nothing to permit. In
//! development the Vite dev server proxies `/api` and `/v1` here, which gives
//! the same property without a build step.
//!
//! It also means a browser on another machine reaches the panel over the
//! non-loopback bind M3.5 built, with the same key and the same redaction
//! rules, for no extra work.
//!
//! Installed as a **fallback**, so every route in [`crate::app`] is matched
//! first. A file cannot shadow an endpoint; only a request that matches no
//! endpoint reaches here.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use crate::state::GatewayState;

/// The document served for a route the client renders itself.
const INDEX: &str = "index.html";

/// Serve a file from the configured web root.
pub async fn serve(State(state): State<Arc<GatewayState>>, uri: Uri) -> Response {
    let Some(root) = state.config.web_root.as_ref() else {
        // No panel was built into this deployment. A plain 404 rather than an
        // explanation, because this is also the response a mistyped API path
        // gets, and inventing a body for it would change what every existing
        // client sees when it gets a path wrong.
        return StatusCode::NOT_FOUND.into_response();
    };

    let requested = uri.path();
    match resolve(root, requested) {
        // A real file: send it.
        Some(path) if path.is_file() => send(&path).await,
        // Anything else that could be a client-side route gets the document,
        // which is what makes deep links work: the panel's router reads the
        // path the browser already has. A request that looks like an asset -
        // `/assets/main.abc123.js` - is *not* given the document, because
        // answering a missing script with HTML produces a syntax error in the
        // console instead of the 404 that says what actually happened.
        _ if looks_like_a_route(requested) => send(&root.join(INDEX)).await,
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Turn a request path into a path under `root`, or nothing.
///
/// Returns `None` for anything that is not a plain relative path. This is the
/// only thing standing between a request and the rest of the filesystem, so it
/// is a whitelist: every component must be an ordinary name. `..` is not
/// resolved-then-checked, it is refused, and so are absolute paths, Windows
/// drive prefixes, and the current-directory component that would let `.` pad a
/// path into looking ordinary.
fn resolve(root: &Path, requested: &str) -> Option<PathBuf> {
    let trimmed = requested.trim_start_matches('/');
    if trimmed.is_empty() {
        return Some(root.join(INDEX));
    }
    // Percent-encoding is decoded by nothing here on purpose: a legitimate
    // asset name produced by a bundler is already URL-safe, and decoding would
    // reintroduce the separators this function exists to reject.
    if trimmed.contains('\0') {
        return None;
    }

    let candidate = Path::new(trimmed);
    if candidate
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(root.join(candidate))
}

/// Whether a path with no file behind it should be answered with the document.
///
/// A path whose last segment carries an extension is an asset that is missing;
/// anything else may be a route the panel knows how to render.
fn looks_like_a_route(requested: &str) -> bool {
    requested
        .rsplit('/')
        .next()
        .is_none_or(|last| !last.contains('.'))
}

/// Read a file and answer with it.
async fn send(path: &Path) -> Response {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let mut response = (StatusCode::OK, bytes).into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type(path));
            // The document is the one file that must never be cached: it names
            // the hashed assets, so a stale copy points a browser at scripts
            // that a redeploy has already removed. The assets themselves are
            // content-hashed by the bundler and safe to keep.
            let directive = if path.file_name().is_some_and(|name| name == INDEX) {
                "no-cache"
            } else {
                "public, max-age=31536000, immutable"
            };
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(directive));
            response
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The content type for a file, by extension.
///
/// A short table rather than a dependency. Everything a bundler emits is here;
/// anything else is served as bytes, which a browser will offer to download
/// rather than mis-render.
fn content_type(path: &Path) -> HeaderValue {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    HeaderValue::from_static(match extension.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_is_the_document() {
        let root = Path::new("/srv/panel");
        assert_eq!(resolve(root, "/"), Some(root.join(INDEX)));
        assert_eq!(resolve(root, ""), Some(root.join(INDEX)));
    }

    #[test]
    fn an_ordinary_asset_resolves_under_the_root() {
        let root = Path::new("/srv/panel");
        assert_eq!(
            resolve(root, "/assets/main.abc123.js"),
            Some(root.join("assets/main.abc123.js"))
        );
    }

    #[test]
    fn nothing_escapes_the_web_root() {
        // The whole reason this function is a whitelist. Each of these is a
        // real request someone will make on purpose.
        let root = Path::new("/srv/panel");
        for attempt in [
            "/../etc/passwd",
            "/assets/../../etc/passwd",
            "/./../../etc/shadow",
            "/a/b/../../../../etc/passwd",
            "/..",
        ] {
            assert_eq!(resolve(root, attempt), None, "{attempt} was not refused");
        }

        // Repeated leading slashes are not an escape: they collapse to a name
        // *under* the root, which is a file that will not exist. Asserted so
        // that the distinction between "refused" and "harmless" stays
        // deliberate rather than becoming an accident of the trim.
        assert_eq!(
            resolve(root, "//etc/passwd"),
            Some(root.join("etc/passwd")),
            "a doubled slash must stay inside the root, not be read as absolute"
        );
    }

    #[test]
    fn a_current_directory_component_is_refused_rather_than_flattened() {
        // `.` is harmless on its own and is how a traversal is padded to get
        // past a check that only looks for `..`.
        assert_eq!(resolve(Path::new("/srv/panel"), "/./assets/x.js"), None);
    }

    #[test]
    fn a_route_gets_the_document_and_a_missing_asset_does_not() {
        // Answering a missing script with HTML turns a 404 into an unexplained
        // syntax error in the browser console.
        assert!(looks_like_a_route("/models"));
        assert!(looks_like_a_route("/settings/appearance"));
        assert!(looks_like_a_route("/"));
        assert!(!looks_like_a_route("/assets/main.abc123.js"));
        assert!(!looks_like_a_route("/favicon.ico"));
    }

    #[test]
    fn content_types_cover_what_a_bundler_emits() {
        let cases = [
            ("index.html", "text/html; charset=utf-8"),
            ("main.js", "text/javascript; charset=utf-8"),
            ("main.mjs", "text/javascript; charset=utf-8"),
            ("style.css", "text/css; charset=utf-8"),
            ("icon.svg", "image/svg+xml"),
            ("font.woff2", "font/woff2"),
            ("main.js.map", "application/json; charset=utf-8"),
            ("mystery.bin", "application/octet-stream"),
        ];
        for (name, expected) in cases {
            assert_eq!(content_type(Path::new(name)), expected, "{name}");
        }
    }

    #[test]
    fn an_uppercase_extension_is_still_recognised() {
        // Assets copied in from a design tool routinely arrive as `.PNG`.
        assert_eq!(content_type(Path::new("logo.PNG")), "image/png");
    }
}
