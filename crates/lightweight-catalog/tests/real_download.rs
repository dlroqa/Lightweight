//! The download path against the real network.
//!
//! Opt-in, like the real-engine tier: set `HERMES_TEST_NETWORK=1` to include
//! them. They fetch a real model — the smallest pinned one, about 100 MB — from
//! HuggingFace and check what actually arrives, because every claim this
//! project makes about verification is a claim about bytes that travelled.
//!
//! Skipped rather than failed when the variable is absent, so an offline run of
//! the suite stays green; `scripts/check.sh` says out loud when it skips them.

use std::path::{Path, PathBuf};

use lightweight_catalog::install::{AddModel, InstallProgress, Installer};
use lightweight_catalog::record::Integrity;
use lightweight_catalog::{CatalogError, CatalogStore};
use lightweight_core::Actionable;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const NETWORK_ENV: &str = "HERMES_TEST_NETWORK";

/// The pinned model these tests use: the smallest one.
const PINNED: &str = "smollm2-135m-instruct-q4_k_m";

fn enabled() -> bool {
    std::env::var_os(NETWORK_ENV).is_some()
}

/// A throwaway profile that cleans up after itself.
struct Profile(PathBuf);

impl Profile {
    fn new(tag: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("hermes-netdl-{tag}-{unique}"));
        std::fs::create_dir_all(&path).expect("profile dir");
        Self(path)
    }

    fn models(&self) -> PathBuf {
        self.0.join("models")
    }

    fn downloads(&self) -> PathBuf {
        self.0.join("downloads")
    }

    fn catalog(&self) -> PathBuf {
        self.0.join("catalog.json")
    }

    fn installer(&self) -> Installer {
        Installer::new(self.models(), self.downloads()).expect("installer")
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A progress channel that is drained, so nothing is dropped for the wrong
/// reason while a test is watching it.
fn progress() -> (
    mpsc::Sender<InstallProgress>,
    tokio::task::JoinHandle<Vec<InstallProgress>>,
) {
    let (tx, mut rx) = mpsc::channel(64);
    let handle = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(update) = rx.recv().await {
            seen.push(update);
        }
        seen
    });
    (tx, handle)
}

#[tokio::test]
async fn a_pinned_model_downloads_and_verifies_against_its_recorded_digest() {
    if !enabled() {
        eprintln!("skipped: set {NETWORK_ENV}=1 to run the network tier");
        return;
    }

    let profile = Profile::new("pinned");
    let installer = profile.installer();
    let mut store = CatalogStore::open(profile.catalog()).expect("catalog");
    let (tx, updates) = progress();

    let started = std::time::Instant::now();
    let model = installer
        .add(
            &mut store,
            &AddModel::Pinned { id: PINNED.into() },
            &tx,
            &CancellationToken::new(),
        )
        .await
        .expect("the pinned model should download");
    drop(tx);
    let elapsed = started.elapsed();

    let expected = lightweight_catalog::manifest::by_id(PINNED).expect("pinned entry");
    assert_eq!(
        model.sha256, expected.sha256,
        "digest must match the manifest"
    );
    assert_eq!(model.bytes, expected.size, "size must match the manifest");
    assert_eq!(
        model.integrity,
        Integrity::Manifest,
        "a pinned download is verified against a digest recorded beforehand"
    );
    assert!(model.integrity.verified());

    // The file is in the models directory, and nothing is left staged.
    assert!(model.path.is_file(), "{} is missing", model.path.display());
    assert!(model.path.starts_with(profile.models()));
    assert_eq!(
        std::fs::read_dir(profile.downloads())
            .expect("downloads dir")
            .count(),
        0,
        "a staged file was left behind"
    );

    // The header was read, not guessed.
    assert_eq!(model.architecture, "llama");
    assert!(model.supported);
    assert!(model.context_length.is_some());

    // And it survives a restart.
    let reopened = CatalogStore::open(profile.catalog()).expect("reopen");
    assert_eq!(
        reopened.get(PINNED).map(|m| m.sha256.clone()),
        Some(model.sha256.clone())
    );

    let seen = updates.await.expect("progress task");
    assert!(
        seen.iter()
            .any(|u| matches!(u, InstallProgress::Downloading { .. })),
        "progress was never reported"
    );
    assert_eq!(seen.last(), Some(&InstallProgress::Done));

    let mib = model.bytes as f64 / (1024.0 * 1024.0);
    eprintln!(
        "downloaded {mib:.1} MiB in {:.1}s ({:.2} MiB/s)",
        elapsed.as_secs_f64(),
        mib / elapsed.as_secs_f64().max(f64::MIN_POSITIVE)
    );
}

