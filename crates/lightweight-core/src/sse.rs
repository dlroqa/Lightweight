//! Server-sent event framing.
//!
//! Both halves of the gateway need this and they need it to agree. We *decode*
//! the engine's stream and we *encode* our own, and the two must round-trip:
//! anything we can read from an engine we must be able to re-emit without
//! losing a byte, because spec section 12's chunk contract is defined in terms
//! of what the client finally receives.
//!
//! It lives in `lightweight-core` — pure, no I/O, no async — because the alternative
//! was two implementations, one in the backend and one in the API layer, which
//! is exactly how a framing bug survives a test suite.
//!
//! The rules implemented here are the ones the format actually specifies, and
//! the ones the client SDKs rely on:
//!
//! * A frame ends at a blank line. `\n`, `\r\n` and a bare `\r` all end a line.
//! * `data:` may appear repeatedly; the values are joined with `\n`.
//! * A single leading space after the colon is part of the syntax, not the
//!   data, so `data: {}` and `data:{}` carry the same payload.
//! * A line beginning with `:` is a comment. The openai SDK's decoder ignores
//!   them, which is what makes them usable as keep-alives during a long
//!   prefill — the one thing standing between a 90-second first token and a
//!   client that gives up on an idle socket.
//! * A frame carrying no `data` field dispatches nothing.

use std::collections::VecDeque;

/// The terminal frame of an OpenAI-compatible stream.
pub const DONE_DATA: &str = "[DONE]";

/// One decoded event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, when the sender set one.
    pub event: Option<String>,
    /// The `data:` payload, with multi-line values already joined.
    pub data: String,
    /// The `id:` field, when the sender set one.
    pub id: Option<String>,
}

impl SseEvent {
    /// A plain data-only event.
    pub fn data(payload: impl Into<String>) -> Self {
        Self {
            event: None,
            data: payload.into(),
            id: None,
        }
    }

    /// Whether this is the `[DONE]` sentinel that ends an OpenAI stream.
    pub fn is_done(&self) -> bool {
        self.data.trim() == DONE_DATA
    }
}

/// Why a stream could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SseDecodeError {
    /// A single frame exceeded the cap without ever ending.
    ///
    /// Not a theoretical concern: the decoder buffers until it sees a blank
    /// line, so a peer that never sends one — a proxy streaming an HTML error
    /// page, an engine writing a stack trace — would otherwise grow the buffer
    /// until the machine ran out of memory. On a box whose whole reason for
    /// existing is fitting in constrained RAM, that is a real failure mode.
    #[error("a server-sent event exceeded {limit} bytes without ending")]
    FrameTooLarge { limit: usize },
}

/// Incremental server-sent event decoder.
///
/// Fed arbitrary byte chunks as they arrive off the socket, and yields whole
/// events. Chunk boundaries are meaningless — a frame routinely arrives split
/// across two TCP segments, and the decoder that assumes otherwise works
/// perfectly until the first slow network.
#[derive(Debug)]
pub struct SseDecoder {
    buffer: String,
    ready: VecDeque<SseEvent>,
    limit: usize,
    /// Set once the cap is exceeded, so every later call keeps failing rather
    /// than silently resuming mid-frame with a corrupt payload.
    poisoned: bool,
}

/// Default cap on a single frame.
///
/// Generous — a chat completion chunk is a few hundred bytes, and even a whole
/// non-streamed response with a long tool-call argument list stays well under
/// this — but small enough that a runaway peer is stopped early.
const DEFAULT_FRAME_LIMIT: usize = 8 * 1024 * 1024;

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::with_limit(DEFAULT_FRAME_LIMIT)
    }

    pub fn with_limit(limit: usize) -> Self {
        Self {
            buffer: String::new(),
            ready: VecDeque::new(),
            limit,
            poisoned: false,
        }
    }

    /// Feed the next bytes off the wire.
    ///
    /// Invalid UTF-8 is replaced rather than rejected. The payloads are model
    /// output, and losing a whole conversation to one malformed byte would be
    /// a worse outcome than a replacement character in it. A split multi-byte
    /// character across chunks is handled by holding the partial frame: a
    /// frame only completes at a blank line, by which point the character is
    /// whole.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), SseDecodeError> {
        if self.poisoned {
            return Err(SseDecodeError::FrameTooLarge { limit: self.limit });
        }
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        self.extract()
    }

    /// Take the next decoded event, if one is ready.
    pub fn next_event(&mut self) -> Option<SseEvent> {
        self.ready.pop_front()
    }

    /// Take everything decoded so far.
    pub fn drain(&mut self) -> Vec<SseEvent> {
        self.ready.drain(..).collect()
    }

    /// Whether a partial frame is still buffered.
    ///
    /// True at the end of a stream means the peer stopped mid-frame — a
    /// truncated connection rather than a clean end.
    pub fn has_pending(&self) -> bool {
        !self.buffer.is_empty()
    }

    fn extract(&mut self) -> Result<(), SseDecodeError> {
        loop {
            let Some(end) = find_frame_end(&self.buffer) else {
                if self.buffer.len() > self.limit {
                    self.poisoned = true;
                    self.buffer.clear();
                    return Err(SseDecodeError::FrameTooLarge { limit: self.limit });
                }
                return Ok(());
            };
            let frame: String = self.buffer.drain(..end.frame_bytes).collect();
            let frame = &frame[..end.content_bytes];
            if let Some(event) = parse_frame(frame) {
                self.ready.push_back(event);
            }
        }
    }
}

