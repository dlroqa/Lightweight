//! `hermes bench` — measure what this machine does with a model.
//!
//! It brings its own engine. A sweep reloads between buckets, which takes
//! minutes and holds a model resident the whole time, and doing that to a
//! gateway somebody is being served by would be a strange way to find out how
//! fast it is. The gateway has its own, gentler benchmark that measures
//! whatever is already loaded; this is the one that varies the parameters.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hermes_backend_llamacpp::backend::ProcessBackend;
use hermes_backend_llamacpp::manifest::PINNED_BUILD;
use hermes_bench::fit::{Calibration, fit_run};
use hermes_bench::record::{
    BenchmarkRun, EngineFingerprint, MachineFingerprint, ModelFingerprint, Prediction, Sample,
};
use hermes_bench::{BenchmarkStore, RunPlan, Runner};
use hermes_core::{GgmlType, ModelId, RuntimeParams};
use hermes_gguf::{GgufFile, ModelMetadata};
use hermes_inference::{InferenceBackend, LoadRequest};
use hermes_memory::Estimator;
use hermes_system_info::{CpuInfo, DataPaths, MemoryProbe, SystemMemoryProbe};
use tokio_util::sync::CancellationToken;

use crate::serve::describe;

/// What the command was asked to do.
pub struct BenchOptions {
    pub model: PathBuf,
    pub n_ctx: Option<u32>,
    pub threads: Option<u32>,
    pub kv_type: String,
    pub prompt_tokens: u32,
    pub generate_tokens: u32,
    pub repetitions: u32,
    /// Physical batch sizes to sweep. One value means one bucket and no sweep.
    pub ubatch: Vec<u32>,
    /// Slot counts to sweep, one engine per value.
    ///
    /// The second sweep axis, and it crosses with `ubatch`: a bucket is one
    /// pair. Any value above one adds the concurrent scenario, which drives
    /// that many clients at the same moment - without it a multi-slot engine
    /// would be measured one client at a time and would report exactly what a
    /// single-slot engine does.
    pub parallel: Vec<u32>,
    /// Write a calibration fit beside the runs.
    pub fit: bool,
    pub json: bool,
}