#[tokio::test]
async fn an_interrupted_download_resumes_from_what_is_already_on_disk() {
    if !enabled() {
        eprintln!("skipped: set {NETWORK_ENV}=1 to run the network tier");
        return;
    }

    let profile = Profile::new("resume");
    let entry = lightweight_catalog::manifest::by_id(PINNED).expect("pinned entry");
    let destination = profile.models().join(entry.file);
    std::fs::create_dir_all(profile.models()).expect("models dir");

    // Cancel part way through, which leaves the partial file in place on
    // purpose - that is what the next attempt resumes from.
    //
    // Cancelled after a fixed number of *bytes*, not after a fixed time. A
    // timer races the connection: on a fast link the transfer finishes first
    // and the test quietly stops testing resumption, which is the shape of a
    // test that passes for the wrong reason.
    let cancel = CancellationToken::new();
    let stop_after = entry.size / 4;
    let stopper = cancel.clone();

    let client = lightweight_download::client("hermes-test").expect("client");
    let first = lightweight_download::fetch(
        &client,
        lightweight_download::Fetch {
            url: &entry.url(),
            destination: &destination,
            expected_sha256: Some(entry.sha256),
            total_size: Some(entry.size),
            what: "the model",
        },
        &move |downloaded, _| {
            if downloaded >= stop_after {
                stopper.cancel();
            }
        },
        &cancel,
    )
    .await;

    let partial = lightweight_download::partial_path(&destination);
    let err = first.expect_err(
        "the transfer was cancelled a quarter of the way through and must not have completed",
    );
    assert_eq!(err.code(), "cancelled");
    let stopped_at = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
    assert!(stopped_at > 0, "nothing was written before cancelling");
    assert!(!destination.exists(), "an incomplete file was put in place");

    // Second attempt: same destination, partial still there.
    let second = lightweight_download::fetch(
        &client,
        lightweight_download::Fetch {
            url: &entry.url(),
            destination: &destination,
            expected_sha256: Some(entry.sha256),
            total_size: Some(entry.size),
            what: "the model",
        },
        &|_, _| {},
        &CancellationToken::new(),
    )
    .await
    .expect("the resumed download should complete");

    assert!(
        second.resumed,
        "the second attempt refetched from zero instead of resuming"
    );
    assert_eq!(
        second.sha256, entry.sha256,
        "resumed bytes must still verify"
    );
    assert_eq!(second.bytes, entry.size);
    assert!(!partial.exists(), "the partial file was not consumed");
    eprintln!("resumed from {stopped_at} bytes and still verified");
}

#[tokio::test]
async fn a_digest_that_does_not_match_is_refused_and_the_bytes_are_discarded() {
    if !enabled() {
        eprintln!("skipped: set {NETWORK_ENV}=1 to run the network tier");
        return;
    }

    let profile = Profile::new("tampered");
    let entry = lightweight_catalog::manifest::by_id(PINNED).expect("pinned entry");
    let destination = profile.models().join(entry.file);
    std::fs::create_dir_all(profile.models()).expect("models dir");

    // One character different: what a corrupted or substituted file looks like.
    let mut tampered = entry.sha256.to_owned();
    tampered.replace_range(
        0..1,
        if entry.sha256.starts_with('a') {
            "b"
        } else {
            "a"
        },
    );

    let client = lightweight_download::client("hermes-test").expect("client");
    let err = lightweight_download::fetch(
        &client,
        lightweight_download::Fetch {
            url: &entry.url(),
            destination: &destination,
            expected_sha256: Some(&tampered),
            total_size: Some(entry.size),
            what: "the model",
        },
        &|_, _| {},
        &CancellationToken::new(),
    )
    .await
    .expect_err("a mismatched digest must be refused");

    assert_eq!(err.code(), "download_corrupt");
    assert!(!destination.exists(), "an unverified file was put in place");
    assert!(
        !lightweight_download::partial_path(&destination).exists(),
        "the failed bytes were left where a resume would inherit them"
    );
}

