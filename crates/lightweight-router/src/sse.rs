//! Rewriting a node's event stream as it passes through.
//!
//! The router changes exactly one thing in a stream: the `model` field of each
//! JSON event, from the node's local name to the route's public one. Every
//! other byte is forwarded as the node sent it — keep-alive comments during a
//! long prefill, queue notices, `[DONE]`, an in-band error frame — because each
//! of those is part of a contract the node already honours and the client
//! already relies on.
//!
//! Frames are emitted the moment their terminating blank line arrives, never
//! accumulated: the router adds no latency to a token beyond the time it takes
//! to read one frame.
//!
//! The stream can also end badly. A node that drops the connection after
//! output has reached the client cannot be replaced — another model cannot
//! finish this one's sentence — so the failure is surfaced the way the gateway
//! surfaces its own: one `data: {"error":…}` frame and no `[DONE]`, which every
//! OpenAI client reads as a failed stream rather than a short answer.
//!
//! While it rewrites, it also notices two things, for observability only:
//!
//! * **The first generated output.** A chunk counts when one of its choices
//!   carries a non-empty `delta.content`, `delta.reasoning_content` or
//!   `delta.tool_calls` (chat), or a non-empty `text` (completions). A
//!   keep-alive comment, a queue notice, the role-only opening delta, an empty
//!   delta, a usage-only chunk and an error frame do not. This is the event
//!   the router's time to first token stops at — the same set of events the
//!   gateway's own TTFT counts.
//! * **`usage.prompt_tokens`**, when the node sends it (the final usage chunk
//!   a client asks for with `stream_options.include_usage`).

use lightweight_core::sse::{DONE_DATA, encode_data};
use serde_json::{Value, json};

/// The largest frame held while waiting for its end, matching the codec in
/// `lightweight-core`: past this, the peer is not speaking SSE.
const FRAME_LIMIT: usize = 8 * 1024 * 1024;

/// Rewrites one stream, frame by frame.
#[derive(Debug)]
pub struct FrameRewriter {
    public_model: String,
    buffer: Vec<u8>,
    saw_done: bool,
    saw_error: bool,
    /// Set once the stream has been ended by this rewriter; nothing after it
    /// is forwarded.
    finished: bool,
    /// Whether a frame carrying generated output has been forwarded.
    generated: bool,
    /// The node's own prompt count, if it reported one.
    prompt_tokens: Option<u32>,
}

impl FrameRewriter {
    pub fn new(public_model: impl Into<String>) -> Self {
        Self {
            public_model: public_model.into(),
            buffer: Vec::new(),
            saw_done: false,
            saw_error: false,
            finished: false,
            generated: false,
            prompt_tokens: None,
        }
    }

    /// Whether any frame forwarded so far carried generated output — see the
    /// module documentation for exactly which frames count.
    pub fn has_generated(&self) -> bool {
        self.generated
    }

    /// `usage.prompt_tokens` from the stream, once the node has sent it.
    pub fn prompt_tokens(&self) -> Option<u32> {
        self.prompt_tokens
    }

