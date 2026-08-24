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
//! [`hermes_system_info::network`] keeps a partial address list rather than
//! discarding it. A panel that showed `0%` where the honest answer is "this
//! platform has no probe yet" would be reporting an idle machine, which is the
//! one reading an operator must never be given wrongly.
//!
//! Nothing here is a rate. `/proc/stat` publishes counters, and a percentage
//! needs two readings of them; this endpoint hands over the counters and the
//! caller differences consecutive polls, exactly as the panel's charts do with
//! `/api/v1/metrics`. See [`hermes_system_info::load`] for why no background
//! sampler was added to make that look easier.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use hermes_core::{Actionable, Bytes};
use hermes_system_info::{
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
#[derive(Debug, Serialize)]
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
    fn from_probe<E: Actionable>(result: Result<T, E>) -> Self {
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
pub struct DiskReport {
    /// The directory probed — the models directory, because that is where a
    /// download lands and therefore the only filesystem whose free space
    /// decides anything.
    path: PathBuf,
    #[serde(flatten)]
    space: DiskSpace,
    used: Bytes,
    pressure: f64,
}

impl DiskReport {
    fn new(path: PathBuf, space: DiskSpace) -> Self {
        Self {
            path,
            used: space.used(),
            pressure: space.pressure(),
            space,
        }
    }
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
pub async fn system(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    axum::Json(report(&state)).into_response()
}

/// Read every probe once.
///
/// Split from the handler so the report can be asserted on directly, without a
/// server, a socket or a key.
pub fn report(state: &GatewayState) -> SystemReport {
    SystemReport {
        os: OsReport::detect(),
        cpu: CpuReport::detect(),
        cpu_times: Probed::from_probe(hermes_system_info::cpu_times()),
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
    let models = paths.models_dir();
    Probed::from_probe(
        hermes_system_info::space_for(&models).map(|space| DiskReport::new(models.clone(), space)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::GatewayConfig;
    use hermes_backend_mock::MockBackend;
    use hermes_system_info::DataPaths;

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
        assert!(body["disk"]["total"].is_null());
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    #[test]
    fn disk_is_measured_through_the_models_directory() {
        // The filesystem a download lands on is the only one whose free space
        // decides anything, so that is the one reported.
        let root = std::env::temp_dir().join("hermes-system-report-test");
        let paths = DataPaths::rooted_at(&root);
        paths.create_all().expect("create the test directories");

        let body = json(&state(GatewayConfig {
            paths: Some(paths.clone()),
            ..GatewayConfig::default()
        }));

        assert_eq!(body["disk"]["state"], "read");
        assert_eq!(body["disk"]["path"], paths.models_dir().to_str().unwrap());
        assert!(body["disk"]["total"].as_u64().unwrap() > 0);
        assert!(body["disk"]["available"].as_u64().is_some());

        std::fs::remove_dir_all(&root).ok();
    }
}
