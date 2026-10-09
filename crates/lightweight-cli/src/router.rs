//! `hermes router`: one OpenAI-compatible endpoint over several gateways.
//!
//! A thin wrapper over `lightweight-router`, in the shape `fleet` already set:
//! read a JSON file from the config directory (or `--config`), refuse it whole
//! if anything in it is wrong, and run until interrupted. Nothing here changes
//! what `hermes serve` does; the router is a separate process that talks to
//! gateways over HTTP, exactly as a client would.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use lightweight_router::RouterConfig;
use lightweight_system_info::DataPaths;
use tokio_util::sync::CancellationToken;

/// Where the configuration lives when `--config` was not given.
fn default_config_path() -> Result<PathBuf, String> {
    let paths = DataPaths::discover().map_err(crate::serve::describe)?;
    Ok(paths.config_dir().join("router.json"))
}

fn config_path(config: Option<PathBuf>) -> Result<PathBuf, String> {
    match config {
        Some(path) => Ok(path),
        None => default_config_path(),
    }
}

/// Read and validate the file. A Jev key the environment does not hold is
/// looked for in the operating system's credential store, where the panel
/// saves one.
fn load(
    config: Option<PathBuf>,
    store: &dyn lightweight_router::secret_store::SecretStore,
) -> Result<(PathBuf, lightweight_router::Loaded), String> {
    let path = config_path(config)?;
    let loaded = lightweight_router::load_with_store(&path, store).map_err(|err| match err {
        lightweight_router::config::LoadError::Read { .. } => {
            format!("{err}. Write one, or pass --config <path>.")
        }
        other => other.to_string(),
    })?;
    Ok((path, loaded))
}

/// `hermes router admin-token`: print the running router's admin token, which
/// the panel asks for before it saves settings.
pub fn admin_token(config: Option<PathBuf>, out: &mut String) -> Result<ExitCode, String> {
    let path = lightweight_router::admin::token_path(&config_path(config)?);
    let token = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "no admin token at {}: start the router with this configuration first (a router \
             listening off loopback has none)",
            path.display()
        )
    })?;
    out.push_str(token.trim());
    out.push('\n');
    Ok(ExitCode::SUCCESS)
}

/// `hermes router validate-config`: check the file and the environment it
/// names, without listening or contacting a node.
pub fn validate(config: Option<PathBuf>, out: &mut String) -> Result<ExitCode, String> {
    let store = lightweight_router::secret_store::os_store();
    let (path, loaded) = load(config, store.as_ref())?;
    let config = loaded.config;
    let topology = &config.topology;
    out.push_str(&format!("{} is valid.\n", path.display()));
    out.push_str(&format!(
        "  {} node(s), {} route(s), {} deployment(s); default route: {}\n",
        topology.nodes().len(),
        topology.routes().len(),
        topology.deployments().len(),
        topology
            .default_route()
            .map_or("none (`default` is refused)", |route| route.name.as_str()),
    ));
    summarize(&config, out);
    for warning in config.warnings() {
        out.push_str(&format!("  warning: {warning}\n"));
    }
    Ok(ExitCode::SUCCESS)
}

/// The route table, as the operator configured it.
fn summarize(config: &RouterConfig, out: &mut String) {
    if let Some(budget) = config.pre_commit_budget {
        out.push_str(&format!(
            "  pre-commit request budget: {} ms for every client request, until its response \
             starts (a non-streamed answer starts only when it is complete)\n",
            budget.as_millis()
        ));
    }
    for route in config.topology.routes() {
        let deployments: Vec<&str> = route
            .deployments
            .iter()
            .map(lightweight_router::domain::DeploymentId::as_str)
            .collect();
        out.push_str(&format!(
            "  route {:<16} {} -> {}\n",
            route.name.as_str(),
            route.policy.as_str(),
            deployments.join(", ")
        ));
    }
    if let Some(auto) = &config.auto {
        out.push_str(&format!(
            "  auto  {:<16} {} -> {}\n",
            lightweight_router::auto_route::AUTO_ROUTE,
            if auto.enabled { "on" } else { "off" },
            auto.summary()
        ));
        if let Some(scoring) = &auto.scoring {
            let weights = scoring.weights;
            out.push_str(&format!(
                "  scoring {:<14} {} -> classifier {} + prior {} \
                 (moves decisions within {:.3} of the threshold); route history observational \
                 only (half-life {}s, min samples {})\n",
                "",
                if scoring.enabled { "on" } else { "off" },
                weights.classifier,
                weights.prior,
                weights.influence_radius(),
                scoring.history.half_life.as_secs(),
                scoring.history.min_samples,
            ));
        }
    }
}