/// Run the benchmark, returning the report to print.
pub async fn run(options: &BenchOptions) -> Result<String, String> {
    let paths = DataPaths::discover().map_err(describe)?;
    paths.create_all().map_err(describe)?;

    let file = GgufFile::open(&options.model).map_err(describe)?;
    let metadata = ModelMetadata::from_file(&file).map_err(describe)?;
    let cpu = CpuInfo::detect();
    let cache_type = GgmlType::from_name(&options.kv_type).ok_or_else(|| {
        format!(
            "`{}` is not a KV cache type this build knows",
            options.kv_type
        )
    })?;

    let backend = ProcessBackend::new(paths.runtime_dir()).map_err(describe)?;
    let store = BenchmarkStore::new(paths.benchmarks_dir());
    let estimator = Estimator::headless();

    let machine = MachineFingerprint::detect();
    let engine = EngineFingerprint {
        backend: backend.id().to_string(),
        build: Some(PINNED_BUILD.to_owned()),
        ggml_variant: Some(cpu.expected_ggml_variant().to_owned()),
    };
    let model = ModelFingerprint {
        id: model_name(&options.model),
        architecture: metadata.architecture.clone(),
        quantization: metadata.quantization_label(),
        parameters: metadata.param_count,
    };

    // One bucket per (ubatch, slots) pair. Ordered with slots on the outside so
    // that a sweep of both reads as "this many clients, at each batch size".
    let buckets: Vec<(u32, u32)> = options
        .parallel
        .iter()
        .flat_map(|slots| options.ubatch.iter().map(move |ubatch| (*slots, *ubatch)))
        .collect();

    let mut samples = Vec::new();
    for (index, (slots, ubatch)) in buckets.iter().enumerate() {
        let params = params_for(
            options, &metadata, &estimator, cache_type, &cpu, *ubatch, *slots,
        )?;
        // The concurrent scenario is added only where there is something for
        // it to measure. At one slot it would be `Decode` under another name.
        let mut scenarios = hermes_bench::record::Scenario::ALL.to_vec();
        if *slots > 1 {
            scenarios.push(hermes_bench::record::Scenario::ConcurrentDecode);
        }
        let plan = RunPlan {
            prompt_tokens: options.prompt_tokens,
            generate_tokens: options.generate_tokens,
            repetitions: options.repetitions,
            scenarios,
            concurrent: *slots,
        };

        if !options.json {
            println!(
                "\nbucket {}/{}: ctx {} per client, slots {}, ubatch {}, batch {}, threads {}",
                index + 1,
                buckets.len(),
                params.n_ctx,
                params.n_parallel,
                params.n_ubatch,
                params.n_batch,
                params.threads.unwrap_or_else(|| cpu.default_threads()),
            );
        }

        // A fresh engine per bucket. `VmHWM` is a high-water mark for the life
        // of a process, so a second bucket measured in the same engine would
        // inherit the first bucket's peak and report it as its own.
        let bucket = measure_bucket(
            &backend,
            &options.model,
            &metadata,
            params,
            &plan,
            &estimator,
            options.json,
        )
        .await?;
        samples.extend(bucket);
    }

    if samples.is_empty() {
        return Err("nothing was measured".to_owned());
    }

    let run = BenchmarkRun {
        id: BenchmarkStore::new_id(),
        at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default(),
        machine,
        engine,
        model,
        samples,
    };
    let path = store.save(&run).map_err(describe)?;

    let fitted = if options.fit {
        let calibration_path = paths.data_dir().join("calibration.json");
        let mut calibration = Calibration::load(&calibration_path).map_err(describe)?;
        let fits = fit_run(&run);
        for fit in fits.clone() {
            calibration.insert(fit);
        }
        calibration.save(&calibration_path).map_err(describe)?;
        Some((calibration_path, fits))
    } else {
        None
    };

    // The engine is not left running: this command owns it, and a benchmark
    // that quietly kept a model resident afterwards would be spending memory
    // nobody asked it to spend.
    let _ = backend.shutdown().await;

    Ok(if options.json {
        render_json(&run, fitted.as_ref().map(|(_, fits)| fits.as_slice()))
    } else {
        render_human(&run, &path, fitted.as_ref())
    })
}

