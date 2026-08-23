//! The `serve` command: acquire the engine, admit a model, load it, serve it.
//!
//! The whole stack in one command — engine acquisition, RAM admission, process
//! supervision, and the OpenAI-compatible gateway on top of the loaded
//! instance.
//!
//! Two things are worth noticing. The admission check measures the model
//! against this machine's free memory *before* anything is launched, so a load
//! that cannot fit is refused with numbers and suggestions rather than
//! discovered as an OOM kill thirty seconds later. And the gateway advertises
//! the context the model was actually loaded with, under an id that encodes
//! it — which is what keeps a client from sizing prompts to a window that does
//! not exist.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hermes_backend_llamacpp::backend::ProcessBackend;
use hermes_core::{Actionable, ModelId, RuntimeParams, units::Bytes};
use hermes_gateway::catalog::{Catalog, ResidentModel};
use hermes_gateway::{AuthPolicy, GatewayConfig, GatewayState};
use hermes_gguf::{GgufFile, ModelMetadata};
use hermes_inference::{InferenceBackend, LoadProgress, LoadRequest};
use hermes_memory::{Estimator, Verdict};
use hermes_system_info::{CpuInfo, DataPaths, MemoryProbe as _, SystemMemoryProbe};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Everything `serve` needs.
pub struct ServeOptions {
    pub model: PathBuf,
    /// `None` selects the largest context that loads safely here.
    pub n_ctx: Option<u32>,
    pub threads: Option<u32>,
    pub kv_type: String,
    /// Load even when the RAM estimate says it will not fit.
    ///
    /// Present because the estimate is an upper bound: it does not discount a
    /// declared sliding window, and its compute term is uncalibrated until a
    /// benchmark has run. A user who knows their model better than we do
    /// should be able to say so — and be told plainly what they are overriding.
    pub force: bool,
    /// Addresses or hostnames to bind the gateway to.
    ///
    /// A machine on an overlay network usually holds several addresses at
    /// once — a LAN address, a mesh address, both families of each — and
    /// serving on two of them should not require opening the wildcard. Empty
    /// means loopback.
    pub hosts: Vec<String>,
    /// Port to bind. `0` picks a free one.
    pub port: u16,
    /// Key required on every request. Mandatory as soon as any bind is
    /// reachable from another machine.
    pub api_key: Option<String>,
}

/// The environment variable holding the API key.
///
/// Preferred over the flag: an argument is visible in `ps` and readable from
/// `/proc` by anyone on the machine, which is a poor place for a credential.
pub const API_KEY_ENV: &str = "HERMES_API_KEY";

