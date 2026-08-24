//! Turning generation events into the bytes a client reads.
//!
//! Three things happen here that are easy to leave out and expensive to
//! discover later.
//!
//! **The slot is released by `Drop`, not by reaching the end.** The response
//! body owns its permit and its cancellation token, and hyper drops the body
//! when the client disconnects. That single fact is what makes a closed laptop
//! lid stop the engine decoding, and it is why no cancellation path can leak a
//! slot: there is no path that skips `Drop`.
//!
//! **Keep-alives during prefill only.** On a CPU without AVX, processing a
//! long prompt can occupy 30 to 120 seconds before the first token exists. A
//! socket silent for that long is dropped by intermediaries and by some
//! clients, so a comment frame goes out every 15 seconds — until the first
//! event arrives, and never after, because once tokens are flowing the silence
//! is over.
//!
//! **A failure after the first byte ends the stream deliberately.** Headers are
//! already sent, so it cannot become an HTTP status. Dropping the connection
//! would look like a truncated stream and invite a blind retry; instead a final
//! chunk carries `finish_reason: "error"` and an error body, then `[DONE]`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::{self, StreamExt};
use hermes_api::chat::UsageBody;
use hermes_api::error::ErrorEnvelope;
use hermes_api::stream::ChunkBuilder;
use hermes_core::Actionable;
use hermes_inference::generation::{FinishReason, GenerationEvent};
use hermes_inference::{BackendError, GenerationStream};
use hermes_observability::targets;
// tokio's clock rather than the standard one, so that every duration this
// module reports — a queue wait, a time to first token — is measurable under
// `tokio::time::pause()`. A test that has to sleep in real time to check a
// 15-second interval is a test nobody runs.
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::metrics::{GenerationRecord, Metrics};
use crate::scheduler::{SlotPermit, Ticket};

/// How often a keep-alive goes out while the engine is still reading the
/// prompt.
///
/// Fifteen seconds is well inside the shortest idle timeout worth worrying
/// about, and costs six bytes.
pub const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Holds a request's slot, records what it cost, and cancels its work when
/// dropped.
///
/// The entire cancellation design rests on this type being owned by the
/// response body. Nothing needs to detect a disconnect: hyper drops the body,
/// the body drops this, and the token cancels — which drops the upstream
/// response, closes the connection to the engine, and stops it decoding.
///
/// Metrics are written from the same `Drop`, and for the same reason: a
/// generation that ended because the client walked away is exactly the one a
/// counter placed at the end of a happy path would miss, and it is the one
/// worth knowing about.
#[derive(Debug)]
pub struct RequestGuard {
    cancel: CancellationToken,
    /// Held, never read. Dropping it is the point.
    permit: Option<SlotPermit>,
    metrics: Option<Arc<Metrics>>,
    record: GenerationRecord,
    started: Instant,
}

impl RequestGuard {
    pub fn new(cancel: CancellationToken, permit: Option<SlotPermit>) -> Self {
        Self {
            cancel,
            permit,
            metrics: None,
            record: GenerationRecord::default(),
            started: Instant::now(),
        }
    }

    /// The same guard, reporting what it measured when it is dropped.
    #[must_use]
    pub fn reporting_to(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Record how long this request waited for its slot.
    pub fn waited(&mut self, queue_wait: Duration) {
        self.record.queue_wait = queue_wait;
    }

    /// Take the slot, for a request that was still queued when its response
    /// started.
    pub fn admitted(&mut self, permit: SlotPermit) {
        self.permit = Some(permit);
    }

    /// The measurements this request is accumulating.
    pub fn record_mut(&mut self) -> &mut GenerationRecord {
        &mut self.record
    }

    /// Note the moment the client could first have seen a token.
    ///
    /// Only the first call counts. Time to first token is a latency, and a
    /// latency that is overwritten by every subsequent token is a measure of
    /// nothing.
    pub fn first_token(&mut self) {
        if self.record.time_to_first_token.is_none() {
            self.record.time_to_first_token = Some(self.started.elapsed());
        }
    }

    /// Cancel the work now rather than when the client finishes reading.
    ///
    /// `Drop` does this anyway; calling it early releases the slot as soon as
    /// the last frame is queued, which on a single-slot gateway is the
    /// difference between the next request waiting and not.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(metrics) = &self.metrics {
            self.record.total = self.started.elapsed();
            metrics.record_generation(&self.record);
        }
        // Explicit, and last: the slot must not be handed to the next request
        // until this one has finished accounting for itself.
        drop(self.permit.take());
    }
}

