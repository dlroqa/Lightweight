//! Getting a model into the catalog: from disk, from the pinned list, or from a
//! link.
//!
//! The three routes converge on the same three steps — get the bytes, learn
//! their digest, read the GGUF header — and differ only in how much can be
//! promised about the bytes afterwards. That difference is carried in
//! [`Integrity`] and is never smoothed over: a download with no digest to check
//! against is *recorded*, not verified, and the catalog says so.
//!
//! Two rules that are easy to miss and expensive to get wrong:
//!
//! * **An import references the file where it is; it does not copy it.** A user
//!   who already has a 4 GB model should not need 8 GB to add it. The
//!   consequence is that removing an imported model must never delete the file,
//!   because it is not ours — only a downloaded model lives in our directory.
//! * **A downloaded file lands in the cache and is moved into place only once
//!   it has passed.** `models_dir` therefore never contains a partial or
//!   unverified file, which is what makes scanning it safe.

use std::path::{Path, PathBuf};

use hermes_download::{Fetch, ProgressSink};
use hermes_gguf::{GgufFile, ModelMetadata};
use hermes_observability::targets;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::CatalogError;
use crate::manifest::{self, CatalogModel};
use crate::record::{InstalledModel, Integrity, Source, slug_for};
use crate::store::CatalogStore;

/// `EXDEV`: a rename across filesystems.
///
/// `std::io::ErrorKind::CrossesDevices` is still unstable, so the raw code is
/// matched, exactly as the download layer matches `ENOSPC`. Windows reports
/// `ERROR_NOT_SAME_DEVICE`.
const CROSS_DEVICE_ERRNO: i32 = if cfg!(windows) { 17 } else { 18 };

/// What stage an install has reached.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum InstallProgress {
    /// Working out where the file is and what its digest should be.
    Resolving,
    Downloading {
        downloaded: u64,
        total: Option<u64>,
    },
    /// Hashing a file that is already on disk, during an import.
    Hashing {
        done: u64,
        total: u64,
    },
    /// Reading the GGUF header.
    Reading,
    Done,
}

/// Where a model should come from.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum AddModel {
    /// One of the pinned models.
    Pinned { id: String },
    /// A direct https link, with an optional digest to check it against.
    Link {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
    },
}

/// Adds models to the catalog.
#[derive(Debug)]
pub struct Installer {
    client: reqwest::Client,
    /// Where downloaded weights end up.
    models_dir: PathBuf,
    /// Where a transfer runs before it has passed verification.
    downloads_dir: PathBuf,
}

impl Installer {
    pub fn new(
        models_dir: impl Into<PathBuf>,
        downloads_dir: impl Into<PathBuf>,
    ) -> Result<Self, CatalogError> {
        Ok(Self {
            client: hermes_download::client(concat!("hermes-gateway/", env!("CARGO_PKG_VERSION")))?,
            models_dir: models_dir.into(),
            downloads_dir: downloads_dir.into(),
        })
    }

    /// Register a model that is already on this machine.
    ///
    /// The file is left where it is. What this costs is one full read to
    /// compute the digest, which is what makes a later corruption detectable.
    /// Register a model that is already on this machine.
    ///
    /// Composes [`Installer::scan_local`] and [`Installer::commit`]. Callers
    /// that hold the catalog behind a lock should use those two directly, so
    /// the lock is not held across the hashing — see the note on
    /// [`Installer::fetch`].
    pub async fn import(
        &self,
        store: &mut CatalogStore,
        path: impl AsRef<Path>,
        progress: &mpsc::Sender<InstallProgress>,
    ) -> Result<InstalledModel, CatalogError> {
        let scanned = self.scan_local(path, progress).await?;
        let record = Self::commit(store, scanned)?;
        let _ = progress.try_send(InstallProgress::Done);
        Ok(record)
    }