    /// Feed bytes off the wire; returns whatever complete frames they finished.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((content, total)) = frame_end(&self.buffer) {
            let frame: Vec<u8> = self.buffer.drain(..total).collect();
            self.emit(&frame[..content], &frame, &mut out);
        }
        if self.buffer.len() > FRAME_LIMIT {
            self.buffer.clear();
            out.extend(self.fail(
                "upstream_stream_invalid",
                "the upstream node sent an event larger than the router accepts",
            ));
        }
        out
    }

    /// The node closed the stream. Returns anything that must still be sent.
    ///
    /// A stream that ended without `[DONE]` and without an error frame was cut
    /// off, and is reported as such — the client must not mistake it for a
    /// complete answer.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        if self.saw_done || self.saw_error {
            self.finished = true;
            // A trailing partial frame after `[DONE]` carries nothing a client
            // would read; it is dropped rather than forwarded half-formed.
            return Vec::new();
        }
        self.fail(
            "upstream_stream_interrupted",
            "the upstream node ended the stream before it was complete",
        )
    }

    /// The connection to the node failed mid-stream.
    pub fn abort(&mut self) -> Vec<u8> {
        if self.finished || self.saw_done || self.saw_error {
            self.finished = true;
            return Vec::new();
        }
        self.fail(
            "upstream_stream_interrupted",
            "the connection to the upstream node failed during the response",
        )
    }

    /// Whether the rewriter has ended the stream itself; nothing more will be
    /// forwarded.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Whether the stream reached `[DONE]`.
    pub fn completed(&self) -> bool {
        self.saw_done
    }

    fn fail(&mut self, code: &str, message: &str) -> Vec<u8> {
        self.finished = true;
        self.saw_error = true;
        let body = json!({
            "error": {
                "message": message,
                "type": "server_error",
                "code": code,
            }
        });
        encode_data(&body.to_string()).into_bytes()
    }

    fn emit(&mut self, content: &[u8], raw: &[u8], out: &mut Vec<u8>) {
        let Ok(text) = std::str::from_utf8(content) else {
            out.extend_from_slice(raw);
            return;
        };

        let mut data_lines = Vec::new();
        let mut other_lines = Vec::new();
        for line in text.split(['\n', '\r']).filter(|line| !line.is_empty()) {
            if let Some(value) = line.strip_prefix("data:") {
                data_lines.push(value.strip_prefix(' ').unwrap_or(value));
            } else {
                other_lines.push(line);
            }
        }
        if data_lines.is_empty() {
            // A comment or a frame of fields only: a keep-alive. Forwarded
            // untouched, because that is the whole of its job.
            out.extend_from_slice(raw);
            return;
        }

        let data = data_lines.join("\n");
        if data.trim() == DONE_DATA {
            self.saw_done = true;
            out.extend_from_slice(raw);
            return;
        }

        let Ok(Value::Object(mut event)) = serde_json::from_str::<Value>(&data) else {
            out.extend_from_slice(raw);
            return;
        };
        if event.contains_key("error") {
            self.saw_error = true;
        } else if !self.generated && carries_output(&event) {
            self.generated = true;
        }
        if let Some(tokens) = prompt_tokens(&event) {
            self.prompt_tokens = Some(tokens);
        }
        if !matches!(event.get("model"), Some(Value::String(_))) {
            out.extend_from_slice(raw);
            return;
        }
        event.insert("model".into(), Value::String(self.public_model.clone()));

        for line in other_lines {
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
        out.extend_from_slice(encode_data(&Value::Object(event).to_string()).as_bytes());
    }
}

/// Whether one event carries generated output a client would show or act on.
fn carries_output(event: &serde_json::Map<String, Value>) -> bool {
    let non_empty = |value: Option<&Value>| match value {
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        _ => false,
    };
    let Some(Value::Array(choices)) = event.get("choices") else {
        return false;
    };
    choices.iter().any(|choice| {
        let delta = choice.get("delta");
        non_empty(choice.get("text"))
            || delta.is_some_and(|delta| {
                non_empty(delta.get("content"))
                    || non_empty(delta.get("reasoning_content"))
                    || non_empty(delta.get("tool_calls"))
            })
    })
}

/// `usage.prompt_tokens` of an event or a whole response, if present.
fn prompt_tokens(object: &serde_json::Map<String, Value>) -> Option<u32> {
    object
        .get("usage")?
        .get("prompt_tokens")?
        .as_u64()
        .and_then(|tokens| u32::try_from(tokens).ok())
}

/// [`rewrite_body`], also returning the body's `usage.prompt_tokens` — read
/// from the one parse the rewrite already makes.
pub fn rewrite_body_measuring(body: &[u8], public_model: &str) -> (Option<Vec<u8>>, Option<u32>) {
    let Ok(Value::Object(mut object)) = serde_json::from_slice::<Value>(body) else {
        return (None, None);
    };
    let tokens = prompt_tokens(&object);
    if !matches!(object.get("model"), Some(Value::String(_))) {
        return (None, tokens);
    }
    object.insert("model".into(), Value::String(public_model.to_owned()));
    (serde_json::to_vec(&Value::Object(object)).ok(), tokens)
}

/// Rewrite the `model` of a whole JSON response body.
///
/// `None` when the body is not a JSON object with a string `model`, in which
/// case the caller forwards it unchanged.
pub fn rewrite_body(body: &[u8], public_model: &str) -> Option<Vec<u8>> {
    let Ok(Value::Object(mut object)) = serde_json::from_slice::<Value>(body) else {
        return None;
    };
    if !matches!(object.get("model"), Some(Value::String(_))) {
        return None;
    }
    object.insert("model".into(), Value::String(public_model.to_owned()));
    serde_json::to_vec(&Value::Object(object)).ok()
}

