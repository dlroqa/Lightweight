//! One resumable, digest-verified download.
//!
//! This is the engine installer's download loop, lifted out so the model
//! catalog runs the same code rather than a second copy of it. Two copies of an
//! integrity check diverge, and the half that is wrong is the half nobody
//! looked at.
//!
//! What it guarantees, in the order the guarantees matter:
//!
//! 1. **https only.** These bytes get executed or loaded as a model.
//! 2. **The digest is checked before the file is put in place.** A file that
//!    fails verification is deleted, never left where a resume could inherit
//!    it.
//! 3. **A resume re-hashes what is already on disk** rather than trusting it. A
//!    partial file from an earlier run carries no provenance at all.
//! 4. **Progress never blocks the transfer.** The sink is a plain callback and
//!    the caller is expected to drop updates rather than wait — the deadlock
//!    that rule comes from is recorded in `docs/architecture.md`.

use std::io::Read;
use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::error::DownloadError;

/// `ENOSPC` on Unix, `ERROR_DISK_FULL` on Windows.
///
/// `std::io::ErrorKind::StorageFull` is still unstable, so the raw code is
/// matched instead. Getting this right is what turns "download failed" into
/// section 27's actionable "not enough disk space".
pub(crate) const DISK_FULL_ERRNO: i32 = if cfg!(windows) { 112 } else { 28 };

/// What to fetch, and what to check it against.
#[derive(Clone, Copy, Debug)]
pub struct Fetch<'a> {
    pub url: &'a str,
    /// Where the verified file ends up. The transfer runs against a sibling
    /// `.part` file, so this path only ever holds a complete, checked file.
    pub destination: &'a Path,
    /// Lowercase hex sha256, when it is known in advance.
    ///
    /// `None` means the digest is *computed and reported* rather than checked.
    /// That is a weaker promise, and the caller is expected to record which of
    /// the two it got rather than describe both as "verified".
    pub expected_sha256: Option<&'a str>,
    /// Total size, when it is known in advance. Used only for progress.
    pub total_size: Option<u64>,
    /// A short noun for error messages: "the inference engine", "the model".
    pub what: &'a str,
}

/// The result of a completed download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    /// Lowercase hex sha256 of the bytes now at the destination.
    pub sha256: String,
    pub bytes: u64,
    /// Whether an earlier partial file was continued rather than restarted.
    pub resumed: bool,
    /// Whether [`Fetch::expected_sha256`] was supplied and matched.
    pub verified: bool,
}

/// Progress while bytes arrive: `(downloaded, total)`.
///
/// A callback rather than a channel so that the "never block the transfer" rule
/// holds by construction: there is nothing here to await.
pub type ProgressSink<'a> = &'a (dyn Fn(u64, Option<u64>) + Send + Sync);

/// Download `fetch.url` to `fetch.destination`, resuming if a partial exists.
pub async fn fetch(
    client: &reqwest::Client,
    request: Fetch<'_>,
    progress: ProgressSink<'_>,
    cancel: &CancellationToken,
) -> Result<Fetched, DownloadError> {
    require_https(request.url)?;

    if let Some(parent) = request.destination.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| map_io("creating the download directory", err))?;
    }

    let partial = partial_path(request.destination);
    // Re-hash whatever is already on disk rather than trusting it: a partial
    // file from an earlier run carries no guarantee at all. Blocking work, so
    // it leaves the executor.
    let rehash_path = partial.clone();
    let (mut hasher, resume_from) =
        tokio::task::spawn_blocking(move || rehash_partial(&rehash_path))
            .await
            .map_err(|err| DownloadError::Failed {
                what: request.what.to_owned(),
                reason: format!("the hashing task failed: {err}"),
            })??;

    let mut get = client.get(request.url);
    if resume_from > 0 {
        get = get.header(reqwest::header::RANGE, format!("bytes={resume_from}-"));
    }

    let response = get.send().await.map_err(|err| DownloadError::Failed {
        what: request.what.to_owned(),
        reason: err.to_string(),
    })?;

    // A server that ignores the range header answers 200 with the whole file.
    // Appending that to our prefix would corrupt it, so start over.
    let restart = resume_from > 0 && response.status() != reqwest::StatusCode::PARTIAL_CONTENT;
    if restart {
        hasher = Sha256::new();
    }
    let mut written = if restart { 0 } else { resume_from };

    if !response.status().is_success() {
        return Err(DownloadError::Failed {
            what: request.what.to_owned(),
            reason: format!("{} returned HTTP {}", request.url, response.status()),
        });
    }

    // Prefer a size the caller recorded in advance; fall back to what this
    // response claims. A 206 reports only the bytes still to come, so the part
    // already on disk is added back or the bar would run backwards.
    let total = request.total_size.or_else(|| {
        response
            .content_length()
            .map(|len| len.saturating_add(written))
    });

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(!restart && resume_from > 0)
        .truncate(restart || resume_from == 0)
        .open(&partial)
        .await
        .map_err(|err| map_io("opening the download file", err))?;

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if cancel.is_cancelled() {
            // The partial file is left in place on purpose: the next attempt
            // resumes from it.
            return Err(DownloadError::Cancelled);
        }
        let chunk = chunk.map_err(|err| DownloadError::Failed {
            what: request.what.to_owned(),
            reason: err.to_string(),
        })?;
        file.write_all(&chunk)
            .await
            .map_err(|err| map_io("writing the download", err))?;
        // Hashed as it arrives, so verification costs no second pass over the
        // file.
        hasher.update(&chunk);
        written = written.saturating_add(chunk.len() as u64);

        // Never await here. Progress is advisory: if nobody is reading it,
        // dropping an update is correct and blocking the download is not.
        progress(written, total);
    }
    file.flush()
        .await
        .map_err(|err| map_io("flushing the download", err))?;
    drop(file);

    let actual = hex::encode(hasher.finalize());
    if let Some(expected) = request.expected_sha256
        && !actual.eq_ignore_ascii_case(expected)
    {
        // Never leave a file that failed verification where a later run could
        // resume from it and inherit the corruption.
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(DownloadError::Corrupt {
            expected: expected.to_owned(),
            actual,
        });
    }

    tokio::fs::rename(&partial, request.destination)
        .await
        .map_err(|err| map_io("finalising the download", err))?;

    Ok(Fetched {
        sha256: actual,
        bytes: written,
        resumed: resume_from > 0 && !restart,
        verified: request.expected_sha256.is_some(),
    })
}

