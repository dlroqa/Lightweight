//! Driving the engine, and timing what comes back.
//!
//! The runner talks to an [`InferenceBackend`] and nothing else. It does not
//! load models, does not own an engine and does not know whether it is being
//! driven by the CLI or by the gateway — which is what lets the same code
//! measure a sweep that reloads per bucket and a quick run against whatever is
//! already resident, and what lets it be tested against the mock backend with
//! no engine at all.

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use hermes_core::{InstanceId, RuntimeParams};
use hermes_inference::InferenceBackend;
use hermes_inference::generation::{
    ChatMessage, GenerationEvent, GenerationRequest, MessageRole, SamplingParams, Timings,
};
use tokio_util::sync::CancellationToken;

use crate::error::BenchError;
use crate::record::{Prediction, Sample, Scenario};

/// One filler word, repeated to reach a target prompt length.
///
/// Deliberately dull and deliberately fixed. The prompt is a length, not a
/// question: a benchmark that varied its text would vary its token count and
/// its tokenizer behaviour between runs, and comparing two such runs would
/// measure the prompts rather than the machine.
const FILLER: &str = "the quick brown fox jumps over the lazy dog ";

/// How many times the prompt sizer may ask the engine to count.
///
/// A tokenizer is not linear in characters, so the sizer converges rather than
/// solving. Eight rounds is far more than the two or three it takes in
/// practice, and a bound means a pathological tokenizer costs a failed
/// benchmark rather than an unbounded loop against a live engine.
const SIZING_ROUNDS: usize = 8;

/// How close to the requested prompt length is close enough.
///
/// Expressed as a fraction because the tolerable absolute error scales: five
/// tokens out of fifty is a different measurement, five out of five thousand is
/// noise.
const SIZING_TOLERANCE: f64 = 0.05;

/// What to measure.
#[derive(Clone, Debug)]
pub struct RunPlan {
    /// Prompt length, in tokens, for the two prefill scenarios.
    pub prompt_tokens: u32,
    /// Output budget for the decode scenario.
    pub generate_tokens: u32,
    /// How many times to repeat each scenario.
    pub repetitions: u32,
    /// Which scenarios to run.
    pub scenarios: Vec<Scenario>,
    /// How many clients [`Scenario::ConcurrentDecode`] runs at once.
    ///
    /// One means the concurrent scenario measures the same thing `Decode`
    /// does, which is why the CLI only asks for it above one. It is not
    /// clamped to the engine's slot count here: driving more clients than
    /// there are slots is a queue measurement, which is a legitimate thing to
    /// want to see, and the sample records both numbers.
    pub concurrent: u32,
}

impl Default for RunPlan {
    fn default() -> Self {
        Self {
            // Small enough to finish on a slow CPU in a reasonable time, large
            // enough that the per-request overhead is not what is being
            // measured. Not a constant tuned to any one machine: the caller
            // raises it, and a fast machine should.
            prompt_tokens: 512,
            generate_tokens: 32,
            repetitions: 3,
            scenarios: Scenario::ALL.to_vec(),
            concurrent: 1,
        }
    }
}

/// A prompt of a measured length, reused across the repetitions of one
/// scenario so that they are repetitions of the same measurement.
#[derive(Clone, Debug)]
struct SizedPrompt {
    text: String,
    /// What the tokenizer counted, not what was asked for.
    tokens: u32,
}

impl SizedPrompt {
    /// The same prompt with a distinguishing opening.
    ///
    /// Leading, not trailing: the prefix cache matches from the front, so a
    /// distinguishing suffix would not defeat it. The added tokens are a
    /// handful against a prompt of hundreds, and they are counted by the engine
    /// like any others.
    fn with_lead(&self, seed: u32) -> String {
        format!("Run {seed}. {}", self.text)
    }
}

/// Progress, for a caller that is showing it.
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub scenario: Scenario,
    pub repetition: u32,
    pub of: u32,
}

/// Measures one loaded model.
pub struct Runner<'a> {
    backend: &'a dyn InferenceBackend,
    instance: InstanceId,
    params: RuntimeParams,
    threads: u32,
    prediction: Option<Prediction>,
}

