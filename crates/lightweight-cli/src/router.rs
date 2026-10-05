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

fn load(config: Option<PathBuf>) -> Result<(PathBuf, RouterConfig), String> {
    let path = match config {
        Some(path) => path,
        None => default_config_path()?,
    };
    let loaded = lightweight_router::load(&path).map_err(|err| match err {
        lightweight_router::config::LoadError::Read { .. } => {
            format!("{err}. Write one, or pass --config <path>.")
        }
        other => other.to_string(),
    })?;
    Ok((path, loaded))
}

/// `hermes router validate-config`: check the file and the environment it
/// names, without listening or contacting a node.
pub fn validate(config: Option<PathBuf>, out: &mut String) -> Result<ExitCode, String> {
    let (path, config) = load(config)?;
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
    Ok(ExitCode::SUCCESS)
}

/// The route table, as the operator configured it.
fn summarize(config: &RouterConfig, out: &mut String) {
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
    }
}

/// `hermes router`: validate, bind, and serve until interrupted.
pub fn run(config: Option<PathBuf>, listen: &[String]) -> Result<ExitCode, String> {
    let (path, mut config) = load(config)?;
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
            .map_err(|err| err.to_string())?;
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
        summary.push_str(&format!(
            "  auth     {}\n",
            if state.auth.is_enabled() {
                "api key required"
            } else {
                "disabled (loopback only)"
            }
        ));
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
        bound.serve(stop).await
    })?;
    Ok(ExitCode::SUCCESS)
}
