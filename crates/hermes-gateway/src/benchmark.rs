//! The gateway's own benchmark: measure what is loaded, change nothing.
//!
//! Deliberately the smaller of the two benchmarks this product has. It runs the
//! resident model at the parameters it is already resident with, takes a
//! scheduler slot like any other request, and leaves the engine exactly as it
//! found it.
//!
//! Sweeping parameters means reloading the engine, which takes minutes and
//! holds the gateway's only slot the whole time. Doing that to a gateway that
//! somebody is being served by would be a strange way to find out how fast it
//! is, so that job belongs to `hermes bench`, which brings its own engine.

use std::sync::Arc;

use hermes_bench::record::{BenchmarkRun, EngineFingerprint, MachineFingerprint, ModelFingerprint};
use hermes_bench::{BenchError, BenchmarkStore, RunPlan, Runner};

use crate::catalog::ResidentModel;
use crate::control::BenchmarkBody;
use crate::jobs::{Job, Stage};
use crate::scheduler::PeerKey;
use crate::state::GatewayState;

/// Run the plan against whatever is resident, and save what it measured.
pub async fn run(
    state: &Arc<GatewayState>,
    store: &BenchmarkStore,
    resident: &ResidentModel,
    body: &BenchmarkBody,
    job: &Arc<Job>,
) -> Result<String, BenchError> {
    let plan = RunPlan {
        // Smaller defaults than the CLI's. This one shares a machine with
        // whatever else the gateway is doing, and a benchmark that took ten
        // minutes of a single slot would be a denial of service with a progress
        // bar.
        prompt_tokens: body.prompt_tokens.unwrap_or(256),
        generate_tokens: body.generate_tokens.unwrap_or(16),
        repetitions: body.repeat.unwrap_or(2),
        scenarios: hermes_bench::record::Scenario::ALL.to_vec(),
        // One client. This benchmark takes a single scheduler slot like any
        // other request, so driving several at once from inside it would be
        // measuring a queue it is itself the head of.
        concurrent: 1,
    };

    // A slot, like any other request. Held for the whole run so that a
    // benchmark and a user's generation never interleave on one engine and
    // report each other's contention as their own speed.
    //
    // The refusal matters as much as the slot: this used to discard the
    // `Option`, so a queue timeout ran the benchmark with no slot at all -
    // producing exactly the interleaved measurement the line above says it
    // prevents, and producing it silently.
    let _permit = state
        .acquire_slot(crate::scheduler::Band::Bulk, PeerKey::default())
        .await
        .ok_or(BenchError::Busy {
            seconds: state
                .config
                .queue_timeout
                .as_secs()
                .min(u64::from(u32::MAX)) as u32,
        })?;

    let params = resident.effective;
    let runner = Runner::new(
        state.backend.as_ref(),
        resident.instance,
        params,
        params.threads.unwrap_or_default(),
    );

    let job_for_progress = Arc::clone(job);
    let samples = runner
        .run(&plan, move |progress| {
            job_for_progress.advance(Stage::Benchmark {
                scenario: progress.scenario.as_str(),
                repetition: progress.repetition + 1,
                repetitions: progress.of,
            });
        })
        .await?;

    let run = BenchmarkRun {
        id: BenchmarkStore::new_id(),
        at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default(),
        machine: MachineFingerprint::detect(),
        engine: EngineFingerprint {
            backend: state.backend.id().to_string(),
            // Stated by the backend rather than guessed here. Without it a run
            // taken through the gateway could never be compared with one taken
            // by `hermes bench` against the very same engine.
            build: state.backend.capabilities().build,
            ggml_variant: Some(
                hermes_system_info::CpuInfo::detect()
                    .expected_ggml_variant()
                    .to_owned(),
            ),
        },
        model: ModelFingerprint {
            id: resident.id.to_string(),
            architecture: resident.architecture.clone(),
            quantization: resident
                .quantization
                .clone()
                .unwrap_or_else(|| "unknown".to_owned()),
            parameters: resident.param_count,
        },
        samples,
    };

    store.save(&run)?;
    Ok(run.id)
}
