//! The `lightagent` command-line interface.
//!
//! A thin surface over the one runtime: every command drives `lightagent-core`
//! and `lightagent-tools`, and the interface owns no agent logic of its own. The
//! default action is an interactive chat; the other commands set up and inspect
//! the isolated home, its profiles ("bots"), the configuration, the tools and
//! the provider.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod banner;
mod chat;
mod import;
mod serve;
mod slash;

use std::io::IsTerminal as _;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use lightagent_core::{
    AgentProfile, Config, ConfigStore, LightagentPaths, ProfileId, ProfileStore,
};
use lightagent_store::{SessionId, SessionStore};
use lightagent_tools::ToolRegistry;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The Lightagent CLI.
#[derive(Parser)]
#[command(
    name = "lightagent",
    about = "Lightagent — local intelligence with live tools",
    version,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Emit JSON instead of a human-readable report.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Start an interactive agent chat (the default action).
    Chat {
        /// The profile ("bot") to run; defaults to the active one.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Set up the isolated home, a first profile and the configuration.
    Init {
        /// Reconfigure even if a home already exists.
        #[arg(long)]
        force: bool,
        /// The profile id to create (default: `default`).
        #[arg(long)]
        profile: Option<String>,
        /// The provider base URL.
        #[arg(long)]
        base_url: Option<String>,
        /// The default model id.
        #[arg(long)]
        model: Option<String>,
    },
    /// Show or change configuration.
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// List the models the provider offers.
    Models,
    /// List the enabled tools.
    Tools {
        #[command(subcommand)]
        action: Option<ToolsAction>,
    },
    /// Manage agent profiles ("bots").
    Profiles {
        #[command(subcommand)]
        action: Option<ProfilesAction>,
    },
    /// List and inspect saved sessions for the active profile.
    Sessions {
        #[command(subcommand)]
        action: Option<SessionsAction>,
    },
    /// Serve the Lightagent HTTP API (runs, sessions, tools, approvals + SSE).
    Serve {
        /// Address to bind.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to bind.
        #[arg(long, default_value_t = 8735)]
        port: u16,
        /// Environment variable holding the API key (required off loopback).
        #[arg(long)]
        key_env: Option<String>,
        /// Serve the built WebUI panel from this directory (e.g. frontend/dist).
        #[arg(long)]
        web_root: Option<std::path::PathBuf>,
    },
    /// Report the environment, home, profile and provider.
    Doctor,
    /// Import profiles and skills from another agent's home.
    Import {
        #[command(subcommand)]
        source: ImportSource,
    },
    /// Print the welcome mark and exit.
    Banner {
        /// Render unconditionally (bypasses the terminal check), for CI.
        #[arg(long)]
        preview: bool,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the configuration, secrets shown only as references.
    Show,
    /// Print the path to the config file.
    Path,
    /// Read one dotted key (e.g. `inference.base_url`).
    Get { key: String },
    /// Set one dotted key.
    Set { key: String, value: String },
}

#[derive(Subcommand)]
enum ToolsAction {
    /// List the tools, their risk class and description.
    List,
}

#[derive(Subcommand)]
enum ProfilesAction {
    /// List the profiles and mark the active one.
    List,
    /// Show one profile.
    Show { id: String },
    /// Create a profile.
    Create {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        persona: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// Make a profile the active one.
    Use { id: String },
}

#[derive(Subcommand)]
enum ImportSource {
    /// Import Hermes profiles (persona, model routing) and skills.
    Hermes {
        /// The Hermes home to read (default: $HERMES_HOME, else ~/.hermes).
        #[arg(long)]
        from: Option<std::path::PathBuf>,
        /// Show what would be imported without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite profiles and skills that already exist.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum SessionsAction {
    /// List saved sessions, newest first.
    List,
    /// Show one session's transcript and run history.
    Show { id: String },
    /// Delete one session.
    Delete { id: String },
}

/// Entry point: parse, dispatch, and turn an error into a message and exit code.
pub fn run_cli() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: could not start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(dispatch(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: Cli) -> Result<(), String> {
    match cli.command {
        None => {
            greet(cli.json);
            if std::io::stdin().is_terminal() {
                chat::run(None, cli.json).await
            } else {
                println!(
                    "Run `lightagent --help` for the available commands, or `lightagent chat` to start."
                );
                Ok(())
            }
        }
        Some(Command::Chat { profile }) => {
            greet(cli.json);
            chat::run(profile, cli.json).await
        }
        Some(Command::Init {
            force,
            profile,
            base_url,
            model,
        }) => init(force, profile, base_url, model, cli.json),
        Some(Command::Config { action }) => {
            config_cmd(action.unwrap_or(ConfigAction::Show), cli.json)
        }
        Some(Command::Models) => models(cli.json).await,
        Some(Command::Tools { action }) => {
            let _ = action.unwrap_or(ToolsAction::List);
            tools_list(cli.json)
        }
        Some(Command::Profiles { action }) => {
            profiles_cmd(action.unwrap_or(ProfilesAction::List), cli.json)
        }
        Some(Command::Sessions { action }) => {
            sessions_cmd(action.unwrap_or(SessionsAction::List), cli.json)
        }
        Some(Command::Serve {
            host,
            port,
            key_env,
            web_root,
        }) => serve::run(host, port, key_env, web_root).await,
        Some(Command::Doctor) => doctor(cli.json),
        Some(Command::Import { source }) => match source {
            ImportSource::Hermes {
                from,
                dry_run,
                force,
            } => import::hermes(from, dry_run, force, cli.json),
        },
        Some(Command::Banner { preview }) => {
            if preview {
                print!(
                    "{}",
                    banner::render(VERSION, std::env::var_os("NO_COLOR").is_none())
                );
            } else if banner::should_show(cli.json) {
                banner::print(VERSION);
            }
            Ok(())
        }
    }
}

/// Print the welcome mark before an interactive action, honouring every gate.
fn greet(json: bool) {
    if banner::should_show(json) {
        banner::print(VERSION);
    }
}

/// Resolve the home paths, mapping the error to a message.
fn paths() -> Result<LightagentPaths, String> {
    LightagentPaths::resolve().map_err(|error| error.to_string())
}

fn load_config(paths: &LightagentPaths) -> Result<Config, String> {
    ConfigStore::at(paths)
        .load()
        .map_err(|error| error.to_string())
}

fn active_profile_id(store: &ProfileStore) -> Result<Option<ProfileId>, String> {
    store.active().map_err(|error| error.to_string())
}

// --- doctor -----------------------------------------------------------------

fn doctor(json: bool) -> Result<(), String> {
    let paths = paths()?;
    let config = load_config(&paths)?;
    let store = ProfileStore::new(paths.root());
    let active = active_profile_id(&store)?;
    let profiles = store.list().map_err(|error| error.to_string())?;
    let tool_count = ToolRegistry::builtin().names().len();
    let home_exists = paths.root().exists();

    if json {
        let value = serde_json::json!({
            "version": VERSION,
            "home": paths.root().display().to_string(),
            "home_exists": home_exists,
            "config_file": paths.config_file().display().to_string(),
            "provider": config.inference.provider,
            "base_url": config.inference.base_url,
            "active_profile": active.as_ref().map(|id| id.as_str().to_string()),
            "profiles": profiles.iter().map(|id| id.as_str().to_string()).collect::<Vec<_>>(),
            "tools": tool_count,
            "banner": std::io::stderr().is_terminal(),
        });
        println!("{value:#}");
        return Ok(());
    }

    let mut out = String::new();
    out.push_str(&format!("Lightagent {VERSION}\n"));
    out.push_str(&format!(
        "Home:      {}{}\n",
        paths.root().display(),
        if home_exists {
            ""
        } else {
            "  (not created — run `lightagent init`)"
        }
    ));
    out.push_str(&format!("Config:    {}\n", paths.config_file().display()));
    out.push_str(&format!(
        "Provider:  {} @ {}\n",
        config.inference.provider, config.inference.base_url
    ));
    out.push_str(&format!(
        "Profile:   {}\n",
        active
            .as_ref()
            .map(|id| id.as_str())
            .unwrap_or("(none active)")
    ));
    out.push_str(&format!(
        "Profiles:  {}\n",
        if profiles.is_empty() {
            "(none)".to_string()
        } else {
            profiles
                .iter()
                .map(|id| id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    out.push_str(&format!("Tools:     {tool_count} built-in\n"));
    print!("{out}");
    Ok(())
}

// --- tools ------------------------------------------------------------------

fn tools_list(json: bool) -> Result<(), String> {
    let registry = ToolRegistry::builtin();
    let mut rows = Vec::new();
    for name in registry.names() {
        if let Some(tool) = registry.get(&name) {
            let definition = tool.definition();
            rows.push((
                definition.name.clone(),
                definition.risk.as_str().to_string(),
                definition.description.clone(),
            ));
        }
    }

    if json {
        let value: Vec<_> = rows
            .iter()
            .map(|(name, risk, description)| {
                serde_json::json!({ "name": name, "risk": risk, "description": description })
            })
            .collect();
        println!("{}", serde_json::Value::Array(value));
        return Ok(());
    }

    let mut out = String::from("Tools:\n");
    for (name, risk, description) in rows {
        out.push_str(&format!("  {name:<16} [{risk}]  {description}\n"));
    }
    print!("{out}");
    Ok(())
}

// --- profiles ---------------------------------------------------------------

fn profiles_cmd(action: ProfilesAction, json: bool) -> Result<(), String> {
    let paths = paths()?;
    let store = ProfileStore::new(paths.root());
    match action {
        ProfilesAction::List => {
            let active = active_profile_id(&store)?;
            let profiles = store.list().map_err(|error| error.to_string())?;
            if json {
                let value = serde_json::json!({
                    "active": active.as_ref().map(|id| id.as_str().to_string()),
                    "profiles": profiles.iter().map(|id| id.as_str().to_string()).collect::<Vec<_>>(),
                });
                println!("{value:#}");
                return Ok(());
            }
            if profiles.is_empty() {
                println!("No profiles yet. Create one with `lightagent profiles create <id>`.");
                return Ok(());
            }
            for id in profiles {
                let marker = if active.as_ref() == Some(&id) {
                    "* "
                } else {
                    "  "
                };
                println!("{marker}{}", id.as_str());
            }
            Ok(())
        }
        ProfilesAction::Show { id } => {
            let id = ProfileId::new(&id).map_err(|error| error.to_string())?;
            let profile = store.load(&id).map_err(|error| error.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_value(&profile).map_err(|e| e.to_string())?
                );
            } else {
                println!("id:      {}", profile.id.as_str());
                println!("name:    {}", profile.name);
                println!("model:   {}", profile.routing.model);
                println!("persona: {}", first_line(&profile.persona));
            }
            Ok(())
        }
        ProfilesAction::Create {
            id,
            name,
            persona,
            model,
        } => {
            let id = ProfileId::new(&id).map_err(|error| error.to_string())?;
            let profile = AgentProfile::new(
                id.clone(),
                name.unwrap_or_else(|| id.as_str().to_string()),
                persona.unwrap_or_else(|| "You are a helpful local agent.".to_string()),
                model.unwrap_or_else(|| "default".to_string()),
            );
            store.save(&profile).map_err(|error| error.to_string())?;
            println!("Created profile '{}'.", id.as_str());
            Ok(())
        }
        ProfilesAction::Use { id } => {
            let id = ProfileId::new(&id).map_err(|error| error.to_string())?;
            store.set_active(&id).map_err(|error| error.to_string())?;
            println!("Active profile is now '{}'.", id.as_str());
            Ok(())
        }
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

// --- sessions ---------------------------------------------------------------

fn sessions_cmd(action: SessionsAction, json: bool) -> Result<(), String> {
    let paths = paths()?;
    let store = ProfileStore::new(paths.root());
    let active = active_profile_id(&store)?
        .ok_or_else(|| "no active profile — run `lightagent init` first".to_string())?;
    let handle = store.handle(&active);
    let sessions = SessionStore::at_profile(&handle);

    match action {
        SessionsAction::List => {
            let list = sessions.list().map_err(|error| error.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_value(&list).map_err(|e| e.to_string())?
                );
                return Ok(());
            }
            if list.is_empty() {
                println!("No saved sessions for profile '{}'.", active.as_str());
                return Ok(());
            }
            for summary in list {
                println!(
                    "{}  {:<24}  {} msgs, {} runs",
                    summary.id.as_str(),
                    truncate(&summary.title, 24),
                    summary.message_count,
                    summary.run_count
                );
            }
            Ok(())
        }
        SessionsAction::Show { id } => {
            let id = SessionId::parse(&id).map_err(|error| error.to_string())?;
            let session = sessions.load(&id).map_err(|error| error.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_value(&session).map_err(|e| e.to_string())?
                );
                return Ok(());
            }
            println!(
                "session {}  (profile {})",
                session.id.as_str(),
                session.profile
            );
            println!("title: {}", session.title);
            for message in &session.messages {
                println!("  [{}] {}", message.role, first_line(&message.content));
            }
            for run in &session.runs {
                println!(
                    "  run {} — {} ({} tool calls)",
                    run.run_id,
                    run.stop_reason.as_deref().unwrap_or("?"),
                    run.tools.len()
                );
            }
            Ok(())
        }
        SessionsAction::Delete { id } => {
            let id = SessionId::parse(&id).map_err(|error| error.to_string())?;
            if sessions.delete(&id).map_err(|error| error.to_string())? {
                println!("Deleted session {}.", id.as_str());
            } else {
                println!("No session {} to delete.", id.as_str());
            }
            Ok(())
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let kept: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

// --- config -----------------------------------------------------------------

fn config_cmd(action: ConfigAction, json: bool) -> Result<(), String> {
    let paths = paths()?;
    let store = ConfigStore::at(&paths);
    match action {
        ConfigAction::Path => {
            println!("{}", store.path().display());
            Ok(())
        }
        ConfigAction::Show => {
            let config = store.load().map_err(|error| error.to_string())?;
            let value = config.redacted_json();
            if json {
                println!("{value}");
            } else {
                println!("{value:#}");
            }
            Ok(())
        }
        ConfigAction::Get { key } => {
            let config = store.load().map_err(|error| error.to_string())?;
            match get_key(&config, &key) {
                Some(value) => {
                    println!("{value}");
                    Ok(())
                }
                None => Err(format!("unknown or unreadable config key '{key}'")),
            }
        }
        ConfigAction::Set { key, value } => {
            let mut config = store.load().map_err(|error| error.to_string())?;
            set_key(&mut config, &key, &value)?;
            store.save(&config).map_err(|error| error.to_string())?;
            println!("Set {key} = {value}");
            Ok(())
        }
    }
}

/// Read a small, fixed set of dotted keys.
fn get_key(config: &Config, key: &str) -> Option<String> {
    match key {
        "inference.provider" => Some(config.inference.provider.clone()),
        "inference.base_url" => Some(config.inference.base_url.clone()),
        "inference.model" => Some(config.inference.model.clone().unwrap_or_default()),
        "inference.device" => Some(config.inference.device.clone()),
        _ => None,
    }
}

/// Write a small, fixed set of dotted keys.
fn set_key(config: &mut Config, key: &str, value: &str) -> Result<(), String> {
    match key {
        "inference.base_url" => config.inference.base_url = value.to_string(),
        "inference.model" => config.inference.model = Some(value.to_string()),
        "inference.device" => config.inference.device = value.to_string(),
        _ => return Err(format!("unknown or read-only config key '{key}'")),
    }
    Ok(())
}

// --- init -------------------------------------------------------------------

fn init(
    force: bool,
    profile: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    json: bool,
) -> Result<(), String> {
    let paths = paths()?;
    let already = paths.config_file().exists();
    if already && !force {
        return Err(format!(
            "already initialised at {} — pass --force to reconfigure",
            paths.root().display()
        ));
    }
    paths.scaffold().map_err(|error| error.to_string())?;

    let mut config = ConfigStore::at(&paths)
        .load()
        .map_err(|error| error.to_string())?;
    if let Some(base_url) = base_url {
        config.inference.base_url = base_url;
    }
    if let Some(model) = &model {
        config.inference.model = Some(model.clone());
    }
    ConfigStore::at(&paths)
        .save(&config)
        .map_err(|error| error.to_string())?;

    let profile_id = profile.unwrap_or_else(|| "default".to_string());
    let id = ProfileId::new(&profile_id).map_err(|error| error.to_string())?;
    let store = ProfileStore::new(paths.root());
    if store.load(&id).is_err() {
        let profile = AgentProfile::new(
            id.clone(),
            "Default",
            "You are Lightagent, a helpful local agent with live tools.",
            model.unwrap_or_else(|| "default".to_string()),
        );
        store.save(&profile).map_err(|error| error.to_string())?;
    }
    store.set_active(&id).map_err(|error| error.to_string())?;

    if json {
        let value = serde_json::json!({
            "home": paths.root().display().to_string(),
            "config_file": paths.config_file().display().to_string(),
            "active_profile": id.as_str(),
            "base_url": config.inference.base_url,
        });
        println!("{value:#}");
    } else {
        println!("Initialised Lightagent at {}", paths.root().display());
        println!("  active profile: {}", id.as_str());
        println!("  provider:       {}", config.inference.base_url);
        println!("Run `lightagent chat` to start.");
    }
    Ok(())
}

// --- models -----------------------------------------------------------------

async fn models(json: bool) -> Result<(), String> {
    use lightagent_provider_lightweight::{LightweightProvider, ProviderConfig};

    let paths = paths()?;
    let config = load_config(&paths)?;
    let mut provider_config = ProviderConfig::new(config.inference.base_url.clone(), "default");
    if let Some(secret) = &config.inference.api_key
        && let Some(value) = secret.resolve()
    {
        provider_config = provider_config.with_api_key(value);
    }
    let provider = LightweightProvider::new(provider_config).map_err(|error| error.to_string())?;

    match provider.models().await {
        Ok(models) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_value(&models).map_err(|e| e.to_string())?
                );
            } else if models.is_empty() {
                println!("The provider offers no models (is one loaded?).");
            } else {
                for model in models {
                    println!("{model}");
                }
            }
            Ok(())
        }
        Err(error) => Err(format!(
            "could not reach the provider at {}: {error}",
            config.inference.base_url
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_config_keys_round_trip() {
        let mut config = Config::default();
        assert_eq!(
            get_key(&config, "inference.base_url").as_deref(),
            Some("http://127.0.0.1:11434")
        );
        set_key(&mut config, "inference.model", "demo").unwrap();
        assert_eq!(get_key(&config, "inference.model").as_deref(), Some("demo"));
    }

    #[test]
    fn unknown_config_keys_are_rejected() {
        let mut config = Config::default();
        assert!(get_key(&config, "nope").is_none());
        assert!(set_key(&mut config, "nope", "x").is_err());
    }

    #[test]
    fn first_line_takes_only_the_first() {
        assert_eq!(first_line("a\nb\nc"), "a");
        assert_eq!(first_line(""), "");
    }
}
