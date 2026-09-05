//! Deserialize types for the gateway control plane.
//!
//! These reproduce the subset of the engine's control-plane responses that the
//! placement selector reads. They are deliberately partial: serde ignores
//! fields we do not name, so a richer response than we model still parses, and a
//! field the engine renames that we do not depend on cannot break us. Every
//! optional field defaults to absent so a probe that could not answer (a
//! `state: "unavailable"` section, an older engine) still deserializes.

use serde::Deserialize;

/// The subset of `GET /api/v1/gateway` the selector reads.
#[derive(Clone, Debug, Deserialize)]
pub struct GatewayInfo {
    /// The engine build version, when the gateway states one.
    #[serde(default)]
    pub version: Option<String>,
    /// The backend id, e.g. `"llamacpp"` or `"mock"`.
    #[serde(default)]
    pub backend: Option<String>,
    /// The catalog id of the resident model, if one is loaded (with its
    /// `@context` suffix).
    #[serde(default)]
    pub model: Option<String>,
    /// What this engine can do, straight from the backend.
    pub engine_capabilities: EngineCapabilities,
    /// The values a load uses when a request names none.
    #[serde(default)]
    pub defaults: LoadDefaults,
}

/// The engine's advertised capabilities.
#[derive(Clone, Debug, Deserialize)]
pub struct EngineCapabilities {
    /// Where inference runs: `"cpu"`, `"cuda"`, `"metal"` or `"rocm"`.
    pub device: String,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub tool_calls: bool,
    #[serde(default)]
    pub reasoning_content: bool,
    /// Requests the resident engine will serve at once.
    #[serde(default)]
    pub max_concurrent_requests: u32,
    /// KV cache element types the engine accepts, in its own spelling.
    #[serde(default)]
    pub kv_cache_types: Vec<String>,
    /// The engine build these capabilities describe, when there is one.
    #[serde(default)]
    pub build: Option<String>,
}

/// The load defaults the gateway was started with.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LoadDefaults {
    #[serde(default)]
    pub kv_type: Option<String>,
    #[serde(default)]
    pub threads: Option<u32>,
    #[serde(default)]
    pub concurrency: Option<u32>,
    #[serde(default)]
    pub ubatch: Option<u32>,
    /// The load modes this engine accepts, in its own spelling.
    #[serde(default)]
    pub load_modes: Vec<String>,
}

/// The subset of `GET /api/v1/system` the report shows.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct SystemInfo {
    #[serde(default)]
    pub os: OsInfo,
    #[serde(default)]
    pub cpu: CpuInfo,
    #[serde(default)]
    pub memory: MemoryInfo,
}

/// Which platform the engine is built for.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct OsInfo {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub architecture: Option<String>,
}

/// The processor facts placement cares about.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CpuInfo {
    #[serde(default)]
    pub logical_cores: Option<u32>,
    #[serde(default)]
    pub physical_cores: Option<u32>,
    #[serde(default)]
    pub default_threads: Option<u32>,
    /// The single biggest predictor of throughput on this product's target
    /// hardware; worth surfacing on its own.
    #[serde(default)]
    pub has_avx_family: Option<bool>,
}

/// System memory, in bytes. Absent when the memory probe was unavailable.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct MemoryInfo {
    /// `"read"` when the probe answered, `"unavailable"` otherwise.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub available: Option<u64>,
    #[serde(default)]
    pub free: Option<u64>,
}

impl MemoryInfo {
    /// Whether the probe actually read a figure.
    pub fn was_read(&self) -> bool {
        self.total.is_some()
    }
}

/// One row of `GET /api/v1/models`: a catalog entry plus its load state.
#[derive(Clone, Debug, Deserialize)]
pub struct ModelStatus {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// `"loaded"`, `"available"` or `"missing"`.
    pub state: String,
    #[serde(default)]
    pub architecture: Option<String>,
    /// Whether the pinned engine can run this architecture.
    #[serde(default)]
    pub supported: Option<bool>,
    #[serde(default)]
    pub quantization: Option<String>,
    #[serde(default)]
    pub context_length: Option<u64>,
}

impl ModelStatus {
    /// Whether this model is the one currently resident.
    pub fn is_loaded(&self) -> bool {
        self.state == "loaded"
    }

    /// Whether this model's file is present and could be loaded.
    pub fn is_available(&self) -> bool {
        self.state == "available" || self.state == "loaded"
    }
}

/// The OpenAI-shaped list body `GET /api/v1/models` returns.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ListBody {
    #[serde(default)]
    pub data: Vec<ModelStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_report_parses_a_cpu_engine() {
        let json = r#"{
            "version": "b10590",
            "backend": "llamacpp",
            "model": "lfm2@8k",
            "engine_capabilities": {
                "device": "cpu",
                "streaming": true,
                "tool_calls": true,
                "reasoning_content": false,
                "max_concurrent_requests": 1,
                "kv_cache_types": ["f16", "q8_0"],
                "build": "b10590"
            },
            "defaults": {
                "kv_type": "f16",
                "threads": 4,
                "concurrency": null,
                "ubatch": 512,
                "load_modes": ["auto", "none", "mmap", "mlock", "mmap+mlock"]
            }
        }"#;
        let info: GatewayInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.engine_capabilities.device, "cpu");
        assert_eq!(info.engine_capabilities.kv_cache_types.len(), 2);
        assert_eq!(info.defaults.ubatch, Some(512));
        assert_eq!(info.model.as_deref(), Some("lfm2@8k"));
    }

    #[test]
    fn gateway_report_tolerates_a_minimal_body() {
        // Only the one required field; everything else defaults.
        let json = r#"{"engine_capabilities": {"device": "cpu"}}"#;
        let info: GatewayInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.engine_capabilities.device, "cpu");
        assert!(info.defaults.load_modes.is_empty());
        assert!(info.model.is_none());
    }

    #[test]
    fn system_report_parses_and_survives_an_unavailable_probe() {
        let json = r#"{
            "os": {"name": "linux", "architecture": "x86_64"},
            "cpu": {"logical_cores": 4, "default_threads": 4, "has_avx_family": false, "extra": 1},
            "memory": {"state": "unavailable", "code": "probe_unavailable"}
        }"#;
        let info: SystemInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.cpu.logical_cores, Some(4));
        assert_eq!(info.cpu.has_avx_family, Some(false));
        assert_eq!(info.os.name.as_deref(), Some("linux"));
        assert!(!info.memory.was_read());
    }

    #[test]
    fn catalog_rows_expose_load_state() {
        let json = r#"{
            "object": "list",
            "data": [
                {"id": "lfm2", "name": "LFM2", "state": "loaded", "architecture": "lfm2", "supported": true},
                {"id": "old", "state": "missing"}
            ]
        }"#;
        let body: ListBody = serde_json::from_str(json).unwrap();
        assert_eq!(body.data.len(), 2);
        assert!(body.data[0].is_loaded());
        assert!(body.data[0].is_available());
        assert!(!body.data[1].is_available());
    }
}
