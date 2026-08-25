//! Saved benchmark runs, on disk.
//!
//! `paths.benchmarks_dir()` was chosen in M0, has been created at every startup
//! since, and nothing had ever written to it. This is what writes to it.
//!
//! A directory of documents rather than one growing file, modelled on
//! `hermes_store::conversations`: runs accumulate, are read one at a time, and
//! the newest is the one anybody wants. Owner-only, because a run describes the
//! user's hardware in detail and there is no reason for anyone else on the
//! machine to enumerate it.

use std::path::{Path, PathBuf};

use hermes_store::StoreError;
use hermes_store::atomic;
use serde::{Deserialize, Serialize};

use crate::error::BenchError;
use crate::record::{BenchmarkRun, FORMAT_VERSION};

/// How many runs a listing returns.
///
/// A bound on work, not only on the answer: the sort is by modification time
/// before any file is opened.
const LIST_LIMIT: usize = 200;

const ID_BYTES: usize = 8;
const ID_LENGTH: usize = ID_BYTES * 2;

/// The document as written.
#[derive(Serialize, Deserialize)]
struct RunFile {
    version: u32,
    #[serde(flatten)]
    run: BenchmarkRun,
}

/// The benchmark directory.
#[derive(Clone, Debug)]
pub struct BenchmarkStore {
    directory: PathBuf,
}

impl BenchmarkStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// An id for a new run.
    ///
    /// Derived from the clock, unlike a conversation's random id, and
    /// deliberately: runs are read in the order they were taken far more often
    /// than they are read by name, and an id that sorts by time makes a
    /// directory listing legible without opening anything. There is no secrecy
    /// requirement here — a benchmark is not a conversation.
    pub fn new_id() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default();
        format!("{:016x}", nanos as u64)
    }

    /// Whether `id` is one this store could have produced.
    ///
    /// Length and alphabet only, which is enough: a value that passes contains
    /// no separator, no `..`, no null and no drive letter, so joining it to the
    /// directory cannot leave the directory.
    fn is_well_formed(id: &str) -> bool {
        id.len() == ID_LENGTH && id.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    fn path_for(&self, id: &str) -> Result<PathBuf, StoreError> {
        if !Self::is_well_formed(id) {
            return Err(StoreError::MalformedId { id: id.to_owned() });
        }
        Ok(self.directory.join(format!("{id}.json")))
    }

    /// Write one run.
    pub fn save(&self, run: &BenchmarkRun) -> Result<PathBuf, BenchError> {
        let path = self
            .path_for(&run.id)
            .map_err(|_| BenchError::MalformedId { id: run.id.clone() })?;
        atomic::create_private_dir(&self.directory).map_err(|source| BenchError::Write {
            path: self.directory.display().to_string(),
            source,
        })?;
        let file = RunFile {
            version: FORMAT_VERSION,
            run: run.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&file).map_err(|err| BenchError::Encode {
            detail: err.to_string(),
        })?;
        bytes.push(b'\n');
        atomic::write_private(&path, &bytes).map_err(|source| BenchError::Write {
            path: path.display().to_string(),
            source,
        })?;
        Ok(path)
    }

    /// Read one run.
    pub fn get(&self, id: &str) -> Result<BenchmarkRun, BenchError> {
        let path = self
            .path_for(id)
            .map_err(|_| BenchError::MalformedId { id: id.to_owned() })?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(BenchError::NoSuchRun { id: id.to_owned() });
            }
            Err(err) => {
                return Err(BenchError::Read {
                    detail: err.to_string(),
                });
            }
        };
        parse(&bytes).ok_or_else(|| BenchError::Read {
            detail: "the file did not parse as a benchmark run".to_owned(),
        })
    }

    /// Every saved run, newest first.
    ///
    /// A file that will not parse is skipped rather than failing the listing —
    /// one unreadable run from an older format must not make the rest
    /// invisible. That is the opposite of the catalog's rule, and deliberately:
    /// a corrupt catalog would be silently *replaced* by an empty one on the
    /// next write, where a skipped benchmark is only a benchmark not shown.
    pub fn list(&self) -> Result<Vec<BenchmarkRun>, BenchError> {
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(BenchError::Read {
                    detail: err.to_string(),
                });
            }
        };

        let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
                    && path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .is_some_and(Self::is_well_formed)
            })
            .map(|path| {
                let modified = path
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                (modified, path)
            })
            .collect();
        candidates.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
        candidates.truncate(LIST_LIMIT);

        Ok(candidates
            .into_iter()
            .filter_map(|(_, path)| std::fs::read(&path).ok())
            .filter_map(|bytes| parse(&bytes))
            .collect())
    }
}