/// Where a frame ends inside the buffer.
struct FrameEnd {
    /// Bytes to remove, including the blank-line terminator.
    frame_bytes: usize,
    /// Bytes of that which are the frame's own lines.
    content_bytes: usize,
}

/// Find the blank line that terminates the first frame.
fn find_frame_end(buffer: &str) -> Option<FrameEnd> {
    // Ordered longest-first: "\r\n\r\n" also contains "\n\r\n", and matching
    // the shorter one first would leave a stray "\r" at the head of the next
    // frame, which then reads as an empty leading line.
    const TERMINATORS: [&str; 4] = ["\r\n\r\n", "\n\n", "\r\r", "\n\r\n"];
    TERMINATORS
        .iter()
        .filter_map(|terminator| {
            buffer
                .find(terminator)
                .map(|at| (at, at + terminator.len()))
        })
        .min_by_key(|(at, _)| *at)
        .map(|(content_bytes, frame_bytes)| FrameEnd {
            frame_bytes,
            content_bytes,
        })
}

/// Turn one frame's lines into an event.
///
/// Returns `None` for a frame that carries no `data` field at all — a
/// comment-only keep-alive, or a frame of fields we do not use. The format
/// says such a frame dispatches nothing, and a caller that saw an event with
/// an empty payload would have no way to tell it apart from a real one.
fn parse_frame(frame: &str) -> Option<SseEvent> {
    let mut event = SseEvent::default();
    let mut has_data = false;

    for line in frame.split(['\n', '\r']).filter(|line| !line.is_empty()) {
        // A comment. Deliberately not surfaced: keep-alives exist to be
        // invisible above this layer.
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            // A line with no colon is a field with an empty value.
            None => (line, ""),
        };
        match field {
            "data" => {
                if has_data {
                    event.data.push('\n');
                }
                event.data.push_str(value);
                has_data = true;
            }
            "event" => event.event = Some(value.to_owned()),
            "id" => event.id = Some(value.to_owned()),
            // `retry` and anything unrecognized are ignored, per the format.
            _ => {}
        }
    }

    has_data.then_some(event)
}

