//! The streamed chunk sequence.
//!
//! The order below is the contract. It is not OpenAI's documentation talking —
//! it is what the client's accumulator requires
//! (`agent/chat_completion_helpers.py:4095-4267`), and each step exists
//! because leaving it out breaks something specific:
//!
//! 1. **role chunk** — `delta = {"role":"assistant","content":""}`
//! 2. **reasoning deltas** — `delta.reasoning_content`
//! 3. **content deltas** — `delta.content`
//! 4. **tool-call deltas** — the first delta of call *i* carries
//!    `{index, id, type:"function", function:{name, arguments:""}}`; every
//!    later one carries only `{index, function:{arguments}}`
//! 5. **finish chunk** — `{delta:{}, finish_reason:…}`
//! 6. **usage chunk** — `choices: []` plus `usage`, only when the client asked
//! 7. `data: [DONE]`
//!
//! Two of those are easy to get subtly wrong. Repeating a tool call's `id`, or
//! sending a *different* id at an index already used, makes the client open a
//! second call and split the arguments between them — so the id goes out
//! exactly once. And the usage chunk must carry an **empty** `choices` array:
//! the client reads token counts from exactly that chunk and from nowhere else.
//!
//! Every chunk repeats `id`, `object`, `created` and `model`, and `model` is
//! *our* catalog id rather than the engine's file path, because the client
//! reads `chunk.model` back.

use hermes_core::sse;
use hermes_inference::generation::FinishReason;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::chat::UsageBody;

/// A tool-call fragment inside a delta.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ToolCallDelta {
    pub index: u32,
    /// Present on the first delta of a call and never again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// `"function"`, sent alongside the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionDelta>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct FunctionDelta {
    /// Assigned by the client, never concatenated — so it is sent whole, once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Concatenated by the client, in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// The `delta` object of a streamed choice.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Delta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Hermes reads either `reasoning_content` or `reasoning`; we send the
    /// first, which is also what the engine produces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

/// One choice inside a chunk.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: Delta,
    /// `null` until the final content chunk. Serialized even when null: a
    /// client scanning for the key must find it.
    pub finish_reason: Option<String>,
}

/// One `chat.completion.chunk`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    /// Empty on the usage chunk, and only there.
    pub choices: Vec<ChunkChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageBody>,
    /// Engine-measured timings, attached to the usage chunk when we have them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<Value>,
    /// Set only on a stream that failed after headers were sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

impl ChatCompletionChunk {
    /// Render as one `data:` frame.
    ///
    /// Serialization cannot fail for this type — every field is a plain scalar,
    /// string or vector — but the fallible API is what `serde_json` offers, and
    /// this crate forbids `unwrap`. A frame that somehow could not be built is
    /// emitted as an error chunk rather than panicking mid-stream.
    pub fn to_sse_frame(&self) -> String {
        match serde_json::to_string(self) {
            Ok(json) => sse::encode_data(&json),
            Err(err) => sse::encode_data(&format!(
                r#"{{"error":{{"message":"could not encode chunk: {}","type":"server_error"}}}}"#,
                err.to_string().replace('"', "'")
            )),
        }
    }
}

/// Builds the chunks of one streamed completion.
///
/// Holds the identity every chunk repeats, and the tool-call bookkeeping that
/// keeps the client's accumulator correct.
#[derive(Clone, Debug)]
pub struct ChunkBuilder {
    id: String,
    model: String,
    created: u64,
    /// Indexes whose `id` has already been sent, so it is never sent twice.
    announced_tool_calls: Vec<u32>,
}

impl ChunkBuilder {
    /// `id` is the completion id repeated on every chunk; `model` is our
    /// catalog id.
    pub fn new(id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            created: crate::unix_now(),
            announced_tool_calls: Vec::new(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn created(&self) -> u64 {
        self.created
    }

    fn chunk(&self, choices: Vec<ChunkChoice>) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: self.id.clone(),
            object: "chat.completion.chunk".to_owned(),
            created: self.created,
            model: self.model.clone(),
            choices,
            usage: None,
            timings: None,
            error: None,
        }
    }

    fn delta_chunk(&self, delta: Delta) -> ChatCompletionChunk {
        self.chunk(vec![ChunkChoice {
            index: 0,
            delta,
            finish_reason: None,
        }])
    }

    /// Chunk 1: the opening role chunk.
    ///
    /// `content` is an empty string rather than absent or null, because that is
    /// what OpenAI sends and what clients are written against.
    pub fn role(&self) -> ChatCompletionChunk {
        self.delta_chunk(Delta {
            role: Some("assistant".to_owned()),
            content: Some(String::new()),
            ..Delta::default()
        })
    }

    pub fn content(&self, text: impl Into<String>) -> ChatCompletionChunk {
        self.delta_chunk(Delta {
            content: Some(text.into()),
            ..Delta::default()
        })
    }