/// Run the command. Returns once the engine has been shut down.
pub async fn run(options: ServeOptions) -> Result<(), String> {
    let paths = DataPaths::discover().map_err(describe)?;
    paths.create_all().map_err(describe)?;

    // A long-running server that keeps no record of what it did is not
    // serviceable. `hermes-observability` has existed since the first
    // milestone; nothing had called it, so every `tracing` line in the
    // workspace went nowhere. The guard lives until this function returns,
    // because dropping it stops the writers.
    //
    // Privacy Mode is installed here, before the first record: redaction is on
    // by default and prompts cannot be logged even by a mistake in whatever
    // configuration is read later.
    let _logging = hermes_observability::init(hermes_observability::LogConfig {
        directory: Some(paths.logs_dir()),
        filter: "info".to_owned(),
        console: false,
        privacy: hermes_core::privacy::PrivacyMode::Standard,
    })
    .map_err(describe)?;

    // Networking is settled first, and the addresses are claimed, before a
    // byte of the model is read. Loading first would mean a missing API key or
    // an address this machine no longer holds costs a multi-gigabyte load and
    // several seconds before failing - which is exactly what happened the first
    // time this was run against a real model.
    let addresses = resolve_hosts(&options.hosts, options.port)?;
    let key = options.api_key.clone().or_else(read_key_from_environment);
    let bind_ips: Vec<IpAddr> = addresses.iter().map(SocketAddr::ip).collect();
    let auth = AuthPolicy::for_binds(&bind_ips, key).map_err(|_| exposed_without_key(&bind_ips))?;

    let mut listeners = Vec::with_capacity(addresses.len());
    for address in &addresses {
        listeners.push(bind(*address).await?);
    }

    let file = GgufFile::open(&options.model).map_err(describe)?;
    let metadata = ModelMetadata::from_file(&file).map_err(describe)?;

    let cache_type = options
        .kv_type
        .parse()
        .map_err(|_| format!("unknown KV cache type {:?}", options.kv_type))?;
    let cpu = CpuInfo::detect();
    let base = RuntimeParams {
        cache_type_k: cache_type,
        cache_type_v: cache_type,
        threads: Some(options.threads.unwrap_or_else(|| cpu.default_threads())),
        ..RuntimeParams::default()
    };

    let estimator = Estimator::headless();
    let snapshot = SystemMemoryProbe.snapshot().map_err(describe)?;

    // Sizing the context to the machine rather than to a constant is what lets
    // one build serve a small laptop and a large workstation well. A fixed
    // default would either waste a big machine or refuse to load on a small one.
    let (n_ctx, chosen_automatically) = match options.n_ctx {
        Some(requested) => (requested, false),
        None => (
            estimator
                .largest_safe_context(&metadata, base, snapshot, None)
                .unwrap_or(base.n_ctx),
            true,
        ),
    };
    let params = base.with_context(n_ctx);

    // Admission control. Section 19: never promise a model will run just
    // because its weights fit.
    let estimate = estimator.estimate(&metadata, params, snapshot);

    println!(
        "{}  {}  {}",
        metadata.name.as_deref().unwrap_or(&metadata.architecture),
        metadata.quantization_label(),
        metadata.parameters_label().unwrap_or_default()
    );
    println!(
        "  estimated {} (weights {} + KV {} + compute {} + overhead {})",
        estimate.total, estimate.weights, estimate.kv_cache, estimate.compute, estimate.overhead
    );
    println!(
        "  available {}   status {}",
        estimate.budget,
        estimate.verdict.label()
    );
    if chosen_automatically {
        println!(
            "  context {} tokens, chosen to fit this machine (model supports up to {})",
            params.n_ctx,
            metadata
                .context_length
                .map_or_else(|| "unknown".to_owned(), |max| max.to_string())
        );
    }

    if estimate.verdict == Verdict::Insufficient {
        for remedy in estimate.remedies() {
            println!("  - {}", remedy.label);
        }
        if !options.force {
            return Err(format!(
                "refusing to load: short by {}. Pass --force to try anyway.",
                estimate.shortfall()
            ));
        }
        println!("  --force given: loading anyway, which may end in an OOM kill");
    }

    let backend = ProcessBackend::new(paths.runtime_dir()).map_err(describe)?;
    let cancel = CancellationToken::new();
    let (progress_tx, mut progress_rx) = mpsc::channel(32);

    // Reported as it happens: acquiring the engine on a first run is a 16 MB
    // download, and a large model load is tens of seconds. Silence for that
    // long reads as a hang.
    let reporter = tokio::spawn(async move {
        // Only redraw when the whole percent changes. A 16 MB download arrives
        // in thousands of chunks, and a line per chunk buries everything else.
        let mut last_percent = u64::MAX;
        while let Some(update) = progress_rx.recv().await {
            match update {
                LoadProgress::AcquiringRuntime { downloaded, total } => {
                    // `checked_div` rather than a preceding `> 0` test: the
                    // precondition stays part of the expression.
                    if let Some(percent) = downloaded
                        .saturating_mul(100)
                        .checked_div(total.unwrap_or(0))
                        && percent != last_percent
                    {
                        last_percent = percent;
                        print!("\r  downloading engine {percent:>3}%");
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                }
                LoadProgress::VerifyingRuntime => println!("\r  verifying engine       "),
                LoadProgress::StartingEngine => println!("  starting engine"),
                LoadProgress::LoadingWeights => println!("  loading weights"),
                LoadProgress::Ready => println!("  ready"),
            }
        }
    });

    // Captured before the metadata is moved into the load request: the catalog
    // reports what the model *is*, and the gateway answers `/v1/models` from
    // that rather than by asking the engine, which only knows a file path.
    let architecture = metadata.architecture.clone();
    let param_count = metadata.param_count;
    let quantization = Some(metadata.quantization_label());
    let model_max_context_length = metadata.context_length;

    let request = LoadRequest {
        model: ModelId::with_context(
            options
                .model
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("model"),
            params.n_ctx,
        ),
        gguf_path: options.model.clone(),
        metadata: Arc::new(metadata),
        runtime: params,
    };

    let started = std::time::Instant::now();
    let loaded = backend
        .load(request, progress_tx, cancel.clone())
        .await
        .map_err(describe)?;
    let _ = reporter.await;

    println!(
        "\nloaded {} in {:.1}s  (engine variant {}, {} threads)",
        loaded.model,
        started.elapsed().as_secs_f64(),
        cpu.expected_ggml_variant(),
        loaded.effective.threads.unwrap_or(0)
    );

    if let Ok(Some(usage)) = backend.resource_usage().await {
        println!("  engine resident {}  peak {}", usage.rss, usage.peak_rss);
        report_estimate_accuracy(estimate.total, usage.peak_rss);
    }

    let backend = Arc::new(backend);
    let catalog = Arc::new(Catalog::with_resident(ResidentModel {
        id: loaded.model.clone(),
        instance: loaded.instance,
        // The *effective* context, which is what every endpoint advertises. A
        // client sizes its prompts to this number.
        n_ctx: loaded.effective.n_ctx,
        architecture,
        param_count,
        quantization,
        model_max_context_length,
        ram_verdict: Some(estimate.verdict.label().to_owned()),
        backend: Some(backend.id().to_string()),
        model_path: options.model.display().to_string(),
    }));

    let state = Arc::new(GatewayState::new(
        Arc::clone(&backend) as Arc<dyn InferenceBackend>,
        catalog,
        GatewayConfig {
            auth,
            ..GatewayConfig::default()
        },
    ));

    println!();
    for listener in &listeners {
        // Printed for the operator, deliberately not logged: which addresses
        // this machine holds is not something the log file needs to remember.
        let bound = listener
            .local_addr()
            .map_err(|err| format!("could not read the bound address: {err}"))?;
        println!("serving  http://{bound}/v1");
    }
    println!("  model    {}", loaded.model);
    println!("  context  {} tokens", loaded.effective.n_ctx);
    println!(
        "  auth     {}",
        if state.config.auth.is_enabled() {
            "api key required"
        } else {
            "disabled (loopback only)"
        }
    );
    tracing::info!(
        target: hermes_observability::targets::STARTUP,
        port = options.port,
        listeners = listeners.len(),
        auth = state.config.auth.is_enabled(),
        "gateway listening"
    );
    println!("  logs     {}", paths.logs_dir().display());
    println!("\nPress Ctrl-C to stop.");

    // Two ways to stop, and both must leave nothing behind: the user asking,
    // and the engine dying under us. The second is why the engine is a child
    // process at all - it is an event we can observe and report rather than
    // something that takes this process with it.
    let shutdown_state = Arc::clone(&state);
    let death_watch = Arc::clone(&backend);
    let watcher = tokio::spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => println!("\nstopping"),
            () = watch_for_death(&death_watch) => {}
        }
        shutdown_state.shutdown();
    });

    // One server per listener, all sharing the same state, all stopping on the
    // same token. A machine holding several addresses serves them from one
    // engine and one queue - the addresses are a networking detail, not a
    // second gateway.
    let mut servers = Vec::with_capacity(listeners.len());
    for listener in listeners {
        let app = hermes_gateway::app(Arc::clone(&state));
        let stopping = state.shutdown_token();
        servers.push(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { stopping.cancelled().await })
                .await
        }));
    }
    for server in servers {
        match server.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(format!("the gateway stopped: {err}")),
            Err(err) => return Err(format!("the gateway task failed: {err}")),
        }
    }
    watcher.abort();

    backend.shutdown().await.map_err(describe)?;
    println!("engine stopped");
    Ok(())
}

