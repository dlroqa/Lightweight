//! The HTTP client for the gateway control plane.
//!
//! Reads are side-effect free ([`gateway`](RuntimeClient::gateway),
//! [`system`](RuntimeClient::system), [`catalog`](RuntimeClient::catalog)).
//! [`place`](RuntimeClient::place) and [`unload`](RuntimeClient::unload) change
//! what the engine has resident and are therefore only ever called from an
//! explicit action.

use crate::placement::LoadPlan;
use crate::tls;
use crate::wire::{GatewayInfo, ListBody, ModelStatus, SystemInfo};

/// How to reach the gateway's control plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeEndpoint {
    /// The base URL, the same one the provider adapter uses.
    pub base_url: String,
    /// The bearer key, when the gateway requires one. Loopback needs none.
    pub api_key: Option<String>,
}

impl RuntimeEndpoint {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: None,
        }
    }

    /// Set the bearer key.
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }
}

/// Why a control-plane call failed.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("could not reach the gateway: {0}")]
    Transport(String),
    #[error("the gateway answered {status} to {path}: {detail}")]
    Upstream {
        status: u16,
        path: String,
        detail: String,
    },
    #[error("could not parse the gateway's reply to {path}: {reason}")]
    Decode { path: String, reason: String },
}

/// The result of asking the engine to make a model resident.
#[derive(Clone, Debug)]
pub struct PlaceOutcome {
    /// The catalog id that was requested.
    pub model: String,
    /// The load job's id, when the gateway returned one.
    pub job: Option<String>,
}

/// A client of the gateway control plane.
#[derive(Clone, Debug)]
pub struct RuntimeClient {
    client: reqwest::Client,
    endpoint: RuntimeEndpoint,
}

impl RuntimeClient {
    /// Build a client, installing the rustls provider first.
    ///
    /// Without [`tls::ensure_provider`], `reqwest::Client::builder().build()`
    /// panics even for plain HTTP — see [`crate::tls`].
    pub fn new(endpoint: RuntimeEndpoint) -> Result<Self, RuntimeError> {
        tls::ensure_provider();
        let client = reqwest::Client::builder()
            .build()
            .map_err(|err| RuntimeError::Transport(err.to_string()))?;
        Ok(Self { client, endpoint })
    }

    /// The base URL, without a trailing slash.
    fn base(&self) -> &str {
        self.endpoint.base_url.trim_end_matches('/')
    }

    /// Read the body of a successful GET, or map the failure.
    async fn get_text(&self, path: &str) -> Result<String, RuntimeError> {
        let mut request = self.client.get(format!("{}{path}", self.base()));
        if let Some(key) = &self.endpoint.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|err| RuntimeError::Transport(err.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| RuntimeError::Transport(err.to_string()))?;
        if !status.is_success() {
            return Err(RuntimeError::Upstream {
                status: status.as_u16(),
                path: path.to_owned(),
                detail: body.trim().to_owned(),
            });
        }
        Ok(body)
    }

    /// `GET /api/v1/gateway` — how the engine is configured and what it can do.
    pub async fn gateway(&self) -> Result<GatewayInfo, RuntimeError> {
        let path = "/api/v1/gateway";
        let body = self.get_text(path).await?;
        serde_json::from_str(&body).map_err(|err| RuntimeError::Decode {
            path: path.to_owned(),
            reason: err.to_string(),
        })
    }

    /// `GET /api/v1/system` — what the machine reports about itself.
    pub async fn system(&self) -> Result<SystemInfo, RuntimeError> {
        let path = "/api/v1/system";
        let body = self.get_text(path).await?;
        serde_json::from_str(&body).map_err(|err| RuntimeError::Decode {
            path: path.to_owned(),
            reason: err.to_string(),
        })
    }

    /// `GET /api/v1/models` — the catalog with each model's load state.
    pub async fn catalog(&self) -> Result<Vec<ModelStatus>, RuntimeError> {
        let path = "/api/v1/models";
        let body = self.get_text(path).await?;
        let parsed: ListBody = serde_json::from_str(&body).map_err(|err| RuntimeError::Decode {
            path: path.to_owned(),
            reason: err.to_string(),
        })?;
        Ok(parsed.data)
    }