/// `hermes router`: validate, bind, and serve until interrupted.
pub fn run(
    config: Option<PathBuf>,
    listen: &[String],
    web_root: Option<PathBuf>,
) -> Result<ExitCode, String> {
    let path_is_default = config.is_none();
    // One store for the whole run: the key read at start and the key the
    // panel saves are the same entry.
    let store = lightweight_router::secret_store::os_store();
    let (path, loaded) = load(config, store.as_ref())?;
    let lightweight_router::Loaded {
        mut config,
        text,
        store_note,
    } = loaded;
    if !listen.is_empty() {
        config.listen = listen
            .iter()
            .map(|value| {
                value
                    .trim()
                    .parse::<SocketAddr>()
                    .map_err(|_| format!("--listen {value:?} is not a host:port socket address"))
            })
            .collect::<Result<_, _>>()?;
    }

    // Records go to the terminal (or the service manager's journal) rather
    // than to the data directory: a gateway on the same machine writes
    // `gateway.log` there, and the panel reads that file as the gateway's own.
    let _logging = lightweight_observability::init(lightweight_observability::LogConfig {
        directory: None,
        filter: "info".to_owned(),
        console: true,
        privacy: lightweight_core::privacy::PrivacyMode::Standard,
    })
    .map_err(crate::serve::describe)?;

    let runtime = crate::runtime()?;
    runtime.block_on(async move {
        let bound = lightweight_router::bind(&config)
            .await
            .map_err(|err| err.to_string())?
            .with_web_root(web_root.clone());

        // The admin token: minted now, written owner-only beside the file,
        // removed at stop. A router listening off loopback gets none.
        let token_path = lightweight_router::admin::token_path(&path);
        let mut token = None;
        let admin = lightweight_router::admin::loopback_only(&bound.addresses())
            .map_err(str::to_owned)
            .and_then(|()| lightweight_router::admin::generate_token())
            .and_then(|minted| {
                lightweight_router::admin::write_token(&token_path, &minted).map_err(|err| {
                    format!("the admin token could not be written beside the configuration: {err}")
                })?;
                let access =
                    lightweight_router::admin::AdminAccess::new(&minted, &bound.addresses())
                        .map_err(str::to_owned);
                token = Some(minted);
                access
            });
        let admin_line = match &admin {
            Ok(_) => format!(
                "settings from the panel (admin token: `hermes router admin-token`{})",
                if path_is_default {
                    ""
                } else {
                    " --config <same path>"
                }
            ),
            Err(reason) => format!("read-only ({reason})"),
        };
        let bound = bound.with_settings(
            lightweight_router::classifier_settings::ClassifierSettings::new(
                path.clone(),
                &text,
                lightweight_router::secret_store::os_store(),
                std::sync::Arc::new(|name: &str| std::env::var(name).ok()),
                admin,
            ),
        );
        let state = bound.state();

        let mut summary = String::new();
        summary.push_str(&format!(
            "Lightweight router {}\n  config   {}\n",
            env!("CARGO_PKG_VERSION"),
            path.display()
        ));
        for address in bound.addresses() {
            summary.push_str(&format!("  listen   http://{address}/v1\n"));
        }
        if let Some(root) = &web_root {
            for address in bound.addresses() {
                summary.push_str(&format!("  panel    http://{address}/\n"));
            }
            summary.push_str(&format!("  web root {}\n", root.display()));
        }
        summary.push_str(&format!(
            "  auth     {}\n",
            if state.auth.is_enabled() {
                "api key required"
            } else {
                "disabled (loopback only)"
            }
        ));
        summary.push_str(&format!("  admin    {admin_line}\n"));
        if let Some(note) = &store_note {
            summary.push_str(&format!("  key store {note}\n"));
        }
        summarize(&config, &mut summary);
        summary.push_str("\nPress Ctrl-C to stop.\n");
        // Through the same broken-pipe-tolerant path the rest of the CLI uses:
        // a router started as `hermes router | tee` must not die on a closed
        // reader.
        let _ = std::io::Write::write_all(&mut std::io::stdout(), summary.as_bytes());

        let stop = CancellationToken::new();
        let stopping = stop.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            stopping.cancel();
        });
        let served = bound.serve(stop).await;
        if let Some(token) = &token {
            lightweight_router::admin::remove_token(&token_path, token);
        }
        served
    })?;
    Ok(ExitCode::SUCCESS)
}