/// Where the first frame in `buffer` ends: the length of its own lines, and
/// that plus its blank-line terminator.
fn frame_end(buffer: &[u8]) -> Option<(usize, usize)> {
    // Longest first, as in the core codec: `\r\n\r\n` contains `\n\r\n`, and
    // matching the shorter one would leave a stray `\r` on the next frame.
    const TERMINATORS: [&[u8]; 4] = [b"\r\n\r\n", b"\n\n", b"\r\r", b"\n\r\n"];
    TERMINATORS
        .iter()
        .filter_map(|terminator| find(buffer, terminator).map(|at| (at, at + terminator.len())))
        .min_by_key(|(at, _)| *at)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightweight_core::{SseDecoder, SseEvent};

    fn decode(bytes: &[u8]) -> Vec<SseEvent> {
        let mut decoder = SseDecoder::new();
        decoder.feed(bytes).unwrap();
        decoder.drain()
    }

    fn chunk(model: &str, content: &str) -> String {
        encode_data(
            &json!({
                "id": "chatcmpl-1",
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [{"index": 0, "delta": {"content": content}}]
            })
            .to_string(),
        )
    }

    #[test]
    fn the_model_is_rewritten_in_every_chunk_and_nothing_else_changes() {
        let mut rewriter = FrameRewriter::new("Coder");
        let mut out = Vec::new();
        out.extend(rewriter.push(chunk("QwenCoder", "Hel").as_bytes()));
        out.extend(rewriter.push(chunk("QwenCoder", "lo").as_bytes()));
        out.extend(rewriter.push(b"data: [DONE]\n\n"));
        out.extend(rewriter.finish());

        let events = decode(&out);
        assert_eq!(events.len(), 3);
        for event in &events[..2] {
            let value: Value = serde_json::from_str(&event.data).unwrap();
            assert_eq!(value["model"], "Coder");
            assert_eq!(value["object"], "chat.completion.chunk");
        }
        let second: Value = serde_json::from_str(&events[1].data).unwrap();
        assert_eq!(second["choices"][0]["delta"]["content"], "lo");
        assert!(events[2].is_done());
        assert!(!String::from_utf8_lossy(&out).contains("QwenCoder"));
        assert!(rewriter.completed());
    }

    #[test]
    fn frames_are_released_as_soon_as_they_end_even_when_split() {
        let mut rewriter = FrameRewriter::new("Coder");
        let whole = chunk("QwenCoder", "x");
        let (head, tail) = whole.as_bytes().split_at(whole.len() / 2);
        assert!(rewriter.push(head).is_empty(), "half a frame waits");
        let released = rewriter.push(tail);
        assert_eq!(
            decode(&released).len(),
            1,
            "the frame goes the moment it ends"
        );
    }

    #[test]
    fn keep_alives_and_done_pass_byte_for_byte() {
        let mut rewriter = FrameRewriter::new("Coder");
        assert_eq!(rewriter.push(b": keep-alive\n\n"), b": keep-alive\n\n");
        assert_eq!(rewriter.push(b"data: [DONE]\n\n"), b"data: [DONE]\n\n");
        assert!(rewriter.finish().is_empty());
    }

    #[test]
    fn an_in_band_error_is_forwarded_and_not_doubled() {
        let mut rewriter = FrameRewriter::new("Coder");
        let error = encode_data(r#"{"error":{"message":"boom","type":"server_error"}}"#);
        assert_eq!(rewriter.push(error.as_bytes()), error.as_bytes());
        assert!(
            rewriter.finish().is_empty(),
            "the node already said it failed"
        );
    }

    #[test]
    fn a_stream_cut_off_after_output_ends_with_an_error_and_no_done() {
        let mut rewriter = FrameRewriter::new("Coder");
        let mut out = rewriter.push(chunk("QwenCoder", "Here is the").as_bytes());
        out.extend(rewriter.abort());
        let events = decode(&out);
        assert_eq!(events.len(), 2);
        let error: Value = serde_json::from_str(&events[1].data).unwrap();
        assert_eq!(error["error"]["code"], "upstream_stream_interrupted");
        assert!(!events.iter().any(SseEvent::is_done));
        assert!(
            rewriter
                .push(chunk("QwenCoder", "more").as_bytes())
                .is_empty()
        );
    }

    #[test]
    fn a_clean_close_without_done_is_reported_as_truncated() {
        let mut rewriter = FrameRewriter::new("Coder");
        rewriter.push(chunk("QwenCoder", "partial").as_bytes());
        let tail = rewriter.finish();
        let events = decode(&tail);
        let error: Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(error["error"]["code"], "upstream_stream_interrupted");
    }

    #[test]
    fn only_generated_output_counts_as_the_first_token() {
        let mut rewriter = FrameRewriter::new("Coder");
        let event = |delta: Value| {
            encode_data(
                &json!({"model": "QwenCoder", "choices": [{"index": 0, "delta": delta}]})
                    .to_string(),
            )
        };
        rewriter.push(b": keep-alive\n\n");
        rewriter.push(b": queued position=1 waited=3s\n\n");
        rewriter.push(event(json!({"role": "assistant", "content": ""})).as_bytes());
        rewriter.push(event(json!({})).as_bytes());
        rewriter.push(
            encode_data(r#"{"model":"QwenCoder","choices":[],"usage":{"prompt_tokens":3}}"#)
                .as_bytes(),
        );
        assert!(!rewriter.has_generated(), "nothing generated yet");
        rewriter.push(event(json!({"reasoning_content": "hmm"})).as_bytes());
        assert!(rewriter.has_generated(), "reasoning is output");

        let mut tools = FrameRewriter::new("Coder");
        tools.push(event(json!({"tool_calls": [{"index": 0, "id": "c"}]})).as_bytes());
        assert!(tools.has_generated());

        let mut text = FrameRewriter::new("Coder");
        text.push(encode_data(r#"{"model":"m","choices":[{"index":0,"text":""}]}"#).as_bytes());
        assert!(!text.has_generated());
        text.push(encode_data(r#"{"model":"m","choices":[{"index":0,"text":"x"}]}"#).as_bytes());
        assert!(text.has_generated());
    }

    #[test]
    fn an_error_before_any_output_is_not_a_first_token() {
        let mut rewriter = FrameRewriter::new("Coder");
        rewriter
            .push(encode_data(r#"{"error":{"message":"boom","type":"server_error"}}"#).as_bytes());
        assert!(!rewriter.has_generated());
    }

    #[test]
    fn the_prompt_count_is_read_from_a_usage_chunk_or_a_body() {
        let mut rewriter = FrameRewriter::new("Coder");
        assert_eq!(rewriter.prompt_tokens(), None);
        rewriter.push(
            encode_data(
                r#"{"model":"Q","choices":[],"usage":{"prompt_tokens":42,"completion_tokens":2}}"#,
            )
            .as_bytes(),
        );
        assert_eq!(rewriter.prompt_tokens(), Some(42));

        let (body, tokens) = rewrite_body_measuring(
            br#"{"model":"Q","choices":[],"usage":{"prompt_tokens":17}}"#,
            "Coder",
        );
        assert_eq!(tokens, Some(17));
        let body: Value = serde_json::from_slice(&body.unwrap()).unwrap();
        assert_eq!(body["model"], "Coder");
        assert_eq!(rewrite_body_measuring(b"{}", "Coder"), (None, None));
    }

    #[test]
    fn crlf_framing_is_understood() {
        let mut rewriter = FrameRewriter::new("Coder");
        let frame = "data: {\"model\":\"QwenCoder\",\"choices\":[]}\r\n\r\n";
        let events = decode(&rewriter.push(frame.as_bytes()));
        let value: Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(value["model"], "Coder");
    }

    #[test]
    fn a_whole_body_is_rewritten_only_when_it_names_a_model() {
        let body = br#"{"id":"x","model":"QwenCoder","choices":[]}"#;
        let rewritten: Value =
            serde_json::from_slice(&rewrite_body(body, "Coder").unwrap()).unwrap();
        assert_eq!(rewritten["model"], "Coder");
        assert_eq!(rewritten["id"], "x");
        assert!(rewrite_body(br#"{"error":{"message":"no"}}"#, "Coder").is_none());
        assert!(rewrite_body(b"not json", "Coder").is_none());
    }

    #[test]
    fn a_structured_model_on_an_error_frame_is_renamed_and_the_error_kept() {
        let mut rewriter = FrameRewriter::new("Coder");
        let frame = encode_data(
            r#"{"model":"QwenCoder","error":{"message":"boom","type":"server_error","code":"generation_failed"}}"#,
        );
        let events = decode(&rewriter.push(frame.as_bytes()));
        let value: Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(value["model"], "Coder");
        assert_eq!(value["error"]["code"], "generation_failed");
        assert_eq!(
            value["error"]["message"], "boom",
            "free text is never edited"
        );
        assert!(
            rewriter.finish().is_empty(),
            "still recognised as the node's error"
        );
    }
}