/// Compare the prediction against what the engine actually used.
///
/// The compute and overhead terms of the estimate are uncalibrated until a
/// benchmark run fits them, so printing the comparison is how that calibration
/// starts being useful — and how an estimate that is badly wrong becomes
/// visible instead of staying plausible.
fn report_estimate_accuracy(predicted: Bytes, observed_peak: Bytes) {
    if observed_peak == Bytes::ZERO {
        return;
    }
    let ratio = predicted.get() as f64 / observed_peak.get() as f64;
    println!(
        "  estimate was {ratio:.2}x the engine's peak ({predicted} predicted, {observed_peak} observed)"
    );
}

/// Resolve if the engine stops on its own.
///
/// The whole reason the engine is a child process: it dying is an event we can
/// observe and report, rather than something that takes this process with it.
async fn watch_for_death(backend: &ProcessBackend) {
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let health = backend.health().await;
        if let hermes_inference::BackendHealth::Failed { detail } = health {
            // `detail` is already a full sentence from the error's Display, so
            // prefixing it would read "engine stopped unexpectedly: the engine
            // stopped unexpectedly (...)".
            println!("\n{detail}");
            return;
        }
    }
}

fn describe<E: Actionable>(err: E) -> String {
    let mut out = err.to_string();
    let remedies = err.remedies();
    if !remedies.is_empty() {
        out.push_str("\n\nsuggested:");
        for remedy in remedies {
            out.push_str(&format!("\n  - {}", remedy.label));
        }
    }
    out
}

