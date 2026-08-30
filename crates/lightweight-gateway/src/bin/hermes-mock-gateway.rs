//! The gateway, backed by the mock engine, for the contract suite.
//!
//! The highest-value test in this project points the **real** `openai` Python
//! SDK at a real gateway and asserts what the *client* ends up with — that
//! content assembles, that tool-call arguments parse as JSON, that `usage`
//! populates from the terminal chunk. Asserting our own bytes proves we
//! implemented what we intended; asserting the client's result proves the
//! client works.
//!
//! Running that against llama.cpp would need a model, minutes per case, and a
//! way to make a real model produce an empty completion on demand. This binary
//! serves the same gateway over the deterministic backend instead.
//!
//! Test scaffolding lives here rather than in the gateway: the control route
//! below is merged into the router by this binary, so the shipped gateway has
//! no way to be told what to say.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use lightweight_backend_mock::{MockBackend, MockConfig, Script};
use lightweight_core::ModelId;
use lightweight_gateway::catalog::{Catalog, ResidentModel};
use lightweight_gateway::{AuthPolicy, GatewayConfig, GatewayState};
use lightweight_inference::generation::ReasoningControl;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ScriptSpec {
    Content {
        fragments: Vec<String>,
    },
    Reasoning {
        reasoning: Vec<String>,
        content: Vec<String>,
    },
    ToolCall {
        id: String,
        name: String,
        argument_fragments: Vec<String>,
    },
    Empty,
    FailMidStream {
        content: Vec<String>,
        error: String,
    },
    Fail {
        error: String,
    },
}

impl From<ScriptSpec> for Script {
    fn from(spec: ScriptSpec) -> Self {
        match spec {
            ScriptSpec::Content { fragments } => Self::Content(fragments),
            ScriptSpec::Reasoning { reasoning, content } => Self::Reasoning { reasoning, content },
            ScriptSpec::ToolCall {
                id,
                name,
                argument_fragments,
            } => Self::ToolCall {
                id,
                name,
                argument_fragments,
            },
            ScriptSpec::Empty => Self::Empty,
            ScriptSpec::FailMidStream { content, error } => Self::FailMidStream { content, error },
            ScriptSpec::Fail { error } => Self::Fail(error),
        }
    }
}

/// What the next generation should do.
#[derive(Debug, Deserialize)]
struct ControlRequest {
    script: ScriptSpec,
    /// What `count_prompt_tokens` reports, which is how the context-overflow
    /// path is exercised without a 40,000-token prompt.
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    prefill_ms: Option<u64>,
}

struct Control {
    backend: Arc<MockBackend>,
}

/// What the gateway last asked the backend to generate.
///
/// The contract suite uses this to prove that an option the *real* client
/// library sent — `reasoning_effort`, a chat-template switch — survived the
/// whole path and reached the engine boundary, instead of being tolerated and
/// dropped.
async fn last_request(State(control): State<Arc<Control>>) -> axum::Json<serde_json::Value> {
    let Some(request) = control.backend.last_request().await else {
        return axum::Json(json!({ "seen": false }));
    };
    axum::Json(json!({
        "seen": true,
        "reasoning": match request.reasoning {
            ReasoningControl::Default => json!("default"),
            ReasoningControl::Disabled => json!("disabled"),
            ReasoningControl::Effort(ref effort) => json!(effort),
        },
        "template_options": request.template_options,
        "max_tokens": request.max_tokens,
        "message_count": request.messages().len(),
    }))
}

async fn set_script(
    State(control): State<Arc<Control>>,
    axum::Json(request): axum::Json<ControlRequest>,
) -> StatusCode {
    control
        .backend
        .set_config(MockConfig {
            script: request.script.into(),
            prompt_tokens: request.prompt_tokens.unwrap_or(11),
            prefill: Duration::from_millis(request.prefill_ms.unwrap_or(0)),
            ..MockConfig::default()
        })
        .await;
    StatusCode::NO_CONTENT
}

// Multi-threaded, deliberately. A suite that drives two clients at once
// against a current-thread runtime measures the runtime rather than the
// gateway: the two requests would interleave at await points and never
// actually overlap.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut port = 0_u16;
    let mut n_ctx = 4096_u32;
    let mut model_id = "mock-model".to_owned();
    let mut api_key: Option<String> = None;
    let mut concurrency = 1_u32;

    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--port" => port = args.next().unwrap_or_default().parse()?,
            "--ctx" => n_ctx = args.next().unwrap_or_default().parse()?,
            "--model" => model_id = args.next().unwrap_or_default(),
            "--api-key" => api_key = args.next(),
            "--concurrency" => concurrency = args.next().unwrap_or_default().parse()?,
            other => return Err(format!("unknown flag {other}").into()),
        }
    }

    let backend = Arc::new(MockBackend::default());
    let model = ModelId::with_context(&model_id, n_ctx);
    let loaded = backend.make_resident(model.clone(), n_ctx).await;

    let catalog = Arc::new(Catalog::with_resident(ResidentModel {
        id: model,
        instance: loaded.instance,
        n_ctx,
        architecture: "mock".to_owned(),
        param_count: Some(1_200_000_000),
        quantization: Some("Q4_K_M".to_owned()),
        model_max_context_length: Some(131_072),
        ram_verdict: Some("safe".to_owned()),
        backend: Some("mock".to_owned()),
        model_path: "/mock/model.gguf".to_owned(),
        effective: lightweight_core::RuntimeParams::default(),
    }));

    let auth = match api_key {
        Some(key) => AuthPolicy::Required { key },
        None => AuthPolicy::Disabled,
    };
    let state = Arc::new(GatewayState::new(
        Arc::clone(&backend) as Arc<dyn lightweight_inference::InferenceBackend>,
        catalog,
        GatewayConfig {
            auth,
            max_concurrent_requests: concurrency.max(1),
            ..GatewayConfig::default()
        },
    ));

    let control = Arc::new(Control {
        backend: Arc::clone(&backend),
    });
    let app: Router = lightweight_gateway::app(state).merge(
        Router::new()
            .route("/__test__/script", post(set_script))
            .route("/__test__/last-request", axum::routing::get(last_request))
            .with_state(control),
    );

    let listener =
        tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
            .await?;
    let bound = listener.local_addr()?;
    // The suite reads this line to find the port, so it is printed and flushed
    // before serving starts.
    println!(
        "{}",
        json!({
            "base_url": format!("http://127.0.0.1:{}/v1", bound.port()),
            "port": bound.port(),
            "instance": loaded.instance.to_string(),
            // Read by the suite, so a concurrency test cannot silently run
            // against a gateway that was given one slot.
            "concurrency": concurrency.max(1),
        })
    );
    use std::io::Write;
    std::io::stdout().flush()?;

    axum::serve(listener, lightweight_gateway::service(app)).await?;
    Ok(())
}