    /// `POST /api/v1/models/{id}/load` — make `model` resident with `plan`.
    ///
    /// Mutating: it swaps what the engine is serving. Callers gate it behind an
    /// explicit action.
    pub async fn place(&self, model: &str, plan: &LoadPlan) -> Result<PlaceOutcome, RuntimeError> {
        let path = format!("/api/v1/models/{model}/load");
        let mut request = self
            .client
            .post(format!("{}{path}", self.base()))
            .json(&plan.to_body());
        if let Some(key) = &self.endpoint.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|err| RuntimeError::Transport(err.to_string()))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(RuntimeError::Upstream {
                status: status.as_u16(),
                path,
                detail: body.trim().to_owned(),
            });
        }
        // The load endpoint returns a job. Pull an id out best-effort — the
        // outcome is useful without it, so a shape we do not recognise is not an
        // error.
        let job = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                ["job", "id", "job_id"]
                    .iter()
                    .find_map(|key| value.get(*key).and_then(|v| v.as_str()).map(str::to_owned))
            });
        Ok(PlaceOutcome {
            model: model.to_owned(),
            job,
        })
    }

    /// `POST /api/v1/models/unload` — release the resident model.
    pub async fn unload(&self) -> Result<(), RuntimeError> {
        let path = "/api/v1/models/unload";
        let mut request = self.client.post(format!("{}{path}", self.base()));
        if let Some(key) = &self.endpoint.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|err| RuntimeError::Transport(err.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Err(RuntimeError::Upstream {
                status: status.as_u16(),
                path: path.to_owned(),
                detail: detail.trim().to_owned(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Error;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A one-shot loopback server that answers the first request with a fixed
    /// status, content type and body, and hands back the raw request line so a
    /// test can assert the method and path.
    async fn serve_once(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<Result<String, Error>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await?;
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let response = format!(
                "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await?;
            socket.flush().await?;
            Ok(request)
        });
        (format!("http://{addr}"), handle)
    }

    fn client_for(base: &str) -> RuntimeClient {
        RuntimeClient::new(RuntimeEndpoint::new(base)).unwrap()
    }

    #[tokio::test]
    async fn client_builds_without_panicking() {
        assert!(RuntimeClient::new(RuntimeEndpoint::new("http://127.0.0.1:11434")).is_ok());
    }

    #[tokio::test]
    async fn gateway_read_parses_capabilities() {
        let body = r#"{"backend":"llamacpp","model":"lfm2@8k","engine_capabilities":{"device":"cpu","streaming":true,"tool_calls":true,"reasoning_content":false,"max_concurrent_requests":1,"kv_cache_types":["f16","q8_0"],"build":"b10590"},"defaults":{"kv_type":"f16","threads":4,"ubatch":512,"load_modes":["auto","mmap"]}}"#;
        let (base, handle) = serve_once("HTTP/1.1 200 OK", body).await;
        let info = client_for(&base).gateway().await.unwrap();
        assert_eq!(info.engine_capabilities.device, "cpu");
        assert_eq!(info.defaults.threads, Some(4));
        let request = handle.await.unwrap().unwrap();
        assert!(request.starts_with("GET /api/v1/gateway "));
    }

    #[tokio::test]
    async fn system_read_parses_cpu() {
        let body = r#"{"os":{"name":"linux","architecture":"x86_64"},"cpu":{"logical_cores":4,"has_avx_family":false},"memory":{"state":"read","total":8000,"available":4000,"free":3000}}"#;
        let (base, handle) = serve_once("HTTP/1.1 200 OK", body).await;
        let info = client_for(&base).system().await.unwrap();
        assert_eq!(info.cpu.logical_cores, Some(4));
        assert!(info.memory.was_read());
        assert_eq!(info.memory.available, Some(4000));
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn catalog_read_lists_models() {
        let body = r#"{"object":"list","data":[{"id":"lfm2","state":"loaded"},{"id":"old","state":"missing"}]}"#;
        let (base, handle) = serve_once("HTTP/1.1 200 OK", body).await;
        let rows = client_for(&base).catalog().await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].is_loaded());
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn place_posts_the_plan_and_reads_the_job() {
        let body = r#"{"job":"job-abc","kind":"load"}"#;
        let (base, handle) = serve_once("HTTP/1.1 202 Accepted", body).await;
        let policy = crate::placement::PlacementPolicy {
            n_ctx: Some(8192),
            ..crate::placement::PlacementPolicy::default()
        };
        let plan = LoadPlan::from_policy(&policy);
        let outcome = client_for(&base).place("lfm2", &plan).await.unwrap();
        assert_eq!(outcome.job.as_deref(), Some("job-abc"));
        let request = handle.await.unwrap().unwrap();
        assert!(request.starts_with("POST /api/v1/models/lfm2/load "));
        assert!(request.contains("\"ctx\":8192"));
    }

    #[tokio::test]
    async fn an_error_status_becomes_an_upstream_error() {
        let (base, handle) = serve_once("HTTP/1.1 404 Not Found", "no manager").await;
        let error = client_for(&base).gateway().await.unwrap_err();
        match error {
            RuntimeError::Upstream { status, .. } => assert_eq!(status, 404),
            other => panic!("expected an upstream error, got {other:?}"),
        }
        handle.await.unwrap().unwrap();
    }
}