    /// Fetch a model and register it.
    ///
    /// Composes [`Installer::plan`], [`Installer::already_installed`],
    /// [`Installer::fetch`] and [`Installer::commit`].
    pub async fn add(
        &self,
        store: &mut CatalogStore,
        request: &AddModel,
        progress: &mpsc::Sender<InstallProgress>,
        cancel: &CancellationToken,
    ) -> Result<InstalledModel, CatalogError> {
        let _ = progress.try_send(InstallProgress::Resolving);
        let plan = self.plan(request).await?;
        if let Some(existing) = Self::already_installed(store, &plan) {
            let _ = progress.try_send(InstallProgress::Done);
            return Ok(existing);
        }
        let scanned = self.fetch(&plan, progress, cancel).await?;
        let record = Self::commit(store, scanned)?;
        let _ = progress.try_send(InstallProgress::Done);
        Ok(record)
    }

    /// Hash a local file and read its header. **Touches no catalog.**
    pub async fn scan_local(
        &self,
        path: impl AsRef<Path>,
        progress: &mpsc::Sender<InstallProgress>,
    ) -> Result<Scanned, CatalogError> {
        let path = path.as_ref().to_path_buf();
        if !path.is_file() {
            return Err(CatalogError::FileNotFound { path });
        }
        // Absolute, so the record still resolves if the process later runs from
        // a different working directory. Reported rather than quietly falling
        // back to what was given: a catalog entry whose path only works from
        // one directory is a model that goes missing later, for a reason nobody
        // will connect to the import.
        let path = path
            .canonicalize()
            .map_err(|err| CatalogError::io("resolving the model's path", err))?;

        let sink = progress.clone();
        let hash_path = path.clone();
        let (sha256, bytes) = tokio::task::spawn_blocking(move || {
            hermes_download::hash_file(&hash_path, &move |done, total| {
                // `hash_file` reads the size before it starts, so the total is
                // always known here; `done` is the honest fallback if that ever
                // changes, since it never reports more than has been read.
                let _ = sink.try_send(InstallProgress::Hashing {
                    done,
                    total: total.unwrap_or(done),
                });
            })
        })
        .await
        .map_err(|err| CatalogError::io("hashing the model", std::io::Error::other(err)))??;

        let _ = progress.try_send(InstallProgress::Reading);
        let metadata = read_header(&path)?;

        Ok(Scanned {
            // An import has no id yet: it is derived from the file name when
            // the catalog is locked, because that is when a free one can be
            // chosen.
            id: None,
            source: Source::Import {
                original_path: path.clone(),
            },
            path,
            bytes,
            sha256,
            integrity: Integrity::Imported,
            metadata,
        })
    }

    /// Whether this exact file is already installed under the planned id.
    ///
    /// Only ever true when the plan carries a digest to compare against:
    /// without one there is nothing to prove the file on disk is the file the
    /// URL now points at, so it is fetched again.
    pub fn already_installed(store: &CatalogStore, plan: &Plan) -> Option<InstalledModel> {
        let expected = plan.expected_sha256.as_deref()?;
        let existing = store.get(&plan.id)?;
        (existing.is_present() && existing.sha256.eq_ignore_ascii_case(expected))
            .then(|| existing.clone())
    }

    /// Download, verify, and read the header. **Touches no catalog.**
    ///
    /// Deliberately takes no store: this is the long part — minutes for a
    /// gigabyte — and a caller holding a lock across it would block every read
    /// of the catalog for the duration, including the listing a UI uses to show
    /// the download's own progress.
    pub async fn fetch(
        &self,
        plan: &Plan,
        progress: &mpsc::Sender<InstallProgress>,
        cancel: &CancellationToken,
    ) -> Result<Scanned, CatalogError> {
        std::fs::create_dir_all(&self.downloads_dir)
            .map_err(|err| CatalogError::io("creating the downloads directory", err))?;
        std::fs::create_dir_all(&self.models_dir)
            .map_err(|err| CatalogError::io("creating the models directory", err))?;

        let staged = self.downloads_dir.join(&plan.file_name);
        let sink = progress.clone();
        let fetched = hermes_download::fetch(
            &self.client,
            Fetch {
                url: &plan.url,
                destination: &staged,
                expected_sha256: plan.expected_sha256.as_deref(),
                total_size: plan.total_size,
                what: "the model",
            },
            &(move |downloaded, total| {
                let _ = sink.try_send(InstallProgress::Downloading { downloaded, total });
            }) as ProgressSink<'_>,
            cancel,
        )
        .await?;

        // A link that redirected to an error page produces a perfectly valid
        // file that is not a model. Checked before it is moved into the models
        // directory, so nothing that fails this is ever kept.
        let _ = progress.try_send(InstallProgress::Reading);
        let metadata = match read_header(&staged) {
            Ok(metadata) => metadata,
            Err(err) => {
                let _ = std::fs::remove_file(&staged);
                return Err(err);
            }
        };

        let destination = self.models_dir.join(&plan.file_name);
        move_into_place(&staged, &destination)?;

        tracing::info!(
            target: targets::MODEL,
            id = %plan.id,
            // The host, never the whole link: a URL can carry a token in its
            // query string, and a log file is exactly where that must not go.
            host = %host_of(&plan.url),
            integrity = ?plan.integrity,
            bytes = fetched.bytes,
            "model downloaded"
        );

        Ok(Scanned {
            id: Some(plan.id.clone()),
            path: destination,
            bytes: fetched.bytes,
            sha256: fetched.sha256,
            integrity: plan.integrity,
            source: plan.source.clone(),
            metadata,
        })
    }

