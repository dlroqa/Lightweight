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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hermes_backend_llamacpp::backend::ProcessBackend;
use hermes_catalog::CatalogStore;
use hermes_catalog::install::Installer;
use hermes_core::{Actionable, GgmlType, ModelId, RuntimeParams, units::Bytes};
use hermes_gateway::catalog::ResidentModel;
use hermes_gateway::manager::{ModelManager, RuntimeDefaults};
use hermes_gateway::scheduler::{BandLimits, SchedulerConfig};
use hermes_gateway::{AuthPolicy, GatewayConfig, GatewayState};
use hermes_gguf::{GgufFile, ModelMetadata};
use hermes_inference::{InferenceBackend, LoadProgress, LoadRequest};
use hermes_memory::{Estimator, Verdict};
use hermes_system_info::{CpuInfo, DataPaths, MemoryProbe as _, SystemMemoryProbe};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Everything `serve` needs.
pub struct ServeOptions {
    /// The model to load at startup.
    ///
    /// Optional since M6: a gateway can start with nothing loaded and be told
    /// what to load over the control API. `serve <model.gguf>` behaves exactly
    /// as it always has.
    pub model: Option<PathBuf>,
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
    /// Requests the gateway may run at once.
    ///
    /// One number, not two: it sizes the engine's slots *and* the gateway's
    /// queue, and the RAM estimate is computed for the same value — the KV
    /// cache is per sequence, so four concurrent sequences cost four caches.
    /// Splitting it into separate settings would let a machine be configured to
    /// promise more concurrency than it budgeted for.
    pub concurrency: u32,
    /// Directory of built control-panel files to serve at `/`.
    ///
    /// Absent means no panel, which is every deployment before M6b.3 exists.
    /// Serving it from here rather than from a second web server is what keeps
    /// the panel same-origin with the API, so no CORS policy has to be written
    /// to let a page talk to the gateway it was served by.
    pub web_root: Option<PathBuf>,
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
    let resolved = resolve_each(&options.hosts, options.port)?;
    let addresses = bind_addresses(&resolved);
    let key = options.api_key.clone().or_else(read_key_from_environment);
    let bind_ips: Vec<IpAddr> = addresses.iter().map(SocketAddr::ip).collect();
    let auth = AuthPolicy::for_binds(&bind_ips, key).map_err(|_| exposed_without_key(&bind_ips))?;

    let mut listeners = Vec::with_capacity(addresses.len());
    for address in &addresses {
        listeners.push(bind(*address).await?);
    }

    // Asked of the sockets rather than taken from `addresses`: a request for
    // port 0 is answered by the kernel with a real port, and that is the port
    // the operator has to type. Read once, here, and used both in the summary
    // below and by the control API - two readings could disagree.
    let mut bound = Vec::with_capacity(listeners.len());
    for listener in &listeners {
        bound.push(
            listener
                .local_addr()
                .map_err(|err| format!("could not read the bound address: {err}"))?,
        );
    }

    // The engine is created whether or not a model is loaded now: it is what a
    // later `/api/v1/models/{id}/load` will load into.
    let backend = ProcessBackend::new(paths.runtime_dir()).map_err(describe)?;
    let cpu = CpuInfo::detect();
    let concurrency = options.concurrency.max(1);
    let cache_type: GgmlType = options
        .kv_type
        .parse()
        .map_err(|_| format!("unknown KV cache type {:?}", options.kv_type))?;

    let loaded = match &options.model {
        Some(model) => Some(
            load_at_startup(
                &options,
                model,
                &backend,
                LoadShape {
                    cache_type,
                    concurrency,
                    cpu,
                },
            )
            .await?,
        ),
        None => {
            println!("no model loaded — use `hermes models list` and the control API to load one");
            None
        }
    };

    let backend = Arc::new(backend);
    // Ceilings from the context this model was actually loaded with, which was
    // itself chosen from this machine's free memory. A constant here would
    // describe whichever machine it was written on. With nothing loaded the
    // defaults stand until the first load re-derives them.
    let bands = loaded.as_ref().map_or_else(BandLimits::default, |model| {
        BandLimits::for_context(model.n_ctx)
    });
    let catalog = hermes_gateway::catalog::shared(loaded.clone());