impl<'a> Runner<'a> {
    pub fn new(
        backend: &'a dyn InferenceBackend,
        instance: InstanceId,
        params: RuntimeParams,
        threads: u32,
    ) -> Self {
        Self {
            backend,
            instance,
            params,
            threads,
            prediction: None,
        }
    }

    /// Record what the estimator predicted for these same parameters.
    ///
    /// Optional because a quick run against an already-resident model may not
    /// have an estimate to hand. Without it the samples are still a complete
    /// measurement; they are simply not a calibration input.
    pub fn predicting(mut self, prediction: Prediction) -> Self {
        self.prediction = Some(prediction);
        self
    }

    /// Run the plan, reporting progress as it goes.
    pub async fn run<F>(&self, plan: &RunPlan, mut progress: F) -> Result<Vec<Sample>, BenchError>
    where
        F: FnMut(Progress),
    {
        if plan.prompt_tokens >= self.params.n_ctx {
            return Err(BenchError::PromptTooLarge {
                requested: plan.prompt_tokens,
                n_ctx: self.params.n_ctx,
            });
        }

        let mut samples = Vec::new();
        for scenario in &plan.scenarios {
            // Sized once per scenario, not once per repetition. The sizer
            // converges rather than solving, so two repetitions sized
            // separately can land on prompts of noticeably different lengths —
            // and two samples of different lengths are not two repetitions of
            // one measurement.
            let target = match scenario {
                Scenario::Decode => plan.prompt_tokens.min(64),
                _ => plan.prompt_tokens,
            };
            let sized = self.sized_prompt(target).await?;

            for repetition in 0..plan.repetitions {
                progress(Progress {
                    scenario: *scenario,
                    repetition,
                    of: plan.repetitions,
                });
                match scenario {
                    Scenario::ConcurrentDecode => {
                        samples.extend(self.all_at_once(repetition, plan, &sized).await?);
                    }
                    _ => samples.push(self.one(*scenario, repetition, plan, &sized).await?),
                }
            }
        }
        if samples.is_empty() {
            return Err(BenchError::NothingMeasured);
        }
        Ok(samples)
    }

    /// Drive several clients through one decode at the same moment.
    ///
    /// Started together and awaited together, which is the only way the
    /// measurement means anything: launching them in sequence would measure
    /// one generation and then another, which is what [`Scenario::Decode`]
    /// already does. Each client gets a distinct opening so that they do not
    /// share a prefix — clients that did would have their prefills served from
    /// each other's cache entries and report a batching win that is really a
    /// cache hit.
    ///
    /// One sample per client, all carrying the same repetition, so a reader can
    /// see the spread between clients as well as their total.
    async fn all_at_once(
        &self,
        repetition: u32,
        plan: &RunPlan,
        sized: &SizedPrompt,
    ) -> Result<Vec<Sample>, BenchError> {
        let clients = plan.concurrent.max(1);
        // A distinguishing lead per client *and* per repetition, so no two
        // generations anywhere in the run share a prefix.
        let generations = (0..clients).map(|client| {
            let seed = repetition
                .saturating_mul(clients)
                .saturating_add(client)
                .saturating_add(1);
            self.one_with(
                Scenario::ConcurrentDecode,
                repetition,
                sized.with_lead(seed),
                plan.generate_tokens,
                sized.tokens,
            )
        });
        futures_util::future::try_join_all(generations).await
    }