/// Where the in-progress bytes live for a given destination.
///
/// A suffix rather than `with_extension`, which would turn `model.gguf` into
/// `model.part` and collide with the partial for `model.bin` next to it.
pub fn partial_path(destination: &Path) -> PathBuf {
    let mut name = destination.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

/// Refuse anything but https.
fn require_https(url: &str) -> Result<(), DownloadError> {
    let scheme = url.split("://").next().unwrap_or_default();
    if scheme.eq_ignore_ascii_case("https") {
        return Ok(());
    }
    Err(DownloadError::InsecureUrl {
        scheme: if scheme.is_empty() || scheme.len() == url.len() {
            "(none)".to_owned()
        } else {
            scheme.to_ascii_lowercase()
        },
    })
}

/// Hash a file that is already on disk, reporting progress as it goes.
///
/// Blocking and CPU-bound — around a gigabyte per few seconds on the
/// development machine — so callers run it under `spawn_blocking`. It exists
/// here rather than in the catalog because it is the same question the resume
/// path asks: what are the bytes actually on disk?
pub fn hash_file(
    path: &Path,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
) -> Result<(String, u64), DownloadError> {
    let total = std::fs::metadata(path)
        .map_err(|err| map_io("reading the file's size", err))?
        .len();
    let mut file = std::fs::File::open(path).map_err(|err| map_io("opening the file", err))?;

    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut done = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| map_io("reading the file", err))?;
        if read == 0 {
            break;
        }
        match buffer.get(..read) {
            Some(slice) => hasher.update(slice),
            None => break,
        }
        done = done.saturating_add(read as u64);
        progress(done, Some(total));
    }
    Ok((hex::encode(hasher.finalize()), done))
}

/// Hash an existing partial download and report how many bytes it holds.
fn rehash_partial(partial: &Path) -> Result<(Sha256, u64), DownloadError> {
    let mut hasher = Sha256::new();
    let Ok(mut file) = std::fs::File::open(partial) else {
        return Ok((hasher, 0));
    };

    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| map_io("reading the partial download", err))?;
        if read == 0 {
            break;
        }
        match buffer.get(..read) {
            Some(slice) => hasher.update(slice),
            None => break,
        }
        total = total.saturating_add(read as u64);
    }
    Ok((hasher, total))
}

