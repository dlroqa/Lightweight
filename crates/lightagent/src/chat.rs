//! The interactive agent chat.
//!
//! Resolves the active profile, builds the Lightweight provider and the bounded
//! tool executor, and drives the one core loop. Model output is printed as it is
//! returned, tool activity is shown on stderr, and a tool call that needs
//! approval pauses for a yes/no at the prompt before the run resumes.

use std::io::{BufRead as _, Write as _};
use std::sync::Arc;
use std::time::Duration;

use lightagent_core::{
    AgentEvent, AgentLoop, AgentProfile, AgentProvider, ApprovalDecision, Config, ConfigStore,
    LightagentPaths, ModelRouting, PolicyEngine, ProfileId, ProfileStore, ProviderError,
    ProviderFactory, RunId, RunOutcome, StopReason,
};
use lightagent_provider_lightweight::{LightweightProvider, ProviderConfig};
use lightagent_tools::{BoundedExecutor, Delegation, ToolRegistry};
use tokio_util::sync::CancellationToken;

use crate::slash::{self, Slash};

/// Builds Lightweight providers for delegated worker runs.
struct LightweightFactory {
    base_url: String,
    api_key: Option<String>,
}

impl ProviderFactory for LightweightFactory {
    fn provider(&self, routing: &ModelRouting) -> Result<Arc<dyn AgentProvider>, ProviderError> {
        let base = routing
            .base_url
            .clone()
            .unwrap_or_else(|| self.base_url.clone());
        let mut config = ProviderConfig::new(base, routing.model.clone());
        if let Some(key) = &self.api_key {
            config = config.with_api_key(key.clone());
        }
        Ok(Arc::new(LightweightProvider::new(config)?))
    }
}