    async fn one(
        &self,
        scenario: Scenario,
        repetition: u32,
        plan: &RunPlan,
        sized: &SizedPrompt,
    ) -> Result<Sample, BenchError> {
        let (prompt, max_tokens) = match scenario {
            // A distinct opening per repetition, so the engine has not seen
            // this prefix. Without it the second repetition is served from the
            // prefix cache and reports a prefill speed that is really a cache
            // hit — the exact mistake this scenario exists to avoid.
            //
            // "Cold" is still not perfectly cold, and honestly cannot be: the
            // model's own chat template renders an identical preamble ahead of
            // every message, and those tokens legitimately match. The engine
            // reports them as cached, and they are excluded from the rate.
            Scenario::ColdPrefill => (sized.with_lead(repetition), 1),
            // The same prompt every time, which is the point: repetitions after
            // the first should find their prefix already in the cache.
            Scenario::CachedPrefill => (sized.text.clone(), 1),
            Scenario::Decode | Scenario::ConcurrentDecode => {
                (sized.text.clone(), plan.generate_tokens)
            }
        };
        // The sizer's count, exactly as every scenario has recorded since M8:
        // the number the engine evaluated is `prefilled_tokens`, and reading
        // that as the prompt's length makes every rate computed from it wrong
        // by the size of the cache hit.
        self.one_with(scenario, repetition, prompt, max_tokens, sized.tokens)
            .await
    }

    /// Measure one generation of an already-chosen prompt.
    ///
    /// Split out of [`Runner::one`] so that the concurrent scenario can start
    /// several of these at the same moment: the choice of prompt is per
    /// scenario, and the measurement is not.
    async fn one_with(
        &self,
        scenario: Scenario,
        repetition: u32,
        prompt: String,
        max_tokens: u32,
        prompt_tokens: u32,
    ) -> Result<Sample, BenchError> {
        let request = Self::request(&prompt, max_tokens);
        let before = self.usage().await;
        let machine_before = hermes_system_info::cpu_times().ok();
        let started = Instant::now();

        let mut stream = self
            .backend
            .generate(self.instance, request, CancellationToken::new())
            .await
            .map_err(|err| BenchError::Engine {
                detail: err.to_string(),
            })?;

        let mut first_token: Option<Duration> = None;
        let mut timings: Option<Timings> = None;
        let mut generated = 0_u32;
        while let Some(event) = stream.next().await {
            match event {
                Ok(GenerationEvent::ContentDelta { .. }) => {
                    first_token.get_or_insert_with(|| started.elapsed());
                    generated += 1;
                }
                Ok(GenerationEvent::ReasoningDelta { .. }) => {
                    first_token.get_or_insert_with(|| started.elapsed());
                }
                Ok(GenerationEvent::Timings(measured)) => timings = Some(measured),
                Ok(_) => {}
                Err(err) => {
                    return Err(BenchError::Engine {
                        detail: err.to_string(),
                    });
                }
            }
        }
        let wall = started.elapsed();
        let after = self.usage().await;
        let machine_after = hermes_system_info::cpu_times().ok();

        // The engine's own count where it gave one: it counts tokens, and this
        // side counts stream events, which are not the same thing when a
        // fragment carries more than one token.
        let generated_tokens = timings.map_or(generated, |measured| measured.predicted_n);

        Ok(Sample {
            scenario,
            params: self.params,
            threads: self.threads,
            repetition,
            // The tokenizer's count of what was sent, taken before generating.
            // The engine's `prompt_n` is what it *evaluated*, which on a fully
            // cached prompt is a single token — recorded separately below,
            // because reading it as the prompt's length makes every rate
            // computed from it wrong by the size of the cache hit.
            prompt_tokens,
            cached_tokens: timings.map_or(0, |measured| measured.cached_n),
            prefilled_tokens: timings.map_or(0, |measured| measured.prompt_n),
            generated_tokens,
            prefill_ms: timings.map(|measured| measured.prompt_ms),
            decode_ms: timings.map(|measured| measured.predicted_ms),
            time_to_first_token_ms: first_token.map(|elapsed| elapsed.as_millis() as u64),
            wall_ms: wall.as_millis() as u64,
            engine_ticks: ticks_between(&before, &after),
            machine_ticks: match (machine_before, machine_after) {
                (Some(before), Some(after)) => after.total.checked_sub(before.total),
                _ => None,
            },
            rss: after.as_ref().map(|usage| usage.rss),
            peak_rss: after.as_ref().map(|usage| usage.peak_rss),
            predicted: self.prediction,
            busy_slots_per_decode: self
                .backend
                .engine_counters()
                .await
                .ok()
                .flatten()
                .and_then(|counters| counters.busy_slots_per_decode),
        })
    }