/// Turn an I/O error into the most specific download error available.
pub(crate) fn map_io(context: &str, err: std::io::Error) -> DownloadError {
    if err.raw_os_error() == Some(DISK_FULL_ERRNO) {
        // Section 27 lists insufficient disk space as something that must be
        // reported actionably rather than surfacing as a generic write failure.
        return DownloadError::LowDisk {
            path: PathBuf::new(),
            needed: 0,
        };
    }
    DownloadError::io(context.to_owned(), err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::Actionable;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            // The clock alone is not unique: on a coarse timer two tests running in
            // parallel are handed the same name. The counter and the pid settle it.
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hermes-download-{tag}-{}-{unique}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_missing_partial_file_starts_from_zero() {
        let temp = TempDir::new("nopart");
        let (_, resume) = rehash_partial(&temp.0.join("absent.part")).expect("no error");
        assert_eq!(resume, 0);
    }

    #[test]
    fn an_existing_partial_is_rehashed_rather_than_trusted() {
        // A partial file left by an earlier run has no provenance. Re-hashing
        // it is what lets the final digest check still mean something.
        let temp = TempDir::new("part");
        let path = temp.0.join("archive.part");
        std::fs::write(&path, b"hello world").expect("write");

        let (hasher, resume) = rehash_partial(&path).expect("hash");
        assert_eq!(resume, 11);

        let expected = hex::encode(Sha256::digest(b"hello world"));
        assert_eq!(hex::encode(hasher.finalize()), expected);
    }

    #[test]
    fn rehashing_spans_more_than_one_buffer() {
        // The read loop is chunked; a file larger than the buffer would expose
        // an off-by-one in how chunks are fed to the hasher.
        let temp = TempDir::new("bigpart");
        let path = temp.0.join("archive.part");
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &payload).expect("write");

        let (hasher, resume) = rehash_partial(&path).expect("hash");
        assert_eq!(resume, payload.len() as u64);
        assert_eq!(
            hex::encode(hasher.finalize()),
            hex::encode(Sha256::digest(&payload))
        );
    }

    #[test]
    fn a_file_on_disk_is_hashed_and_its_progress_reported() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let temp = TempDir::new("hashfile");
        let path = temp.0.join("model.gguf");
        let payload: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &payload).expect("write");

        let seen = AtomicU64::new(0);
        let (digest, bytes) = hash_file(&path, &|done, _| {
            seen.store(done, Ordering::Relaxed);
        })
        .expect("hash");

        assert_eq!(bytes, payload.len() as u64);
        assert_eq!(digest, hex::encode(Sha256::digest(&payload)));
        // Larger than one buffer, so progress must have been reported at least
        // once and must end at the full size.
        assert_eq!(seen.load(Ordering::Relaxed), payload.len() as u64);
    }

    #[test]
    fn hashing_a_file_that_is_not_there_is_an_error_not_an_empty_digest() {
        let temp = TempDir::new("nofile");
        let err = hash_file(&temp.0.join("absent.gguf"), &|_, _| {}).expect_err("must fail");
        assert_eq!(err.code(), "io_error");
    }

    #[test]
    fn a_disk_full_error_is_reported_as_such_rather_than_as_a_generic_failure() {
        let err = map_io(
            "writing",
            std::io::Error::from_raw_os_error(DISK_FULL_ERRNO),
        );
        assert_eq!(err.code(), "low_disk");

        let other = map_io("writing", std::io::Error::other("something else"));
        assert_eq!(other.code(), "io_error");
    }

    #[test]
    fn only_https_is_accepted() {
        assert!(require_https("https://example.com/a.gguf").is_ok());
        // Case is not significant in a scheme.
        assert!(require_https("HTTPS://example.com/a.gguf").is_ok());

        for rejected in [
            "http://example.com/a.gguf",
            "ftp://example.com/a.gguf",
            "file:///etc/passwd",
            "example.com/a.gguf",
        ] {
            let err = require_https(rejected).expect_err(rejected);
            assert_eq!(err.code(), "insecure_url", "{rejected}");
        }
    }

    #[test]
    fn the_partial_file_keeps_the_whole_name_it_belongs_to() {
        // `with_extension` would map both `model.gguf` and `model.bin` onto
        // `model.part`, so two downloads in one directory would corrupt each
        // other's resume state.
        assert_eq!(
            partial_path(Path::new("/models/model.gguf")),
            PathBuf::from("/models/model.gguf.part")
        );
        assert_ne!(
            partial_path(Path::new("/models/model.gguf")),
            partial_path(Path::new("/models/model.bin"))
        );
    }

    #[tokio::test]
    async fn a_plaintext_url_is_refused_before_anything_is_written() {
        let temp = TempDir::new("insecure");
        let destination = temp.0.join("out.bin");
        let client = crate::client("test").expect("client");

        let err = fetch(
            &client,
            Fetch {
                url: "http://example.com/model.gguf",
                destination: &destination,
                expected_sha256: None,
                total_size: None,
                what: "the model",
            },
            &|_, _| {},
            &CancellationToken::new(),
        )
        .await
        .expect_err("plaintext must be refused");

        assert_eq!(err.code(), "insecure_url");
        assert!(!destination.exists());
        assert!(!partial_path(&destination).exists());
    }
}
