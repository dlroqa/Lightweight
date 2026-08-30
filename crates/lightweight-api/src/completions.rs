//! `POST /v1/completions` — the older, non-chat endpoint.
//!
//! It exists because it is a different thing, not an older spelling of the same
//! thing: `/v1/chat/completions` renders a conversation through the model's
//! chat template, and this endpoint continues raw text with no template at all.
//! Anything that fills in a form, continues a document, or drives a base model
//! with no chat template needs the second, and given only the first it would
//! get an answer to a conversation it never had.
//!
//! Three shapes here differ from the chat endpoint, and each was read from a
//! running engine rather than assumed:
//!
//! * `object` is `text_completion`, on the response and on every stream chunk.
//! * A choice carries **`text`**, not a `message` or a `delta`.
//! * `logprobs` is present and `null` rather than omitted. Clients index it.
//!
//! One shape deliberately differs from the engine's: llama.cpp puts `usage` on
//! the last content chunk, while OpenAI sends a final chunk with an **empty**
//! `choices` array. This gateway follows OpenAI, because OpenAI's clients are
//! what it serves, and the contract suite asserts what the genuine `openai`
//! package ends up holding.

use lightweight_inference::generation::{FinishReason, GenerationRequest, Prompt, SamplingParams};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use lightweight_core::sse;

use crate::chat::{OneOrMany, PromptTokensDetails, StreamOptions, UsageBody};

/// Why a completion request cannot be served.
///
/// Each variant is a case where continuing would return something other than
/// what the client asked for. Silently returning fewer choices, or dropping a
/// parameter that changes the response's shape, would leave the client parsing
/// a reply to a different question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionError {
    /// `prompt` was absent, or every entry in it was empty.
    NoPrompt,
    /// `prompt` held token ids rather than text.
    TokenPrompt,
    /// A parameter this gateway cannot honour, named so the client can drop it.
    Unsupported {
        param: &'static str,
        why: &'static str,
    },
}

impl CompletionError {
    pub const fn param(&self) -> &'static str {
        match self {
            Self::NoPrompt | Self::TokenPrompt => "prompt",
            Self::Unsupported { param, .. } => param,
        }
    }

    pub const fn code(&self) -> &'static str {
        match self {
            Self::NoPrompt | Self::TokenPrompt => "invalid_prompt",
            Self::Unsupported { .. } => "unsupported_parameter",
        }
    }
}

impl std::fmt::Display for CompletionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPrompt => f.write_str("prompt must be a non-empty string, or an array of them"),
            Self::TokenPrompt => f.write_str(
                "prompt must be text: this gateway does not accept pre-tokenized prompts, \
                 because the token ids belong to a tokenizer it cannot verify is yours",
            ),
            Self::Unsupported { param, why } => {
                write!(f, "{param} is not supported by this gateway: {why}")
            }
        }
    }
}

impl std::error::Error for CompletionError {}

/// A prompt, in any of the shapes OpenAI allows.
///
/// The token forms are represented so they can be **refused by name** rather
/// than failing to parse. A client that sent token ids and got "not valid JSON"
/// would have no idea what was wrong.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum CompletionPrompt {
    Text(String),
    Many(Vec<String>),
    /// `[1, 2, 3]` or `[[1, 2], [3, 4]]`.
    Tokens(Value),
}

impl CompletionPrompt {
    /// The prompts to run, or why they cannot be.
    ///
    /// One prompt per completion. An array yields one choice per entry, which
    /// is what the endpoint has always meant, and what a client batching a
    /// classification job over many short inputs is relying on.
    pub fn to_texts(&self) -> Result<Vec<String>, CompletionError> {
        let texts: Vec<String> = match self {
            Self::Text(text) => vec![text.clone()],
            Self::Many(texts) => texts.clone(),
            Self::Tokens(value) => {
                // An empty array is the one token-shaped value that is simply
                // an empty prompt list rather than pre-tokenized input.
                if value.as_array().is_some_and(|entries| entries.is_empty()) {
                    return Err(CompletionError::NoPrompt);
                }
                return Err(CompletionError::TokenPrompt);
            }
        };
        if texts.iter().all(String::is_empty) {
            return Err(CompletionError::NoPrompt);
        }
        Ok(texts)
    }
}

