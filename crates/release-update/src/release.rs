//! The latest published release: its metadata, its checksums and its archives.

use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/dlroqa/Lightweight/releases/latest";

/// The checksum manifest every release publishes beside its archives.
pub(crate) const CHECKSUMS_ASSET: &str = "SHA256SUMS";

/// A published release, as the GitHub API reports it. Drafts and prereleases
/// are never returned by the `latest` endpoint.
#[derive(Debug, Deserialize)]
pub(crate) struct Release {
    pub(crate) tag_name: String,
    pub(crate) html_url: String,
    #[serde(default)]
    pub(crate) assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Asset {
    pub(crate) name: String,
    pub(crate) browser_download_url: String,
    pub(crate) size: u64,
}

impl Release {
    /// The release version without the tag's `v` prefix.
    pub(crate) fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }

    pub(crate) fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }
}

/// An HTTP client for release traffic.
pub(crate) fn client(user_agent: &str) -> Result<reqwest::Client, String> {
    crate::tls::ensure_provider();
    reqwest::Client::builder()
        .user_agent(user_agent.to_owned())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| format!("could not create the update client: {error}"))
}

pub(crate) async fn latest(client: &reqwest::Client) -> Result<Release, String> {
    let response = client
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| format!("could not check for updates: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "GitHub answered HTTP {} while checking for updates",
            response.status().as_u16()
        ));
    }
    response
        .json::<Release>()
        .await
        .map_err(|error| format!("could not read the latest release: {error}"))
}

/// Fetch a small text asset, such as the checksum manifest.
pub(crate) async fn text(client: &reqwest::Client, asset: &Asset) -> Result<String, String> {
    let response = get(client, asset).await?;
    response
        .text()
        .await
        .map_err(|error| format!("could not read {}: {error}", asset.name))
}

/// The sha256 recorded for `name` in a `sha256sum`-format manifest.
pub(crate) fn checksum_for<'a>(manifest: &'a str, name: &str) -> Option<&'a str> {
    manifest.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let digest = fields.next()?;
        // `sha256sum` marks binary-mode entries with a leading `*`.
        let file = fields.next()?.trim_start_matches('*');
        (file == name
            && fields.next().is_none()
            && digest.len() == 64
            && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(digest)
    })
}

/// Download `asset` to `destination`, hashing while writing, and keep the file
/// only when its digest matches `expected_sha256`.
pub(crate) async fn download_verified(
    client: &reqwest::Client,
    asset: &Asset,
    destination: &Path,
    expected_sha256: &str,
) -> Result<(), String> {
    let response = get(client, asset).await?;
    let mut file = tokio::fs::File::create(destination)
        .await
        .map_err(|error| format!("could not create {}: {error}", destination.display()))?;
    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("download of {} failed: {error}", asset.name))?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("could not write {}: {error}", destination.display()))?;
    }
    file.flush()
        .await
        .map_err(|error| format!("could not write {}: {error}", destination.display()))?;
    drop(file);

    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        let _ = tokio::fs::remove_file(destination).await;
        return Err(format!(
            "{} failed verification: expected sha256 {expected_sha256}, got {actual}",
            asset.name
        ));
    }
    Ok(())
}

async fn get(client: &reqwest::Client, asset: &Asset) -> Result<reqwest::Response, String> {
    if !asset.browser_download_url.starts_with("https://") {
        return Err(format!(
            "refusing to download {} over a non-HTTPS URL",
            asset.name
        ));
    }
    let response = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .map_err(|error| format!("could not download {}: {error}", asset.name))?;
    if !response.status().is_success() {
        return Err(format!(
            "GitHub answered HTTP {} for {}",
            response.status().as_u16(),
            asset.name
        ));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "3fbe7fb4847d38a687026a2c0e114fecf271295d01955ba082c8b522902af3c5";

    #[test]
    fn a_checksum_is_found_by_exact_file_name() {
        let manifest = format!(
            "{DIGEST}  lightagent-0.3.22-x86_64-unknown-linux-gnu.tar.gz\n\
             {}  lightagent-0.3.22-x86_64-unknown-linux-gnu.tar.gz.sig\n",
            "0".repeat(64)
        );
        assert_eq!(
            checksum_for(
                &manifest,
                "lightagent-0.3.22-x86_64-unknown-linux-gnu.tar.gz"
            ),
            Some(DIGEST)
        );
        assert_eq!(checksum_for(&manifest, "lightagent-0.3.22"), None);
    }

    #[test]
    fn binary_mode_markers_are_accepted_and_malformed_digests_are_not() {
        assert_eq!(
            checksum_for(&format!("{DIGEST} *a.zip"), "a.zip"),
            Some(DIGEST)
        );
        assert_eq!(checksum_for("not-a-digest  a.zip", "a.zip"), None);
        assert_eq!(
            checksum_for(&format!("{DIGEST}  a.zip extra"), "a.zip"),
            None
        );
    }

    #[test]
    fn the_release_version_drops_the_tag_prefix() {
        let release: Release = serde_json::from_str(
            r#"{"tag_name":"v0.3.22","html_url":"https://example.org","assets":[]}"#,
        )
        .unwrap();
        assert_eq!(release.version(), "0.3.22");
        assert!(release.asset(CHECKSUMS_ASSET).is_none());
    }
}