    /// Put a scanned file into the catalog and persist it.
    ///
    /// The only phase that needs the catalog, and it is all local work.
    pub fn commit(
        store: &mut CatalogStore,
        scanned: Scanned,
    ) -> Result<InstalledModel, CatalogError> {
        // The same bytes twice is not an error, and installing them twice under
        // two ids would be. Only applies to an import: a download already knows
        // the id it is replacing.
        if scanned.id.is_none()
            && let Some(existing) = store.by_digest(&scanned.sha256)
        {
            return Ok(existing.clone());
        }

        let id = match &scanned.id {
            Some(id) => id.clone(),
            None => store.free_id(&slug_for(&scanned.path)),
        };
        let replacing = scanned.id.is_some();
        let record = InstalledModel::new(
            id,
            &scanned.path,
            scanned.bytes,
            &scanned.sha256,
            scanned.integrity,
            scanned.source,
            &scanned.metadata,
        );

        if replacing {
            // A re-download is new bytes under a known id, so the old record
            // describes a file that is gone.
            store.replace(record.clone());
        } else {
            store.insert(record.clone())?;
        }
        store.save()?;

        tracing::info!(
            target: targets::MODEL,
            id = %record.id,
            architecture = %record.architecture,
            bytes = record.bytes,
            integrity = ?record.integrity,
            "model added to the catalog"
        );
        Ok(record)
    }

    /// Work out what to fetch, and what can be promised about it.
    ///
    /// May make one small metadata request (the HuggingFace digest lookup), so
    /// it is async — and takes no catalog, so that call is not made under a
    /// lock either.
    pub async fn plan(&self, request: &AddModel) -> Result<Plan, CatalogError> {
        match request {
            AddModel::Pinned { id } => {
                let model: &CatalogModel = manifest::by_id(id)
                    .ok_or_else(|| CatalogError::UnknownManifestModel { id: id.clone() })?;
                if !manifest::is_recorded(model) {
                    // Refusing beats downgrading: a pinned model exists
                    // precisely so its digest is known in advance.
                    return Err(CatalogError::UnknownManifestModel {
                        id: format!("{id} (its digest has not been recorded)"),
                    });
                }
                Ok(Plan {
                    id: model.id.to_owned(),
                    file_name: model.file_name().to_owned(),
                    url: model.url(),
                    expected_sha256: Some(model.sha256.to_owned()),
                    total_size: Some(model.size),
                    integrity: Integrity::Manifest,
                    source: Source::Manifest {
                        manifest_id: model.id.to_owned(),
                        url: model.url(),
                    },
                })
            }
            AddModel::Link { url, sha256 } => {
                let url = url.trim().to_owned();
                if let Some(supplied) = sha256 {
                    let supplied = supplied.trim();
                    if supplied.len() != 64 || !supplied.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return Err(CatalogError::NotADigest {
                            sha256: supplied.to_owned(),
                        });
                    }
                }

                let file_name = file_name_from_url(&url);
                // A digest the host published is better than none, and costs
                // one small JSON call. Absent that, the user's own digest. If
                // neither exists the download is recorded rather than verified.
                let published = match (&sha256, crate::hf::parse_resolve_link(&url)) {
                    (None, Some(link)) => crate::hf::published_digest(&self.client, &link).await,
                    _ => None,
                };

                let (expected, total, integrity) = match (sha256, published) {
                    (Some(supplied), _) => (
                        Some(supplied.trim().to_ascii_lowercase()),
                        None,
                        Integrity::Supplied,
                    ),
                    (None, Some((digest, size))) => {
                        (Some(digest), Some(size), Integrity::Published)
                    }
                    (None, None) => (None, None, Integrity::Recorded),
                };

                Ok(Plan {
                    id: slug_for(Path::new(&file_name)),
                    file_name,
                    expected_sha256: expected,
                    total_size: total,
                    integrity,
                    source: Source::Link {
                        url: strip_query(&url),
                    },
                    url,
                })
            }
        }
    }
}

