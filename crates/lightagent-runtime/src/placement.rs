//! Device selection and load-plan construction.
//!
//! Pure logic, no I/O: given a [`PlacementPolicy`] (from config) and what the
//! engine reports, decide which device to run on and what runtime parameters to
//! ask for. The device decision is honest about the CPU-only engine — see the
//! crate docs — and the runtime parameters are the knobs the load endpoint
//! actually honours today.

use serde_json::{Map, Value, json};

use crate::wire::EngineCapabilities;

/// Where inference runs.
///
/// Mirrors the engine's own `DeviceKind`. Only `Cpu` is live in the pinned
/// engine; the others are reserved for the backends its plan will add, and are
/// carried here so a policy naming one resolves correctly the day the engine
/// begins reporting it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Cpu,
    Cuda,
    Metal,
    Rocm,
}

impl DeviceKind {
    /// The engine's own spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Cuda => "cuda",
            Self::Metal => "metal",
            Self::Rocm => "rocm",
        }
    }

    /// Parse the engine's spelling. Case-insensitive; unknown names are `None`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "cpu" => Some(Self::Cpu),
            "cuda" | "gpu" => Some(Self::Cuda),
            "metal" => Some(Self::Metal),
            "rocm" => Some(Self::Rocm),
            _ => None,
        }
    }

    /// A human label for a report.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Cuda => "CUDA GPU",
            Self::Metal => "Metal GPU",
            Self::Rocm => "ROCm GPU",
        }
    }
}

/// What device a policy prefers.
///
/// `Auto` takes whatever the engine offers; a specific kind asks for that one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreferredDevice {
    Auto,
    Specific(DeviceKind),
}

impl PreferredDevice {
    /// Parse a policy string. `"auto"` (or empty) is [`PreferredDevice::Auto`].
    pub fn from_name(name: &str) -> Option<Self> {
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
            return Some(Self::Auto);
        }
        DeviceKind::from_name(trimmed).map(Self::Specific)
    }
}

/// The placement policy, as it comes from configuration.
///
/// The string fields are the engine's own spellings and are validated at the
/// config layer; here they are passed through to the load body unchanged so a
/// value this build has never heard of is still forwarded to (and judged by) the
/// engine rather than silently dropped.
#[derive(Clone, Debug, Default)]
pub struct PlacementPolicy {
    /// `"auto"`, `"cpu"`, `"cuda"`, `"metal"` or `"rocm"`.
    pub preferred_device: String,
    /// Whether a specific-but-unavailable device may fall back to the CPU.
    pub allow_cpu_fallback: bool,
    pub n_ctx: Option<u32>,
    pub threads: Option<u32>,
    pub kv_type: Option<String>,
    pub load_mode: Option<String>,
    pub ubatch: Option<u32>,
}

/// Why a device could not be selected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlacementError {
    #[error(
        "the placement policy names an unknown device {0:?}; expected auto, cpu, cuda, metal or rocm"
    )]
    UnknownPreferredDevice(String),
    #[error("the engine reports an unknown device {0:?}")]
    UnknownEngineDevice(String),
    #[error(
        "the policy prefers the {preferred} but the engine runs on the {available}, and CPU fallback is disabled"
    )]
    DeviceUnavailable {
        preferred: &'static str,
        available: &'static str,
    },
}

/// The outcome of resolving a device against what the engine offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacementResolution {
    /// The device the load will run on.
    pub device: DeviceKind,
    /// Set when a specific preference was not met and the CPU was used instead,
    /// so a caller can warn rather than silently ignore the preference.
    pub fell_back_from: Option<DeviceKind>,
}

impl PlacementResolution {
    /// A one-line explanation suitable for a report.
    pub fn summary(&self) -> String {
        match self.fell_back_from {
            None => format!("runs on the {}", self.device.label()),
            Some(wanted) => format!(
                "falls back to the {} ({} is not available on this engine)",
                self.device.label(),
                wanted.label()
            ),
        }
    }
}

/// Resolve the policy's preferred device against the engine's actual one.
///
/// `Auto` accepts whatever the engine reports. A specific preference that
/// matches is honoured. A specific preference that does not match falls back to
/// the CPU when the engine is on the CPU and fallback is allowed; otherwise it
/// is an error, so a caller who insisted on a GPU is told plainly rather than
/// quietly served by the CPU.
pub fn resolve_device(
    policy: &PlacementPolicy,
    capabilities: &EngineCapabilities,
) -> Result<PlacementResolution, PlacementError> {
    let preferred = PreferredDevice::from_name(&policy.preferred_device)
        .ok_or_else(|| PlacementError::UnknownPreferredDevice(policy.preferred_device.clone()))?;
    let available = DeviceKind::from_name(&capabilities.device)
        .ok_or_else(|| PlacementError::UnknownEngineDevice(capabilities.device.clone()))?;

    match preferred {
        PreferredDevice::Auto => Ok(PlacementResolution {
            device: available,
            fell_back_from: None,
        }),
        PreferredDevice::Specific(wanted) if wanted == available => Ok(PlacementResolution {
            device: available,
            fell_back_from: None,
        }),
        PreferredDevice::Specific(wanted) => {
            if policy.allow_cpu_fallback && available == DeviceKind::Cpu {
                Ok(PlacementResolution {
                    device: DeviceKind::Cpu,
                    fell_back_from: Some(wanted),
                })
            } else {
                Err(PlacementError::DeviceUnavailable {
                    preferred: wanted.label(),
                    available: available.label(),
                })
            }
        }
    }
}