/// Write one `data:` frame.
///
/// Multi-line payloads are split across repeated `data:` lines, which is what
/// the format requires — a raw newline inside a value would end the frame and
/// truncate the payload. Our own chunks are single-line JSON, but this is the
/// encoder for anything we emit and it must not be the reason a future
/// pretty-printed payload silently breaks.
pub fn encode_data(payload: &str) -> String {
    let mut out = String::with_capacity(payload.len() + 8);
    for line in payload.split('\n') {
        out.push_str("data: ");
        out.push_str(line.trim_end_matches('\r'));
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Write a comment frame.
///
/// Used as a keep-alive during prefill. The openai SDK's decoder ignores
/// comment lines, so this reaches the socket — proving the connection is alive
/// — without appearing to the client as content.
pub fn encode_comment(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 4);
    for line in text.split('\n') {
        out.push_str(": ");
        out.push_str(line.trim_end_matches('\r'));
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Write the terminal `data: [DONE]` frame.
pub fn encode_done() -> String {
    encode_data(DONE_DATA)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(input: &[&[u8]]) -> Vec<SseEvent> {
        let mut decoder = SseDecoder::new();
        for chunk in input {
            decoder.feed(chunk).expect("decode");
        }
        decoder.drain()
    }

    #[test]
    fn a_simple_frame_decodes_to_its_payload() {
        let events = decode_all(&[b"data: {\"a\":1}\n\n"]);
        assert_eq!(events, vec![SseEvent::data("{\"a\":1}")]);
    }

    #[test]
    fn a_frame_split_across_chunks_still_decodes() {
        // The case that matters on a real socket: a chunk boundary lands in
        // the middle of a frame. A decoder that assumed whole frames per read
        // would work locally and fail over a network.
        let events = decode_all(&[b"data: {\"hel", b"lo\":\"world\"}\n", b"\n"]);
        assert_eq!(events, vec![SseEvent::data("{\"hello\":\"world\"}")]);
    }

    #[test]
    fn several_frames_in_one_chunk_all_decode() {
        let events = decode_all(&[b"data: one\n\ndata: two\n\ndata: three\n\n"]);
        assert_eq!(
            events,
            vec![
                SseEvent::data("one"),
                SseEvent::data("two"),
                SseEvent::data("three"),
            ]
        );
    }

    #[test]
    fn crlf_line_endings_decode_the_same_as_lf() {
        let events = decode_all(&[b"data: {\"a\":1}\r\n\r\n"]);
        assert_eq!(events, vec![SseEvent::data("{\"a\":1}")]);
    }

    #[test]
    fn repeated_data_fields_are_joined_with_newlines() {
        let events = decode_all(&[b"data: first\ndata: second\n\n"]);
        assert_eq!(events, vec![SseEvent::data("first\nsecond")]);
    }

    #[test]
    fn only_one_leading_space_is_stripped() {
        // `data:  x` carries " x", not "x". Getting this wrong would eat a
        // leading space out of every model token that starts with one — which
        // is most of them.
        let events = decode_all(&[b"data:  leading\n\n"]);
        assert_eq!(events, vec![SseEvent::data(" leading")]);
    }

    #[test]
    fn a_comment_frame_dispatches_nothing() {
        // Keep-alives must be invisible above this layer, or every ping would
        // look like an empty completion chunk.
        let events = decode_all(&[b": ping\n\n", b"data: real\n\n"]);
        assert_eq!(events, vec![SseEvent::data("real")]);
    }

    #[test]
    fn the_done_sentinel_is_recognized() {
        let events = decode_all(&[b"data: [DONE]\n\n"]);
        assert!(events[0].is_done());
        assert!(!SseEvent::data("{}").is_done());
    }

    #[test]
    fn event_and_id_fields_are_carried() {
        let events = decode_all(&[b"event: message\nid: 7\ndata: payload\n\n"]);
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].id.as_deref(), Some("7"));
        assert_eq!(events[0].data, "payload");
    }

    #[test]
    fn a_partial_frame_is_held_rather_than_dispatched() {
        let mut decoder = SseDecoder::new();
        decoder.feed(b"data: incomplete\n").expect("feed");
        assert!(decoder.next_event().is_none());
        assert!(decoder.has_pending(), "a truncated stream must be visible");
    }

    #[test]
    fn a_frame_that_never_ends_is_refused_rather_than_buffered_forever() {
        // A peer that never sends a blank line would otherwise grow the buffer
        // until the machine ran out of memory.
        let mut decoder = SseDecoder::with_limit(64);
        let err = decoder.feed(&vec![b'x'; 256]).expect_err("must refuse");
        assert_eq!(err, SseDecodeError::FrameTooLarge { limit: 64 });
        // And stays refused, rather than resuming mid-frame with a corrupt
        // payload.
        assert!(decoder.feed(b"data: x\n\n").is_err());
    }

    #[test]
    fn encoding_then_decoding_returns_the_payload() {
        let payload = r#"{"choices":[{"delta":{"content":"hi"}}]}"#;
        let events = decode_all(&[encode_data(payload).as_bytes()]);
        assert_eq!(events, vec![SseEvent::data(payload)]);
    }

    #[test]
    fn a_multi_line_payload_survives_a_round_trip() {
        // A raw newline inside a value would end the frame and truncate the
        // payload, so the encoder must split it across `data:` lines.
        let payload = "line one\nline two";
        let encoded = encode_data(payload);
        assert_eq!(encoded, "data: line one\ndata: line two\n\n");
        assert_eq!(
            decode_all(&[encoded.as_bytes()]),
            vec![SseEvent::data(payload)]
        );
    }

    #[test]
    fn frames_are_terminated_by_exactly_one_blank_line() {
        // Byte-exact: the client's decoder splits on it, and an extra newline
        // would be read as an empty frame.
        assert_eq!(encode_data("x"), "data: x\n\n");
        assert_eq!(encode_comment("ping"), ": ping\n\n");
        assert_eq!(encode_done(), "data: [DONE]\n\n");
    }

    #[test]
    fn an_empty_data_field_is_still_an_event() {
        // `data:` with nothing after it is a real event with an empty payload,
        // distinct from a comment, which is not an event at all.
        let events = decode_all(&[b"data:\n\n"]);
        assert_eq!(events, vec![SseEvent::data("")]);
    }
}
