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
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use hermes_api::chat::UsageBody;
use hermes_api::error::ErrorEnvelope;
use hermes_api::stream::ChunkBuilder;
use hermes_core::Actionable;
use hermes_inference::generation::{FinishReason, GenerationEvent};
use hermes_inference::{BackendError, GenerationStream};
use hermes_observability::targets;
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

/// How often a keep-alive goes out while the engine is still reading the
/// prompt.
///
/// Fifteen seconds is well inside the shortest idle timeout worth worrying
/// about, and costs six bytes.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Holds a request's slot and cancels its work when dropped.
///
/// The entire cancellation design rests on this type being owned by the
/// response body. Nothing needs to detect a disconnect: hyper drops the body,
/// the body drops this, and the token cancels — which drops the upstream
/// response, closes the connection to the engine, and stops it decoding.
#[derive(Debug)]
pub struct RequestGuard {
    cancel: CancellationToken,
    /// Held, never read. Dropping it is the point.
    _permit: Option<OwnedSemaphorePermit>,
}

impl RequestGuard {
    pub fn new(cancel: CancellationToken, permit: Option<OwnedSemaphorePermit>) -> Self {
        Self {
            cancel,
            _permit: permit,
        }
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// What the stream has produced so far, for the closing log line.
#[derive(Debug, Default)]
struct Outcome {
    content_chunks: u64,
    tool_call_chunks: u64,
}

/// State carried while encoding one streamed completion.
struct Encoder {
    events: GenerationStream,
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

/// Encode a generation into SSE frames.
///
/// The returned stream yields whole frames, in the order spec section 12
/// requires, and ends with `data: [DONE]`.
pub fn encode(
    events: GenerationStream,
    builder: ChunkBuilder,
    guard: RequestGuard,
    include_usage: bool,
) -> impl futures_util::Stream<Item = Result<String, std::convert::Infallible>> + Send {
    let mut encoder = Encoder {
        events,
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

            // A keep-alive is only sent while the engine is still reading the
            // prompt. Once tokens flow, the stream speaks for itself.
            let next = if encoder.started {
                encoder.events.next().await
            } else {
                tokio::select! {
                    event = encoder.events.next() => event,
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

fn absorb(encoder: &mut Encoder, event: GenerationEvent) {
    match event {
        GenerationEvent::Started { .. } => {}
        GenerationEvent::ContentDelta { text } => {
            encoder.outcome.content_chunks += 1;
            let frame = encoder.builder.content(text).to_sse_frame();
            encoder.pending.push_back(frame);
        }
        GenerationEvent::ReasoningDelta { text } => {
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
            let frame = encoder
                .builder
                .tool_call(index, id, name, arguments)
                .to_sse_frame();
            encoder.pending.push_back(frame);
        }
        GenerationEvent::Timings(timings) => {
            encoder.timings = serde_json::to_value(timings).ok();
        }
        GenerationEvent::Finished {
            finish_reason,
            usage,
        } => {
            encoder.usage = Some(UsageBody::from(usage));
            close(encoder, finish_reason);
        }
    }
}

/// End the stream after a failure that arrived once bytes were already sent.
fn fail(encoder: &mut Encoder, err: &BackendError) {
    tracing::warn!(
        target: targets::INFERENCE,
        code = err.code(),
        "generation failed after the response had started"
    );
    let body = ErrorEnvelope::from_error(err).to_value();
    let frame = encoder.builder.error(body).to_sse_frame();
    encoder.pending.push_back(frame);
    encoder.pending.push_back(encoder.builder.done());
    encoder.finished = true;
}

/// Emit the finish chunk, the usage chunk and `[DONE]`.
fn close(encoder: &mut Encoder, finish_reason: FinishReason) {
    if encoder.finished {
        return;
    }
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
    encoder.guard.cancel.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream as futures_stream;
    use hermes_core::{SseDecoder, SseEvent};
    use hermes_inference::generation::Usage;
    use std::sync::Arc;
    use tokio::sync::Semaphore;

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
        let slots = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&slots).acquire_owned().await.expect("permit");
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
        assert_eq!(slots.available_permits(), 0);

        drop(body);
        assert!(cancel.is_cancelled(), "the work was not cancelled");
        assert_eq!(slots.available_permits(), 1, "the slot leaked");
    }
}
