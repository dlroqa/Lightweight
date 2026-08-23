//! `POST /v1/completions`, streamed and not.
//!
//! One request here can be several generations. `prompt` may be an array, and
//! `n` asks for more than one completion each, so the endpoint expands to a
//! list of independent prompts that share a response.
//!
//! They are run **one at a time**, and that is a property of the slot rather
//! than of the endpoint: the gateway holds a single permit for the whole
//! request and reuses it for each generation, so a multi-prompt request cannot
//! starve a single-prompt one waiting behind it. Nothing here caps the
//! concurrency itself — when the scheduler raises the slot count, these become
//! independent units of work without this file changing, which is the reason
//! they are modelled as a queue of requests rather than as one batched call.
//!
//! What the client sees is one response: choices numbered across the whole set
//! in prompt order, and a single `usage` covering all of them.

use std::collections::VecDeque;
use std::sync::Arc;

use futures_util::{StreamExt, stream};
use hermes_api::chat::UsageBody;
use hermes_api::completions::{
    CompletionChoice, CompletionChunkBuilder, CompletionResponse, accumulate_usage,
};
use hermes_api::error::ErrorEnvelope;
use hermes_core::{Actionable, InstanceId};
use hermes_inference::generation::{FinishReason, GenerationEvent, GenerationRequest};
use hermes_inference::{BackendError, GenerationStream};
use hermes_observability::targets;

use crate::state::GatewayState;
use crate::stream::{KEEP_ALIVE_INTERVAL, RequestGuard};

/// One completion still to run.
pub struct PendingCompletion {
    /// The `index` this completion's choice carries, assigned across the whole
    /// expanded set so the client can line choices up with its prompts.
    pub index: u32,
    pub request: GenerationRequest,
    /// The prompt text to repeat at the head of the completion, when `echo`
    /// was asked for.
    ///
    /// Held as the text rather than as a flag because by this point the request
    /// has been expanded and the prompt is no longer reachable from it.
    pub echo: Option<String>,
}

/// Everything the runner needs to produce one request's completions.
pub struct Run {
    pub state: Arc<GatewayState>,
    pub instance: InstanceId,
    pub queue: VecDeque<PendingCompletion>,
    pub builder: CompletionChunkBuilder,
    pub guard: RequestGuard,
    pub model_id: String,
    pub include_usage: bool,
}

/// Collect every completion into one response.
///
/// The aggregating path, and like the chat endpoint's it is the streaming one
/// with an accumulator on the end rather than a second implementation.
pub async fn aggregate(mut run: Run) -> Result<CompletionResponse, BackendError> {
    let _guard = run.guard;
    let mut choices = Vec::with_capacity(run.queue.len());
    let mut usage = UsageBody::default();

    while let Some(pending) = run.queue.pop_front() {
        let cancel = run.state.job_token();
        let mut events = run
            .state
            .backend
            .generate(run.instance, pending.request, cancel)
            .await?;

        let mut text = pending.echo.unwrap_or_default();
        // Length rather than Stop when the engine says nothing: "stop" would
        // tell the client the model chose to end, which we would not know.
        let mut finish_reason = FinishReason::Length;
        while let Some(event) = events.next().await {
            match event? {
                GenerationEvent::ContentDelta { text: fragment } => text.push_str(&fragment),
                GenerationEvent::Finished {
                    finish_reason: reason,
                    usage: measured,
                } => {
                    finish_reason = reason;
                    accumulate_usage(&mut usage, UsageBody::from(measured));
                }
                // A raw completion has no reasoning, no tool calls, and no use
                // for timings it cannot report in this schema.
                _ => {}
            }
        }

        choices.push(CompletionChoice {
            text,
            index: pending.index,
            logprobs: None,
            finish_reason: Some(finish_reason.as_str().to_owned()),
        });
    }

    Ok(CompletionResponse::new(
        run.builder.id().to_owned(),
        run.model_id.clone(),
        choices,
        usage,
    ))
}

/// State carried while streaming one request's completions.
struct Encoder {
    run: Run,
    /// The generation being read, and the choice index it belongs to.
    current: Option<(u32, GenerationStream)>,
    pending: VecDeque<String>,
    usage: UsageBody,
    /// False until the first event of the current generation arrives.
    /// Keep-alives are sent only before it, which is when prefill happens.
    started: bool,
    finished: bool,
    produced_text: bool,
}