/// What the stream has produced so far, for the closing log line.
#[derive(Debug, Default)]
struct Outcome {
    content_chunks: u64,
    tool_call_chunks: u64,
}

/// How a streamed response starts a generation it does not have yet.
///
/// Boxed because the work has to be deferred until a slot is free: the engine
/// serves one request at a time, so calling it before admission would be
/// exactly the queue-jumping the scheduler exists to prevent.
pub type StartGeneration =
    Box<dyn FnOnce() -> BoxFuture<'static, Result<GenerationStream, BackendError>> + Send>;

/// Where a streamed response's events come from.
enum Source {
    /// A generation the engine has already accepted.
    Running(GenerationStream),
    /// A place in the queue, and what to do when it comes up.
    Queued {
        ticket: Ticket,
        start: StartGeneration,
        /// When to stop waiting and say so.
        deadline: Instant,
        /// How often to say where this request stands.
        notice_interval: Duration,
    },
    /// Being transitioned between the two above.
    InTransit,
}

/// State carried while encoding one streamed completion.
struct Encoder {
    source: Source,
    builder: ChunkBuilder,
    guard: RequestGuard,
    pending: VecDeque<String>,
    include_usage: bool,
    /// False until the first event arrives; keep-alives stop then.
    started: bool,
    finished: bool,
    usage: Option<UsageBody>,
    timings: Option<serde_json::Value>,
    outcome: Outcome,
}

/// Encode a running generation into SSE frames.
///
/// The returned stream yields whole frames, in the order spec section 12
/// requires, and ends with `data: [DONE]`.
pub fn encode(
    events: GenerationStream,
    builder: ChunkBuilder,
    guard: RequestGuard,
    include_usage: bool,
) -> impl futures_util::Stream<Item = Result<String, std::convert::Infallible>> + Send {
    encode_source(Source::Running(events), builder, guard, include_usage)
}

/// Encode a response for a request that is still waiting for a slot.
///
/// The difference this makes is the whole of the streamed half of M5. Waiting
/// for admission *before* answering means a queued client sees nothing at all —
/// no headers, no bytes — for however long the request ahead of it takes, which
/// on this hardware is minutes and which every client's read timeout eventually
/// reads as a hung server. Answering first and waiting inside the response
/// turns that silence into a position that visibly counts down.
///
/// The cost is stated rather than hidden: once the response has started, a
/// failure to *begin* generating can no longer be an HTTP status and arrives as
/// this module's terminal error chunk instead. That trade is only made for a
/// request that was genuinely queued — an uncontended one still takes its slot
/// first and keeps the status codes — so the common path is unchanged.
pub fn encode_queued(
    ticket: Ticket,
    start: StartGeneration,
    deadline: Instant,
    notice_interval: Duration,
    builder: ChunkBuilder,
    guard: RequestGuard,
    include_usage: bool,
) -> impl futures_util::Stream<Item = Result<String, std::convert::Infallible>> + Send {
    encode_source(
        Source::Queued {
            ticket,
            start,
            deadline,
            notice_interval,
        },
        builder,
        guard,
        include_usage,
    )
}