/// `POST /v1/completions`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CompletionRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub prompt: Option<CompletionPrompt>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub stream_options: Option<StreamOptions>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Completions per prompt. `1` unless asked otherwise.
    #[serde(default)]
    pub n: Option<u32>,
    /// Whether the prompt is repeated back at the head of the completion.
    #[serde(default)]
    pub echo: Option<bool>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub min_p: Option<f64>,
    #[serde(default)]
    pub presence_penalty: Option<f64>,
    #[serde(default)]
    pub frequency_penalty: Option<f64>,
    #[serde(default)]
    pub repeat_penalty: Option<f64>,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub stop: Option<OneOrMany<String>>,
    /// Per-token probabilities. Refused rather than ignored — see
    /// [`CompletionRequest::validate`].
    #[serde(default)]
    pub logprobs: Option<Value>,
    #[serde(default)]
    pub best_of: Option<u32>,
    #[serde(default)]
    pub suffix: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl CompletionRequest {
    pub fn wants_usage(&self) -> bool {
        self.stream_options
            .as_ref()
            .is_some_and(|options| options.include_usage)
    }

    pub fn echoes_the_prompt(&self) -> bool {
        self.echo.unwrap_or(false)
    }

    /// Completions to produce per prompt, floored at one.
    ///
    /// `n: 0` would ask for a response with no choices, which every client
    /// rejects — including this gateway's own contract suite.
    pub fn completions_per_prompt(&self) -> u32 {
        self.n.unwrap_or(1).max(1)
    }

    /// Refuse the parameters that would change the reply's shape.
    ///
    /// Each of these is refused rather than ignored because ignoring it returns
    /// a well-formed reply to a different request: a client that asked for
    /// `logprobs` and received `null` cannot tell whether the model had nothing
    /// to say or the gateway never asked.
    pub fn validate(&self) -> Result<(), CompletionError> {
        if self
            .logprobs
            .as_ref()
            .is_some_and(|value| !value.is_null() && value != &Value::Bool(false))
        {
            return Err(CompletionError::Unsupported {
                param: "logprobs",
                why: "per-token probabilities are not reported; remove the parameter to continue",
            });
        }
        if self.best_of.is_some_and(|best_of| best_of > 1) {
            return Err(CompletionError::Unsupported {
                param: "best_of",
                why: "generating several completions and returning only the best would spend \
                      the whole budget on discarded work",
            });
        }
        if self.suffix.as_ref().is_some_and(|text| !text.is_empty()) {
            return Err(CompletionError::Unsupported {
                param: "suffix",
                why: "fill-in-the-middle needs tokens that are specific to each model, and \
                      this gateway does not know which yours uses",
            });
        }
        Ok(())
    }

    /// The prompts this request expands to, one per completion.
    ///
    /// `n` multiplies each prompt, in prompt order, which is the order OpenAI
    /// assigns `index` in.
    pub fn expand(&self) -> Result<Vec<String>, CompletionError> {
        self.validate()?;
        let texts = self
            .prompt
            .as_ref()
            .ok_or(CompletionError::NoPrompt)?
            .to_texts()?;
        let per_prompt = self.completions_per_prompt() as usize;
        Ok(texts
            .into_iter()
            .flat_map(|text| std::iter::repeat_n(text, per_prompt))
            .collect())
    }

    /// One engine-neutral request for one of the expanded prompts.
    ///
    /// `max_tokens` is not applied here, for the same reason it is not in the
    /// chat path: clamping needs the model's context and the prompt's size,
    /// which only the gateway knows.
    pub fn to_generation_request(&self, prompt: &str) -> GenerationRequest {
        GenerationRequest {
            max_tokens: self.max_tokens,
            sampling: SamplingParams {
                temperature: self.temperature,
                top_p: self.top_p,
                top_k: self.top_k,
                min_p: self.min_p,
                presence_penalty: self.presence_penalty,
                frequency_penalty: self.frequency_penalty,
                repeat_penalty: self.repeat_penalty,
                seed: self.seed,
                stop: self
                    .stop
                    .clone()
                    .map(OneOrMany::into_vec)
                    .unwrap_or_default(),
            },
            ..GenerationRequest::with_prompt(Prompt::Text(lightweight_core::Private::new(
                prompt.to_owned(),
            )))
        }
    }

    /// The keys we accepted but did not act on.
    pub fn ignored_keys(&self) -> Vec<&str> {
        self.extra.keys().map(String::as_str).collect()
    }
}

