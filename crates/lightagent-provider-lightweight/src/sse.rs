//! A self-contained incremental server-sent event decoder.
//!
//! Reproduced from `lightweight-core::sse` rather than imported, because the
//! adapter must not depend on any `lightweight-*` crate. The rules are exactly
//! the ones the gateway's emitter relies on, so this reader can consume what
//! that writer produces:
//!
//! * A frame ends at a blank line. `\n`, `\r\n` and a bare `\r` all end a line,
//!   and the four blank-line terminators are matched longest-first so
//!   `\r\n\r\n` is not mistaken for `\n\r\n` with a stray `\r` left over.
//! * `data:` may appear repeatedly; the values are joined with `\n`.
//! * Exactly one leading space after the colon is stripped, so `data: {}` and
//!   `data:{}` carry the same payload.
//! * A line beginning with `:` is a comment and dispatches nothing — this is
//!   what makes keep-alive pings and `: queued position=…` frames invisible.
//! * A frame carrying no `data` field dispatches nothing.
//! * A single frame is capped; a peer that never sends a blank line is refused
//!   rather than buffered until the machine runs out of memory.

use std::collections::VecDeque;

/// The terminal payload of an OpenAI-compatible stream.
pub const DONE_DATA: &str = "[DONE]";

/// Default cap on a single frame: generous for any real chunk, small enough to
/// stop a runaway peer early.
const DEFAULT_FRAME_LIMIT: usize = 8 * 1024 * 1024;

/// One decoded event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

impl SseEvent {
    pub fn data(payload: impl Into<String>) -> Self {
        Self {
            event: None,
            data: payload.into(),
            id: None,
        }
    }

    /// Whether this is the `[DONE]` sentinel that ends the stream.
    pub fn is_done(&self) -> bool {
        self.data.trim() == DONE_DATA
    }
}

/// Why a stream could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SseDecodeError {
    #[error("a server-sent event exceeded {limit} bytes without ending")]
    FrameTooLarge { limit: usize },
}

/// Incremental server-sent event decoder. Fed arbitrary byte chunks, yields
/// whole events; chunk boundaries are meaningless.
#[derive(Debug)]
pub struct SseDecoder {
    buffer: String,
    ready: VecDeque<SseEvent>,
    limit: usize,
    poisoned: bool,
}

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

    /// Feed the next bytes off the wire. Invalid UTF-8 is replaced, not
    /// rejected: the payload is model output, and a split multi-byte character
    /// is whole again by the time its frame completes at a blank line.
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

    /// Whether a partial frame is still buffered (a truncated stream, at EOF).
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
    frame_bytes: usize,
    content_bytes: usize,
}

/// Find the blank line terminating the first frame, longest terminator first.
fn find_frame_end(buffer: &str) -> Option<FrameEnd> {
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

/// Turn one frame's lines into an event, or `None` when it carries no `data`.
fn parse_frame(frame: &str) -> Option<SseEvent> {
    let mut event = SseEvent::default();
    let mut has_data = false;

    for line in frame.split(['\n', '\r']).filter(|line| !line.is_empty()) {
        if line.starts_with(':') {
            // A comment: keep-alive or `: queued position=…`. Dispatches
            // nothing, by design.
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
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
            _ => {}
        }
    }

    has_data.then_some(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut decoder = SseDecoder::new();
        for chunk in chunks {
            decoder.feed(chunk).expect("decode");
        }
        decoder.drain()
    }

    #[test]
    fn a_frame_split_across_chunks_reassembles() {
        let events = decode_all(&[b"data: {\"hel", b"lo\":\"world\"}\n", b"\n"]);
        assert_eq!(events, vec![SseEvent::data("{\"hello\":\"world\"}")]);
    }

    #[test]
    fn a_keepalive_ping_dispatches_nothing() {
        let events = decode_all(&[b": ping\n\n", b"data: real\n\n"]);
        assert_eq!(events, vec![SseEvent::data("real")]);
    }

    #[test]
    fn a_queued_position_comment_dispatches_nothing() {
        let events = decode_all(&[b": queued position=3 waited=5s\n\n", b"data: real\n\n"]);
        assert_eq!(events, vec![SseEvent::data("real")]);
    }

    #[test]
    fn the_done_sentinel_is_recognized() {
        let events = decode_all(&[b"data: [DONE]\n\n"]);
        assert!(events[0].is_done());
        assert!(!SseEvent::data("{}").is_done());
    }

    #[test]
    fn repeated_data_fields_join_with_newlines() {
        let events = decode_all(&[b"data: first\ndata: second\n\n"]);
        assert_eq!(events, vec![SseEvent::data("first\nsecond")]);
    }

    #[test]
    fn only_one_leading_space_is_stripped() {
        let events = decode_all(&[b"data:  leading\n\n"]);
        assert_eq!(events, vec![SseEvent::data(" leading")]);
    }

    #[test]
    fn crlf_endings_decode_the_same_as_lf() {
        let events = decode_all(&[b"data: {\"a\":1}\r\n\r\n"]);
        assert_eq!(events, vec![SseEvent::data("{\"a\":1}")]);
    }

    #[test]
    fn an_oversize_frame_is_refused_rather_than_buffered() {
        let mut decoder = SseDecoder::with_limit(64);
        let err = decoder.feed(&vec![b'x'; 256]).expect_err("must refuse");
        assert_eq!(err, SseDecodeError::FrameTooLarge { limit: 64 });
        // And stays refused.
        assert!(decoder.feed(b"data: x\n\n").is_err());
    }
}
