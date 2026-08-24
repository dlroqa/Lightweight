//! The manager's behaviour while a real download is in flight.
//!
//! Opt-in, like the other network tier: set `HERMES_TEST_NETWORK=1`. The
//! property here cannot be checked without a transfer that actually takes
//! time — a mock that returns instantly would pass whether or not the catalog
//! lock were held across it, which is the exact shape of a test that passes for
//! the wrong reason.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hermes_catalog::CatalogStore;
use hermes_catalog::install::{AddModel, Installer};
use hermes_gateway::jobs::{JobKind, Jobs};
use hermes_gateway::manager::{ModelManager, RuntimeDefaults};
use tokio_util::sync::CancellationToken;

const NETWORK_ENV: &str = "HERMES_TEST_NETWORK";
const PINNED: &str = "smollm2-135m-instruct-q4_k_m";

/// A throwaway profile that cleans up after itself.
struct Profile(PathBuf);

impl Profile {
    fn new(tag: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("hermes-mgr-{tag}-{unique}"));
        std::fs::create_dir_all(&path).expect("profile dir");
        Self(path)
    }

    fn manager(&self) -> ModelManager {
        ModelManager::new(
            CatalogStore::open(self.0.join("catalog.json")).expect("catalog"),
            Installer::new(self.0.join("models"), self.0.join("downloads")).expect("installer"),
            RuntimeDefaults::default(),
        )
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_catalog_answers_while_a_real_download_is_running() {
    if std::env::var_os(NETWORK_ENV).is_none() {
        eprintln!("skipped: set {NETWORK_ENV}=1 to run the network tier");
        return;
    }

    let profile = Profile::new("listing");
    let manager = Arc::new(profile.manager());
    let jobs = Jobs::new();
    let job = jobs.start(JobKind::Download, &CancellationToken::new());

    let downloading = {
        let manager = Arc::clone(&manager);
        let job = Arc::clone(&job);
        tokio::spawn(async move {
            manager
                .install(&AddModel::Pinned { id: PINNED.into() }, &job)
                .await
        })
    };

    // Poll the listing throughout the transfer. Holding the catalog lock across
    // the download would make each of these wait for the whole thing — which is
    // what a UI refreshing its model list during a download would experience.
    let mut slowest = Duration::ZERO;
    let mut polls = 0;
    while !downloading.is_finished() {
        let started = Instant::now();
        let _ = tokio::time::timeout(Duration::from_secs(5), manager.models())
            .await
            .expect("listing the catalog blocked behind the download");
        slowest = slowest.max(started.elapsed());
        polls += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let model = downloading
        .await
        .expect("the download task")
        .expect("the download");
    assert_eq!(model.id, PINNED);

    assert!(
        polls > 1,
        "the download finished too quickly to prove anything: {polls} poll(s)"
    );
    assert!(
        slowest < Duration::from_millis(500),
        "listing the catalog took {slowest:?} during the download"
    );
    eprintln!("listed the catalog {polls} times mid-download, slowest {slowest:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_that_is_cancelled_leaves_the_catalog_untouched() {
    if std::env::var_os(NETWORK_ENV).is_none() {
        eprintln!("skipped: set {NETWORK_ENV}=1 to run the network tier");
        return;
    }

    let profile = Profile::new("cancel");
    let manager = Arc::new(profile.manager());
    let jobs = Jobs::new();
    let job = jobs.start(JobKind::Download, &CancellationToken::new());

    let downloading = {
        let manager = Arc::clone(&manager);
        let job = Arc::clone(&job);
        tokio::spawn(async move {
            manager
                .install(&AddModel::Pinned { id: PINNED.into() }, &job)
                .await
        })
    };

    tokio::time::sleep(Duration::from_millis(300)).await;
    job.cancel();

    let outcome = tokio::time::timeout(Duration::from_secs(60), downloading)
        .await
        .expect("the cancelled download should return promptly")
        .expect("the download task");

    match outcome {
        // Cancelled before it finished: nothing may be recorded.
        Err(err) => {
            assert_eq!(hermes_core::Actionable::code(&err), "cancelled");
            assert!(
                manager.models().await.is_empty(),
                "a cancelled download left a record behind"
            );
        }
        // Fast enough to beat the cancel. Nothing is wrong, and there is
        // nothing to assert about a cancellation that never applied.
        Ok(model) => {
            eprintln!(
                "the download completed before it could be cancelled: {}",
                model.id
            );
        }
    }
}