    pub fn reasoning(&self, text: impl Into<String>) -> ChatCompletionChunk {
        self.delta_chunk(Delta {
            reasoning_content: Some(text.into()),
            ..Delta::default()
        })
    }

    /// A tool-call delta, with the id and name sent at most once per index.
    ///
    /// The suppression is the point. A second `id` at an index the client has
    /// already seen forces it to start a brand new call and split the
    /// arguments across two — so even if a backend repeats itself, the client
    /// never sees it.
    pub fn tool_call(
        &mut self,
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    ) -> ChatCompletionChunk {
        let first_time = !self.announced_tool_calls.contains(&index);
        let id = match (first_time, id) {
            (true, Some(id)) => {
                self.announced_tool_calls.push(index);
                Some(id)
            }
            _ => None,
        };
        let name = if first_time { name } else { None };

        let function = (name.is_some() || arguments.is_some()).then(|| FunctionDelta {
            name,
            // The first delta of a call carries an empty argument string, so
            // the client has a slot to concatenate into.
            arguments: arguments.or_else(|| id.as_ref().map(|_| String::new())),
        });

        self.delta_chunk(Delta {
            tool_calls: Some(vec![ToolCallDelta {
                index,
                r#type: id.as_ref().map(|_| "function".to_owned()),
                id,
                function,
            }]),
            ..Delta::default()
        })
    }

    /// Chunk 5: the finish chunk, with an empty delta.
    pub fn finish(&self, reason: FinishReason) -> ChatCompletionChunk {
        self.chunk(vec![ChunkChoice {
            index: 0,
            delta: Delta::default(),
            finish_reason: Some(reason.as_str().to_owned()),
        }])
    }

    /// Chunk 6: the usage chunk — empty `choices`, and the token counts.
    pub fn usage(&self, usage: UsageBody, timings: Option<Value>) -> ChatCompletionChunk {
        ChatCompletionChunk {
            choices: Vec::new(),
            usage: Some(usage),
            timings,
            ..self.chunk(Vec::new())
        }
    }

    /// A terminal chunk for a failure that happened after headers were sent.
    ///
    /// Once the response has started, an error can no longer be an HTTP status.
    /// Dropping the connection would look to the client like a truncated
    /// stream and invite a blind retry; this ends the stream deliberately,
    /// with `finish_reason: "error"` and a body saying what went wrong.
    pub fn error(&self, body: Value) -> ChatCompletionChunk {
        ChatCompletionChunk {
            error: Some(body),
            ..self.chunk(vec![ChunkChoice {
                index: 0,
                delta: Delta::default(),
                finish_reason: Some(FinishReason::Error.as_str().to_owned()),
            }])
        }
    }

    /// Chunk 7: `data: [DONE]`.
    pub fn done(&self) -> String {
        sse::encode_done()
    }

    /// A keep-alive comment frame.
    ///
    /// Sent during prefill only. Prefill on a CPU without AVX can silently
    /// occupy 30 to 120 seconds, and an idle socket for that long is dropped by
    /// intermediaries and by some clients. The openai SDK's decoder ignores
    /// comment lines, so this is invisible to the caller.
    pub fn keep_alive(&self) -> String {
        sse::encode_comment("ping")
    }