fn parse(bytes: &[u8]) -> Option<BenchmarkRun> {
    serde_json::from_slice::<RunFile>(bytes)
        .ok()
        .map(|file| file.run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{
        EngineFingerprint, MachineFingerprint, ModelFingerprint, Sample, Scenario,
    };
    use hermes_core::{RuntimeParams, units::Bytes};

    fn scratch(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!("hermes-bench-{tag}-{unique}"));
        std::fs::create_dir_all(&path).expect("scratch dir");
        path
    }

    fn a_run(id: &str) -> BenchmarkRun {
        BenchmarkRun {
            id: id.to_owned(),
            at_unix: 1_700_000_000,
            machine: MachineFingerprint {
                cpu_model: Some("A Processor".to_owned()),
                physical_cores: 4,
                logical_cores: 4,
                isa_features: vec!["sse4.2".to_owned()],
                total_memory: Bytes::from_mib(8_192),
                os: "linux".to_owned(),
                architecture: "x86_64".to_owned(),
            },
            engine: EngineFingerprint {
                backend: "llama.cpp".to_owned(),
                build: Some("b10590".to_owned()),
                ggml_variant: Some("sse42".to_owned()),
            },
            model: ModelFingerprint {
                id: "a-model@2k".to_owned(),
                architecture: "llama".to_owned(),
                quantization: "Q4_K_M".to_owned(),
                parameters: Some(135_000_000),
            },
            samples: vec![Sample {
                scenario: Scenario::Decode,
                params: RuntimeParams::default(),
                threads: 4,
                repetition: 0,
                prompt_tokens: 12,
                cached_tokens: 0,
                prefilled_tokens: 12,
                generated_tokens: 24,
                prefill_ms: Some(500.0),
                decode_ms: Some(24_000.0),
                time_to_first_token_ms: Some(600),
                wall_ms: 24_700,
                engine_ticks: Some(9_000),
                machine_ticks: Some(10_000),
                rss: Some(Bytes::from_mib(180)),
                peak_rss: Some(Bytes::from_mib(190)),
                predicted: None,
            }],
        }
    }

    #[test]
    fn a_run_survives_the_round_trip() {
        let directory = scratch("roundtrip");
        let store = BenchmarkStore::new(&directory);
        let run = a_run(&BenchmarkStore::new_id());
        store.save(&run).expect("save");
        let read = store.get(&run.id).expect("get");
        assert_eq!(read.id, run.id);
        assert_eq!(read.samples.len(), 1);
        assert_eq!(read.samples[0].generated_tokens, 24);
        assert_eq!(read.machine.physical_cores, 4);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_saved_run_contains_no_path_no_prompt_and_no_hostname() {
        // The structural guarantee: there is nowhere in the record to put any
        // of them, and this is the test that fails if somebody adds one.
        let directory = scratch("privacy");
        let store = BenchmarkStore::new(&directory);
        let run = a_run(&BenchmarkStore::new_id());
        let path = store.save(&run).expect("save");
        let written = std::fs::read_to_string(&path).expect("read back");
        assert!(!written.contains(".gguf"), "{written}");
        assert!(!written.contains("/home/"), "{written}");
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        let hostname = hostname.trim();
        if !hostname.is_empty() {
            assert!(!written.contains(hostname), "{written}");
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_id_that_could_not_have_come_from_here_never_reaches_the_filesystem() {
        let directory = scratch("traversal");
        let store = BenchmarkStore::new(&directory);
        for id in ["../../etc/passwd", "..", "", "not-hex-at-all-nope!!"] {
            assert!(store.get(id).is_err(), "{id} was treated as an id");
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_directory_that_has_never_been_written_to_lists_nothing_rather_than_failing() {
        let store = BenchmarkStore::new(std::env::temp_dir().join("hermes-bench-absent-dir"));
        assert!(store.list().expect("an empty listing").is_empty());
    }

    #[test]
    fn an_unparseable_file_is_skipped_rather_than_hiding_every_other_run() {
        let directory = scratch("skip");
        let store = BenchmarkStore::new(&directory);
        let good = a_run(&BenchmarkStore::new_id());
        store.save(&good).expect("save");
        std::fs::write(
            directory.join(format!("{}.json", "ab".repeat(8))),
            b"{ not json",
        )
        .expect("write junk");
        let listed = store.list().expect("list");
        assert_eq!(listed.len(), 1, "the readable run is still listed");
        let _ = std::fs::remove_dir_all(&directory);
    }
}