    async fn usage(&self) -> Option<hermes_inference::ResourceSnapshot> {
        self.backend.resource_usage().await.ok().flatten()
    }

    /// Build a prompt of about `target` tokens, measured by the engine.
    ///
    /// The engine's tokenizer decides, not a guess about characters per token:
    /// the count that is recorded is the count that will be prefilled, and a
    /// harness that assumed four characters per token would report throughput
    /// against a token count the model never saw.
    async fn sized_prompt(&self, target: u32) -> Result<SizedPrompt, BenchError> {
        let lead = "";
        // A ceiling on what may be built, derived from the target rather than
        // fixed. Without it a tokenizer that answers the same number whatever
        // it is handed sends the correction below multiplying without bound —
        // eight rounds of scaling by forty produces a prompt of some hundreds
        // of terabytes, and the process is killed rather than told anything.
        // No real tokenizer needs four words per requested token; one that
        // appears to is not responding to length, and the loop below detects
        // that and stops.
        let ceiling = (target as usize).saturating_mul(4).max(16);
        let mut words = (target.max(1) as usize).min(ceiling);
        let mut best: Option<(u32, String)> = None;
        let mut previous_count: Option<u32> = None;

        for _ in 0..SIZING_ROUNDS {
            let prompt = format!("{lead}{}", FILLER.repeat(words.max(1) / 9 + 1));
            let counted = self.count(&prompt).await?;
            let error = (f64::from(counted) - f64::from(target)).abs() / f64::from(target.max(1));
            let closer = best
                .as_ref()
                .is_none_or(|(previous, _)| counted.abs_diff(target) < previous.abs_diff(target));
            if closer {
                best = Some((counted, prompt));
            }
            if error <= SIZING_TOLERANCE {
                break;
            }
            // A tokenizer that returns the same count for a different prompt
            // length is not one this can steer, and asking it again will not
            // change that.
            if previous_count == Some(counted) {
                break;
            }
            previous_count = Some(counted);

            // Scale by what was actually observed rather than stepping: a
            // tokenizer's characters-per-token is stable within one model, so
            // one corrected guess usually lands.
            let ratio = f64::from(target) / f64::from(counted.max(1));
            let next = ((words as f64 * ratio).round().max(1.0) as usize).min(ceiling);
            if next == words {
                break;
            }
            words = next;
        }

        match best {
            Some((counted, prompt))
                if (f64::from(counted) - f64::from(target)).abs() / f64::from(target.max(1))
                    <= SIZING_TOLERANCE * 4.0 =>
            {
                Ok(SizedPrompt {
                    text: prompt,
                    tokens: counted,
                })
            }
            Some((counted, _)) => Err(BenchError::PromptSize {
                requested: target,
                achieved: counted,
            }),
            None => Err(BenchError::NothingMeasured),
        }
    }

    async fn count(&self, prompt: &str) -> Result<u32, BenchError> {
        let request = Self::request(prompt, 1);
        self.backend
            .count_prompt_tokens(self.instance, &request)
            .await
            .map_err(|err| BenchError::Engine {
                detail: err.to_string(),
            })
    }

    /// The one request shape every scenario uses.
    ///
    /// Sampling is pinned: temperature zero and a fixed seed, so two runs of
    /// the same scenario differ by what the machine did and not by what the
    /// model chose.
    fn request(prompt: &str, max_tokens: u32) -> GenerationRequest {
        let mut request = GenerationRequest::new(vec![ChatMessage::new(MessageRole::User, prompt)]);
        request.max_tokens = Some(max_tokens);
        request.sampling = SamplingParams {
            temperature: Some(0.0),
            seed: Some(7),
            ..SamplingParams::default()
        };
        request
    }
}

/// Engine ticks consumed between two readings.
fn ticks_between(
    before: &Option<hermes_inference::ResourceSnapshot>,
    after: &Option<hermes_inference::ResourceSnapshot>,
) -> Option<u64> {
    let before = before.as_ref()?.cpu_ticks?;
    let after = after.as_ref()?.cpu_ticks?;
    after.since(&before)
}