    // The catalog is opened whether or not a model was named, so
    // `/api/v1/models` can answer and a model can be loaded later.
    let manager = build_manager(&paths, cache_type, options.threads, concurrency)?;

    let state = Arc::new(
        GatewayState::new(
            Arc::clone(&backend) as Arc<dyn InferenceBackend>,
            catalog,
            GatewayConfig {
                auth,
                max_concurrent_requests: concurrency,
                scheduler: SchedulerConfig {
                    interactive: bands,
                    ..SchedulerConfig::default()
                },
                // The control API describes the machine and the service, so it
                // needs both: where this process may write, and where it is
                // answering. Neither is discoverable from inside a handler.
                paths: Some(paths.clone()),
                bound_addresses: bound.clone(),
                web_root: options.web_root.clone(),
                ..GatewayConfig::default()
            },
        )
        .with_manager(Arc::clone(&manager)),
    );

    // Registering the model that was named on the command line happens after
    // the gateway is built, not before it serves: hashing a multi-gigabyte file
    // takes seconds, and none of them should be seconds where the gateway is
    // not yet answering. A failure here costs a catalog entry, never the
    // service.
    if let Some(model) = options.model.clone() {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            if let Err(err) = manager.register_at_startup(model).await {
                tracing::warn!(
                    target: hermes_observability::targets::MODEL,
                    error = %err,
                    "the model served from the command line could not be added to the catalog"
                );
            }
        });
    }

    println!();
    for address in &bound {
        // Printed for the operator, deliberately not logged: which addresses
        // this machine holds is not something the log file needs to remember.
        println!("serving  http://{address}/v1");
    }
    match &loaded {
        Some(model) => {
            println!("  model    {}", model.id);
            println!("  context  {} tokens", model.n_ctx);
        }
        None => {
            println!("  model    none loaded");
            println!("  load one POST /api/v1/models/<id>/load");
        }
    }
    println!(
        "  requests {concurrency} at a time{}",
        if concurrency == 1 {
            " (others queue; short requests go first)"
        } else {
            ""
        }
    );
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

    // On stderr, and after the summary rather than before the model load, for
    // one reason: this is the last thing the operator sees before the gateway
    // goes quiet, and a warning that scrolls past during a thirty-second load
    // is a warning nobody reads.
    if let Some(advice) = unreachable_bind_advice(&resolved) {
        eprintln!("\n{advice}");
    }
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

/// The machine-shaped parameters a startup load needs.
#[derive(Clone, Debug)]
struct LoadShape {
    cache_type: GgmlType,
    concurrency: u32,
    cpu: CpuInfo,
}

/// Load the model named on the command line.
///
/// This is the M0-M5 startup path, unchanged in what it does: read the header,
/// size the context to the machine, admit against free memory, load, and report
/// what it cost. It is a function now only so that `serve` can skip it when no
/// model was named.
async fn load_at_startup(
    options: &ServeOptions,
    model_path: &Path,
    backend: &ProcessBackend,
    shape: LoadShape,
) -> Result<ResidentModel, String> {
    let file = GgufFile::open(model_path).map_err(describe)?;
    let metadata = ModelMetadata::from_file(&file).map_err(describe)?;

    let base = RuntimeParams {
        cache_type_k: shape.cache_type,
        cache_type_v: shape.cache_type,
        threads: Some(
            options
                .threads
                .unwrap_or_else(|| shape.cpu.default_threads()),
        ),
        // The engine is told the same number the gateway will hand out, so the
        // estimate, the KV cache and the queue all describe one machine.
        n_parallel: shape.concurrency,
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
            model_path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("model"),
            params.n_ctx,
        ),
        gguf_path: model_path.to_path_buf(),
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
        shape.cpu.expected_ggml_variant(),
        loaded.effective.threads.unwrap_or(0)
    );

    if let Ok(Some(usage)) = backend.resource_usage().await {
        println!("  engine resident {}  peak {}", usage.rss, usage.peak_rss);
        report_estimate_accuracy(estimate.total, usage.peak_rss);
    }

    Ok(ResidentModel {
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
        backend: Some(hermes_backend_llamacpp::backend::BACKEND_ID.to_string()),
        model_path: model_path.display().to_string(),
    })
}