fn encode_source(
    source: Source,
    builder: ChunkBuilder,
    guard: RequestGuard,
    include_usage: bool,
) -> impl futures_util::Stream<Item = Result<String, std::convert::Infallible>> + Send {
    let mut encoder = Encoder {
        source,
        builder,
        guard,
        pending: VecDeque::new(),
        include_usage,
        started: false,
        finished: false,
        usage: None,
        timings: None,
        outcome: Outcome::default(),
    };
    // The role chunk goes out before anything is generated. It costs one frame
    // and proves to the client that the request was accepted, which matters
    // when the next thing to happen is a minute of prefill.
    let role_frame = encoder.builder.role().to_sse_frame();
    encoder.pending.push_back(role_frame);

    stream::unfold(encoder, |mut encoder| async move {
        loop {
            if let Some(frame) = encoder.pending.pop_front() {
                return Some((Ok(frame), encoder));
            }
            if encoder.finished {
                return None;
            }

            if matches!(encoder.source, Source::Queued { .. }) {
                wait_for_a_slot(&mut encoder).await;
                continue;
            }

            let Source::Running(events) = &mut encoder.source else {
                // Not queued and not running: admission failed and said so in
                // a frame already queued above.
                close(&mut encoder, FinishReason::Error);
                continue;
            };

            // A keep-alive is only sent while the engine is still reading the
            // prompt. Once tokens flow, the stream speaks for itself.
            let next = if encoder.started {
                events.next().await
            } else {
                tokio::select! {
                    event = events.next() => event,
                    () = tokio::time::sleep(KEEP_ALIVE_INTERVAL) => {
                        let ping = encoder.builder.keep_alive();
                        encoder.pending.push_back(ping);
                        continue;
                    }
                }
            };

            match next {
                Some(Ok(event)) => {
                    encoder.started = true;
                    absorb(&mut encoder, event);
                }
                Some(Err(err)) => {
                    fail(&mut encoder, &err);
                }
                None => {
                    // The backend's stream ended without a finish event, which
                    // means it stopped early. Close the stream properly rather
                    // than leaving the client waiting on a connection that
                    // will never say anything else.
                    close(&mut encoder, FinishReason::Length);
                }
            }
        }
    })
}

/// Wait for admission, telling the client where it stands while it waits.
///
/// Leaves `encoder.source` as [`Source::Running`] when the generation has
/// started, or queues the frames that end the stream when it cannot.
async fn wait_for_a_slot(encoder: &mut Encoder) {
    let Source::Queued {
        mut ticket,
        start,
        deadline,
        notice_interval,
    } = std::mem::replace(&mut encoder.source, Source::InTransit)
    else {
        return;
    };

    tokio::select! {
        granted = ticket.granted() => {
            let waited = ticket.waited();
            match granted {
                Some(permit) => {
                    drop(ticket);
                    tracing::info!(
                        target: targets::SCHEDULER,
                        id = encoder.builder.id(),
                        waited_ms = waited.as_millis() as u64,
                        "admitted after waiting"
                    );
                    encoder.guard.admitted(permit);
                    encoder.guard.waited(waited);
                    match start().await {
                        Ok(events) => encoder.source = Source::Running(events),
                        Err(err) => fail(encoder, &err),
                    }
                }
                // The scheduler is gone, which happens only at shutdown.
                None => fail(encoder, &BackendError::Cancelled),
            }
        }
        () = tokio::time::sleep_until(deadline) => {
            tracing::warn!(
                target: targets::SCHEDULER,
                id = encoder.builder.id(),
                waited_ms = ticket.waited().as_millis() as u64,
                position = ticket.position(),
                "gave up waiting for a slot"
            );
            drop(ticket);
            fail_with(encoder, "server_busy", queue_timeout_envelope());
        }
        () = tokio::time::sleep(notice_interval) => {
            let notice = encoder.builder.queued(ticket.position(), ticket.waited());
            encoder.pending.push_back(notice);
            encoder.source = Source::Queued { ticket, start, deadline, notice_interval };
        }
    }
}

