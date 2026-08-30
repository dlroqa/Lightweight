//! `GET /api/v1/system` — what this machine is, and how loaded it is.
//!
//! Four probes answer here, and they fail independently: CPU topology is read
//! from `/proc/cpuinfo`, processor time from `/proc/stat`, memory from
//! `/proc/meminfo`, and free space from `statvfs`. Any of them can be missing on
//! a platform none of them has been verified on yet.
//!
//! So this endpoint never fails as a whole. Each section carries its own
//! outcome, and [`Probed`] makes the difference between "measured" and "could
//! not measure" impossible to overlook — the same reason
//! [`lightweight_system_info::network`] keeps a partial address list rather than
//! discarding it. A panel that showed `0%` where the honest answer is "this
//! platform has no probe yet" would be reporting an idle machine, which is the
//! one reading an operator must never be given wrongly.
//!
//! Nothing here is a rate. `/proc/stat` publishes counters, and a percentage
//! needs two readings of them; this endpoint hands over the counters and the
//! caller differences consecutive polls, exactly as the panel's charts do with
//! `/api/v1/metrics`. See [`lightweight_system_info::load`] for why no background
//! sampler was added to make that look easier.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use lightweight_core::{Actionable, Bytes};
use lightweight_system_info::{
    CpuInfo, CpuTimes, DiskSpace, MemoryProbe, MemorySnapshot, SystemMemoryProbe,
};
use serde::Serialize;

use crate::routes::authorize;
use crate::state::GatewayState;

/// One probe's outcome: what it read, or why it could not read it.
///
/// Tagged rather than nullable. A `null` section would leave a client to guess
/// whether the number is zero, absent, or unsupported here, and the three call
/// for three different things on screen.
///
/// Lives here because this is where the pattern started, and is used by
/// [`crate::control`] too: a RAM estimate that could not be computed is the
/// same shape of answer as a disk figure that could not be read.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Probed<T> {
    Read {
        #[serde(flatten)]
        reading: T,
    },
    /// The stable `code` is the same one the error taxonomy uses, so a client
    /// can distinguish "not implemented on this platform" from "the file was
    /// there and would not parse" without matching on prose.
    Unavailable { code: &'static str, message: String },
}

impl<T> Probed<T> {
    pub(crate) fn from_probe<E: Actionable>(result: Result<T, E>) -> Self {
        match result {
            Ok(reading) => Self::Read { reading },
            Err(err) => Self::Unavailable {
                code: err.code(),
                message: err.to_string(),
            },
        }
    }
}

/// What the processor is, and what that implies for inference.
///
/// The derived figures travel with it rather than being recomputed by the
/// caller. `default_threads` and `expected_ggml_variant` are decisions this
/// crate already makes, and a second implementation of them in the panel would
/// be a second answer waiting to disagree with the engine's.
#[derive(Debug, Serialize)]
pub struct CpuReport {
    #[serde(flatten)]
    info: CpuInfo,
    default_threads: u32,
    thread_choices: Vec<u32>,
    /// Advisory. The engine scores every variant at startup and the backend
    /// reports what it actually loaded; this is what to show before that.
    expected_ggml_variant: &'static str,
    /// Surfaced on its own because its absence is the single biggest predictor
    /// of low throughput on this product's target hardware.
    has_avx_family: bool,
}

impl CpuReport {
    fn detect() -> Self {
        let info = CpuInfo::detect();
        Self {
            default_threads: info.default_threads(),
            thread_choices: info.thread_choices(),
            expected_ggml_variant: info.expected_ggml_variant(),
            has_avx_family: info.has_avx_family(),
            info,
        }
    }
}

/// System memory, with the figures the estimator's verdicts rest on.
#[derive(Debug, Serialize)]
pub struct MemoryReport {
    #[serde(flatten)]
    snapshot: MemorySnapshot,
    used: Bytes,
    swap_used: Bytes,
    /// 0.0 to 1.0.
    pressure: f64,
}

impl From<MemorySnapshot> for MemoryReport {
    fn from(snapshot: MemorySnapshot) -> Self {
        Self {
            used: snapshot.used(),
            swap_used: snapshot.swap_used(),
            pressure: snapshot.pressure(),
            snapshot,
        }
    }
}