    /// A comment frame saying where in the queue this request is.
    ///
    /// A comment rather than a chunk, deliberately. A queued request has
    /// produced no tokens, so any `data:` frame would be a chunk shaped like a
    /// completion that is not one, and every strict client would have to be
    /// taught to ignore it. Comments are already discarded by the SSE decoder
    /// in every client this gateway is checked against, and they are plainly
    /// readable in `curl` — which is where the question "is it stuck, or is it
    /// waiting?" actually gets asked.
    pub fn queued(&self, position: u32, waited: Duration) -> String {
        sse::encode_comment(&format!(
            "queued position={position} waited={}s",
            waited.as_secs()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::SseDecoder;

    fn field(chunk: &ChatCompletionChunk) -> Value {
        serde_json::to_value(chunk).expect("serialize")
    }

    #[test]
    fn the_role_chunk_carries_an_empty_string_not_null() {
        // Clients are written against OpenAI's shape, where the opening delta
        // has `content: ""`. `null` is one more way to look like an empty
        // response.
        let builder = ChunkBuilder::new("chatcmpl-1", "m@8k");
        let json = field(&builder.role());
        assert_eq!(json["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(json["choices"][0]["delta"]["content"], "");
        assert!(json["choices"][0]["finish_reason"].is_null());
        assert_eq!(json["object"], "chat.completion.chunk");
    }

    #[test]
    fn every_chunk_repeats_the_identity_the_client_reads() {
        let builder = ChunkBuilder::new("chatcmpl-42", "lfm2@8k");
        for chunk in [
            builder.role(),
            builder.content("hi"),
            builder.finish(FinishReason::Stop),
            builder.usage(UsageBody::default(), None),
        ] {
            let json = field(&chunk);
            assert_eq!(json["id"], "chatcmpl-42");
            // Our catalog id, never the engine's model path: the client reads
            // `chunk.model` back and keys its context cache on it.
            assert_eq!(json["model"], "lfm2@8k");
            assert_eq!(json["object"], "chat.completion.chunk");
            assert!(json["created"].is_number());
        }
    }

    #[test]
    fn the_usage_chunk_has_an_empty_choices_array() {
        // The client reads token counts from exactly this chunk. A choice in
        // it would also read as another (empty) content delta.
        let builder = ChunkBuilder::new("chatcmpl-1", "m@8k");
        let json = field(&builder.usage(
            UsageBody {
                prompt_tokens: 36,
                completion_tokens: 3,
                total_tokens: 39,
                prompt_tokens_details: None,
            },
            None,
        ));
        assert_eq!(json["choices"], serde_json::json!([]));
        assert_eq!(json["usage"]["prompt_tokens"], 36);
        assert_eq!(json["usage"]["total_tokens"], 39);
    }

    #[test]
    fn a_tool_calls_id_and_name_go_out_exactly_once() {
        // A repeated or changed id at a known index makes the client open a
        // second call and split the arguments between the two.
        let mut builder = ChunkBuilder::new("chatcmpl-1", "m@8k");
        let first =
            field(&builder.tool_call(0, Some("call_1".into()), Some("read_file".into()), None));
        assert_eq!(
            first["choices"][0]["delta"]["tool_calls"][0]["id"],
            "call_1"
        );
        assert_eq!(
            first["choices"][0]["delta"]["tool_calls"][0]["type"],
            "function"
        );
        assert_eq!(
            first["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        assert_eq!(
            first["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            ""
        );

        // Even if the backend repeats itself, the client must not see it.
        let second = field(&builder.tool_call(
            0,
            Some("call_1".into()),
            Some("read_file".into()),
            Some("{\"path\":".into()),
        ));
        let call = &second["choices"][0]["delta"]["tool_calls"][0];
        assert!(call.get("id").is_none(), "id repeated: {second}");
        assert!(call["function"].get("name").is_none(), "name repeated");
        assert_eq!(call["function"]["arguments"], "{\"path\":");
        assert_eq!(call["index"], 0);
    }

    #[test]
    fn a_second_tool_call_gets_its_own_index_and_id() {
        let mut builder = ChunkBuilder::new("chatcmpl-1", "m@8k");
        let _ = builder.tool_call(0, Some("call_1".into()), Some("a".into()), None);
        let second = field(&builder.tool_call(1, Some("call_2".into()), Some("b".into()), None));
        let call = &second["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["index"], 1);
        assert_eq!(call["id"], "call_2");
    }

    #[test]
    fn the_finish_chunk_has_an_empty_delta_and_a_reason() {
        let builder = ChunkBuilder::new("chatcmpl-1", "m@8k");
        let json = field(&builder.finish(FinishReason::ToolCalls));
        assert_eq!(json["choices"][0]["delta"], serde_json::json!({}));
        assert_eq!(json["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn an_error_after_headers_ends_the_stream_cleanly() {
        // The alternative is a dropped connection, which reads as a truncated
        // stream and triggers a blind retry.
        let builder = ChunkBuilder::new("chatcmpl-1", "m@8k");
        let json = field(&builder.error(serde_json::json!({"message": "engine died"})));
        assert_eq!(json["choices"][0]["finish_reason"], "error");
        assert_eq!(json["error"]["message"], "engine died");
    }

    #[test]
    fn a_chunk_frames_as_exactly_one_sse_event() {
        let builder = ChunkBuilder::new("chatcmpl-1", "m@8k");
        let frame = builder.content("hello").to_sse_frame();
        assert!(frame.starts_with("data: {"), "{frame}");
        assert!(frame.ends_with("\n\n"), "{frame:?}");

        let mut decoder = SseDecoder::new();
        decoder.feed(frame.as_bytes()).expect("decode");
        let events = decoder.drain();
        assert_eq!(events.len(), 1);
        let parsed: ChatCompletionChunk =
            serde_json::from_str(&events[0].data).expect("round trip");
        assert_eq!(parsed.choices[0].delta.content.as_deref(), Some("hello"));
    }

    #[test]
    fn content_deltas_never_carry_a_role() {
        // A role on every chunk is harmless to some clients and confusing to
        // others; OpenAI sends it once, and so do we.
        let builder = ChunkBuilder::new("chatcmpl-1", "m@8k");
        let json = field(&builder.content("x"));
        assert!(json["choices"][0]["delta"].get("role").is_none());
    }

    #[test]
    fn a_keep_alive_is_a_comment_frame() {
        // Comment frames are ignored by the client's decoder, which is what
        // makes them usable during a long prefill.
        let builder = ChunkBuilder::new("chatcmpl-1", "m@8k");
        assert_eq!(builder.keep_alive(), ": ping\n\n");
        assert_eq!(builder.done(), "data: [DONE]\n\n");
    }
}
