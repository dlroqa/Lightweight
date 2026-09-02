//! `lightagent serve` — the HTTP API over the one runtime.
//!
//! Wires the Lightweight provider and the bounded tool executor into the
//! transport-agnostic [`lightagent_api`] server through a [`RunFactory`], so the
//! API crate never learns a transport. Loopback binds are open (the network is
//! the boundary); a non-loopback bind refuses to start without a key.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lightagent_api::manager::{self, RunFactory, RunManager, RunStatus, StartRun};
use lightagent_api::{AppState, AuthConfig, Scope, router};
use lightagent_core::{
    AgentEvent, AgentEventSink, AgentLoop, ApprovalDecision, Config, ConfigStore, LightagentPaths,
    PolicyEngine, ProfileStore, RunId, StopReason,
};
use lightagent_provider_lightweight::{LightweightProvider, ProviderConfig};
use lightagent_store::SessionStore;
use lightagent_tools::{BoundedExecutor, Delegation, Tool, ToolRegistry};
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use crate::chat::{LightweightFactory, mcp_tools, resolve_profile, web_context, workspace_context};

/// Builds and drives a real run with the Lightweight provider per request.
struct LightweightRunFactory {
    root: PathBuf,
    config: Config,
    mcp_tools: Vec<Arc<dyn Tool>>,
}

#[async_trait]
impl RunFactory for LightweightRunFactory {
    async fn run(
        &self,
        request: StartRun,
        sink: AgentEventSink,
        cancel: CancellationToken,
        decisions: UnboundedReceiver<ApprovalDecision>,
    ) -> RunStatus {
        let store = ProfileStore::new(&self.root);
        let profile = match resolve_profile(&store, &self.config, request.profile) {
            Ok(profile) => profile,
            Err(error) => {
                fail(&sink, &error);
                return RunStatus::Failed;
            }
        };

        let base_url = profile
            .routing
            .base_url
            .clone()
            .unwrap_or_else(|| self.config.inference.base_url.clone());
        let model = if profile.routing.model.is_empty() {
            self.config
                .inference
                .model
                .clone()
                .unwrap_or_else(|| "default".to_string())
        } else {
            profile.routing.model.clone()
        };
        let api_key = self
            .config
            .inference
            .api_key
            .as_ref()
            .and_then(|secret| secret.resolve());

        let mut provider_config = ProviderConfig::new(base_url.clone(), model);
        if let Some(key) = &api_key {
            provider_config = provider_config.with_api_key(key.clone());
        }
        let provider = match LightweightProvider::new(provider_config) {
            Ok(provider) => provider,
            Err(error) => {
                fail(&sink, &error.to_string());
                return RunStatus::Failed;
            }
        };

        let workspace_dir = store.handle(&profile.id).workspace_dir();
        let delegation = Delegation {
            profiles: Arc::new(store),
            factory: Arc::new(LightweightFactory { base_url, api_key }),
            worker_registry: ToolRegistry::worker_default(),
            worker_per_call: Duration::from_secs(60),
            worker_max_output_bytes: 262_144,
        };
        let mut registry = ToolRegistry::builtin();
        for tool in &self.mcp_tools {
            registry.insert(Arc::clone(tool));
        }
        let mut executor = BoundedExecutor::new(
            registry,
            PolicyEngine::new(profile.approval_policy.into()),
            Duration::from_secs(60),
            262_144,
        )
        .with_run(RunId::new())
        .with_delegation(delegation);
        if let Some(web) = web_context(&self.config) {
            executor = executor.with_web(web);
        }
        if let Some(workspace) = workspace_context(&self.config, workspace_dir) {
            executor = executor.with_workspace(workspace);
        }

        let agent = AgentLoop::from_profile(provider, executor, &profile);
        manager::drive(agent, request.message, sink, cancel, decisions).await
    }
}

fn fail(sink: &AgentEventSink, message: &str) {
    let _ = sink.send(AgentEvent::Error {
        message: message.to_owned(),
    });
    let _ = sink.send(AgentEvent::RunCompleted {
        reason: StopReason::Error,
    });
}

/// Bind and serve the API until interrupted.
pub async fn run(
    host: String,
    port: u16,
    key_env: Option<String>,
    web_root: Option<PathBuf>,
) -> Result<(), String> {
    let paths = LightagentPaths::resolve().map_err(|error| error.to_string())?;
    let config = ConfigStore::at(&paths)
        .load()
        .map_err(|error| error.to_string())?;
    let store = ProfileStore::new(paths.root());
    let active = store
        .active()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "no active profile — run `lightagent init` first".to_string())?;
    let sessions = SessionStore::at_profile(&store.handle(&active));

    let is_loopback = matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1");
    let auth = match key_env.and_then(|var| std::env::var(&var).ok()) {
        Some(key) => AuthConfig::keyed(key, [Scope::Admin]),
        None => {
            if !is_loopback {
                return Err(
                    "a non-loopback bind requires --key-env naming a variable holding an API key"
                        .to_string(),
                );
            }
            AuthConfig::open()
        }
    };

    let mcp = mcp_tools(&config).await;
    let factory = Arc::new(LightweightRunFactory {
        root: paths.root().to_path_buf(),
        config,
        mcp_tools: mcp,
    });
    let state = AppState {
        manager: RunManager::new(factory),
        auth,
        sessions,
        web_root: web_root.clone(),
    };

    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|error| format!("could not bind {addr}: {error}"))?;
    let bound = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or(addr);
    println!(
        "Lightagent API listening on http://{bound}/api/lightagent/v1  (profile '{}')",
        active.as_str()
    );
    if is_loopback {
        println!("Loopback bind: no API key required.");
    }
    if let Some(root) = &web_root {
        println!(
            "Serving the panel from {} at http://{bound}/",
            root.display()
        );
    }
    axum::serve(listener, router(state).into_make_service())
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}