/// One choice of a text completion.
///
/// Two fields are serialized even when null, because the OpenAI schema has them
/// on every choice and clients index them rather than testing for their
/// presence: `logprobs`, and `finish_reason` on a chunk that is not the last.
///
/// `finish_reason` is an `Option` rather than an empty string for exactly that
/// reason — `""` is not one of the values the schema defines, and a typed
/// client deserializing this into an enum would reject the whole chunk.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompletionChoice {
    pub text: String,
    pub index: u32,
    pub logprobs: Option<Value>,
    pub finish_reason: Option<String>,
}

/// A complete, non-streamed text completion.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    pub usage: UsageBody,
}

impl CompletionResponse {
    pub fn new(
        id: impl Into<String>,
        model: impl Into<String>,
        choices: Vec<CompletionChoice>,
        usage: UsageBody,
    ) -> Self {
        Self {
            id: id.into(),
            object: "text_completion".to_owned(),
            created: crate::unix_now(),
            model: model.into(),
            choices,
            usage,
        }
    }
}

/// One chunk of a streamed text completion.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompletionChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageBody>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

impl CompletionChunk {
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

/// Builds the chunks of one streamed text completion.
///
/// The mirror of [`crate::stream::ChunkBuilder`], and separate from it on
/// purpose: the two endpoints agree on almost nothing but the id, and a shared
/// builder would have to branch on which one it was serving in every method.
#[derive(Clone, Debug)]
pub struct CompletionChunkBuilder {
    id: String,
    model: String,
    created: u64,
}

impl CompletionChunkBuilder {
    pub fn new(id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            created: crate::unix_now(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    fn chunk(&self, choices: Vec<CompletionChoice>) -> CompletionChunk {
        CompletionChunk {
            id: self.id.clone(),
            object: "text_completion".to_owned(),
            created: self.created,
            model: self.model.clone(),
            choices,
            usage: None,
            error: None,
        }
    }

    /// A fragment of one choice's text.
    pub fn text(&self, index: u32, text: impl Into<String>) -> CompletionChunk {
        self.chunk(vec![CompletionChoice {
            text: text.into(),
            index,
            logprobs: None,
            finish_reason: None,
        }])
    }

    /// The chunk that closes one choice.
    ///
    /// Carries an empty `text` alongside the reason, because the schema has no
    /// way to say "this choice is finished" other than a choice object.
    pub fn finish(&self, index: u32, reason: FinishReason) -> CompletionChunk {
        self.chunk(vec![CompletionChoice {
            text: String::new(),
            index,
            logprobs: None,
            finish_reason: Some(reason.as_str().to_owned()),
        }])
    }

    /// The terminal usage chunk, with an empty `choices` array.
    ///
    /// OpenAI's shape, not the engine's: llama.cpp attaches usage to the last
    /// content chunk instead. A client that reads usage from a chunk with no
    /// choices — which the `openai` package does — needs this one.
    pub fn usage(&self, usage: UsageBody) -> CompletionChunk {
        CompletionChunk {
            usage: Some(usage),
            ..self.chunk(Vec::new())
        }
    }

    /// A stream that failed after its headers went out.
    pub fn error(&self, body: Value) -> CompletionChunk {
        CompletionChunk {
            error: Some(body),
            ..self.chunk(Vec::new())
        }
    }

    pub fn done(&self) -> String {
        sse::encode_done()
    }

    /// A comment frame, to hold the connection open during a long prefill.
    pub fn keep_alive(&self) -> String {
        sse::encode_comment("keep-alive")
    }
}

/// Sum the per-completion usages of one request into the total it reports.
///
/// A multi-prompt request is several generations, and the client is owed one
/// set of numbers covering all of them: billing, budgeting and the context
/// arithmetic a client does next all work on the total, not on the last one.
pub fn accumulate_usage(total: &mut UsageBody, part: UsageBody) {
    total.prompt_tokens = total.prompt_tokens.saturating_add(part.prompt_tokens);
    total.completion_tokens = total
        .completion_tokens
        .saturating_add(part.completion_tokens);
    total.total_tokens = total.total_tokens.saturating_add(part.total_tokens);
    let cached = total
        .prompt_tokens_details
        .map_or(0, |details| details.cached_tokens)
        .saturating_add(part.prompt_tokens_details.map_or(0, |d| d.cached_tokens));
    total.prompt_tokens_details = Some(PromptTokensDetails {
        cached_tokens: cached,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> CompletionRequest {
        serde_json::from_str(json).expect("a tolerant parse")
    }

    #[test]
    fn a_string_prompt_is_one_completion() {
        let request = parse(r#"{"prompt":"The capital of France is","max_tokens":8}"#);
        assert_eq!(
            request.expand().expect("ok"),
            vec!["The capital of France is".to_owned()]
        );
        assert_eq!(request.max_tokens, Some(8));
    }

    #[test]
    fn an_array_prompt_is_one_completion_each() {
        // What the endpoint has always meant, and what a client batching short
        // classification inputs relies on. Refusing it because one machine is
        // slow would bake that machine into the product.
        let request = parse(r#"{"prompt":["alpha","beta","gamma"]}"#);
        assert_eq!(
            request.expand().expect("ok"),
            vec!["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()]
        );
    }

    #[test]
    fn n_multiplies_each_prompt_in_prompt_order() {
        // OpenAI assigns `index` across the whole set, prompt-major, so the
        // expansion order is part of the contract rather than an implementation
        // detail.
        let request = parse(r#"{"prompt":["a","b"],"n":2}"#);
        assert_eq!(
            request.expand().expect("ok"),
            vec![
                "a".to_owned(),
                "a".to_owned(),
                "b".to_owned(),
                "b".to_owned()
            ]
        );
    }

    #[test]
    fn n_zero_still_produces_one_completion() {
        // A response with no choices is rejected outright by the client, so
        // zero is read as the one completion it must have meant.
        let request = parse(r#"{"prompt":"x","n":0}"#);
        assert_eq!(request.expand().expect("ok").len(), 1);
    }

    #[test]
    fn a_token_prompt_is_refused_by_name() {
        // Refused, not mis-parsed: a client that sent token ids and got "not
        // valid JSON" back would have nothing to go on.
        for body in [r#"{"prompt":[1,2,3]}"#, r#"{"prompt":[[1,2],[3,4]]}"#] {
            let err = parse(body).expand().expect_err("must refuse");
            assert_eq!(err, CompletionError::TokenPrompt);
            assert_eq!(err.param(), "prompt");
            assert!(err.to_string().contains("text"), "{err}");
        }
    }

    #[test]
    fn an_absent_or_empty_prompt_is_refused() {
        assert_eq!(
            parse(r#"{"max_tokens":4}"#).expand().expect_err("refuse"),
            CompletionError::NoPrompt
        );
        assert_eq!(
            parse(r#"{"prompt":""}"#).expand().expect_err("refuse"),
            CompletionError::NoPrompt
        );
        assert_eq!(
            parse(r#"{"prompt":[]}"#).expand().expect_err("refuse"),
            CompletionError::NoPrompt
        );
    }

    #[test]
    fn parameters_that_would_change_the_reply_are_refused_by_name() {
        // Ignoring these returns a well-formed reply to a different request.
        for (body, param) in [
            (r#"{"prompt":"x","logprobs":5}"#, "logprobs"),
            (r#"{"prompt":"x","best_of":4}"#, "best_of"),
            (r#"{"prompt":"x","suffix":"</code>"}"#, "suffix"),
        ] {
            let err = parse(body).expand().expect_err("must refuse");
            assert_eq!(err.param(), param);
            assert_eq!(err.code(), "unsupported_parameter");
            assert!(err.to_string().contains(param), "{err}");
        }
    }

    #[test]
    fn the_absent_forms_of_those_parameters_are_not_refused() {
        // `logprobs: null` and `logprobs: false` are what a client library
        // sends for "I did not ask for this", and refusing them would break
        // every request that library makes.
        for body in [
            r#"{"prompt":"x","logprobs":null}"#,
            r#"{"prompt":"x","logprobs":false}"#,
            r#"{"prompt":"x","best_of":1}"#,
            r#"{"prompt":"x","suffix":""}"#,
        ] {
            assert!(parse(body).expand().is_ok(), "{body} was refused");
        }
    }

    #[test]
    fn a_text_prompt_reaches_the_backend_as_text_not_as_a_message() {
        let request = parse(r#"{"prompt":"continue this","temperature":0.2,"stop":"END"}"#);
        let generation = request.to_generation_request("continue this");
        match &generation.prompt {
            Prompt::Text(text) => assert_eq!(text.reveal(), "continue this"),
            Prompt::Chat(_) => panic!("a completion must not become a conversation"),
        }
        assert!(generation.messages().is_empty());
        assert_eq!(generation.sampling.temperature, Some(0.2));
        assert_eq!(generation.sampling.stop, vec!["END".to_owned()]);
        // Nothing about tools: this endpoint has none, and sending an empty
        // declaration would still cost prompt tokens on some templates.
        assert!(generation.tools.is_empty());
    }

    #[test]
    fn a_request_full_of_unknown_fields_still_parses() {
        // The same tolerance the chat endpoint has. A 400 here would break a
        // client for a field that changes nothing.
        let request = parse(r#"{"prompt":"x","user":"someone","future_key":{"a":1}}"#);
        assert!(request.expand().is_ok());
        assert!(request.ignored_keys().contains(&"future_key"));
    }

    #[test]
    fn a_streamed_chunk_carries_the_text_completion_shape() {
        // `text`, not `delta`; `logprobs` present and null; `text_completion`
        // on every chunk. Clients index all three.
        let builder = CompletionChunkBuilder::new("cmpl-1", "m@8k");
        let json: Value =
            serde_json::from_str(&sse_payload(&builder.text(0, " Paris"))).expect("json");
        assert_eq!(json["object"], "text_completion");
        assert_eq!(json["choices"][0]["text"], " Paris");
        assert_eq!(json["choices"][0]["index"], 0);
        assert!(json["choices"][0]["logprobs"].is_null());
        // Null, never "": an empty string is not one of the values the schema
        // defines, and a typed client would reject the chunk over it.
        assert!(json["choices"][0]["finish_reason"].is_null());
        assert!(json["choices"][0].get("delta").is_none());
        assert!(json.get("usage").is_none());
    }

    #[test]
    fn the_usage_chunk_has_an_empty_choices_array() {
        // OpenAI's shape rather than the engine's, which attaches usage to the
        // last content chunk. The `openai` package reads it from a chunk with
        // no choices.
        let builder = CompletionChunkBuilder::new("cmpl-1", "m@8k");
        let json: Value = serde_json::from_str(&sse_payload(&builder.usage(UsageBody {
            prompt_tokens: 5,
            completion_tokens: 2,
            total_tokens: 7,
            prompt_tokens_details: None,
        })))
        .expect("json");
        assert_eq!(json["object"], "text_completion");
        assert_eq!(json["choices"].as_array().map(Vec::len), Some(0));
        assert_eq!(json["usage"]["total_tokens"], 7);
    }

    #[test]
    fn a_finish_chunk_names_the_choice_it_closes() {
        let builder = CompletionChunkBuilder::new("cmpl-1", "m@8k");
        let json: Value =
            serde_json::from_str(&sse_payload(&builder.finish(3, FinishReason::Length)))
                .expect("json");
        assert_eq!(json["choices"][0]["index"], 3);
        assert_eq!(json["choices"][0]["finish_reason"], "length");
        assert_eq!(json["choices"][0]["text"], "");
    }

    #[test]
    fn usage_from_several_completions_is_summed() {
        // One request, one set of numbers: a client budgeting its context works
        // on the total, not on whichever generation happened to finish last.
        let mut total = UsageBody::default();
        accumulate_usage(
            &mut total,
            UsageBody {
                prompt_tokens: 5,
                completion_tokens: 2,
                total_tokens: 7,
                prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 1 }),
            },
        );
        accumulate_usage(
            &mut total,
            UsageBody {
                prompt_tokens: 6,
                completion_tokens: 3,
                total_tokens: 9,
                prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 4 }),
            },
        );
        assert_eq!(total.prompt_tokens, 11);
        assert_eq!(total.completion_tokens, 5);
        assert_eq!(total.total_tokens, 16);
        assert_eq!(
            total.prompt_tokens_details.map(|d| d.cached_tokens),
            Some(5)
        );
    }

    #[test]
    fn the_response_object_is_a_text_completion() {
        let response = CompletionResponse::new(
            "cmpl-1",
            "m@8k",
            vec![CompletionChoice {
                text: " Paris".into(),
                index: 0,
                logprobs: None,
                finish_reason: Some("stop".into()),
            }],
            UsageBody::default(),
        );
        let json = serde_json::to_value(&response).expect("serialize");
        assert_eq!(json["object"], "text_completion");
        assert_eq!(json["choices"][0]["text"], " Paris");
        // Present and null, never absent: clients index it.
        assert!(json["choices"][0]["logprobs"].is_null());
        assert!(
            json["choices"][0]
                .as_object()
                .is_some_and(|choice| { choice.contains_key("logprobs") })
        );
    }

    /// Strip the `data: ` framing so the payload can be parsed as JSON.
    fn sse_payload(chunk: &CompletionChunk) -> String {
        chunk
            .to_sse_frame()
            .trim_start_matches("data: ")
            .trim_end()
            .to_owned()
    }
}