/// Space on one filesystem, and which directory it was measured through.
#[derive(Debug, Serialize)]
pub struct FilesystemReport {
    path: PathBuf,
    #[serde(flatten)]
    space: DiskSpace,
    used: Bytes,
    pressure: f64,
}

impl FilesystemReport {
    fn new(path: PathBuf, space: DiskSpace) -> Self {
        Self {
            path,
            used: space.used(),
            pressure: space.pressure(),
            space,
        }
    }
}

/// Both filesystems a download touches.
///
/// A download does not land in one place. It accumulates in the downloads
/// directory under the **cache** root, is verified there, and is then moved
/// into the models directory under the **data** root. On this platform those
/// are `~/.cache/...` and `~/.local/share/...`, which are usually one
/// filesystem and are not required to be — `lightweight_catalog::install` already
/// falls back to a copy when the final rename returns `EXDEV`, so the code has
/// known they can differ since M6a.
///
/// Reporting one number would therefore be reporting the wrong one roughly
/// whenever it matters. Both are given, and `same_filesystem` says whether the
/// distinction is live on this machine so a caller can collapse them when it
/// is not.
#[derive(Debug, Serialize)]
pub struct DiskReport {
    /// Where the bytes accumulate first, and where a long transfer runs out.
    downloads: Probed<FilesystemReport>,
    /// Where verified weights end up, and what the final move needs free.
    models: Probed<FilesystemReport>,
    /// `None` when either side could not be identified.
    same_filesystem: Option<bool>,
}

/// Which platform this is, as the binary was built for it.
#[derive(Debug, Serialize)]
pub struct OsReport {
    /// `linux`, `macos`, `windows`, ...
    name: &'static str,
    /// `unix` or `windows`.
    family: &'static str,
    /// `x86_64`, `aarch64`, ...
    architecture: &'static str,
}

impl OsReport {
    const fn detect() -> Self {
        Self {
            name: std::env::consts::OS,
            family: std::env::consts::FAMILY,
            architecture: std::env::consts::ARCH,
        }
    }
}

/// The whole reply.
#[derive(Debug, Serialize)]
pub struct SystemReport {
    os: OsReport,
    cpu: CpuReport,
    /// Cumulative processor ticks. Difference two of these for a percentage.
    cpu_times: Probed<CpuTimes>,
    memory: Probed<MemoryReport>,
    disk: Probed<DiskReport>,
}

/// `GET /api/v1/system`.
///
/// The probes are moved off the runtime with `spawn_blocking`, as every other
/// blocking read in this workspace is. `/proc` answers instantly, but `statvfs`
/// is a filesystem call: if a model directory is on a network mount that has
/// gone away, it blocks for as long as the mount's timeout. A panel polling
/// this once a second would then hold every worker on a four-core box and take
/// the gateway down with it — the endpoint that exists to report trouble must
/// not be able to cause it.
pub async fn system(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }

    let probed = Arc::clone(&state);
    match tokio::task::spawn_blocking(move || report(&probed)).await {
        Ok(report) => axum::Json(report).into_response(),
        // The task itself cannot panic - every probe returns a `Result` - so
        // this is a runtime that is shutting down. Said plainly rather than
        // reported as a broken probe.
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": {
                    "message": format!("the system probe could not be run: {err}"),
                    "type": "server_error",
                    "code": "probe_unavailable",
                }
            })),
        )
            .into_response(),
    }
}

/// Read every probe once.
///
/// Split from the handler so the report can be asserted on directly, without a
/// server, a socket or a key.
pub fn report(state: &GatewayState) -> SystemReport {
    SystemReport {
        os: OsReport::detect(),
        cpu: CpuReport::detect(),
        cpu_times: Probed::from_probe(lightweight_system_info::cpu_times()),
        memory: Probed::from_probe(SystemMemoryProbe.snapshot().map(MemoryReport::from)),
        disk: disk_report(state),
    }
}