/// Turn `--host` values into addresses to bind.
///
/// Each value may be a literal address in either family, or a name. Accepting
/// names is what keeps addresses out of configuration files and out of this
/// repository: a machine's overlay address can be reissued, but its name
/// usually cannot, and `hermes serve --host "$(hostname)"` works on a LAN, on a
/// mesh network, and on a laptop that moves between them.
///
/// A name that resolves to several addresses yields several binds — which is
/// the common case on a dual-stack host, and exactly what the operator asked
/// for by naming the machine rather than one of its addresses.
fn resolve_hosts(hosts: &[String], port: u16) -> Result<Vec<SocketAddr>, String> {
    if hosts.is_empty() {
        return Ok(vec![SocketAddr::new(
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port,
        )]);
    }

    let mut addresses: Vec<SocketAddr> = Vec::new();
    for host in hosts {
        let host = host.trim();
        if host.is_empty() {
            continue;
        }
        let resolved: Vec<SocketAddr> = match host.parse::<IpAddr>() {
            Ok(address) => vec![SocketAddr::new(address, port)],
            Err(_) => (host, port)
                .to_socket_addrs()
                .map_err(|err| {
                    format!(
                        "could not resolve {host:?} to an address to bind: {err}. \
                         Pass an address this machine holds, or a name that resolves to one."
                    )
                })?
                .collect(),
        };
        if resolved.is_empty() {
            return Err(format!("{host:?} resolved to no addresses"));
        }
        for address in resolved {
            // A name and a literal can resolve to the same place; binding it
            // twice would fail on the second attempt for no good reason.
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
    }

    if addresses.is_empty() {
        return Err("no addresses to bind".to_owned());
    }
    Ok(addresses)
}

/// Bind one address, explaining the failures an operator will actually hit.
async fn bind(address: SocketAddr) -> Result<tokio::net::TcpListener, String> {
    tokio::net::TcpListener::bind(address).await.map_err(|err| {
        let explanation = match err.kind() {
            // The common failure on any overlay network: the interface is
            // down, or the address was reissued and this one no longer exists.
            std::io::ErrorKind::AddrNotAvailable => {
                " — this machine does not currently hold that address. \
                 Check the interface is up, or bind by name instead so the \
                 current address is looked up at startup."
            }
            std::io::ErrorKind::AddrInUse => {
                " — something else is already listening there. Choose another \
                 port, or stop the other process."
            }
            std::io::ErrorKind::PermissionDenied => {
                " — ports below 1024 need privileges this process does not have."
            }
            _ => "",
        };
        format!("could not bind {address}: {err}{explanation}")
    })
}

/// Read the API key from the environment.
///
/// An empty value is treated as absent, so `HERMES_API_KEY=` in a service file
/// cannot silently look like authentication.
fn read_key_from_environment() -> Option<String> {
    std::env::var(API_KEY_ENV)
        .ok()
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
}

/// Explain a refusal to bind somewhere reachable without a key.
///
/// Refusing is the whole point — there is no default key and never will be, so
/// an unconfigured gateway cannot end up reachable by accident. What this adds
/// is a key the operator can actually use, since needing one and having none is
/// the entire problem at this moment.
fn exposed_without_key(addresses: &[IpAddr]) -> String {
    let exposed: Vec<String> = addresses
        .iter()
        .filter(|address| !address.is_loopback())
        .map(ToString::to_string)
        .collect();

    let mut message = format!(
        "binding to {} makes this gateway reachable from other machines, \
         so an API key is required.",
        exposed.join(", ")
    );
    if let Ok(suggestion) = hermes_gateway::auth::generate_key() {
        message.push_str(&format!(
            "\n\n  Put one in the environment, for example:\n\n    export {API_KEY_ENV}={suggestion}\n\
             \n  --api-key works too, but an argument is visible in `ps` and in shell history."
        ));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_host_means_loopback() {
        // The default must stay exactly what it was: a gateway nobody asked to
        // expose is not exposed.
        let addresses = resolve_hosts(&[], 8737).expect("loopback");
        assert_eq!(addresses.len(), 1);
        assert!(addresses[0].ip().is_loopback());
        assert_eq!(addresses[0].port(), 8737);
    }

    #[test]
    fn literal_addresses_in_both_families_are_accepted() {
        let addresses = resolve_hosts(&["127.0.0.1".into(), "::1".into()], 0).expect("resolve");
        assert_eq!(addresses.len(), 2);
        assert!(addresses.iter().all(|address| address.ip().is_loopback()));
    }

    #[test]
    fn a_name_resolves_to_whatever_it_currently_points_at() {
        // Binding by name is what lets a machine whose address is reissued -
        // every overlay network does this - keep working without editing
        // anything.
        let addresses = resolve_hosts(&["localhost".into()], 8737).expect("resolve");
        assert!(!addresses.is_empty());
        assert!(addresses.iter().all(|address| address.ip().is_loopback()));
        assert!(addresses.iter().all(|address| address.port() == 8737));
    }

    #[test]
    fn the_same_address_twice_is_bound_once() {
        // A name and a literal often name the same place, and the second bind
        // would fail for no reason worth reporting.
        let addresses =
            resolve_hosts(&["127.0.0.1".into(), "127.0.0.1".into()], 8737).expect("resolve");
        assert_eq!(addresses.len(), 1);
    }

    #[test]
    fn an_unresolvable_name_says_so() {
        let err =
            resolve_hosts(&["no-such-host.invalid".into()], 8737).expect_err("must not resolve");
        assert!(err.contains("no-such-host.invalid"), "{err}");
    }

    #[test]
    fn the_refusal_names_the_exposed_address_and_offers_a_key() {
        // RFC 5737 documentation address: nothing here belongs to a real
        // network.
        let exposed = IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 10));
        let message = exposed_without_key(&[IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), exposed]);
        assert!(message.contains("192.0.2.10"), "{message}");
        assert!(!message.contains("127.0.0.1"), "{message}");
        assert!(message.contains(API_KEY_ENV), "{message}");
    }
}
