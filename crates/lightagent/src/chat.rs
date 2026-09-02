//! The interactive agent chat.
//!
//! Resolves the active profile, builds the Lightweight provider and the bounded
//! tool executor, and drives the one core loop. Model output is printed as it is
//! returned, tool activity is shown on stderr, and a tool call that needs
//! approval pauses for a yes/no at the prompt before the run resumes.

use std::collections::HashMap;
use std::io::{BufRead as _, Write as _};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use lightagent_core::{
    AgentEvent, AgentLoop, AgentProfile, AgentProvider, ApprovalDecision, Config, ConfigStore,
    LightagentPaths, ModelRouting, PolicyEngine, ProfileId, ProfileStore, ProviderError,
    ProviderFactory, RunId, RunOutcome, StopReason,
};
use lightagent_provider_lightweight::{LightweightProvider, ProviderConfig};
use lightagent_store::{RunRecord, Session, SessionStore, StoredMessage, ToolHistoryEntry};
use lightagent_tools::{BoundedExecutor, Delegation, ToolRegistry, WebContext, WebPolicy};
use tokio_util::sync::CancellationToken;

use crate::slash::{self, Slash};

/// Build the web context for a run when web access is enabled, else `None`.
///
/// Shared by `chat` and `serve`. The client disables automatic redirects so
/// `web.fetch` follows them under its own per-hop SSRF guard, and carries the
/// configured per-request timeout. The search key is resolved here and held only
/// in memory. `None` when web is disabled or the client cannot be built.
pub(crate) fn web_context(config: &Config) -> Option<WebContext> {
    if !config.web.enabled {
        return None;
    }
    lightagent_provider_lightweight::ensure_provider();
    let timeout = Duration::from_secs(config.web.timeout_secs.max(1));
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("lightagent/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let policy = WebPolicy {
        allow_domains: config.web.allow_domains.clone(),
        block_private_addresses: config.web.block_private_addresses,
        max_fetch_bytes: config.web.max_fetch_bytes,
        timeout,
        search_endpoint: config.web.search.endpoint.clone(),
        search_query_param: config.web.search.query_param.clone(),
        search_api_key: config
            .web
            .search
            .api_key
            .as_ref()
            .and_then(|key| key.resolve()),
        search_max_results: config.web.search.max_results,
    };
    Some(WebContext {
        client,
        policy: Arc::new(policy),
    })
}

/// Builds Lightweight providers for delegated worker runs.
pub(crate) struct LightweightFactory {
    pub(crate) base_url: String,
    pub(crate) api_key: Option<String>,
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

    let session_store = SessionStore::at_profile(&store.handle(&profile.id));
    let mut session = Session::new(profile.id.as_str(), "chat session");

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
    let mut executor = BoundedExecutor::new(
        ToolRegistry::builtin(),
        PolicyEngine::new(profile.approval_policy.into()),
        Duration::from_secs(60),
        262_144,
    )
    .with_run(RunId::new())
    .with_delegation(delegation);
    if let Some(web) = web_context(&config) {
        executor = executor.with_web(web);
    }

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
        session.push_message(StoredMessage::new("user", &line));
        let outcome = agent
            .run(line, CancellationToken::new())
            .await
            .map_err(|error| error.to_string())?;
        let events = drive(&agent, outcome, &stdin).await?;
        record_turn(&mut session, &events);
        if let Err(error) = session_store.save(&session) {
            eprintln!("· could not save session: {error}");
        }
    }
    if !session.runs.is_empty() {
        println!("\nSession saved as {}.", session.id.as_str());
    }
    Ok(())
}

/// Fold one completed run's events into the session: the assistant's answer as a
/// message, and a run record with its tool history.
fn record_turn(session: &mut Session, events: &[AgentEvent]) {
    let mut run_id = String::new();
    let mut content = String::new();
    let mut stop_reason = None;
    let mut names: HashMap<String, String> = HashMap::new();
    let mut arguments: HashMap<String, String> = HashMap::new();
    let mut tools = Vec::new();

    for event in events {
        match event {
            AgentEvent::RunStarted { run, .. } => run_id = run.as_str().to_string(),
            AgentEvent::Content { text } => content.push_str(text),
            AgentEvent::ToolCallRequested { call } => {
                arguments.insert(call.id.clone(), call.arguments.clone());
            }
            AgentEvent::ToolCallStarted { id, name } => {
                names.insert(id.clone(), name.clone());
            }
            AgentEvent::ToolCallCompleted { id, outcome } => tools.push(ToolHistoryEntry {
                tool: names.get(id).cloned().unwrap_or_else(|| id.clone()),
                arguments_preview: preview(arguments.get(id).map(String::as_str).unwrap_or("")),
                outcome: if outcome.is_error { "error" } else { "ok" }.to_string(),
                duration_ms: None,
            }),
            AgentEvent::RunCompleted { reason } => stop_reason = Some(format!("{reason:?}")),
            _ => {}
        }
    }

    if !content.is_empty() {
        session.push_message(StoredMessage::new("assistant", content));
    }
    let now = SystemTime::now();
    session.push_run(RunRecord {
        run_id,
        started_at: now,
        ended_at: Some(now),
        stop_reason,
        tools,
    });
}

fn preview(text: &str) -> String {
    const MAX: usize = 120;
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        text.chars().take(MAX).collect()
    }
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
) -> Result<Vec<AgentEvent>, String> {
    loop {
        match outcome {
            RunOutcome::Completed { events } => {
                render(&events);
                return Ok(events);
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
pub(crate) fn resolve_profile(
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