#[tokio::test]
async fn a_link_that_is_not_a_model_is_deleted_rather_than_catalogued() {
    if !enabled() {
        eprintln!("skipped: set {NETWORK_ENV}=1 to run the network tier");
        return;
    }

    let profile = Profile::new("nothtml");
    let installer = profile.installer();
    let mut store = CatalogStore::open(profile.catalog()).expect("catalog");
    let (tx, _updates) = progress();

    // A real URL that returns a perfectly valid file which is not a GGUF.
    let err = installer
        .add(
            &mut store,
            &AddModel::Link {
                url: "https://huggingface.co/robots.txt".into(),
                sha256: None,
            },
            &tx,
            &CancellationToken::new(),
        )
        .await
        .expect_err("an HTML or text file must not be registered as a model");

    assert!(matches!(err, CatalogError::NotAGguf { .. }), "{err}");
    assert!(store.is_empty(), "a non-model reached the catalog");
    assert_eq!(
        std::fs::read_dir(profile.downloads())
            .expect("downloads dir")
            .count(),
        0,
        "the downloaded non-model was left on disk"
    );
}

#[tokio::test]
async fn a_pasted_huggingface_link_is_verified_against_the_published_digest() {
    if !enabled() {
        eprintln!("skipped: set {NETWORK_ENV}=1 to run the network tier");
        return;
    }

    let profile = Profile::new("pasted");
    let installer = profile.installer();
    let mut store = CatalogStore::open(profile.catalog()).expect("catalog");
    let (tx, _updates) = progress();
    let entry = lightweight_catalog::manifest::by_id(PINNED).expect("pinned entry");

    // The same file, reached the way a user would: by pasting the link, with
    // no digest of their own. The digest comes from the LFS metadata.
    let model = installer
        .add(
            &mut store,
            &AddModel::Link {
                url: entry.url(),
                sha256: None,
            },
            &tx,
            &CancellationToken::new(),
        )
        .await
        .expect("a pasted link should download");

    assert_eq!(
        model.integrity,
        Integrity::Published,
        "a HuggingFace link must be verified, not merely recorded"
    );
    assert!(model.integrity.verified());
    assert_eq!(model.sha256, entry.sha256);
    // The stored link carries no query string.
    assert_eq!(model.source.url(), Some(entry.url().as_str()));
}

/// The import path needs a file, and the download tests leave one behind.
#[tokio::test]
async fn an_imported_file_is_referenced_where_it_is_rather_than_copied() {
    if !enabled() {
        eprintln!("skipped: set {NETWORK_ENV}=1 to run the network tier");
        return;
    }

    let profile = Profile::new("import");
    let installer = profile.installer();
    let mut store = CatalogStore::open(profile.catalog()).expect("catalog");
    let (tx, _updates) = progress();

    // Fetch once, then import the file from where it landed.
    let downloaded = installer
        .add(
            &mut store,
            &AddModel::Pinned { id: PINNED.into() },
            &tx,
            &CancellationToken::new(),
        )
        .await
        .expect("download");

    let elsewhere = profile.0.join("moved.gguf");
    std::fs::rename(&downloaded.path, &elsewhere).expect("move the file out");
    let mut second = CatalogStore::open(profile.0.join("second.json")).expect("catalog");

    let imported = installer
        .import(&mut second, &elsewhere, &tx)
        .await
        .expect("import");

    assert_eq!(
        imported.sha256, downloaded.sha256,
        "same bytes, same digest"
    );
    assert_eq!(imported.integrity, Integrity::Imported);
    assert!(!imported.integrity.verified(), "an import checks nothing");
    assert_eq!(
        imported.path,
        elsewhere.canonicalize().unwrap_or(elsewhere.clone()),
        "the file must be referenced where it is"
    );
    // Nothing was copied into our own directory.
    let copied = Path::new(&profile.models()).join("moved.gguf");
    assert!(!copied.exists(), "the import copied the file");
}