/// Free space where models are stored.
///
/// A gateway built without data paths — every test that constructs one
/// directly, and the mock gateway the contract suite drives — has no models
/// directory to ask about. That is reported as its own condition rather than as
/// a probe failure, because nothing is broken: there is simply no filesystem
/// this gateway is responsible for.
fn disk_report(state: &GatewayState) -> Probed<DiskReport> {
    let Some(paths) = state.config.paths.as_ref() else {
        return Probed::Unavailable {
            code: "no_data_directory",
            message: "this gateway was started without a data directory, so it \
                      has no model storage to measure"
                .to_owned(),
        };
    };

    let downloads = paths.downloads_dir();
    let models = paths.models_dir();
    Probed::Read {
        reading: DiskReport {
            same_filesystem: same_filesystem(&downloads, &models),
            downloads: filesystem_report(downloads),
            models: filesystem_report(models),
        },
    }
}

fn filesystem_report(path: PathBuf) -> Probed<FilesystemReport> {
    Probed::from_probe(
        lightweight_system_info::space_for(&path)
            .map(|space| FilesystemReport::new(path.clone(), space)),
    )
}

/// Whether two directories sit on the same filesystem.
///
/// From the device id in each one's metadata, which is what `EXDEV` is decided
/// by. `statvfs` publishes an `f_fsid` that would look like the right field to
/// use and is documented upstream as having no clear meaning anywhere, so it is
/// not used.
///
/// `None` when either path cannot be stat'd — usually because it does not
/// exist yet. Unknown is reported as unknown rather than guessed either way.
#[cfg(unix)]
fn same_filesystem(left: &std::path::Path, right: &std::path::Path) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;

    let left = std::fs::metadata(left).ok()?;
    let right = std::fs::metadata(right).ok()?;
    Some(left.dev() == right.dev())
}