fn absorb(encoder: &mut Encoder, event: GenerationEvent) {
    match event {
        GenerationEvent::Started { .. } => {}
        GenerationEvent::ContentDelta { text } => {
            encoder.outcome.content_chunks += 1;
            encoder.guard.first_token();
            let frame = encoder.builder.content(text).to_sse_frame();
            encoder.pending.push_back(frame);
        }
        GenerationEvent::ReasoningDelta { text } => {
            // Reasoning counts as a first token. It is output, it reaches the
            // client, and a thinking model can spend a whole budget in it — a
            // latency measured only to the first *visible* token would report
            // minutes of silence that the client was not experiencing.
            encoder.guard.first_token();
            let frame = encoder.builder.reasoning(text).to_sse_frame();
            encoder.pending.push_back(frame);
        }
        GenerationEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments,
        } => {
            encoder.outcome.tool_call_chunks += 1;
            encoder.guard.first_token();
            let frame = encoder
                .builder
                .tool_call(index, id, name, arguments)
                .to_sse_frame();
            encoder.pending.push_back(frame);
        }
        GenerationEvent::Timings(timings) => {
            // With per-token timings enabled upstream this arrives once per
            // token rather than once per generation, and each one supersedes
            // the last. That is the point: a generation the client abandons
            // half way still leaves behind what it had cost so far, where
            // before there was nothing until the final chunk that never came.
            let record = encoder.guard.record_mut();
            record.prefill = Some(Duration::from_secs_f64(timings.prompt_ms / 1000.0));
            record.decode = Some(Duration::from_secs_f64(timings.predicted_ms / 1000.0));
            record.cached_tokens = timings.cached_n;
            // Overwritten by every timing, and finally by the usage chunk when
            // one arrives. Taking only the first — which an `is_empty` guard
            // here quietly did — makes an abandoned generation report the one
            // token it had produced when the first timing came, rather than the
            // twenty it had produced when the client walked away. Measured
            // against a real engine: 1 token reported for an eight-second
            // generation, before this line was a plain assignment.
            record.completion_tokens = timings.predicted_n;
            encoder.timings = serde_json::to_value(timings).ok();
        }
        GenerationEvent::Finished {
            finish_reason,
            usage,
        } => {
            let record = encoder.guard.record_mut();
            record.prompt_tokens = usage.prompt_tokens;
            record.completion_tokens = usage.completion_tokens;
            if usage.cached_tokens > 0 {
                record.cached_tokens = usage.cached_tokens;
            }
            encoder.usage = Some(UsageBody::from(usage));
            close(encoder, finish_reason);
        }
    }
}

/// End the stream after a failure that arrived once bytes were already sent.
fn fail(encoder: &mut Encoder, err: &BackendError) {
    fail_with(encoder, err.code(), ErrorEnvelope::from_error(err));
}

/// The same, for a refusal that is the gateway's own rather than a backend's.
fn fail_with(encoder: &mut Encoder, code: &str, envelope: ErrorEnvelope) {
    tracing::warn!(
        target: targets::INFERENCE,
        code,
        "generation failed after the response had started"
    );
    encoder.guard.record_mut().finish_reason = Some(FinishReason::Error);
    let frame = encoder.builder.error(envelope.to_value()).to_sse_frame();
    encoder.pending.push_back(frame);
    encoder.pending.push_back(encoder.builder.done());
    encoder.finished = true;
}

/// The refusal a queued request gets when its wait runs out.
///
/// Word for word the body the non-streamed path returns with a 503, and the
/// same `server_busy` code: whether a client was queued before or after the
/// headers went out is our implementation detail, and it must not change what
/// the client is told went wrong.
fn queue_timeout_envelope() -> ErrorEnvelope {
    ErrorEnvelope::invalid_request(
        "the gateway is busy with another request; try again shortly",
        "server_busy",
    )
}

