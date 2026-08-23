//! Command-line access to the gateway's model and system facts.
//!
//! Everything here is a thin wrapper over the same library calls the daemon and
//! the desktop UI use. That is deliberate: it keeps the crates honest about
//! being usable without a running server, and it means a user can diagnose "why
//! will this model not load" without launching anything.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use std::fmt::Write as _;
use std::io::Write as _;

use clap::{Parser, Subcommand};

mod serve;
use hermes_core::{Actionable, GgmlType, units::Bytes};
use hermes_gguf::{GgufFile, ModelMetadata};
use hermes_memory::{Estimator, RuntimeParams, Verdict};
use hermes_system_info::{CpuInfo, MemoryProbe, SystemMemoryProbe};

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
        model: PathBuf,
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
        /// Load even if the RAM estimate says it will not fit.
        #[arg(long)]
        force: bool,
        /// Address to bind.
        ///
        /// Loopback by default: the gateway is a local service, and section 23
        /// makes exposure beyond this machine a deliberate act rather than
        /// something a user can end up with by accident.
        #[arg(long, default_value = "127.0.0.1")]
        host: IpAddr,
        /// Port to bind. `0` picks a free one and prints it.
        #[arg(long, default_value_t = 8737)]
        port: u16,
        /// Require this key on every request.
        ///
        /// Optional on loopback and mandatory otherwise; binding a non-loopback
        /// address without one is refused rather than silently exposed.
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
}

/// Appends a formatted line to the output buffer.
///
/// Reports are rendered into a `String` and written once, rather than printed a
/// line at a time. Two reasons: `println!` panics if the reader has closed the
/// pipe - `hermes sysinfo | head` would crash, because Rust ignores SIGPIPE at
/// startup and turns it into a write error - and a rendered `String` is
/// something tests can assert against.
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut out = String::new();

    let outcome = run(&cli, &mut out);

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

fn run(cli: &Cli, out: &mut String) -> Result<ExitCode, String> {
    match &cli.command {
        Command::Serve {
            model,
            ctx,
            threads,
            kv_type,
            force,
            host,
            port,
            api_key,
        } => {
            // Only this command needs an async runtime, so it is built here
            // rather than wrapping every command in one.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|err| format!("could not start the async runtime: {err}"))?;
            runtime.block_on(serve::run(serve::ServeOptions {
                model: model.clone(),
                n_ctx: *ctx,
                threads: *threads,
                kv_type: kv_type.clone(),
                force: *force,
                host: *host,
                port: *port,
                api_key: api_key.clone(),
            }))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Sysinfo => {
            sysinfo(out, cli.json);
            Ok(ExitCode::SUCCESS)
        }
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

            let estimator = if *headless {
                Estimator::headless()
            } else {
                Estimator::default()
            };
            let estimate = estimator
                .estimate_now(&metadata, params, &SystemMemoryProbe)
                .map_err(|err| err.to_string())?;

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

    if json {
        let value = serde_json::json!({
            "cpu": cpu,
            "memory": memory.as_ref().ok(),
            "memory_error": memory.as_ref().err().map(ToString::to_string),
            "expected_ggml_variant": cpu.expected_ggml_variant(),
            "default_threads": cpu.default_threads(),
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

fn render_estimate(out: &mut String, metadata: &ModelMetadata, estimate: &hermes_memory::Estimate) {
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
        hermes_memory::Confidence::Measured => "measured for this model",
        hermes_memory::Confidence::Coarse => "uncalibrated; compute and overhead are estimates",
        hermes_memory::Confidence::Partial => "PARTIAL; some metadata was missing",
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