#[cfg(not(unix))]
fn same_filesystem(_left: &std::path::Path, _right: &std::path::Path) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::GatewayConfig;
    use lightweight_backend_mock::MockBackend;
    // Only the two disk tests below use it, and both are Linux-only because the
    // disk probe is. Off Linux an unconditional import is an unused one, which
    // `-D warnings` turns into a failed build.
    #[cfg(target_os = "linux")]
    use lightweight_system_info::DataPaths;

    fn state(config: GatewayConfig) -> GatewayState {
        GatewayState::new(
            Arc::new(MockBackend::default()),
            crate::catalog::shared(None),
            config,
        )
    }

    fn json(state: &GatewayState) -> serde_json::Value {
        serde_json::to_value(report(state)).expect("a report always serializes")
    }

    #[test]
    fn the_cpu_section_is_always_answered() {
        // Topology detection cannot fail: it falls back to the logical core
        // count rather than erroring, so this section is never `unavailable`.
        let body = json(&state(GatewayConfig::default()));
        assert!(body["cpu"]["logical_cores"].as_u64().unwrap() >= 1);
        assert!(body["cpu"]["default_threads"].as_u64().unwrap() >= 1);
        assert!(body["cpu"]["expected_ggml_variant"].is_string());
        assert!(body["cpu"]["has_avx_family"].is_boolean());
    }

    #[test]
    fn every_probe_says_whether_it_was_read() {
        // The property the whole module exists for: a client can always tell a
        // measurement from a missing measurement.
        let body = json(&state(GatewayConfig::default()));
        for section in ["cpu_times", "memory", "disk"] {
            let state_field = body[section]["state"]
                .as_str()
                .unwrap_or_else(|| panic!("{section} must carry a state"));
            assert!(
                state_field == "read" || state_field == "unavailable",
                "{section} reported {state_field:?}"
            );
        }
    }

    #[test]
    fn a_gateway_without_data_paths_says_so_rather_than_reporting_zero() {
        // Zero free space would read as a full disk and would be wrong in the
        // direction that refuses every download.
        let body = json(&state(GatewayConfig::default()));
        assert_eq!(body["disk"]["state"], "unavailable");
        assert_eq!(body["disk"]["code"], "no_data_directory");
        assert!(body["disk"]["models"].is_null());
        assert!(body["disk"]["downloads"].is_null());
    }

    // Every platform the probe is implemented for, so the gateway is shown to
    // surface the counter - not only Linux, where this began. The macOS and
    // Windows paths reach the same JSON through `lightweight-sys`, and CI runs this
    // on all three.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn processor_time_is_reported_as_counters_not_a_percentage() {
        // If this ever becomes a percentage, the client's differencing breaks
        // silently and the charts start lying.
        let body = json(&state(GatewayConfig::default()));
        assert_eq!(body["cpu_times"]["state"], "read");
        let total = body["cpu_times"]["total"].as_u64().expect("total ticks");
        let idle = body["cpu_times"]["idle"].as_u64().expect("idle ticks");
        assert!(total > 0);
        assert!(idle <= total);
        assert!(
            body["cpu_times"]["utilization"].is_null(),
            "a single reading cannot carry a utilization"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn memory_carries_both_the_budget_and_the_context() {
        let body = json(&state(GatewayConfig::default()));
        assert_eq!(body["memory"]["state"], "read");
        let total = body["memory"]["total"].as_u64().expect("total");
        let available = body["memory"]["available"].as_u64().expect("available");
        let free = body["memory"]["free"].as_u64().expect("free");
        assert!(total > 0);
        assert!(available <= total);
        // `available` is the estimator's budget and `free` is context; both are
        // reported because they differ by several times on a warm machine.
        assert!(free <= total);
        assert!(body["memory"]["used"].as_u64().is_some());
        assert!(body["memory"]["pressure"].as_f64().is_some());
    }

    /// A directory nothing else will touch, in the workspace's usual shape:
    /// a tag plus a unique suffix. A fixed name would be shared by two
    /// concurrent runs and by a second user on the same machine, and this test
    /// deletes what it creates.
    ///
    /// Behind the same `cfg` as its only two callers: dead code off Linux, and
    /// `-D dead-code` is part of the gate.
    #[cfg(target_os = "linux")]
    fn scratch_root(tag: &str) -> PathBuf {
        // The clock alone is not unique: on a coarse timer two tests running in
        // parallel are handed the same name. The counter and the pid settle it.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "hermes-system-{tag}-{}-{unique}-{sequence}",
            std::process::id()
        ))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn both_filesystems_a_download_touches_are_reported() {
        // A download accumulates under the cache root and is then moved under
        // the data root. Reporting only one of them would report the wrong one
        // on any machine where they differ - which `install` already handles by
        // falling back to a copy on `EXDEV`.
        let root = scratch_root("both-filesystems");
        let paths = DataPaths::rooted_at(&root);
        paths.create_all().expect("create the test directories");

        let body = json(&state(GatewayConfig {
            paths: Some(paths.clone()),
            ..GatewayConfig::default()
        }));

        assert_eq!(body["disk"]["state"], "read");
        for (section, expected) in [
            ("models", paths.models_dir()),
            ("downloads", paths.downloads_dir()),
        ] {
            let reading = &body["disk"][section];
            assert_eq!(reading["state"], "read", "{section}");
            assert_eq!(reading["path"], expected.to_str().unwrap(), "{section}");
            assert!(reading["total"].as_u64().unwrap() > 0, "{section}");
            assert!(reading["available"].as_u64().is_some(), "{section}");
        }

        // Both are under one root here, so the answer is knowable and true.
        assert_eq!(body["disk"]["same_filesystem"], true);

        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_filesystem_that_cannot_be_read_does_not_sink_the_other_one() {
        // The directories are deliberately not created. Each side reports its
        // own failure, and the report as a whole still arrives - a partial
        // answer beats no answer, as the address probe already has it.
        let paths = DataPaths::rooted_at(scratch_root("absent"));

        let body = json(&state(GatewayConfig {
            paths: Some(paths),
            ..GatewayConfig::default()
        }));

        assert_eq!(body["disk"]["state"], "read");
        assert_eq!(body["disk"]["models"]["state"], "unavailable");
        assert_eq!(body["disk"]["models"]["code"], "disk_probe_failed");
        // Neither path could be stat'd, so sameness is unknown rather than
        // guessed in either direction.
        assert!(body["disk"]["same_filesystem"].is_null());
    }
}