/// Emit the finish chunk, the usage chunk and `[DONE]`.
fn close(encoder: &mut Encoder, finish_reason: FinishReason) {
    if encoder.finished {
        return;
    }
    encoder.guard.record_mut().finish_reason = Some(finish_reason);
    let frame = encoder.builder.finish(finish_reason).to_sse_frame();
    encoder.pending.push_back(frame);

    if encoder.include_usage {
        let usage = encoder.usage.unwrap_or_default();
        let frame = encoder
            .builder
            .usage(usage, encoder.timings.clone())
            .to_sse_frame();
        encoder.pending.push_back(frame);
    }
    encoder.pending.push_back(encoder.builder.done());
    encoder.finished = true;

    if encoder.outcome.content_chunks == 0 && encoder.outcome.tool_call_chunks == 0 {
        // A stream with no content and no tool calls is what makes a client
        // raise `EmptyStreamError` and retry blindly. We never fabricate a
        // token to avoid it — that would be lying about what the model said —
        // but it is always worth a line in the log, because the cause is
        // usually a sampler or template problem that is otherwise invisible.
        tracing::warn!(
            target: targets::INFERENCE,
            id = encoder.builder.id(),
            finish_reason = finish_reason.as_str(),
            "the model produced no content and no tool calls"
        );
    }

    // Cancellation is idempotent and the permit is released either way; this
    // just does it as soon as the work is done rather than when the client
    // finishes reading.
    encoder.guard.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream as futures_stream;
    use hermes_core::{SseDecoder, SseEvent};
    use hermes_inference::generation::Usage;

    fn events(items: Vec<Result<GenerationEvent, BackendError>>) -> GenerationStream {
        futures_stream::iter(items).boxed()
    }

    async fn collect(
        items: Vec<Result<GenerationEvent, BackendError>>,
        include_usage: bool,
    ) -> Vec<SseEvent> {
        let guard = RequestGuard::new(CancellationToken::new(), None);
        let builder = ChunkBuilder::new("chatcmpl-test", "m@4k");
        let frames: Vec<String> = encode(events(items), builder, guard, include_usage)
            .filter_map(|frame| async move { frame.ok() })
            .collect()
            .await;

        let mut decoder = SseDecoder::new();
        for frame in &frames {
            decoder.feed(frame.as_bytes()).expect("decode");
        }
        assert!(!decoder.has_pending(), "a frame was left half-written");
        decoder.drain()
    }

    fn json(event: &SseEvent) -> serde_json::Value {
        serde_json::from_str(&event.data).expect("chunk json")
    }

    #[tokio::test]
    async fn the_chunk_order_is_role_content_finish_usage_done() {
        // This sequence is the contract. Every element of it is read by the
        // client, and the usage chunk is read from nowhere else.
        let events = collect(
            vec![
                Ok(GenerationEvent::Started {
                    prompt_tokens: Some(11),
                }),
                Ok(GenerationEvent::ContentDelta { text: "Hi".into() }),
                Ok(GenerationEvent::ContentDelta { text: "!".into() }),
                Ok(GenerationEvent::Finished {
                    finish_reason: FinishReason::Stop,
                    usage: Usage::new(11, 2),
                }),
            ],
            true,
        )
        .await;

        assert_eq!(json(&events[0])["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(json(&events[1])["choices"][0]["delta"]["content"], "Hi");
        assert_eq!(json(&events[2])["choices"][0]["delta"]["content"], "!");
        assert_eq!(json(&events[3])["choices"][0]["finish_reason"], "stop");
        let usage = json(&events[4]);
        assert_eq!(usage["choices"], serde_json::json!([]));
        assert_eq!(usage["usage"]["prompt_tokens"], 11);
        assert_eq!(usage["usage"]["completion_tokens"], 2);
        assert!(events[5].is_done());
        assert_eq!(events.len(), 6);
    }

    #[tokio::test]
    async fn the_usage_chunk_is_omitted_unless_it_was_asked_for() {
        let events = collect(
            vec![
                Ok(GenerationEvent::ContentDelta { text: "x".into() }),
                Ok(GenerationEvent::Finished {
                    finish_reason: FinishReason::Stop,
                    usage: Usage::new(1, 1),
                }),
            ],
            false,
        )
        .await;
        assert!(
            events.iter().all(|event| event.is_done()
                || !json(event)
                    .as_object()
                    .is_some_and(|chunk| chunk.contains_key("usage"))),
            "usage was sent without stream_options.include_usage"
        );
        assert!(events.last().expect("frames").is_done());
    }

    #[tokio::test]
    async fn timings_ride_along_with_the_usage_chunk() {
        let events = collect(
            vec![
                Ok(GenerationEvent::ContentDelta { text: "x".into() }),
                Ok(GenerationEvent::Timings(
                    hermes_inference::generation::Timings {
                        prompt_n: 36,
                        prompt_ms: 2040.73,
                        predicted_n: 3,
                        predicted_ms: 171.5,
                        cached_n: 0,
                    },
                )),
                Ok(GenerationEvent::Finished {
                    finish_reason: FinishReason::Stop,
                    usage: Usage::new(36, 3),
                }),
            ],
            true,
        )
        .await;
        let usage_chunk = json(&events[events.len() - 2]);
        assert_eq!(usage_chunk["timings"]["prompt_n"], 36);
        assert_eq!(usage_chunk["timings"]["predicted_n"], 3);
    }

    #[tokio::test]
    async fn an_abandoned_generation_reports_what_it_had_cost_so_far() {
        // With per-token timings each one supersedes the last. Keeping only the
        // first — which is what a "set it if unset" guard here did — made an
        // eight-second generation report the single token it had produced when
        // the first timing arrived. Found by running it against a real engine,
        // not by a type error.
        fn timings(predicted_n: u32, predicted_ms: f64) -> GenerationEvent {
            GenerationEvent::Timings(hermes_inference::generation::Timings {
                prompt_n: 10,
                prompt_ms: 1000.0,
                predicted_n,
                predicted_ms,
                cached_n: 4,
            })
        }

        let metrics = Arc::new(Metrics::new());
        let guard =
            RequestGuard::new(CancellationToken::new(), None).reporting_to(Arc::clone(&metrics));
        let builder = ChunkBuilder::new("chatcmpl-test", "m@4k");
        let events = events(vec![
            Ok(GenerationEvent::ContentDelta { text: "a".into() }),
            Ok(timings(1, 500.0)),
            Ok(GenerationEvent::ContentDelta { text: "b".into() }),
            Ok(timings(20, 8000.0)),
            Ok(GenerationEvent::ContentDelta { text: "c".into() }),
            Ok(GenerationEvent::Finished {
                finish_reason: FinishReason::Stop,
                usage: Usage::new(10, 21),
            }),
        ]);

        // Read the role chunk and three content chunks, then walk away: no
        // finish chunk is ever read, which is what a disconnect looks like.
        let mut body = Box::pin(encode(events, builder, guard, true));
        for _ in 0..4 {
            assert!(body.next().await.is_some());
        }
        drop(body);

        let snapshot = metrics.snapshot(Default::default(), None);
        assert_eq!(
            snapshot.tokens.decoded, 20,
            "the last timing is the one that counts"
        );
        assert_eq!(snapshot.decode.total_ms, 8000);
        assert_eq!(snapshot.tokens.cached, 4);
        assert_eq!(
            snapshot.finish_reasons.cancelled, 1,
            "a client that walked away is not an error"
        );
        assert_eq!(snapshot.finish_reasons.error, 0);
    }

    #[tokio::test]
    async fn a_failure_mid_stream_ends_with_an_error_chunk_and_done() {
        // Not a dropped connection: that reads as a truncated stream and makes
        // the client retry blindly.
        let events = collect(
            vec![
                Ok(GenerationEvent::ContentDelta {
                    text: "partial".into(),
                }),
                Err(BackendError::EngineCrashed {
                    detail: "signal 9".into(),
                    exit_code: None,
                    signal: Some(9),
                    tail: vec![],
                }),
            ],
            true,
        )
        .await;

        let error_chunk = json(&events[events.len() - 2]);
        assert_eq!(error_chunk["choices"][0]["finish_reason"], "error");
        assert_eq!(error_chunk["error"]["code"], "engine_crashed");
        assert!(events.last().expect("frames").is_done());
    }

    #[tokio::test]
    async fn a_backend_stream_that_just_stops_is_still_closed_properly() {
        let events = collect(
            vec![Ok(GenerationEvent::ContentDelta { text: "x".into() })],
            true,
        )
        .await;
        // "length" rather than "stop": we do not know the model chose to end.
        assert_eq!(
            json(&events[events.len() - 3])["choices"][0]["finish_reason"],
            "length"
        );
        assert!(events.last().expect("frames").is_done());
    }

    #[tokio::test]
    async fn an_empty_generation_still_produces_a_well_formed_stream() {
        // The client will treat this as an empty response and retry; what it
        // must not get is a malformed or truncated one.
        let events = collect(
            vec![Ok(GenerationEvent::Finished {
                finish_reason: FinishReason::Stop,
                usage: Usage::new(5, 0),
            })],
            true,
        )
        .await;
        assert_eq!(json(&events[0])["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(json(&events[1])["choices"][0]["finish_reason"], "stop");
        assert!(events.last().expect("frames").is_done());
    }

    #[tokio::test]
    async fn tool_call_deltas_are_re_emitted_with_the_id_sent_once() {
        let events = collect(
            vec![
                Ok(GenerationEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("read_file".into()),
                    arguments: Some(String::new()),
                }),
                Ok(GenerationEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    arguments: Some("{\"path\":\"a\"}".into()),
                }),
                Ok(GenerationEvent::Finished {
                    finish_reason: FinishReason::ToolCalls,
                    usage: Usage::new(9, 2),
                }),
            ],
            false,
        )
        .await;

        let first = json(&events[1]);
        assert_eq!(
            first["choices"][0]["delta"]["tool_calls"][0]["id"],
            "call_1"
        );
        let second = json(&events[2]);
        assert!(
            second["choices"][0]["delta"]["tool_calls"][0]
                .get("id")
                .is_none()
        );
        assert_eq!(
            second["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );
        assert_eq!(
            json(&events[3])["choices"][0]["finish_reason"],
            "tool_calls"
        );
    }

    #[tokio::test]
    async fn a_slow_prefill_is_kept_alive_with_comment_frames() {
        // The failure this prevents: a socket idle for a whole minute while a
        // long prompt is processed, dropped by something in between.
        tokio::time::pause();
        let guard = RequestGuard::new(CancellationToken::new(), None);
        let builder = ChunkBuilder::new("chatcmpl-test", "m@4k");

        // A stream whose first event only arrives after a long prefill.
        let slow = stream::once(async {
            tokio::time::sleep(Duration::from_secs(65)).await;
            Ok(GenerationEvent::Finished {
                finish_reason: FinishReason::Stop,
                usage: Usage::new(9000, 0),
            })
        })
        .boxed();

        let frames: Vec<String> = encode(slow, builder, guard, false)
            .filter_map(|frame| async move { frame.ok() })
            .collect()
            .await;

        let pings = frames.iter().filter(|frame| frame.starts_with(':')).count();
        assert_eq!(pings, 4, "expected a ping every 15s of a 65s prefill");
        // And none after the first event: once tokens flow the stream speaks
        // for itself.
        let last_ping = frames.iter().rposition(|frame| frame.starts_with(':'));
        let first_data = frames.iter().position(|frame| frame.starts_with("data"));
        assert!(matches!((last_ping, first_data), (Some(ping), Some(data)) if ping > data));
        assert!(
            frames
                .iter()
                .skip(last_ping.unwrap_or(0) + 1)
                .all(|frame| frame.starts_with("data"))
        );
    }

    #[tokio::test]
    async fn dropping_the_stream_releases_the_slot_and_cancels_the_work() {
        // The property the whole cancellation design rests on: a client that
        // disappears must not leave a permit held or an engine decoding.
        let slots = crate::scheduler::Scheduler::new(1, Default::default());
        let permit = slots.try_admit().expect("permit");
        let cancel = CancellationToken::new();
        let guard = RequestGuard::new(cancel.clone(), Some(permit));
        let builder = ChunkBuilder::new("chatcmpl-test", "m@4k");

        let endless = stream::repeat_with(|| {
            Ok(GenerationEvent::ContentDelta {
                text: "tick".into(),
            })
        })
        .boxed();

        let mut body = Box::pin(encode(endless, builder, guard, false));
        assert!(body.next().await.is_some());
        assert_eq!(slots.snapshot().running, 1);

        drop(body);
        assert!(cancel.is_cancelled(), "the work was not cancelled");
        assert_eq!(slots.snapshot().running, 0, "the slot leaked");
    }
}
