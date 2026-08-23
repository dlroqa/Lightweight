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

use std::net::{IpAddr, SocketAddr};
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
    /// Address to bind the gateway to.
    pub host: IpAddr,
    /// Port to bind. `0` picks a free one.
    pub port: u16,
    /// Key required on every request. Mandatory for a non-loopback bind.
    pub api_key: Option<String>,
}

/// Run the command. Returns once the engine has been shut down.
pub async fn run(options: ServeOptions) -> Result<(), String> {
    let paths = DataPaths::discover().map_err(describe)?;
    paths.create_all().map_err(describe)?;

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

    // The auth policy is decided by where we are binding, not by preference.
    // Loopback keeps whatever the user configured, including nothing; anything
    // else demands a key rather than being silently exposed.
    let auth = AuthPolicy::for_bind(options.host, options.api_key.clone()).map_err(|_| {
        format!(
            "binding to {} would expose this gateway to the network; pass --api-key to allow it",
            options.host
        )
    })?;

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

    let listener = tokio::net::TcpListener::bind(SocketAddr::new(options.host, options.port))
        .await
        .map_err(|err| format!("could not bind {}:{}: {err}", options.host, options.port))?;
    let bound = listener
        .local_addr()
        .map_err(|err| format!("could not read the bound address: {err}"))?;

    println!("\nserving  http://{bound}/v1");
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
    println!("\nPress Ctrl-C to stop.");

    // Two ways to stop, and both must leave nothing behind: the user asking,
    // and the engine dying under us. The second is why the engine is a child
    // process at all - it is an event we can observe and report rather than
    // something that takes this process with it.
    let shutdown_state = Arc::clone(&state);
    let death_watch = Arc::clone(&backend);
    let shutdown = async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => println!("\nstopping"),
            () = watch_for_death(&death_watch) => {}
        }
        shutdown_state.shutdown();
    };

    axum::serve(listener, hermes_gateway::app(Arc::clone(&state)))
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|err| format!("the gateway stopped: {err}"))?;

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