/// What one download is going to do, decided before any bytes move.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The catalog id this will be installed under.
    pub id: String,
    /// The file name it is stored as, taken from the URL but never trusted.
    pub file_name: String,
    pub url: String,
    /// The digest to check against, when one could be established.
    pub expected_sha256: Option<String>,
    pub total_size: Option<u64>,
    pub integrity: Integrity,
    pub source: Source,
}

/// A file that is on disk, hashed, and confirmed to be a GGUF.
///
/// The result of the long phase, and everything [`Installer::commit`] needs. It
/// exists so that the hashing and the downloading happen with no catalog lock
/// held.
#[derive(Clone, Debug)]
pub struct Scanned {
    /// The id a download already chose; `None` for an import, whose id is
    /// derived when the catalog is locked.
    pub id: Option<String>,
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub integrity: Integrity,
    pub source: Source,
    pub metadata: ModelMetadata,
}

/// Read a GGUF header, reporting a file that is not one as such.
///
/// Public because the load path asks the same question of the same file, and a
/// second copy of "is this a model?" is a second answer waiting to disagree.
pub fn read_header(path: &Path) -> Result<ModelMetadata, CatalogError> {
    let file = GgufFile::open(path).map_err(|err| CatalogError::NotAGguf {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;
    ModelMetadata::from_file(&file).map_err(|err| CatalogError::NotAGguf {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })
}

/// Move a staged file into the models directory.
///
/// A rename when both sit on one filesystem, which is the normal case; a copy
/// when they do not. `~/.cache` and `~/.local/share` are usually the same
/// mount and are not guaranteed to be, and a failed install because someone put
/// their cache on a different disk would be a genuinely baffling error.
fn move_into_place(staged: &Path, destination: &Path) -> Result<(), CatalogError> {
    match std::fs::rename(staged, destination) {
        Ok(()) => Ok(()),
        // `EXDEV` and nothing else. Falling back to a copy on *any* rename
        // failure would turn a permissions problem into a second, misleading
        // error about copying, and hide the first.
        Err(err) if err.raw_os_error() == Some(CROSS_DEVICE_ERRNO) => {
            std::fs::copy(staged, destination).map_err(|err| {
                CatalogError::io(
                    format!("copying the model to {}", destination.display()),
                    err,
                )
            })?;
            // Best effort: the copy succeeded, so the model is installed. A
            // staged file that will not delete is wasted disk, not a failure.
            let _ = std::fs::remove_file(staged);
            Ok(())
        }
        // The file is downloaded and verified; it is only in the wrong place.
        // Naming it is what lets someone move it by hand rather than fetch a
        // gigabyte again.
        Err(err) => Err(CatalogError::io(
            format!(
                "moving the verified model from {} to {}",
                staged.display(),
                destination.display()
            ),
            err,
        )),
    }
}

/// The last path segment of a URL, without any query string.
fn file_name_from_url(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    // Only ever a *path* segment. Splitting the whole URL on `/` would turn
    // `https://example.com` — which has no path at all — into a model file
    // named after the host.
    let after_scheme = without_query
        .split_once("://")
        .map_or(without_query, |(_, rest)| rest);
    let Some((_, path)) = after_scheme.split_once('/') else {
        return "model.gguf".to_owned();
    };
    let name = path.rsplit('/').next().unwrap_or("");
    let name = name.trim();
    // A path component cannot be trusted to be a file name: it arrives from
    // outside and gets joined onto our models directory.
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        "model.gguf".to_owned()
    } else {
        name.to_owned()
    }
}

/// The URL without its query string or fragment.
///
/// What gets stored in the catalog file. A link can carry a token, and the
/// catalog is a file a user might reasonably paste into a bug report.
fn strip_query(url: &str) -> String {
    url.split(['?', '#']).next().unwrap_or(url).to_owned()
}

/// The host part of a URL, for logging.
fn host_of(url: &str) -> String {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("unknown")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_name_is_taken_from_the_link_but_never_trusted() {
        assert_eq!(
            file_name_from_url("https://huggingface.co/o/r/resolve/main/Model-Q4_K_M.gguf"),
            "Model-Q4_K_M.gguf"
        );
        assert_eq!(
            file_name_from_url("https://example.com/m.gguf?download=true&token=abc"),
            "m.gguf"
        );
        // Nothing usable at the end of the path: a name of our own rather than
        // an empty one, or one that could climb out of the models directory.
        for hostile in [
            "https://example.com/",
            "https://example.com/..",
            "https://example.com/.",
            "https://example.com",
        ] {
            assert_eq!(file_name_from_url(hostile), "model.gguf", "{hostile}");
        }
    }

    #[test]
    fn a_stored_link_keeps_no_query_string() {
        // The catalog file is something a user might paste into an issue. A
        // token in a query string must not travel with it.
        assert_eq!(
            strip_query("https://example.com/m.gguf?token=secret"),
            "https://example.com/m.gguf"
        );
        assert_eq!(
            strip_query("https://example.com/m.gguf"),
            "https://example.com/m.gguf"
        );
    }

    #[test]
    fn only_the_host_is_ever_logged() {
        assert_eq!(
            host_of("https://huggingface.co/o/r/resolve/main/m.gguf?token=secret"),
            "huggingface.co"
        );
        assert_eq!(host_of("nonsense"), "unknown");
    }

    /// A file that exists no matter where the test is run from.
    const REAL_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/install.rs");

    /// A store holding one model, for the phase tests.
    ///
    /// The record is built directly rather than through
    /// [`InstalledModel::new`], which would need a whole `ModelMetadata`: these
    /// tests are about the id, the digest and whether the file is there.
    fn store_with(id: &str, sha256: &str, path: &Path) -> CatalogStore {
        let mut store = CatalogStore::in_memory();
        store
            .insert(InstalledModel {
                id: id.to_owned(),
                name: id.to_owned(),
                path: path.to_path_buf(),
                bytes: 10,
                sha256: sha256.to_owned(),
                integrity: Integrity::Manifest,
                source: Source::Link {
                    url: "https://example.com/m.gguf".into(),
                },
                architecture: "llama".into(),
                supported: true,
                param_count: None,
                quantization: None,
                context_length: Some(4096),
                weight_bytes: None,
                added_at: 0,
                last_loaded_at: None,
                last_n_ctx: None,
            })
            .expect("insert");
        store
    }

    #[test]
    fn a_file_we_can_prove_we_already_hold_is_not_fetched_again() {
        let plan = Plan {
            id: "m".into(),
            file_name: "m.gguf".into(),
            url: "https://example.com/m.gguf".into(),
            expected_sha256: Some("aa".into()),
            total_size: None,
            integrity: Integrity::Manifest,
            source: Source::Link {
                url: "https://example.com/m.gguf".into(),
            },
        };

        // The file has to exist for the record to count as present, so the
        // check is made against a file that certainly does. Not `file!()`,
        // which is relative to the workspace root while a test runs in the
        // crate directory - the first version of this test asserted a `cwd`
        // instead of a behaviour.
        let real = Path::new(REAL_FILE);
        let store = store_with("m", "AA", real);
        assert!(
            Installer::already_installed(&store, &plan).is_some(),
            "a digest match is not case-sensitive"
        );

        // Different bytes under the same id: fetch it.
        let other = store_with("m", "bb", real);
        assert!(Installer::already_installed(&other, &plan).is_none());
    }

    #[test]
    fn without_a_digest_nothing_can_be_proven_so_the_file_is_fetched() {
        // The case that a comparison against an empty string used to handle by
        // accident. With no expected digest there is nothing to show that the
        // bytes on disk are the bytes this URL serves now.
        let plan = Plan {
            id: "m".into(),
            file_name: "m.gguf".into(),
            url: "https://example.com/m.gguf".into(),
            expected_sha256: None,
            total_size: None,
            integrity: Integrity::Recorded,
            source: Source::Link {
                url: "https://example.com/m.gguf".into(),
            },
        };
        let store = store_with("m", "aa", Path::new(REAL_FILE));
        assert!(Installer::already_installed(&store, &plan).is_none());
    }

    #[test]
    fn a_record_whose_file_is_gone_is_fetched_again() {
        let plan = Plan {
            id: "m".into(),
            file_name: "m.gguf".into(),
            url: "https://example.com/m.gguf".into(),
            expected_sha256: Some("aa".into()),
            total_size: None,
            integrity: Integrity::Manifest,
            source: Source::Link {
                url: "https://example.com/m.gguf".into(),
            },
        };
        let store = store_with("m", "aa", Path::new("/definitely/not/here.gguf"));
        assert!(
            Installer::already_installed(&store, &plan).is_none(),
            "a catalog entry with no file must not stop a re-download"
        );
    }

    #[tokio::test]
    async fn a_pinned_model_is_planned_with_its_recorded_digest() {
        let installer = Installer::new("/models", "/downloads").expect("installer");
        let plan = installer
            .plan(&AddModel::Pinned {
                id: "smollm2-135m-instruct-q4_k_m".into(),
            })
            .await
            .expect("plan");

        assert_eq!(plan.integrity, Integrity::Manifest);
        assert!(plan.expected_sha256.is_some());
        assert!(plan.total_size.is_some());
        assert!(plan.url.starts_with("https://huggingface.co/"));
    }

    #[tokio::test]
    async fn an_unknown_pinned_id_is_refused_rather_than_guessed_at() {
        let installer = Installer::new("/models", "/downloads").expect("installer");
        let err = installer
            .plan(&AddModel::Pinned { id: "gpt-4".into() })
            .await
            .expect_err("unknown");
        assert!(matches!(err, CatalogError::UnknownManifestModel { .. }));
    }

    #[tokio::test]
    async fn a_supplied_digest_is_used_and_a_malformed_one_is_refused() {
        let installer = Installer::new("/models", "/downloads").expect("installer");

        let plan = installer
            .plan(&AddModel::Link {
                url: "https://example.com/m.gguf".into(),
                sha256: Some("A".repeat(64)),
            })
            .await
            .expect("plan");
        assert_eq!(plan.integrity, Integrity::Supplied);
        // Normalised, so it can be compared against a computed digest.
        assert_eq!(
            plan.expected_sha256.as_deref(),
            Some("a".repeat(64).as_str())
        );

        for bad in ["abc", "", &"z".repeat(64)] {
            let err = installer
                .plan(&AddModel::Link {
                    url: "https://example.com/m.gguf".into(),
                    sha256: Some(bad.to_owned()),
                })
                .await
                .expect_err("malformed digest");
            assert!(matches!(err, CatalogError::NotADigest { .. }), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_link_with_no_digest_anywhere_is_recorded_not_verified() {
        // The honest downgrade. `example.com` is not HuggingFace, so there is
        // no published digest to look up and no network call is made.
        let installer = Installer::new("/models", "/downloads").expect("installer");
        let plan = installer
            .plan(&AddModel::Link {
                url: "https://example.com/m.gguf".into(),
                sha256: None,
            })
            .await
            .expect("plan");

        assert_eq!(plan.integrity, Integrity::Recorded);
        assert!(plan.expected_sha256.is_none());
        assert_eq!(plan.id, "m");
    }
}