/// Run the interactive session until end-of-input or `/exit`.
pub async fn run(profile: Option<String>, _json: bool) -> Result<(), String> {
    let paths = LightagentPaths::resolve().map_err(|error| error.to_string())?;
    let config = ConfigStore::at(&paths)
        .load()
        .map_err(|error| error.to_string())?;
    let store = ProfileStore::new(paths.root());
    let profile = resolve_profile(&store, &config, profile)?;

    let base_url = profile
        .routing
        .base_url
        .clone()
        .unwrap_or_else(|| config.inference.base_url.clone());
    let model = if profile.routing.model.is_empty() {
        config
            .inference
            .model
            .clone()
            .unwrap_or_else(|| "default".to_string())
    } else {
        profile.routing.model.clone()
    };
    let api_key = config
        .inference
        .api_key
        .as_ref()
        .and_then(|secret| secret.resolve());

    let mut provider_config = ProviderConfig::new(base_url.clone(), model.clone());
    if let Some(key) = &api_key {
        provider_config = provider_config.with_api_key(key.clone());
    }
    let provider = LightweightProvider::new(provider_config).map_err(|error| error.to_string())?;

    let delegation = Delegation {
        profiles: Arc::new(store),
        factory: Arc::new(LightweightFactory { base_url, api_key }),
        worker_registry: ToolRegistry::worker_default(),
        worker_per_call: Duration::from_secs(60),
        worker_max_output_bytes: 262_144,
    };
    let executor = BoundedExecutor::new(
        ToolRegistry::builtin(),
        PolicyEngine::new(profile.approval_policy.into()),
        Duration::from_secs(60),
        262_144,
    )
    .with_run(RunId::new())
    .with_delegation(delegation);

    let agent = AgentLoop::from_profile(provider, executor, &profile);

    println!(
        "Lightagent chat — profile '{}', model '{}'.",
        profile.id.as_str(),
        model
    );
    println!("Type a message, or /help for commands. /exit to leave.");

    let stdin = std::io::stdin();
    loop {
        print!("\n› ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin
            .lock()
            .read_line(&mut line)
            .map_err(|error| error.to_string())?
            == 0
        {
            break; // end of input
        }
        let line = line.trim_end().to_string();
        if line.trim().is_empty() {
            continue;
        }
        if let Some(command) = slash::parse(&line) {
            if handle_slash(command) {
                break;
            }
            continue;
        }
        let outcome = agent
            .run(line, CancellationToken::new())
            .await
            .map_err(|error| error.to_string())?;
        drive(&agent, outcome, &stdin).await?;
    }
    Ok(())
}

/// Handle a slash command; returns true when the session should end.
fn handle_slash(command: Slash) -> bool {
    match command {
        Slash::Exit => return true,
        Slash::Help => {
            println!("Commands: /help  /tools  /new  /stop  /exit");
        }
        Slash::Tools => {
            for name in ToolRegistry::builtin().names() {
                println!("  {name}");
            }
        }
        Slash::New => println!("(new run)"),
        Slash::Stop => println!("(nothing running)"),
        Slash::Approve | Slash::Reject => {
            println!("(no tool call is awaiting a decision)");
        }
        Slash::Unknown(word) => println!("unknown command: /{word} (try /help)"),
    }
    false
}

/// Drive a run to completion, prompting for approval each time it pauses.
async fn drive(
    agent: &AgentLoop<LightweightProvider, BoundedExecutor>,
    mut outcome: RunOutcome,
    stdin: &std::io::Stdin,
) -> Result<(), String> {
    loop {
        match outcome {
            RunOutcome::Completed { events } => {
                render(&events);
                return Ok(());
            }
            RunOutcome::AwaitingApproval {
                events,
                request,
                suspended,
            } => {
                render(&events);
                eprintln!(
                    "\n⚠ approval needed: {} [{}]\n  arguments: {}",
                    request.tool,
                    request.risk.as_str(),
                    request.arguments_preview
                );
                eprint!("  approve? [y/N] ");
                let _ = std::io::stderr().flush();
                let mut answer = String::new();
                stdin
                    .lock()
                    .read_line(&mut answer)
                    .map_err(|error| error.to_string())?;
                let granted = matches!(answer.trim(), "y" | "Y" | "yes");
                let decision = if granted {
                    ApprovalDecision::grant(request.id)
                } else {
                    ApprovalDecision::deny(request.id)
                };
                outcome = agent
                    .resume(suspended, decision, CancellationToken::new())
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }
    }
}

/// Print the events of a completed (or paused) segment.
fn render(events: &[AgentEvent]) {
    for event in events {
        match event {
            AgentEvent::Content { text } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ToolCallStarted { name, .. } => eprintln!("\n· running {name}…"),
            AgentEvent::ToolCallCompleted { outcome, .. } => {
                if outcome.is_error {
                    eprintln!("· tool error: {}", outcome.content);
                }
            }
            AgentEvent::RunCompleted { reason } => {
                if !matches!(reason, StopReason::EndTurn) {
                    eprintln!("\n(run ended: {reason:?})");
                } else {
                    println!();
                }
            }
            _ => {}
        }
    }
}

/// Resolve the profile to run: the named one, else the active one, else a
/// built-in default that needs no prior `init`.
fn resolve_profile(
    store: &ProfileStore,
    config: &Config,
    name: Option<String>,
) -> Result<AgentProfile, String> {
    let id = match name {
        Some(name) => Some(ProfileId::new(&name).map_err(|error| error.to_string())?),
        None => store.active().map_err(|error| error.to_string())?,
    };
    match id {
        Some(id) => store.load(&id).map_err(|error| error.to_string()),
        None => default_profile(config),
    }
}

fn default_profile(config: &Config) -> Result<AgentProfile, String> {
    let id = ProfileId::new("default").map_err(|error| error.to_string())?;
    let model = config
        .inference
        .model
        .clone()
        .unwrap_or_else(|| "default".to_string());
    Ok(AgentProfile::new(
        id,
        "Default",
        "You are Lightagent, a helpful local agent with live tools.",
        model,
    ))
}