/// Load, measure and unload one bucket.
#[allow(clippy::too_many_arguments)]
async fn measure_bucket(
    backend: &ProcessBackend,
    model_path: &Path,
    metadata: &ModelMetadata,
    params: RuntimeParams,
    plan: &RunPlan,
    estimator: &Estimator,
    quiet: bool,
) -> Result<Vec<Sample>, String> {
    let snapshot = SystemMemoryProbe.snapshot().map_err(describe)?;
    let estimate = estimator.estimate(metadata, params, snapshot);
    if !estimate.verdict.is_admissible() {
        // The same refusal `serve` gives, with the same remedies. A benchmark
        // that loaded anyway would measure an OOM kill.
        let mut refusal = format!(
            "this bucket does not fit: short by {}",
            estimate.shortfall()
        );
        for remedy in estimate.remedies() {
            refusal.push_str(&format!("\n  - {}", remedy.label));
        }
        return Err(refusal);
    }
    let prediction = Prediction {
        weights: estimate.weights,
        kv_cache: estimate.kv_cache,
        compute: estimate.compute,
        overhead: estimate.overhead,
        total: estimate.total,
        confidence: estimate.confidence,
    };

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(16);
    let reporter = tokio::spawn(async move { while progress_rx.recv().await.is_some() {} });

    let request = LoadRequest {
        model: ModelId::with_context(model_name(model_path), params.n_ctx),
        gguf_path: model_path.to_path_buf(),
        metadata: Arc::new(metadata.clone()),
        runtime: params,
    };
    let loaded = backend
        .load(request, progress_tx, CancellationToken::new())
        .await
        .map_err(describe)?;
    let _ = reporter.await;

    let threads = loaded.effective.threads.unwrap_or_default();
    let runner =
        Runner::new(backend, loaded.instance, loaded.effective, threads).predicting(prediction);
    let samples = runner
        .run(plan, |progress| {
            if !quiet {
                print!(
                    "\r  {} {}/{}   ",
                    progress.scenario.as_str(),
                    progress.repetition + 1,
                    progress.of
                );
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
        })
        .await
        .map_err(describe)?;
    if !quiet {
        println!();
    }

    // Unload before the next bucket, so its peak starts from nothing.
    backend.unload(loaded.instance).await.map_err(describe)?;
    // The supervisor stops the child on unload; give the kernel a moment to
    // reap it before the next load asks for the same memory.
    tokio::time::sleep(Duration::from_millis(200)).await;

    Ok(samples)
}

/// The parameters for one bucket.
fn params_for(
    options: &BenchOptions,
    metadata: &ModelMetadata,
    estimator: &Estimator,
    cache_type: GgmlType,
    cpu: &CpuInfo,
    ubatch: u32,
    slots: u32,
) -> Result<RuntimeParams, String> {
    let mut params = RuntimeParams {
        cache_type_k: cache_type,
        cache_type_v: cache_type,
        threads: Some(options.threads.unwrap_or_else(|| cpu.default_threads())),
        n_ubatch: ubatch,
        // Priced and launched for the slots this bucket measures: `n_ctx` is
        // the window one client gets, and the engine is asked for that many
        // times as many cells.
        n_parallel: slots.max(1),
        ..RuntimeParams::default()
    };
    // `n_ubatch` may not exceed `n_batch`; the engine refuses the pair, and
    // finding that out from a failed launch several minutes into a sweep is a
    // poor way to learn it.
    params.n_batch = params.n_batch.max(ubatch);

    let snapshot = SystemMemoryProbe.snapshot().map_err(describe)?;
    let n_ctx = match options.n_ctx {
        Some(requested) => requested,
        // Sized to the machine, exactly as `serve` does it: a benchmark that
        // picked a constant would refuse to run on a small machine and waste a
        // large one.
        None => estimator
            .largest_safe_context(metadata, params, snapshot, None)
            .unwrap_or(params.n_ctx),
    };
    Ok(params.with_context(n_ctx))
}

fn model_name(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("model")
        .to_owned()
}

fn render_json(run: &BenchmarkRun, fits: Option<&[hermes_bench::fit::Fit]>) -> String {
    let value = serde_json::json!({
        "run": run,
        "fits": fits,
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
}

fn render_human(
    run: &BenchmarkRun,
    path: &Path,
    fitted: Option<&(PathBuf, Vec<hermes_bench::fit::Fit>)>,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let cores = run.machine.logical_cores;

    let _ = writeln!(out, "\n{} on {}", run.model.id, describe_machine(run));
    let _ = writeln!(
        out,
        "  engine {} {}",
        run.engine.backend,
        run.engine.build.as_deref().unwrap_or("(unpinned)")
    );

    // Every scenario the run actually measured, not only the default three:
    // a concurrent bucket that was measured and not printed is a measurement
    // nobody reads.
    let mut scenarios = hermes_bench::record::Scenario::ALL.to_vec();
    if run
        .samples
        .iter()
        .any(|sample| sample.scenario == hermes_bench::record::Scenario::ConcurrentDecode)
    {
        scenarios.push(hermes_bench::record::Scenario::ConcurrentDecode);
    }

    for scenario in scenarios {
        let samples: Vec<&Sample> = run
            .samples
            .iter()
            .filter(|sample| sample.scenario == scenario)
            .collect();
        if samples.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n{}", scenario.as_str());
        for sample in samples {
            // Each scenario is shown the rate it set out to measure. A decode
            // rate from a one-token prefill run is an artefact of the timer,
            // not a property of the machine, and printing it beside a real one
            // invites somebody to quote it.
            let rate = match scenario {
                hermes_bench::record::Scenario::Decode
                | hermes_bench::record::Scenario::ConcurrentDecode => format!(
                    "decode {:>10}",
                    per_second(sample.decode_tokens_per_second())
                ),
                _ => format!(
                    "prefill {:>9}",
                    per_second(sample.prefill_tokens_per_second())
                ),
            };
            let _ = writeln!(
                out,
                "  slots {:>2}  ubatch {:>5}  prompt {:>6}  prefilled {:>6}  cached {:>6}  \
                 gen {:>4}  {rate}  ttft {:>8}  cores {:>5}{}",
                sample.params.n_parallel,
                sample.params.n_ubatch,
                sample.prompt_tokens,
                sample.prefilled_tokens,
                sample.cached_tokens,
                sample.generated_tokens,
                sample
                    .time_to_first_token_ms
                    .map_or_else(|| "—".to_owned(), |ms| format!("{ms} ms")),
                sample
                    .cores_used(cores)
                    .map_or_else(|| "—".to_owned(), |used| format!("{used:.2}")),
                // The engine's own answer to "were these batched together, or
                // served one after another?" - shown only where more than one
                // client was in flight, because at one slot it is always 1.
                sample
                    .busy_slots_per_decode
                    .map_or_else(String::new, |busy| format!("  busy {busy:>5.2}"),),
            );
        }
    }

    if let Some(peak) = run
        .samples
        .iter()
        .filter_map(|sample| sample.peak_rss)
        .max()
    {
        let _ = writeln!(out, "\npeak resident {peak}");
        if let Some(prediction) = run.samples.iter().find_map(|sample| sample.predicted) {
            let _ = writeln!(
                out,
                "  predicted {} ({} of it exact: weights and KV cache)",
                prediction.total,
                hermes_core::units::Bytes(prediction.exact())
            );
        }
    }

    let _ = writeln!(out, "\nsaved to {}", path.display());

    if let Some((calibration_path, fits)) = fitted {
        let _ = writeln!(
            out,
            "fitted {} bucket(s) into {}",
            fits.len(),
            calibration_path.display()
        );
        for fit in fits {
            match (fit.compute_bytes_per_ubatch, fit.overhead_bytes) {
                (Some(slope), Some(intercept)) => {
                    let _ = writeln!(
                        out,
                        "  {} {}: {:.0} bytes per ubatch token, {} fixed",
                        fit.bucket.architecture,
                        fit.bucket.quantization,
                        slope,
                        hermes_core::units::Bytes(intercept.max(0.0) as u64)
                    );
                }
                _ => {
                    let _ = writeln!(
                        out,
                        "  {} {}: one ubatch value measured, so no slope — largest residual {}",
                        fit.bucket.architecture,
                        fit.bucket.quantization,
                        hermes_core::units::Bytes(fit.max_residual_bytes)
                    );
                }
            }
        }
        let _ = writeln!(
            out,
            "\nNothing reads this file yet: the estimator keeps its shipped defaults."
        );
    }

    // Said every time, because the number above is the most quotable thing this
    // command produces and it is a fact about one machine.
    let _ = writeln!(
        out,
        "\nThese figures describe this machine and this engine build. They are not\n\
         a property of the software and do not transfer to other hardware."
    );
    out
}

fn describe_machine(run: &BenchmarkRun) -> String {
    format!(
        "{} ({} cores, {})",
        run.machine.cpu_model.as_deref().unwrap_or("an unnamed CPU"),
        run.machine.physical_cores,
        if run.machine.isa_features.is_empty() {
            "no detected instruction sets".to_owned()
        } else {
            run.machine.isa_features.join(" ")
        }
    )
}

fn per_second(rate: Option<f64>) -> String {
    rate.map_or_else(|| "—".to_owned(), |value| format!("{value:.2} t/s"))
}
