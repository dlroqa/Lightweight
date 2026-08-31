//! Command-line access to the gateway's model and system facts.
//!
//! Everything here is a thin wrapper over the same library calls the daemon and
//! the desktop UI use. That is deliberate: it keeps the crates honest about
//! being usable without a running server, and it means a user can diagnose "why
//! will this model not load" without launching anything.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::path::PathBuf;
use std::process::ExitCode;

use std::fmt::Write as _;
use std::io::Write as _;

use clap::{Parser, Subcommand};

mod bench;
mod models;
mod serve;
use lightweight_core::{Actionable, GgmlType, units::Bytes};
use lightweight_gguf::{GgufFile, ModelMetadata};
use lightweight_memory::{Estimator, RuntimeParams, Verdict};
use lightweight_system_info::{
    CpuInfo, MemoryProbe, SystemMemoryProbe, classified_addresses, reachable_addresses,
};

#[derive(Parser)]
#[command(
    name = "hermes",
    about = "Hermes CPU Inference Gateway",
    version,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Emit JSON instead of a human-readable report.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Report CPU topology, instruction sets and memory.
    Sysinfo,
    /// Read a GGUF file's metadata without loading its weights.
    Inspect {
        model: PathBuf,
        /// Accept a file whose tensor data is absent, such as a partial
        /// download or a captured header.
        #[arg(long)]
        header_only: bool,
    },
    /// Load a model and serve the OpenAI-compatible API.
    ///
    /// Acquires the engine, admits the model against this machine's free
    /// memory, loads it, and serves `/v1/chat/completions`, `/v1/models`,
    /// `/health` and `/props` until interrupted.
    Serve {
        /// The model to load at startup.
        ///
        /// Optional: with no model the gateway starts empty and waits to be
        /// told what to load over the control API, which is what the desktop
        /// shell drives.
        model: Option<PathBuf>,
        /// Context length in tokens.
        ///
        /// Omitted, the largest size that still loads safely on this machine is
        /// chosen, bounded by what the model supports. That is what lets the
        /// same build use an 8K context on a small laptop and 128K on a
        /// workstation, instead of a constant that is wrong for both.
        #[arg(long)]
        ctx: Option<u32>,
        /// Inference threads. Defaults to the physical core count.
        #[arg(long)]
        threads: Option<u32>,
        #[arg(long, default_value = "f16")]
        kv_type: String,
        /// Physical batch size.
        ///
        /// Larger raises prompt-processing throughput and the compute buffers
        /// with it; the estimate prices both. Absent leaves the engine's
        /// default, which is what every deployment has been running.
        #[arg(long)]
        ubatch: Option<u32>,
        /// Threads for prompt processing. Defaults to `--threads`.
        ///
        /// Separate because the two phases are different work: prefill is
        /// compute-bound and decode is memory-bound. Which way that cuts is a
        /// property of the machine, so there is no default derived here.
        #[arg(long)]
        threads_batch: Option<u32>,
        /// How the weights are brought into memory: auto, none, mmap, mlock or
        /// mmap+mlock.
        ///
        /// A locking mode pins the weights against swap, and is checked against
        /// this user's locked-memory allowance before the engine is launched.
        #[arg(long)]
        load_mode: Option<String>,
        /// Load even if the RAM estimate says it will not fit.
        #[arg(long)]
        force: bool,
        /// Address or hostname to bind. Repeatable.
        ///
        /// Loopback by default: the gateway is a local service, and section 23
        /// makes reaching it from another machine a deliberate act rather than
        /// something a user can end up with by accident.
        ///
        /// A name is resolved at startup, which is usually the better choice on
        /// a machine whose address can be reissued. Repeat the flag to serve
        /// several addresses — a LAN address and an overlay address, say — from
        /// the one engine.
        #[arg(long, default_value = "127.0.0.1")]
        host: Vec<String>,
        /// Port to bind. `0` picks a free one and prints it.
        #[arg(long, default_value_t = lightweight_gateway::DEFAULT_PORT)]
        port: u16,
        /// Requests to run at once, or `auto`.
        ///
        /// `auto` derives it from this machine and says where the number came
        /// from: one slot per four cores, because a single generation was
        /// measured to keep close to four busy, and fewer than that if a
        /// full-sized window for every client would not fit in memory. On a
        /// small CPU that is one, which is what this has always defaulted to;
        /// on a machine with cores and memory to spare it is more, without
        /// anybody having to know to ask.
        ///
        /// A number overrides it and is honoured exactly. The engine is given
        /// the same number of slots and the RAM estimate is computed for it,
        /// so the answer to "does this fit?" stays honest either way.
        #[arg(long, default_value = "auto")]
        concurrency: serve::Concurrency,
        /// Serve the control panel's built files at `/`.
        ///
        /// The directory a `vite build` produced. Serving it from the gateway
        /// keeps the page and the API on one origin, so no cross-origin policy
        /// has to be written to let the panel talk to the gateway that served
        /// it. Without this, `/` is a 404 and the API is unchanged.
        #[arg(long, value_name = "DIR")]
        web_root: Option<PathBuf>,
        /// Require this key on every request.
        ///
        /// Optional on loopback and mandatory as soon as any bind is reachable
        /// from another machine; such a bind without a key is refused rather
        /// than silently exposed.
        ///
        /// Prefer `HERMES_API_KEY`: an argument is visible in `ps`, readable
        /// from `/proc`, and kept in shell history.
        #[arg(long)]
        api_key: Option<String>,
    },
    /// Estimate what a model would cost to load, and whether it fits.
    Estimate {
        model: PathBuf,
        /// Context length in tokens.
        #[arg(long, default_value_t = 4096)]
        ctx: u32,
        /// KV cache element type, for example `f16` or `q8_0`.
        #[arg(long, default_value = "f16")]
        kv_type: String,
        /// Physical batch size. Compute buffers scale with this.
        #[arg(long, default_value_t = 512)]
        ubatch: u32,
        /// Concurrent sequences.
        #[arg(long, default_value_t = 1)]
        parallel: u32,
        /// Budget for the headless daemon rather than the desktop app.
        #[arg(long)]
        headless: bool,
        #[arg(long)]
        header_only: bool,
    },
    /// Measure what this machine does with a model.
    ///
    /// Brings its own engine and reloads between buckets, so it never disturbs
    /// a gateway that is serving. Results are saved under the data directory,
    /// and they are facts about this machine and this engine build rather than
    /// about the software.
    Bench {
        model: PathBuf,
        /// Context to load with. Sized to the machine when absent.
        #[arg(long)]
        ctx: Option<u32>,
        #[arg(long)]
        threads: Option<u32>,
        #[arg(long, default_value = "f16")]
        kv_type: String,
        /// Prompt length to measure prefill against.
        #[arg(long, default_value_t = 512)]
        prompt_tokens: u32,
        /// Output budget to measure decode against.
        #[arg(long, default_value_t = 32)]
        generate_tokens: u32,
        /// How many times to repeat each scenario.
        #[arg(long, default_value_t = 3)]
        repeat: u32,
        /// Physical batch sizes to sweep, reloading the engine for each.
        ///
        /// Two or more values are what make a calibration fit possible: the
        /// compute term scales with this and the overhead term does not, so a
        /// single value cannot separate them.
        #[arg(long, value_delimiter = ',', default_values_t = [512_u32])]
        ubatch: Vec<u32>,
        /// Slot counts to sweep, reloading the engine for each.
        ///
        /// Above one, the run adds a scenario that drives that many clients at
        /// the same moment - which is the only way to see whether the engine
        /// batches them or serves them in turn. Each client is given the whole
        /// `--ctx`, so the memory cost rises with this.
        #[arg(long, value_delimiter = ',', default_values_t = [1_u32])]
        parallel: Vec<u32>,
        /// Fit the measured residuals into `calibration.json`.
        ///
        /// Every load path reads that file and spends a fit that passes the
        /// trust rules; one that does not is ignored and the shipped defaults
        /// stand. Sweep at least three `--ubatch` values, or the fit will not
        /// have enough points to be believed.
        #[arg(long)]
        fit: bool,
    },
    /// Manage the models this machine has.
    ///
    /// The catalog is a file, not a service, so every one of these works with
    /// no gateway running.
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Manage the API keys remote agents authenticate with.
    ///
    /// Keys are stored hashed: one is shown once, when it is created, and never
    /// again. A key that is lost is replaced, not recovered.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// Show the persisted gateway configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum KeyAction {
    /// Mint a new key and print it once.
    Create {
        /// A label for the key, e.g. the agent it is for.
        #[arg(long)]
        name: Option<String>,
        /// Cap requests per minute for this key.
        #[arg(long)]
        per_minute: Option<u32>,
        /// Cap requests per day for this key.
        #[arg(long)]
        per_day: Option<u32>,
    },
    /// List the keys, by prefix. Secrets are never shown.
    List,
    /// Revoke a key by its id (from `key list`).
    Revoke { id: String },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the persisted bind configuration.
    Show,
}