/// The runtime parameters to send to `POST /api/v1/models/{id}/load`.
///
/// Built from the policy. Absent fields are omitted from the body, so the engine
/// applies its own default for each — the body carries only what the policy
/// actually chose to override.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadPlan {
    pub n_ctx: Option<u32>,
    pub threads: Option<u32>,
    pub kv_type: Option<String>,
    pub load_mode: Option<String>,
    pub ubatch: Option<u32>,
    /// Replace the resident model even if the id already matches.
    pub force: bool,
}

impl LoadPlan {
    /// Derive the runtime parameters from the policy.
    pub fn from_policy(policy: &PlacementPolicy) -> Self {
        Self {
            n_ctx: policy.n_ctx,
            threads: policy.threads,
            kv_type: policy.kv_type.clone(),
            load_mode: policy.load_mode.clone(),
            ubatch: policy.ubatch,
            force: false,
        }
    }

    /// Whether this plan overrides nothing — an empty body loads the model as it
    /// is, which is the engine's ordinary case.
    pub fn is_empty(&self) -> bool {
        self.n_ctx.is_none()
            && self.threads.is_none()
            && self.kv_type.is_none()
            && self.load_mode.is_none()
            && self.ubatch.is_none()
            && !self.force
    }

    /// The JSON body for the load request.
    ///
    /// The field names are the load endpoint's own (`ctx`, `threads`, `kv_type`,
    /// `load_mode`, `ubatch`, `force`).
    pub fn to_body(&self) -> Value {
        let mut body = Map::new();
        if let Some(ctx) = self.n_ctx {
            body.insert("ctx".into(), json!(ctx));
        }
        if let Some(threads) = self.threads {
            body.insert("threads".into(), json!(threads));
        }
        if let Some(kv_type) = &self.kv_type {
            body.insert("kv_type".into(), json!(kv_type));
        }
        if let Some(load_mode) = &self.load_mode {
            body.insert("load_mode".into(), json!(load_mode));
        }
        if let Some(ubatch) = self.ubatch {
            body.insert("ubatch".into(), json!(ubatch));
        }
        if self.force {
            body.insert("force".into(), json!(true));
        }
        Value::Object(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(device: &str) -> EngineCapabilities {
        EngineCapabilities {
            device: device.to_owned(),
            streaming: true,
            tool_calls: true,
            reasoning_content: false,
            max_concurrent_requests: 1,
            kv_cache_types: vec!["f16".to_owned()],
            build: None,
        }
    }

    fn policy(preferred: &str, fallback: bool) -> PlacementPolicy {
        PlacementPolicy {
            preferred_device: preferred.to_owned(),
            allow_cpu_fallback: fallback,
            ..PlacementPolicy::default()
        }
    }

    #[test]
    fn auto_takes_whatever_the_engine_offers() {
        let resolution = resolve_device(&policy("auto", true), &caps("cpu")).unwrap();
        assert_eq!(resolution.device, DeviceKind::Cpu);
        assert!(resolution.fell_back_from.is_none());
    }

    #[test]
    fn a_matching_preference_is_honoured() {
        // Forward-compatible: the day the engine reports cuda, a cuda policy is met.
        let resolution = resolve_device(&policy("cuda", false), &caps("cuda")).unwrap();
        assert_eq!(resolution.device, DeviceKind::Cuda);
        assert!(resolution.fell_back_from.is_none());
    }

    #[test]
    fn an_unavailable_gpu_falls_back_to_cpu_when_allowed() {
        let resolution = resolve_device(&policy("cuda", true), &caps("cpu")).unwrap();
        assert_eq!(resolution.device, DeviceKind::Cpu);
        assert_eq!(resolution.fell_back_from, Some(DeviceKind::Cuda));
        assert!(resolution.summary().contains("falls back"));
    }

    #[test]
    fn an_unavailable_gpu_is_refused_when_fallback_is_off() {
        let error = resolve_device(&policy("metal", false), &caps("cpu")).unwrap_err();
        assert!(matches!(error, PlacementError::DeviceUnavailable { .. }));
    }

    #[test]
    fn an_unknown_preference_is_rejected() {
        let error = resolve_device(&policy("tpu", true), &caps("cpu")).unwrap_err();
        assert_eq!(
            error,
            PlacementError::UnknownPreferredDevice("tpu".to_owned())
        );
    }

    #[test]
    fn an_empty_plan_makes_an_empty_body() {
        let plan = LoadPlan::from_policy(&PlacementPolicy::default());
        assert!(plan.is_empty());
        assert_eq!(plan.to_body(), json!({}));
    }

    #[test]
    fn a_plan_body_carries_only_overrides() {
        let p = PlacementPolicy {
            n_ctx: Some(8192),
            kv_type: Some("q8_0".to_owned()),
            ..PlacementPolicy::default()
        };
        let plan = LoadPlan::from_policy(&p);
        let body = plan.to_body();
        assert_eq!(body["ctx"], json!(8192));
        assert_eq!(body["kv_type"], json!("q8_0"));
        assert!(body.get("threads").is_none());
    }
}