/// Open the catalog and the installer for this profile.
fn build_manager(
    paths: &DataPaths,
    kv_type: GgmlType,
    threads: Option<u32>,
    concurrency: u32,
) -> Result<Arc<ModelManager>, String> {
    let store = CatalogStore::open(paths.catalog_file()).map_err(describe)?;
    let installer = Installer::new(paths.models_dir(), paths.downloads_dir()).map_err(describe)?;
    Ok(Arc::new(ModelManager::new(
        store,
        installer,
        RuntimeDefaults {
            kv_type,
            threads,
            concurrency,
        },
    )))
}

/// Render an error with its remedies, the way every command reports one.
///
/// `pub(crate)` so `hermes models` reports catalog errors identically; the
/// body is unchanged.
pub(crate) fn describe<E: Actionable>(err: E) -> String {
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
///
/// Kept as the composition `run` itself performs, so the tests that have
/// covered host resolution since M3.5 keep asserting exactly what they always
/// asserted. If this ever stops matching what `run` does, the tests below stop
/// being evidence about the real path — so it composes the same two functions
/// and nothing else.
#[cfg(test)]
fn resolve_hosts(hosts: &[String], port: u16) -> Result<Vec<SocketAddr>, String> {
    Ok(bind_addresses(&resolve_each(hosts, port)?))
}

/// One `--host` value and what it turned out to mean.
///
/// The addresses alone are not enough to tell a deliberate loopback bind from
/// an accidental one, and that distinction is the whole point: `--host
/// localhost` and `--host "$(hostname)"` can produce byte-identical addresses
/// while meaning opposite things. Keeping what the operator typed, and whether
/// it was a name or a literal, is what makes the difference recoverable.
#[derive(Clone, Debug)]
struct ResolvedHost {
    /// Exactly what was typed, for saying it back to them.
    requested: String,
    /// A literal address rather than a name. A literal is never a surprise:
    /// whatever it resolves to is what it says.
    was_literal: bool,
    addresses: Vec<SocketAddr>,
}

impl ResolvedHost {
    /// Whether this value could plausibly have been meant to reach elsewhere.
    ///
    /// A literal address is what it is. `localhost` — and, per RFC 6761, any
    /// name under `.localhost` — is an explicit request for loopback. Every
    /// other name is a machine's name, and naming a machine is how an operator
    /// asks for the machine, not for this process.
    fn was_meant_to_reach_elsewhere(&self) -> bool {
        if self.was_literal {
            return false;
        }
        let name = self
            .requested
            .trim()
            .trim_end_matches('.')
            .to_ascii_lowercase();
        name != "localhost" && !name.ends_with(".localhost")
    }

    fn is_loopback_only(&self) -> bool {
        self.addresses
            .iter()
            .all(|address| address.ip().is_loopback())
    }
}

/// Flatten resolved hosts into the addresses to bind, in the order given.
///
/// A name and a literal can name the same place; binding it twice would fail on
/// the second attempt for no good reason.
fn bind_addresses(resolved: &[ResolvedHost]) -> Vec<SocketAddr> {
    let mut addresses: Vec<SocketAddr> = Vec::new();
    for host in resolved {
        for address in &host.addresses {
            if !addresses.contains(address) {
                addresses.push(*address);
            }
        }
    }
    addresses
}

/// Resolve each `--host` value, keeping what it was asked to be.
fn resolve_each(hosts: &[String], port: u16) -> Result<Vec<ResolvedHost>, String> {
    if hosts.is_empty() {
        return Ok(vec![ResolvedHost {
            requested: "127.0.0.1".to_owned(),
            was_literal: true,
            addresses: vec![SocketAddr::new(
                IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                port,
            )],
        }]);
    }

    let mut resolved: Vec<ResolvedHost> = Vec::new();
    for host in hosts {
        let host = host.trim();
        if host.is_empty() {
            continue;
        }
        let (was_literal, addresses): (bool, Vec<SocketAddr>) = match host.parse::<IpAddr>() {
            Ok(address) => (true, vec![SocketAddr::new(address, port)]),
            Err(_) => (
                false,
                (host, port)
                    .to_socket_addrs()
                    .map_err(|err| {
                        format!(
                            "could not resolve {host:?} to an address to bind: {err}. \
                             Pass an address this machine holds, or a name that resolves to one."
                        )
                    })?
                    .collect(),
            ),
        };
        if addresses.is_empty() {
            return Err(format!("{host:?} resolved to no addresses"));
        }
        resolved.push(ResolvedHost {
            requested: host.to_owned(),
            was_literal,
            addresses,
        });
    }

    if resolved.is_empty() {
        return Err("no addresses to bind".to_owned());
    }
    Ok(resolved)
}

/// Explain a bind that will reach nobody, when reaching somebody was the point.
///
/// The failure this catches is the quietest one in the whole remote path. An
/// operator who wants their other devices to use the gateway types the
/// machine's name, and on most Linux installs `/etc/hosts` maps that name to a
/// loopback address — Debian and Ubuntu write `127.0.1.1 <hostname>` at
/// install time, and that entry wins over whatever a LAN or an overlay network
/// publishes for the same name. Every visible signal then says success: the
/// name resolved, the bind succeeded, the gateway prints that it is serving.
/// Authentication is silently *off*, because the bind really is local. The only
/// symptom is that the other machine cannot connect, which looks like a
/// firewall, a port, or a broken client — anything but the name.
///
/// Returning advice rather than an error is deliberate. A name that resolves to
/// loopback is unusual but not invalid, and refusing to serve would break a
/// working configuration to make a point. Being impossible to miss is enough.
fn unreachable_bind_advice(resolved: &[ResolvedHost]) -> Option<String> {
    // Only when nothing at all is exposed. One reachable bind means the
    // operator got what they asked for, whatever else is in the list.
    if !resolved.iter().all(ResolvedHost::is_loopback_only) {
        return None;
    }
    let surprising: Vec<&ResolvedHost> = resolved
        .iter()
        .filter(|host| host.was_meant_to_reach_elsewhere())
        .collect();
    if surprising.is_empty() {
        return None;
    }

    // One line per name that disappointed, each aligned under the first so a
    // machine with two collapsed names reads as one warning rather than two.
    let lines: Vec<String> = surprising
        .iter()
        .map(|host| {
            let addresses: Vec<String> = host
                .addresses
                .iter()
                .map(|address| address.ip().to_string())
                .collect();
            format!(
                "--host {:?} resolved only to {}, which is loopback.",
                host.requested,
                addresses.join(", ")
            )
        })
        .collect();

    let mut message = format!("warning: {}", lines.join("\n         "));
    message.push_str(
        "\n  Only this machine can reach the gateway, and authentication stays off \
         because\n  nothing is exposed. Many Linux installs map the hostname to a loopback \
         address\n  in /etc/hosts, and that entry wins over any name the network publishes \
         for it.\n",
    );
    message.push_str(&describe_reachable_addresses());
    Some(message)
}

/// The "bind one of these instead" half of the advice.
///
/// Split out because its three outcomes are genuinely different answers, and
/// collapsing them would say something untrue in two of the three cases:
/// addresses to offer, no addresses to offer, and no way to look.
fn describe_reachable_addresses() -> String {
    match hermes_system_info::reachable_addresses() {
        Ok(addresses) if !addresses.is_empty() => {
            let mut text = String::from("\n  Addresses another machine could reach this one at:\n");
            for address in &addresses {
                // Formatted as the flag to pass, because that is what the
                // operator is about to do with it. An IPv6 literal needs no
                // brackets here - `--host` takes an address, not a URL.
                text.push_str(&format!("    --host {address}\n"));
            }
            text.push_str(
                "\n  Bind one of those, or a name that resolves to one, \
                 and set HERMES_API_KEY.\n",
            );
            text
        }
        Ok(_) => String::from(
            "\n  This machine currently holds no address another machine could reach.\n  \
             Check that the network interface, or the overlay network, is up.\n",
        ),
        Err(_) => String::from(
            "\n  List this machine's addresses and bind one of them: `ip -brief addr` on\n  \
             Linux, `ifconfig` on macOS, `ipconfig` on Windows.\n",
        ),
    }
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

    /// Build a resolved host the way `resolve_each` would, without touching DNS.
    fn resolved(requested: &str, was_literal: bool, addresses: &[&str]) -> ResolvedHost {
        ResolvedHost {
            requested: requested.to_owned(),
            was_literal,
            addresses: addresses
                .iter()
                .map(|address| SocketAddr::new(address.parse().expect("test address"), 8737))
                .collect(),
        }
    }

    #[test]
    fn a_machine_name_that_collapsed_to_loopback_is_reported() {
        // The failure this whole diagnostic exists for. Debian and Ubuntu write
        // `127.0.1.1 <hostname>` into /etc/hosts at install time, so the
        // obvious way to ask for remote access - naming the machine - binds
        // loopback, serves nobody, and looks like success.
        let advice = unreachable_bind_advice(&[resolved("some-host", false, &["127.0.1.1"])])
            .expect("a name that reached nobody must be reported");
        assert!(advice.contains("some-host"), "{advice}");
        assert!(advice.contains("127.0.1.1"), "{advice}");
        assert!(advice.contains("loopback"), "{advice}");
        assert!(
            advice.contains("/etc/hosts"),
            "the advice must name the cause, not just the symptom: {advice}"
        );
    }

    #[test]
    fn an_explicit_loopback_request_is_not_second_guessed() {
        // `--host localhost` and the 127.0.0.1 default are what someone asks
        // for when they want a local service. Warning about them would train
        // the operator to ignore the warning that matters.
        assert_eq!(
            unreachable_bind_advice(&[resolved("localhost", false, &["127.0.0.1", "::1"])]),
            None
        );
        assert_eq!(
            unreachable_bind_advice(&[resolved("127.0.0.1", true, &["127.0.0.1"])]),
            None
        );
        assert_eq!(
            unreachable_bind_advice(&[resolved("::1", true, &["::1"])]),
            None
        );
        // RFC 6761 reserves the whole .localhost tree for loopback.
        assert_eq!(
            unreachable_bind_advice(&[resolved("gateway.localhost", false, &["127.0.0.1"])]),
            None
        );
        assert_eq!(
            unreachable_bind_advice(&resolve_each(&[], 8737).expect("default")),
            None
        );
    }

    #[test]
    fn nothing_is_said_when_any_bind_is_actually_reachable() {
        // One exposed bind means the operator got what they asked for. The
        // loopback bind alongside it is a convenience, not a mistake.
        // RFC 5737 documentation address.
        let mixed = [
            resolved("some-host", false, &["192.0.2.10"]),
            resolved("localhost", false, &["127.0.0.1"]),
        ];
        assert_eq!(unreachable_bind_advice(&mixed), None);

        // Even when the name that collapsed is in the list, alongside a literal
        // that did not: something is exposed, so nothing is broken.
        let partial = [
            resolved("some-host", false, &["127.0.1.1"]),
            resolved("192.0.2.10", true, &["192.0.2.10"]),
        ];
        assert_eq!(unreachable_bind_advice(&partial), None);
    }

    #[test]
    fn the_advice_offers_something_to_do_next() {
        // Whatever this machine holds is unknown to the test, so the assertion
        // is that one of the three honest answers was given - never silence.
        let advice = unreachable_bind_advice(&[resolved("some-host", false, &["127.0.1.1"])])
            .expect("reported");
        assert!(
            advice.contains("--host ")
                || advice.contains("no address another machine could reach")
                || advice.contains("ip -brief addr"),
            "the warning must say what to do next: {advice}"
        );
    }

    #[test]
    fn resolution_records_whether_a_literal_or_a_name_was_given() {
        // The distinction the diagnostic rests on. A literal is never a
        // surprise; a name can be.
        let resolved =
            resolve_each(&["127.0.0.1".into(), "localhost".into()], 8737).expect("resolve");
        assert!(resolved[0].was_literal);
        assert!(!resolved[0].was_meant_to_reach_elsewhere());
        assert!(!resolved[1].was_literal);
        assert!(
            !resolved[1].was_meant_to_reach_elsewhere(),
            "localhost is an explicit request for loopback"
        );
        assert!(resolved.iter().all(ResolvedHost::is_loopback_only));
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