#[derive(Subcommand)]
enum ModelsAction {
    /// List installed models.
    List,
    /// List the models this build is known to run and can download.
    Available,
    /// Register a .gguf that is already on this machine.
    ///
    /// The file is referenced where it is, never copied, so importing a 4 GB
    /// model costs no extra disk.
    Import { path: PathBuf },
    /// Download a model.
    ///
    /// Either one of the pinned ids from `hermes models available`, whose
    /// digest is recorded in this build, or any direct https link with
    /// `--url`. A HuggingFace link is verified against the digest the site
    /// publishes; any other link is recorded rather than verified unless you
    /// pass `--sha256`.
    Add {
        /// A pinned model id.
        id: Option<String>,
        /// A direct https link to a .gguf.
        #[arg(long)]
        url: Option<String>,
        /// Expected sha256, for a link that is not on HuggingFace.
        #[arg(long)]
        sha256: Option<String>,
    },
    /// Remove a model from the catalog.
    Remove {
        id: String,
        /// Also delete the file.
        ///
        /// Only ever applies to a model this program downloaded. An imported
        /// file belongs to you and is left alone.
        #[arg(long)]
        delete: bool,
    },
}

/// `hermes models ...`.
///
/// The read-only actions need no async runtime; import and add do, because they
/// hash and download. Built here rather than around every command, the same way
/// `serve` does it.
macro_rules! line {
    ($out:expr) => {{
        // Writing to a `String` is infallible; the result exists only to
        // satisfy the `fmt::Write` signature.
        let _ = writeln!($out);
    }};
    ($out:expr, $($arg:tt)*) => {{
        let _ = writeln!($out, $($arg)*);
    }};
}

