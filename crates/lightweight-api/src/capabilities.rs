//! `GET /v1/capabilities`.
//!
//! A small, versioned description of the public inference contract. It is
//! deliberately separate from the gateway's `/api/v1` control plane: an agent
//! harness needs to know whether it can safely make inference requests, not how
//! the provider stores, loads, or manages models.

use serde::{Deserialize, Serialize};

/// The public provider contract identifier.
pub const PROTOCOL_NAME: &str = "lightweight-public-inference";
/// The first supported public provider contract version.
pub const PROTOCOL_VERSION: u32 = 1;

/// The `GET /v1/capabilities` body.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapabilitiesBody {
    pub object: String,
    pub protocol: ProtocolInfo,
    pub server: ServerInfo,
    pub endpoints: EndpointSet,
    pub features: FeatureSet,
    pub state: CapabilityState,
    pub limits: CapabilityLimits,
}

impl CapabilitiesBody {
    pub fn new(
        server_version: impl Into<String>,
        model: Option<CapabilityModel>,
        max_concurrent_requests: u32,
    ) -> Self {
        Self {
            object: "capability.list".to_owned(),
            protocol: ProtocolInfo {
                name: PROTOCOL_NAME.to_owned(),
                version: PROTOCOL_VERSION,
                compatible_versions: vec![PROTOCOL_VERSION],
            },
            server: ServerInfo {
                name: "Lightweight".to_owned(),
                version: server_version.into(),
            },
            endpoints: EndpointSet {
                models: "/v1/models".to_owned(),
                chat_completions: "/v1/chat/completions".to_owned(),
                completions: "/v1/completions".to_owned(),
            },
            features: FeatureSet {
                streaming: true,
                sse_done: true,
                usage_chunk: true,
                chat_completions: true,
                completions: true,
                tools: true,
                tool_call_deltas: true,
                tool_choice: true,
                parallel_tool_calls: true,
                reasoning_content: true,
            },
            state: CapabilityState {
                model_loaded: model.is_some(),
                model,
            },
            limits: CapabilityLimits {
                max_concurrent_requests,
            },
        }
    }
}

/// The versioned protocol identity a client branches on.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProtocolInfo {
    pub name: String,
    pub version: u32,
    pub compatible_versions: Vec<u32>,
}

/// Non-sensitive server identity.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// Public inference routes supplied by this provider.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EndpointSet {
    pub models: String,
    pub chat_completions: String,
    pub completions: String,
}

/// Public protocol features, rather than engine or control-plane capabilities.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FeatureSet {
    pub streaming: bool,
    pub sse_done: bool,
    pub usage_chunk: bool,
    pub chat_completions: bool,
    pub completions: bool,
    pub tools: bool,
    pub tool_call_deltas: bool,
    pub tool_choice: bool,
    pub parallel_tool_calls: bool,
    pub reasoning_content: bool,
}

/// Provider readiness as seen by an authenticated inference client.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapabilityState {
    pub model_loaded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<CapabilityModel>,
}

/// The single model this gateway can currently serve.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapabilityModel {
    pub id: String,
    /// The effective served context, not the model metadata ceiling.
    pub context_length: u32,
}

/// Provider-wide limits that do not disclose machine internals.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapabilityLimits {
    pub max_concurrent_requests: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_one_is_a_public_inference_contract() {
        let body = CapabilitiesBody::new(
            "0.2.4",
            Some(CapabilityModel {
                id: "mock-model@4k".into(),
                context_length: 4096,
            }),
            1,
        );
        let value = serde_json::to_value(body).expect("serialize");
        assert_eq!(value["object"], "capability.list");
        assert_eq!(value["protocol"]["name"], PROTOCOL_NAME);
        assert_eq!(value["protocol"]["version"], PROTOCOL_VERSION);
        assert_eq!(value["state"]["model_loaded"], true);
        assert_eq!(value["state"]["model"]["context_length"], 4096);
    }

    #[test]
    fn the_public_contract_does_not_advertise_control_routes_or_paths() {
        let value =
            serde_json::to_string(&CapabilitiesBody::new("0.2.4", None, 1)).expect("serialize");
        assert!(!value.contains("/api/v1"));
        assert!(!value.contains("model_path"));
        assert!(!value.contains("/home/"));
    }
}
