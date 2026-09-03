//! An OpenAI-compatible embeddings client (`POST /v1/embeddings`) for semantic
//! retrieval.
//!
//! The Lightweight gateway does not serve embeddings, so this targets a
//! separately configured OpenAI-compatible endpoint (Ollama, llama.cpp, OpenAI,
//! …). It shares the crate's transport discipline — the rustls provider is
//! installed before the client is built — and depends only on `lightagent-core`.

use lightagent_core::provider::ProviderError;
use serde::Deserialize;
use serde_json::json;

use crate::tls;

/// A client for an OpenAI-compatible embeddings endpoint.
#[derive(Clone, Debug)]
pub struct EmbeddingClient {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    embedding: Vec<f32>,
}

impl EmbeddingClient {
    /// Build a client for `base_url` (the server root, e.g. `http://127.0.0.1:11434`).
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
    ) -> Result<Self, ProviderError> {
        tls::ensure_provider();
        let client = reqwest::Client::builder()
            .build()
            .map_err(|err| ProviderError::Transport(err.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.into(),
            api_key,
        })
    }

    fn base(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    /// Embed `inputs` with `model`, returning one vector per input in order.
    pub async fn embed(
        &self,
        model: &str,
        inputs: &[String],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let body = json!({ "model": model, "input": inputs });
        let mut request = self
            .client
            .post(format!("{}/v1/embeddings", self.base()))
            .json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|err| ProviderError::Transport(err.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| ProviderError::Transport(err.to_string()))?;
        if !status.is_success() {
            return Err(ProviderError::Upstream(format!(
                "the embeddings endpoint answered {status}"
            )));
        }
        let parsed: EmbeddingResponse =
            serde_json::from_str(&text).map_err(|err| ProviderError::Upstream(err.to_string()))?;
        let vectors: Vec<Vec<f32>> = parsed
            .data
            .into_iter()
            .map(|datum| datum.embedding)
            .collect();
        if vectors.len() != inputs.len() {
            return Err(ProviderError::Upstream(format!(
                "expected {} embeddings, got {}",
                inputs.len(),
                vectors.len()
            )));
        }
        Ok(vectors)
    }
}