fn models_command(cli: &Cli, out: &mut String, action: &ModelsAction) -> Result<ExitCode, String> {
    let paths = lightweight_system_info::DataPaths::discover().map_err(serve::describe)?;
    let mut store = models::open(&paths)?;

    match action {
        ModelsAction::List => {
            if cli.json {
                render_json(out, &store.models().collect::<Vec<_>>());
            } else {
                models::list(out, &store);
            }
        }
        ModelsAction::Available => {
            if cli.json {
                render_json(out, &lightweight_catalog::manifest::MODELS);
            } else {
                models::available(out, &store);
            }
        }
        ModelsAction::Import { path } => {
            runtime()?.block_on(models::import(out, &paths, &mut store, path))?;
        }
        ModelsAction::Add { id, url, sha256 } => {
            let request = models::add_request(id.as_deref(), url.as_deref(), sha256.as_deref())?;
            runtime()?.block_on(models::add(out, &paths, &mut store, &request))?;
        }
        ModelsAction::Remove { id, delete } => {
            models::remove(out, &mut store, id, *delete)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn key_command(cli: &Cli, out: &mut String, action: &KeyAction) -> Result<ExitCode, String> {
    let paths = lightweight_system_info::DataPaths::discover().map_err(serve::describe)?;
    let store = lightweight_store::ApiKeyStore::new(paths.api_keys_file());

    match action {
        KeyAction::Create {
            name,
            per_minute,
            per_day,
        } => {
            let limit = lightweight_store::RateLimit {
                per_minute: *per_minute,
                per_day: *per_day,
            };
            let (record, full) = store
                .create(name.as_deref().unwrap_or(""), limit)
                .map_err(serve::describe)?;
            if cli.json {
                render_json(
                    out,
                    &serde_json::json!({
                        "id": record.id,
                        "name": record.name,
                        "prefix": record.prefix,
                        "created_at": record.created_at,
                        "key": full,
                    }),
                );
            } else {
                line!(out, "{full}");
                line!(out);
                line!(
                    out,
                    "This is the only time the key is shown. Copy it now — it"
                );
                line!(
                    out,
                    "is stored hashed and cannot be recovered, only replaced."
                );
                line!(out);
                line!(out, "  id     {}", record.id);
                if !record.name.is_empty() {
                    line!(out, "  name   {}", record.name);
                }
                line!(
                    out,
                    "Set HERMES_API_KEY on the agent, or pass it as the OpenAI api_key."
                );
            }
        }
        KeyAction::List => {
            let keys = store.list().map_err(serve::describe)?;
            if cli.json {
                render_json(out, &keys);
            } else if keys.is_empty() {
                line!(
                    out,
                    "No API keys. Create one with `hermes key create --name <label>`."
                );
            } else {
                for record in &keys {
                    let limit = describe_limit(record.limit);
                    let label = if record.name.is_empty() {
                        "(unnamed)"
                    } else {
                        &record.name
                    };
                    line!(out, "{}  {}  {label}  {limit}", record.id, record.prefix);
                }
            }
        }
        KeyAction::Revoke { id } => {
            let removed = store.revoke(id).map_err(serve::describe)?;
            if cli.json {
                render_json(out, &serde_json::json!({ "revoked": removed, "id": id }));
            } else if removed {
                line!(
                    out,
                    "Revoked {id}. A gateway already running keeps it until restarted."
                );
            } else {
                line!(out, "No key with id {id}.");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn describe_limit(limit: lightweight_store::RateLimit) -> String {
    match (limit.per_minute, limit.per_day) {
        (None, None) => "unlimited".to_owned(),
        (Some(m), None) => format!("{m}/min"),
        (None, Some(d)) => format!("{d}/day"),
        (Some(m), Some(d)) => format!("{m}/min, {d}/day"),
    }
}

fn config_command(cli: &Cli, out: &mut String, action: &ConfigAction) -> Result<ExitCode, String> {
    let paths = lightweight_system_info::DataPaths::discover().map_err(serve::describe)?;
    let store = lightweight_store::ApiConfigStore::new(paths.api_config_file());
    let config = store.load().map_err(serve::describe)?;

    match action {
        ConfigAction::Show => {
            if cli.json {
                render_json(out, &config);
            } else {
                line!(out, "config file  {}", store.path().display());
                if config.hosts.is_empty() {
                    line!(
                        out,
                        "hosts        (none set — binds loopback unless --host is given)"
                    );
                } else {
                    line!(out, "hosts        {}", config.hosts.join(", "));
                }
                match config.port {
                    Some(port) => line!(out, "port         {port}"),
                    None => line!(out, "port         (none set — default applies)"),
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// A multi-threaded runtime for the commands that do I/O.
fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("could not start the async runtime: {err}"))
}

/// Appends a formatted line to the output buffer.
///
/// Reports are rendered into a `String` and written once, rather than printed a
/// line at a time. Two reasons: `println!` panics if the reader has closed the
/// pipe - `hermes sysinfo | head` would crash, because Rust ignores SIGPIPE at
/// startup and turns it into a write error - and a rendered `String` is
/// something tests can assert against.
fn main() -> ExitCode {
    // `get_matches` rather than `parse` so the raw `ArgMatches` is in hand: it
    // is the only thing that can tell a flag the user typed from a default clap
    // supplied, which is what lets `config/api.json` sit *under* the flags
    // without changing a byte of `--help`. `FromArgMatches` then produces the
    // same `Cli` `parse` would have.
    let matches = <Cli as clap::CommandFactory>::command().get_matches();
    let cli =
        <Cli as clap::FromArgMatches>::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());
    let mut out = String::new();

    let outcome = run(&cli, &matches, &mut out);

    // A closed pipe is not an error: it is what `| head` does. Exit as the
    // default SIGPIPE disposition would, rather than panicking or reporting a
    // failure the user did not cause.
    if let Err(err) = std::io::stdout().write_all(out.as_bytes())
        && err.kind() == std::io::ErrorKind::BrokenPipe
    {
        return ExitCode::from(141);
    }
    let _ = std::io::stdout().flush();

    match outcome {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli, matches: &clap::ArgMatches, out: &mut String) -> Result<ExitCode, String> {
    match &cli.command {
        Command::Serve {
            model,
            ctx,
            threads,
            kv_type,
            ubatch,
            threads_batch,
            load_mode,
            force,
            host,
            port,
            api_key,
            concurrency,
            web_root,
        } => {
            // Only this command needs an async runtime, so it is built here
            // rather than wrapping every command in one.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|err| format!("could not start the async runtime: {err}"))?;
            // Whether the user actually typed these, as opposed to clap
            // filling in the documented default. A default means the persisted
            // config, if any, may speak; a typed flag always wins.
            let serve_matches = matches.subcommand_matches("serve");
            let from_cli = |name: &str| {
                serve_matches.and_then(|m| m.value_source(name))
                    == Some(clap::parser::ValueSource::CommandLine)
            };
            runtime.block_on(serve::run(serve::ServeOptions {
                model: model.clone(),
                n_ctx: *ctx,
                threads: *threads,
                kv_type: kv_type.clone(),
                ubatch: *ubatch,
                threads_batch: *threads_batch,
                load_mode: load_mode.clone(),
                force: *force,
                hosts: host.clone(),
                hosts_explicit: from_cli("host"),
                port: *port,
                port_explicit: from_cli("port"),
                api_key: api_key.clone(),
                concurrency: *concurrency,
                web_root: web_root.clone(),
            }))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Sysinfo => {
            sysinfo(out, cli.json);
            Ok(ExitCode::SUCCESS)
        }
        Command::Bench {
            model,
            ctx,
            threads,
            kv_type,
            prompt_tokens,
            generate_tokens,
            repeat,
            ubatch,
            parallel,
            fit,
        } => {
            let options = bench::BenchOptions {
                model: model.clone(),
                n_ctx: *ctx,
                threads: *threads,
                kv_type: kv_type.clone(),
                prompt_tokens: *prompt_tokens,
                generate_tokens: *generate_tokens,
                repetitions: *repeat,
                ubatch: ubatch.clone(),
                parallel: parallel.clone(),
                fit: *fit,
                json: cli.json,
            };
            let report = runtime()?.block_on(bench::run(&options))?;
            line!(out, "{report}");
            Ok(ExitCode::SUCCESS)
        }
        Command::Models { action } => models_command(cli, out, action),
        Command::Key { action } => key_command(cli, out, action),
        Command::Config { action } => config_command(cli, out, action),
        Command::Inspect { model, header_only } => {
            let metadata = load_metadata(model, *header_only)?;
            if cli.json {
                render_json(out, &metadata);
            } else {
                render_inspect(out, &metadata);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Estimate {
            model,
            ctx,
            kv_type,
            ubatch,
            parallel,
            headless,
            header_only,
        } => {
            let metadata = load_metadata(model, *header_only)?;
            let cache_type: GgmlType = kv_type
                .parse()
                .map_err(|_| format!("unknown KV cache type {kv_type:?}"))?;

            let params = RuntimeParams {
                n_ctx: *ctx,
                n_ubatch: *ubatch,
                n_parallel: *parallel,
                cache_type_k: cache_type,
                cache_type_v: cache_type,
                ..RuntimeParams::default()
            };

            let base = if *headless {
                lightweight_memory::ComputeModel::headless()
            } else {
                lightweight_memory::ComputeModel::default()
            };
            // This machine's own coefficients when `hermes bench --fit` has
            // earned them for exactly these settings, and the shipped ones
            // otherwise. A data directory that cannot be discovered is not a
            // reason to refuse an estimate: it means there is nowhere a fit
            // could have been written, which is the same answer as no fit.
            let estimator = match lightweight_system_info::DataPaths::discover() {
                Ok(paths) => {
                    lightweight_bench::estimator_for(
                        &paths.calibration_file(),
                        &lightweight_bench::engine_fingerprint(
                            &lightweight_backend_llamacpp::backend::ProcessBackend::new(
                                paths.runtime_dir(),
                            )
                            .map_err(describe)?,
                        ),
                        &metadata,
                        params,
                        base,
                    )
                    .0
                }
                Err(_) => Estimator::new(base),
            };
            // `describe`, not `to_string`: without a reading there is no
            // verdict at all, and the remedy is the only thing that tells the
            // user they can still load with --force.
            let estimate = estimator
                .estimate_now(&metadata, params, &SystemMemoryProbe)
                .map_err(describe)?;

            if cli.json {
                render_json(out, &estimate);
            } else {
                render_estimate(out, &metadata, &estimate);
            }

            // A refused load is a failure exit code, so scripts and the
            // installer can branch on it without parsing the report.
            Ok(if estimate.verdict.is_admissible() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

fn load_metadata(path: &PathBuf, header_only: bool) -> Result<ModelMetadata, String> {
    let file = if header_only {
        GgufFile::open_header_only(path)
    } else {
        GgufFile::open(path)
    }
    .map_err(describe)?;
    ModelMetadata::from_file(&file).map_err(describe)
}

/// Render an error together with what the user can do about it.
///
/// Spec section 27: an error must be actionable. The `Actionable` trait makes
/// the remedies available; this is where they reach a person.
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

fn render_json<T: serde::Serialize>(out: &mut String, value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => line!(out, "{text}"),
        Err(err) => eprintln!("error: could not serialize output: {err}"),
    }
}

fn sysinfo(out: &mut String, json: bool) {
    let cpu = CpuInfo::detect();
    let memory = SystemMemoryProbe.snapshot();
    let addresses = reachable_addresses();
    let classified = classified_addresses();

    if json {
        let value = serde_json::json!({
            "cpu": cpu,
            "memory": memory.as_ref().ok(),
            "memory_error": memory.as_ref().err().map(ToString::to_string),
            "expected_ggml_variant": cpu.expected_ggml_variant(),
            "default_threads": cpu.default_threads(),
            // The addresses another machine could reach this one at. Under
            // `--json` so a script can pick a bind address instead of asking a
            // human to read `ip addr` and copy one across.
            "reachable_addresses": addresses.as_ref().ok().map(|found| {
                found.iter().map(ToString::to_string).collect::<Vec<_>>()
            }),
            "reachable_addresses_error": addresses.as_ref().err().map(ToString::to_string),
            // Same addresses, each tagged with the reserved range it falls in,
            // so a script can prefer an overlay address without guessing.
            "addresses": classified.as_ref().ok().map(|found| {
                found.iter().map(|entry| serde_json::json!({
                    "address": entry.address.to_string(),
                    "scope": entry.scope,
                })).collect::<Vec<_>>()
            }),
        });
        render_json(out, &value);
        return;
    }

    line!(out, "CPU");
    line!(
        out,
        "  Model            {}",
        cpu.model.as_deref().unwrap_or("unknown")
    );
    line!(out, "  Architecture     {}", cpu.architecture);
    line!(out, "  Physical cores   {}", cpu.physical_cores);
    line!(out, "  Logical cores    {}", cpu.logical_cores);
    line!(out, "  Default threads  {}", cpu.default_threads());

    let features: Vec<&str> = cpu.features.iter().map(|f| f.label()).collect();
    line!(
        out,
        "  Instruction sets {}",
        if features.is_empty() {
            "none detected".to_owned()
        } else {
            features.join(", ")
        }
    );
    line!(out, "  Engine variant   {}", cpu.expected_ggml_variant());
    if !cpu.has_avx_family() && cpu.architecture == "x86_64" {
        // Not a warning: the engine ships a variant for this case and will run
        // correctly. But throughput will be well below an AVX2 machine's, and
        // it is better to say so than to let the user guess.
        line!(
            out,
            "                   (no AVX on this CPU; expect lower throughput)"
        );
    }

    line!(out, "\nMemory");
    match memory {
        Ok(snapshot) => {
            line!(out, "  Total            {}", snapshot.total);
            line!(out, "  Available        {}", snapshot.available);
            line!(out, "  Free             {}", snapshot.free);
            line!(
                out,
                "  Swap             {} used of {}",
                snapshot.swap_used(),
                snapshot.swap_total
            );
            line!(
                out,
                "  Pressure         {:.0}%",
                snapshot.pressure() * 100.0
            );
        }
        Err(err) => line!(out, "  unavailable: {err}"),
    }

    // Answering "what do I pass to --host?" without making the operator go and
    // read `ip addr`. It is the question the remote path turns on, and getting
    // it wrong is silent: a name that resolves to loopback binds successfully
    // and serves nobody.
    line!(out, "\nNetwork");
    match &classified {
        Ok(found) if !found.is_empty() => {
            for (index, entry) in found.iter().enumerate() {
                line!(
                    out,
                    "  {:<17}{:<39}  {}",
                    if index == 0 { "Reachable at" } else { "" },
                    entry.address.to_string(),
                    entry.scope.label()
                );
            }
            line!(
                out,
                "  {:<17}serve these with `--host <address>`; a key is then required",
                ""
            );
        }
        Ok(_) => line!(
            out,
            "  Reachable at     loopback only - no other machine can reach this one"
        ),
        Err(err) => line!(out, "  unavailable: {err}"),
    }
}

fn render_inspect(out: &mut String, metadata: &ModelMetadata) {
    line!(out, "Model");
    line!(
        out,
        "  Name             {}",
        metadata.name.as_deref().unwrap_or("-")
    );
    line!(
        out,
        "  Architecture     {}{}",
        metadata.architecture,
        if metadata.supported {
            ""
        } else {
            "  (NOT SUPPORTED by the CPU backend)"
        }
    );
    line!(
        out,
        "  Parameters       {}",
        metadata
            .parameters_label()
            .unwrap_or_else(|| "unknown".into())
    );
    line!(out, "  Quantization     {}", metadata.quantization_label());
    line!(
        out,
        "  Model size       {}",
        metadata
            .weight_bytes
            .map_or_else(|| "unknown".to_owned(), |b| Bytes(b).to_string())
    );
    line!(out, "  Context length   {}", opt(metadata.context_length));
    line!(out, "  GGUF version     {}", metadata.gguf_version);

    line!(out, "\nGeometry");
    line!(out, "  Layers           {}", opt(metadata.block_count));
    line!(out, "  Embedding        {}", opt(metadata.embedding_length));
    line!(
        out,
        "  Feed-forward     {}",
        opt(metadata.feed_forward_length)
    );
    line!(out, "  Attention heads  {}", opt(metadata.head_count));
    match (&metadata.head_count_kv, metadata.has_per_layer_kv_heads()) {
        (Some(heads), true) => line!(out, "  KV heads         per layer {heads:?}"),
        (Some(heads), false) => line!(out, "  KV heads         {}", opt(heads.first().copied())),
        (None, _) => line!(out, "  KV heads         unknown"),
    }
    line!(out, "  Head dimension   {}", opt(metadata.head_dim_k()));
    line!(out, "  GQA ratio        {}", opt(metadata.gqa_ratio()));
    if let Some(window) = metadata.sliding_window {
        line!(out, "  Sliding window   {window}");
    }

    line!(out, "\nTokenizer");
    line!(
        out,
        "  Model            {}",
        metadata.tokenizer.model.as_deref().unwrap_or("-")
    );
    line!(out, "  Vocabulary       {}", opt(metadata.vocab_size));
    line!(
        out,
        "  Chat template    {}",
        if metadata.tokenizer.has_chat_template {
            "yes"
        } else {
            "no  (completion only; cannot format a conversation)"
        }
    );

    line!(out, "\nTensors ({} total)", metadata.tensor_count);
    for (ty, stat) in &metadata.quantization.by_type {
        line!(
            out,
            "  {:<10} {:>5} tensors  {:>12}  {:>6.2} bits/weight",
            ty.to_string(),
            stat.tensors,
            stat.bytes
                .map_or_else(|| "unsizeable".to_owned(), |b| Bytes(b).to_string()),
            ty.bits_per_element().unwrap_or(0.0)
        );
    }

    if !metadata.missing.is_empty() {
        line!(out, "\nMissing metadata");
        for key in &metadata.missing {
            line!(out, "  {key}");
        }
    }
}

fn render_estimate(
    out: &mut String,
    metadata: &ModelMetadata,
    estimate: &lightweight_memory::Estimate,
) {
    line!(
        out,
        "{} at {} context, {} KV cache",
        metadata.name.as_deref().unwrap_or(&metadata.architecture),
        estimate.params.n_ctx,
        estimate.params.cache_type_k
    );

    line!(out, "\nEstimated memory");
    line!(
        out,
        "  Weights          {:>12}",
        estimate.weights.to_string()
    );
    line!(
        out,
        "  KV cache         {:>12}",
        estimate.kv_cache.to_string()
    );
    line!(
        out,
        "  Compute buffers  {:>12}",
        estimate.compute.to_string()
    );
    line!(
        out,
        "  Runtime overhead {:>12}",
        estimate.overhead.to_string()
    );
    line!(out, "  {:<17}{:>12}", "Total", estimate.total.to_string());

    line!(out, "\nThis machine");
    line!(
        out,
        "  Available        {:>12}",
        estimate.budget.to_string()
    );
    line!(
        out,
        "  Safety margin    {:>12}",
        estimate.margin.to_string()
    );
    if estimate.snapshot.swap_used() > Bytes::ZERO {
        line!(
            out,
            "  Swap in use      {:>12}   (not counted as headroom)",
            estimate.snapshot.swap_used().to_string()
        );
    }

    let confidence = match estimate.confidence {
        // Reachable for the first time in M10: a fit is keyed by the machine,
        // the engine build *and* the settings, so saying "for this model" alone
        // would claim less precision than the number actually has.
        lightweight_memory::Confidence::Measured => "measured on this machine, at these settings",
        lightweight_memory::Confidence::Coarse => {
            "uncalibrated; compute and overhead are estimates"
        }
        lightweight_memory::Confidence::Partial => "PARTIAL; some metadata was missing",
    };
    line!(
        out,
        "\nStatus: {}  ({confidence})",
        estimate.verdict.label()
    );

    if estimate.verdict == Verdict::Insufficient {
        line!(
            out,
            "  Short by         {:>12}",
            estimate.shortfall().to_string()
        );
    }
    if let Some(context) = estimate.max_context_that_fits {
        line!(out, "  Largest context that fits: {context} tokens");
    }

    let remedies = estimate.remedies();
    if !remedies.is_empty() {
        line!(out, "\nSuggested actions");
        for remedy in remedies {
            line!(out, "  - {}", remedy.label);
        }
    }

    if !estimate.missing.is_empty() {
        line!(out, "\nMissing metadata (the total is a lower bound)");
        for key in &estimate.missing {
            line!(out, "  {key}");
        }
    }
}

fn opt<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reports are rendered into a buffer rather than printed line by line.
    ///
    /// That is not a style preference. `println!` panics when the reader has
    /// closed the pipe, so `hermes sysinfo | head` used to abort with
    /// "failed printing to stdout: Broken pipe" - a crash in an ordinary
    /// invocation, in a crate that denies panics. Rust ignores SIGPIPE at
    /// startup, so the signal never arrives and the write returns an error
    /// instead; restoring the default disposition would need `unsafe`, which
    /// this crate forbids. Buffering means there is exactly one fallible write,
    /// handled in `main`.
    #[test]
    fn sysinfo_renders_into_the_buffer_rather_than_printing() {
        let mut out = String::new();
        sysinfo(&mut out, false);

        assert!(out.contains("CPU"), "missing CPU section:\n{out}");
        assert!(out.contains("Physical cores"), "missing topology:\n{out}");
        assert!(out.contains("Memory"), "missing memory section:\n{out}");
        assert!(out.ends_with('\n'), "report should end with a newline");
    }

    #[test]
    fn sysinfo_json_is_machine_readable() {
        let mut out = String::new();
        sysinfo(&mut out, true);

        let value: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert!(value["cpu"]["physical_cores"].as_u64().unwrap_or(0) >= 1);
        assert!(value["expected_ggml_variant"].is_string());
    }

    #[test]
    fn json_and_human_output_never_both_appear() {
        // A caller piping `--json` into a parser must not receive a banner.
        let mut json = String::new();
        sysinfo(&mut json, true);
        assert!(!json.contains("Physical cores"));
    }

    #[test]
    fn optional_values_render_as_unknown_rather_than_empty() {
        assert_eq!(opt(None::<u64>), "unknown");
        assert_eq!(opt(Some(42u64)), "42");
    }
}
