//! Byte-exact transcripts of what we put on the wire.
//!
//! The higher-level tests assert *meaning* — that content assembles, that
//! usage arrives. This one asserts bytes, because framing regressions hide
//! from meaning-level tests: a stray blank line, a lost `\n\n`, a field that
//! starts serializing as `null` instead of being omitted. Each of those still
//! decodes for a tolerant client and breaks a strict one.
//!
//! Only the two genuinely volatile fields are normalized (`id` and `created`).
//! Everything else, including key order, is pinned: a change to any of it is a
//! change to the contract and should have to be made deliberately.
//!
//! Run with `UPDATE_GOLDEN=1` to rewrite the files after an intended change,
//! then read the diff before committing it.

use std::path::PathBuf;

use futures_util::StreamExt;
use lightweight_api::stream::ChunkBuilder;
use lightweight_gateway::stream::{RequestGuard, encode};
use lightweight_inference::BackendError;
use lightweight_inference::generation::{FinishReason, GenerationEvent, Timings, Usage};
use tokio_util::sync::CancellationToken;

/// Produce a transcript with volatile fields flattened.
async fn transcript(
    events: Vec<Result<GenerationEvent, BackendError>>,
    include_usage: bool,
) -> String {
    let guard = RequestGuard::new(CancellationToken::new(), None);
    let builder = ChunkBuilder::new("chatcmpl-GOLDEN", "mock-model@4k");
    let stream = futures_util::stream::iter(events).boxed();

    let raw: String = encode(stream, builder, guard, include_usage)
        .filter_map(|frame| async move { frame.ok() })
        .collect::<Vec<String>>()
        .await
        .concat();

    normalize(&raw)
}

/// Replace the one field that cannot be deterministic.
///
/// `created` is seconds since the epoch. The id is already fixed above.
fn normalize(transcript: &str) -> String {
    let mut out = String::with_capacity(transcript.len());
    let mut rest = transcript;
    while let Some(at) = rest.find("\"created\":") {
        out.push_str(&rest[..at]);
        out.push_str("\"created\":0");
        rest = &rest[at + "\"created\":".len()..];
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        rest = &rest[digits..];
    }
    out.push_str(rest);
    out
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

fn compare(name: &str, actual: &str) {
    let path = golden_path(name);
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap_or(&path)).expect("create golden dir");
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing golden file {}: {err}. Run with UPDATE_GOLDEN=1 to create it.",
            path.display()
        )
    });
    assert_eq!(
        actual, expected,
        "the bytes on the wire changed.\n--- expected ---\n{expected}\n--- actual ---\n{actual}"
    );
}

#[tokio::test]
async fn a_plain_streamed_completion_is_byte_stable() {
    let actual = transcript(
        vec![
            Ok(GenerationEvent::Started {
                prompt_tokens: Some(12),
            }),
            Ok(GenerationEvent::ContentDelta {
                text: "Hello".into(),
            }),
            Ok(GenerationEvent::ContentDelta { text: ", ".into() }),
            Ok(GenerationEvent::ContentDelta {
                text: "world".into(),
            }),
            Ok(GenerationEvent::Timings(Timings {
                prompt_n: 12,
                prompt_ms: 2040.73,
                predicted_n: 3,
                predicted_ms: 171.527,
                cached_n: 0,
            })),
            Ok(GenerationEvent::Finished {
                finish_reason: FinishReason::Stop,
                usage: Usage::new(12, 3),
            }),
        ],
        true,
    )
    .await;
    compare("chat_stream.sse", &actual);
}

#[tokio::test]
async fn a_tool_call_stream_is_byte_stable() {
    let actual = transcript(
        vec![
            Ok(GenerationEvent::Started {
                prompt_tokens: Some(30),
            }),
            Ok(GenerationEvent::ToolCallDelta {
                index: 0,
                id: Some("call_abc".into()),
                name: Some("read_file".into()),
                arguments: Some(String::new()),
            }),
            Ok(GenerationEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments: Some("{\"path\":".into()),
            }),
            Ok(GenerationEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments: Some("\"a.txt\"}".into()),
            }),
            Ok(GenerationEvent::Finished {
                finish_reason: FinishReason::ToolCalls,
                usage: Usage::new(30, 12),
            }),
        ],
        false,
    )
    .await;
    compare("tool_call_stream.sse", &actual);
}

#[tokio::test]
async fn a_stream_that_fails_after_headers_is_byte_stable() {
    let actual = transcript(
        vec![
            Ok(GenerationEvent::ContentDelta {
                text: "partial".into(),
            }),
            Err(BackendError::EngineCrashed {
                detail: "killed (signal 9)".into(),
                exit_code: None,
                signal: Some(9),
                tail: vec![],
            }),
        ],
        true,
    )
    .await;
    compare("failed_stream.sse", &actual);
}