/// Encode the whole expanded request as one SSE stream.
pub fn encode(
    run: Run,
) -> impl futures_util::Stream<Item = Result<String, std::convert::Infallible>> + Send {
    let encoder = Encoder {
        run,
        current: None,
        pending: VecDeque::new(),
        usage: UsageBody::default(),
        started: false,
        finished: false,
        produced_text: false,
    };

    stream::unfold(encoder, |mut encoder| async move {
        loop {
            if let Some(frame) = encoder.pending.pop_front() {
                return Some((Ok(frame), encoder));
            }
            if encoder.finished {
                return None;
            }

            // Nothing running: start the next completion, or finish.
            if encoder.current.is_none() {
                let Some(pending) = encoder.run.queue.pop_front() else {
                    close(&mut encoder);
                    continue;
                };
                let cancel = encoder.run.state.job_token();
                match encoder
                    .run
                    .state
                    .backend
                    .generate(encoder.run.instance, pending.request, cancel)
                    .await
                {
                    Ok(events) => {
                        encoder.started = false;
                        if let Some(echo) = pending.echo.filter(|text| !text.is_empty()) {
                            encoder.produced_text = true;
                            let frame =
                                encoder.run.builder.text(pending.index, echo).to_sse_frame();
                            encoder.pending.push_back(frame);
                        }
                        encoder.current = Some((pending.index, events));
                    }
                    Err(err) => {
                        // Headers are already out, so this cannot become a
                        // status. It ends the whole stream rather than the one
                        // completion: a client that received choices 0 and 1
                        // and nothing for 2 has no way to tell that from a
                        // model that produced nothing.
                        fail(&mut encoder, &err);
                    }
                }
                continue;
            }

            let Some((index, events)) = encoder.current.as_mut() else {
                continue;
            };
            let index = *index;

            let next = if encoder.started {
                events.next().await
            } else {
                tokio::select! {
                    event = events.next() => event,
                    () = tokio::time::sleep(KEEP_ALIVE_INTERVAL) => {
                        let ping = encoder.run.builder.keep_alive();
                        encoder.pending.push_back(ping);
                        continue;
                    }
                }
            };

            match next {
                Some(Ok(event)) => {
                    encoder.started = true;
                    absorb(&mut encoder, index, event);
                }
                Some(Err(err)) => fail(&mut encoder, &err),
                None => {
                    // The generation ended without saying why. Close this
                    // choice cleanly and move to the next one.
                    finish_choice(&mut encoder, index, FinishReason::Length);
                }
            }
        }
    })
}

fn absorb(encoder: &mut Encoder, index: u32, event: GenerationEvent) {
    match event {
        GenerationEvent::ContentDelta { text } => {
            encoder.produced_text = true;
            let frame = encoder.run.builder.text(index, text).to_sse_frame();
            encoder.pending.push_back(frame);
        }
        GenerationEvent::Finished {
            finish_reason,
            usage,
        } => {
            accumulate_usage(&mut encoder.usage, UsageBody::from(usage));
            finish_choice(encoder, index, finish_reason);
        }
        _ => {}
    }
}

/// Close one choice and make room for the next completion.
fn finish_choice(encoder: &mut Encoder, index: u32, reason: FinishReason) {
    let frame = encoder.run.builder.finish(index, reason).to_sse_frame();
    encoder.pending.push_back(frame);
    encoder.current = None;
    encoder.started = false;
}

/// End the stream after a failure that arrived once bytes were already sent.
fn fail(encoder: &mut Encoder, err: &BackendError) {
    tracing::warn!(
        target: targets::INFERENCE,
        code = err.code(),
        "a completion failed after the response had started"
    );
    let body = ErrorEnvelope::from_error(err).to_value();
    let frame = encoder.run.builder.error(body).to_sse_frame();
    encoder.pending.push_back(frame);
    encoder.pending.push_back(encoder.run.builder.done());
    encoder.finished = true;
    encoder.current = None;
}

/// Emit the terminal usage chunk and `[DONE]`.
fn close(encoder: &mut Encoder) {
    if encoder.finished {
        return;
    }
    if encoder.run.include_usage {
        let frame = encoder.run.builder.usage(encoder.usage).to_sse_frame();
        encoder.pending.push_back(frame);
    }
    encoder.pending.push_back(encoder.run.builder.done());
    encoder.finished = true;

    if !encoder.produced_text {
        // Never fabricated, only reported: an empty completion is usually a
        // sampler or template problem, and it is invisible otherwise.
        tracing::warn!(
            target: targets::INFERENCE,
            id = encoder.run.builder.id(),
            "the model produced no text for any prompt"
        );
    }

    encoder.run.guard.cancel();
}
