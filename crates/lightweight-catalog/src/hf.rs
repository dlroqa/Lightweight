//! Reading a published digest for a HuggingFace link.
//!
//! A pasted link normally arrives with no digest, which means the bytes can
//! only be *recorded* rather than *verified*. HuggingFace is the exception
//! worth handling: every large file there is a Git LFS object, and an LFS
//! object id **is** the file's sha256. One small JSON call turns a pasted link
//! into a verified download.
//!
//! It is a convenience, never a requirement. A lookup that fails for any reason
//! — offline, a private repo, a rate limit, a shape we do not recognise —
//! returns `None` and the download proceeds unverified and says so. Refusing
//! the download instead would mean HuggingFace being briefly unreachable
//! stopped a user adding a model they could otherwise have added.

use serde::Deserialize;

/// The host this applies to.
const HOST: &str = "huggingface.co";

/// A `resolve` link broken into the parts the API needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolveLink {
    pub repo: String,
    pub revision: String,
    pub path: String,
}

/// Parse `https://huggingface.co/{owner}/{repo}/resolve/{rev}/{path}`.
///
/// Returns `None` for any other shape, including other hosts — this is a
/// recogniser, not a validator, and everything it does not recognise simply
/// takes the unverified path.
pub fn parse_resolve_link(url: &str) -> Option<ResolveLink> {
    let rest = url.strip_prefix("https://")?;
    let (host, rest) = rest.split_once('/')?;
    // A port would be unusual and is not something we want to follow.
    if !host.eq_ignore_ascii_case(HOST) {
        return None;
    }
    // Drop a query string or fragment before splitting the path.
    let rest = rest
        .split_once(['?', '#'])
        .map_or(rest, |(before, _)| before);

    let parts: Vec<&str> = rest.split('/').collect();
    let resolve_at = parts.iter().position(|part| *part == "resolve")?;
    // owner/repo before `resolve`, at least one path segment after the
    // revision.
    if resolve_at != 2 || parts.len() < resolve_at + 3 {
        return None;
    }
    let repo = parts.get(..2)?.join("/");
    let revision = (*parts.get(resolve_at + 1)?).to_owned();
    let path = parts.get(resolve_at + 2..)?.join("/");
    if repo.is_empty() || revision.is_empty() || path.is_empty() {
        return None;
    }
    Some(ResolveLink {
        repo,
        revision,
        path,
    })
}

#[derive(Debug, Deserialize)]
struct TreeEntry {
    path: String,
    lfs: Option<LfsInfo>,
}

#[derive(Debug, Deserialize)]
struct LfsInfo {
    /// The LFS object id, which for these files is the sha256.
    oid: String,
    size: u64,
}

/// The sha256 and size HuggingFace publishes for the file a link points at.
///
/// `None` whenever the answer is not unambiguous — see the module note on why
/// that is a downgrade rather than a failure.
pub async fn published_digest(
    client: &reqwest::Client,
    link: &ResolveLink,
) -> Option<(String, u64)> {
    let url = format!(
        "https://{HOST}/api/models/{}/tree/{}?recursive=true",
        link.repo, link.revision
    );
    let response = client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let entries: Vec<TreeEntry> = response.json().await.ok()?;
    let entry = entries.into_iter().find(|entry| entry.path == link.path)?;
    let lfs = entry.lfs?;
    // A digest of the wrong shape is worse than none: it would fail every
    // download with a mismatch that looks like corruption.
    let looks_like_sha256 = lfs.oid.len() == 64 && lfs.oid.bytes().all(|b| b.is_ascii_hexdigit());
    looks_like_sha256.then_some((lfs.oid, lfs.size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resolve_link_is_split_into_repo_revision_and_path() {
        let link = parse_resolve_link(
            "https://huggingface.co/bartowski/Qwen_Qwen3-1.7B-GGUF/resolve/main/Qwen_Qwen3-1.7B-Q4_K_M.gguf",
        )
        .expect("a resolve link");
        assert_eq!(link.repo, "bartowski/Qwen_Qwen3-1.7B-GGUF");
        assert_eq!(link.revision, "main");
        assert_eq!(link.path, "Qwen_Qwen3-1.7B-Q4_K_M.gguf");
    }

    #[test]
    fn a_download_query_string_is_not_part_of_the_path() {
        // `?download=true` is what the site's own copy button appends.
        let link = parse_resolve_link(
            "https://huggingface.co/owner/repo/resolve/main/model.gguf?download=true",
        )
        .expect("link");
        assert_eq!(link.path, "model.gguf");
    }

    #[test]
    fn a_file_in_a_subdirectory_keeps_its_whole_path() {
        let link =
            parse_resolve_link("https://huggingface.co/owner/repo/resolve/main/Q4/model.gguf")
                .expect("link");
        assert_eq!(link.path, "Q4/model.gguf");
    }

    #[test]
    fn anything_else_is_simply_not_recognised() {
        // Each of these must take the unverified path rather than error.
        for url in [
            "https://example.com/model.gguf",
            "https://huggingface.co/owner/repo/blob/main/model.gguf",
            "https://huggingface.co/owner/repo/resolve/main/",
            "https://huggingface.co/owner/resolve/main/model.gguf",
            "http://huggingface.co/owner/repo/resolve/main/model.gguf",
            "not a url",
        ] {
            assert!(parse_resolve_link(url).is_none(), "{url}");
        }
    }

    #[test]
    fn the_host_is_matched_without_regard_to_case_but_not_loosely() {
        assert!(parse_resolve_link("https://HuggingFace.co/o/r/resolve/main/m.gguf").is_some());
        // A look-alike host must not be treated as HuggingFace.
        assert!(
            parse_resolve_link("https://huggingface.co.evil.example/o/r/resolve/main/m.gguf")
                .is_none()
        );
    }
}
