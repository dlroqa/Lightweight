//! The OpenAI-compatible wire format.
//!
//! This crate is the *contract*, not the server: types, their serde
//! behaviour, and the codec that turns generation events into the exact bytes
//! a client reads. The gateway supplies the HTTP.
//!
//! The contract implemented here is **OpenAI's**, not any particular client's.
//! This gateway is a model provider: an agent harness, a chat UI, a script with
//! `curl` — anything that speaks that API — is a client, and none of them is
//! privileged.
//!
//! Two rules run through all of it, and both were learned by reading a real
//! client's source rather than a specification, because a specification does
//! not say which parts are load-bearing in practice.
//!
//! **Tolerance is not politeness, it is correctness.** Every request type
//! carries `#[serde(default)]` and a `#[serde(flatten)]` catch-all, and
//! `deny_unknown_fields` appears nowhere in this workspace. Hermes sends
//! `reasoning_effort`, `think`, `options.num_ctx` and `stream_options`, and
//! will send more in versions we have never seen. A 400 for an unrecognized
//! field would break a working client on an upgrade we had no part in.
//!
//! **The chunk sequence is the contract.** A client assembles content, tool
//! calls and token counts from the order and shape of the chunks, so
//! [`stream::ChunkBuilder`] owns that order and the golden-file tests pin the
//! bytes.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod chat;
pub mod error;
pub mod models;
pub mod props;
pub mod stream;

pub use chat::{
    ChatCompletionRequest, ChatCompletionResponse, Choice, RequestMessage, ResponseMessage,
    StreamOptions, UsageBody,
};
pub use error::{ErrorBody, ErrorEnvelope};
pub use models::{ModelList, ModelRow};
pub use props::PropsBody;
pub use stream::{ChatCompletionChunk, ChunkBuilder, Delta, ToolCallDelta};

/// Seconds since the Unix epoch, for the `created` field.
///
/// A clock before 1970 yields 0 rather than a panic: this is decoration on a
/// response, and no client behaviour depends on it.
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}
