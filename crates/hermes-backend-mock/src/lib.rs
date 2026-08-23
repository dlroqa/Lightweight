//! A deterministic backend that runs no engine.
//!
//! Two jobs, both of which a real engine is bad at.
//!
//! **It makes the layers above testable.** The gateway's contract — chunk
//! order, the usage chunk, the terminal `[DONE]`, what happens when a
//! generation fails halfway — is about *framing*, not about what a model says.
//! Testing that against llama.cpp would need a model, a download and tens of
//! seconds per case, and would still not let us ask for a failure at token
//! three.
//!
//! **It proves the abstraction is real.** Spec sections 28 and 37 promise that
//! the engine can be replaced without the gateway noticing. A second
//! implementation of [`InferenceBackend`] that the same suite passes against is
//! evidence for that; a comment saying so is not.
//!
//! Every behaviour is scripted up front, so a test states its scenario and
//! reads its assertions with nothing in between.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use futures_util::stream::{self, StreamExt};
use hermes_core::{GgmlType, InstanceId, ModelId, RuntimeParams, units::Bytes};
use hermes_inference::generation::{
    FinishReason, GenerationEvent, GenerationRequest, Timings, Usage,
};
use hermes_inference::{
    BackendCapabilities, BackendError, BackendHealth, BackendId, DeviceKind, GenerationStream,
    InferenceBackend, LoadProgress, LoadRequest, LoadedModel, ResourceSnapshot,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub const BACKEND_ID: BackendId = BackendId("mock");

/// What the mock should do when asked to generate.
#[derive(Clone, Debug)]
pub enum Script {
    /// Emit these content fragments, then finish normally.
    Content(Vec<String>),
    /// Emit reasoning fragments, then content, then finish.
    Reasoning {
        reasoning: Vec<String>,
        content: Vec<String>,
    },
    /// Emit one tool call, split across deltas the way an engine does.
    ToolCall {
        id: String,
        name: String,
        /// Argument fragments, concatenated by the client in order.
        argument_fragments: Vec<String>,
    },
    /// Produce nothing at all and finish.
    ///
    /// The case that makes a client raise `EmptyStreamError` and retry
    /// blindly, so the gateway's handling of it is worth testing deliberately.
    Empty,
    /// Emit some content and then fail, with output already on the wire.
    FailMidStream { content: Vec<String>, error: String },
    /// Refuse before generating anything.
    Fail(String),
    /// Emit content forever, one fragment every `interval`.
    ///
    /// For cancellation tests: the stream only ends when someone stops it.
    Endless {
        fragment: String,
        interval: Duration,
    },
}

impl Default for Script {
    fn default() -> Self {
        Self::Content(vec!["Hello".into(), ", ".into(), "world".into()])
    }
}

/// How the mock behaves.
#[derive(Clone, Debug)]
pub struct MockConfig {
    pub script: Script,
    /// Simulated prefill, before the first event.
    ///
    /// Non-zero is how the keep-alive path gets exercised: on a CPU without
    /// AVX a real prefill can occupy a minute or more.
    pub prefill: Duration,
    /// Delay between content fragments.
    pub token_interval: Duration,
    /// What `count_prompt_tokens` reports.
    pub prompt_tokens: u32,
    /// Fail loading, rather than generating.
    pub load_error: Option<String>,
    /// The context the loaded instance claims to have.
    pub n_ctx: u32,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            script: Script::default(),
            prefill: Duration::ZERO,
            token_interval: Duration::ZERO,
            prompt_tokens: 11,
            load_error: None,
            n_ctx: RuntimeParams::default().n_ctx,
        }
    }
}

/// A backend that produces scripted output.
#[derive(Debug)]
pub struct MockBackend {
    config: Mutex<MockConfig>,
    resident: Mutex<Option<LoadedModel>>,
    /// Counts generations, so a test can assert that a request the gateway
    /// should have refused never reached an engine.
    generations: AtomicU64,
    /// Counts loads, which is how residency and unload behaviour is checked.
    loads: AtomicU64,
    /// The last request this backend was asked to generate.
    ///
    /// Kept so a test can assert what actually reached the engine boundary —
    /// which is the only way to prove that a request option a client sent was
    /// forwarded rather than quietly dropped in the layers above.
    last_request: Mutex<Option<GenerationRequest>>,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new(MockConfig::default())
    }
}

impl MockBackend {
    pub fn new(config: MockConfig) -> Self {
        Self {
            config: Mutex::new(config),
            resident: Mutex::new(None),
            generations: AtomicU64::new(0),
            loads: AtomicU64::new(0),
            last_request: Mutex::new(None),
        }
    }

    /// A backend that answers every request with `content`.
    pub fn saying(content: impl Into<String>) -> Self {
        Self::new(MockConfig {
            script: Script::Content(vec![content.into()]),
            ..MockConfig::default()
        })
    }

    /// Replace the script between requests.
    pub async fn set_script(&self, script: Script) {
        self.config.lock().await.script = script;
    }

    pub async fn set_config(&self, config: MockConfig) {
        *self.config.lock().await = config;
    }

    pub fn generation_count(&self) -> u64 {
        self.generations.load(Ordering::Relaxed)
    }

    pub fn load_count(&self) -> u64 {
        self.loads.load(Ordering::Relaxed)
    }

    /// What the last generation was actually asked for.
    pub async fn last_request(&self) -> Option<GenerationRequest> {
        self.last_request.lock().await.clone()
    }

    /// Load a model without a GGUF file on disk.
    ///
    /// The trait's `load` takes a path and parsed metadata, which a test that
    /// only cares about the wire contract should not have to produce.
    pub async fn make_resident(&self, model: ModelId, n_ctx: u32) -> LoadedModel {
        let loaded = LoadedModel {
            model,
            backend: BACKEND_ID,
            instance: InstanceId::new(),
            effective: RuntimeParams::default().with_context(n_ctx),
            loaded_at: SystemTime::now(),
        };
        self.loads.fetch_add(1, Ordering::Relaxed);
        *self.resident.lock().await = Some(loaded.clone());
        loaded
    }
}

#[async_trait::async_trait]
impl InferenceBackend for MockBackend {
    fn id(&self) -> BackendId {
        BACKEND_ID
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            device: DeviceKind::Cpu,
            streaming: true,
            tool_calls: true,
            reasoning_content: true,
            max_concurrent_requests: 1,
            kv_cache_types: vec![GgmlType::F16, GgmlType::Q8_0],
        }
    }

    async fn load(
        &self,
        request: LoadRequest,
        progress: tokio::sync::mpsc::Sender<LoadProgress>,
        cancel: CancellationToken,
    ) -> Result<LoadedModel, BackendError> {
        // `try_send` for the same reason the real backend uses it: a caller
        // that stops draining progress must not be able to stall a load.
        let _ = progress.try_send(LoadProgress::StartingEngine);
        if cancel.is_cancelled() {
            return Err(BackendError::Cancelled);
        }
        if let Some(detail) = self.config.lock().await.load_error.clone() {
            return Err(BackendError::EngineCrashed {
                detail,
                exit_code: Some(1),
                signal: None,
                tail: vec!["mock: refusing to load".into()],
            });
        }
        let _ = progress.try_send(LoadProgress::LoadingWeights);
        let loaded = self
            .make_resident(request.model.clone(), request.runtime.n_ctx)
            .await;
        let _ = progress.try_send(LoadProgress::Ready);
        Ok(loaded)
    }

    async fn unload(&self, instance: InstanceId) -> Result<(), BackendError> {
        let mut resident = self.resident.lock().await;
        if resident
            .as_ref()
            .is_some_and(|loaded| loaded.instance == instance)
        {
            *resident = None;
        }
        Ok(())
    }

    async fn generate(
        &self,
        instance: InstanceId,
        request: GenerationRequest,
        cancel: CancellationToken,
    ) -> Result<GenerationStream, BackendError> {
        self.ensure_resident(instance).await?;
        self.generations.fetch_add(1, Ordering::Relaxed);
        *self.last_request.lock().await = Some(request);

        let config = self.config.lock().await.clone();
        if let Script::Fail(detail) = &config.script {
            return Err(BackendError::GenerationFailed {
                detail: detail.clone(),
            });
        }
        Ok(script_stream(config, cancel))
    }

    async fn count_prompt_tokens(
        &self,
        instance: InstanceId,
        _request: &GenerationRequest,
    ) -> Result<u32, BackendError> {
        self.ensure_resident(instance).await?;
        Ok(self.config.lock().await.prompt_tokens)
    }

    async fn health(&self) -> BackendHealth {
        if self.resident.lock().await.is_some() {
            BackendHealth::Ready
        } else {
            BackendHealth::Stopped
        }
    }

    async fn resource_usage(&self) -> Result<Option<ResourceSnapshot>, BackendError> {
        if self.resident.lock().await.is_none() {
            return Ok(None);
        }
        Ok(Some(ResourceSnapshot {
            rss: Bytes::from_mib(512),
            peak_rss: Bytes::from_mib(600),
            cpu_percent: Some(0.0),
        }))
    }

    async fn shutdown(&self) -> Result<(), BackendError> {
        *self.resident.lock().await = None;
        Ok(())
    }
}

impl MockBackend {
    async fn ensure_resident(&self, instance: InstanceId) -> Result<(), BackendError> {
        let resident = self.resident.lock().await;
        match resident.as_ref() {
            Some(loaded) if loaded.instance == instance => Ok(()),
            _ => Err(BackendError::NoModelLoaded),
        }
    }
}

/// One step of a scripted stream.
enum Step {
    Event(GenerationEvent),
    Error(BackendError),
    /// Repeat the previous fragment forever, for cancellation tests.
    Repeat(String),
}

/// Turn a script into the event sequence a real backend would produce.
fn script_stream(config: MockConfig, cancel: CancellationToken) -> GenerationStream {
    let prompt_tokens = config.prompt_tokens;
    let mut steps: Vec<Step> = vec![Step::Event(GenerationEvent::Started {
        prompt_tokens: Some(prompt_tokens),
    })];
    let mut completion_tokens = 0_u32;
    let mut finish = FinishReason::Stop;
    let mut trailing_error = None;

    let push_content = |steps: &mut Vec<Step>, fragments: &[String], count: &mut u32| {
        for fragment in fragments {
            *count += 1;
            steps.push(Step::Event(GenerationEvent::ContentDelta {
                text: fragment.clone(),
            }));
        }
    };

    match &config.script {
        Script::Content(fragments) => {
            push_content(&mut steps, fragments, &mut completion_tokens);
        }
        Script::Reasoning { reasoning, content } => {
            for fragment in reasoning {
                completion_tokens += 1;
                steps.push(Step::Event(GenerationEvent::ReasoningDelta {
                    text: fragment.clone(),
                }));
            }
            push_content(&mut steps, content, &mut completion_tokens);
        }
        Script::ToolCall {
            id,
            name,
            argument_fragments,
        } => {
            finish = FinishReason::ToolCalls;
            // The first delta carries the id and the name; every later one
            // carries only an argument fragment. That is the discipline the
            // client's accumulator depends on.
            steps.push(Step::Event(GenerationEvent::ToolCallDelta {
                index: 0,
                id: Some(id.clone()),
                name: Some(name.clone()),
                arguments: Some(String::new()),
            }));
            for fragment in argument_fragments {
                completion_tokens += 1;
                steps.push(Step::Event(GenerationEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    arguments: Some(fragment.clone()),
                }));
            }
        }
        Script::Empty => {}
        Script::FailMidStream { content, error } => {
            push_content(&mut steps, content, &mut completion_tokens);
            trailing_error = Some(error.clone());
        }
        // Handled before the stream is built.
        Script::Fail(_) => {}
        Script::Endless { fragment, .. } => {
            steps.push(Step::Repeat(fragment.clone()));
        }
    }

    if let Some(detail) = trailing_error {
        steps.push(Step::Error(BackendError::GenerationFailed { detail }));
    } else if !matches!(config.script, Script::Endless { .. }) {
        steps.push(Step::Event(GenerationEvent::Timings(Timings {
            prompt_n: prompt_tokens,
            prompt_ms: 100.0,
            predicted_n: completion_tokens,
            predicted_ms: 50.0,
            cached_n: 0,
        })));
        steps.push(Step::Event(GenerationEvent::Finished {
            finish_reason: finish,
            usage: Usage::new(prompt_tokens, completion_tokens),
        }));
    }

    let interval = config.token_interval;
    let prefill = config.prefill;
    let endless_interval = match config.script {
        Script::Endless { interval, .. } => interval,
        _ => Duration::from_millis(10),
    };

    stream::unfold(
        (steps.into_iter(), cancel, true),
        move |(mut steps, cancel, first)| async move {
            let delay = if first { prefill } else { interval };
            if !delay.is_zero() {
                tokio::select! {
                    () = cancel.cancelled() => return None,
                    () = tokio::time::sleep(delay) => {}
                }
            } else if cancel.is_cancelled() {
                return None;
            }

            let step = steps.next()?;
            match step {
                Step::Event(event) => Some((Ok(event), (steps, cancel, false))),
                Step::Error(err) => Some((Err(err), (steps, cancel, false))),
                Step::Repeat(fragment) => {
                    // Put the repeat back so the stream never ends on its own.
                    let event = GenerationEvent::ContentDelta {
                        text: fragment.clone(),
                    };
                    tokio::select! {
                        () = cancel.cancelled() => None,
                        () = tokio::time::sleep(endless_interval) => Some((
                            Ok(event),
                            (
                                vec![Step::Repeat(fragment)].into_iter(),
                                cancel,
                                false,
                            ),
                        )),
                    }
                }
            }
        },
    )
    .boxed()
}

/// A backend behind an `Arc`, which is how the gateway holds one.
pub fn shared(config: MockConfig) -> Arc<MockBackend> {
    Arc::new(MockBackend::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_inference::generation::ChatMessage;

    async fn collect(backend: &MockBackend) -> Vec<GenerationEvent> {
        let loaded = backend.make_resident(ModelId::new("mock@4k"), 4096).await;
        let stream = backend
            .generate(
                loaded.instance,
                GenerationRequest::new(vec![ChatMessage::user("hi")]),
                CancellationToken::new(),
            )
            .await
            .expect("stream");
        stream
            .filter_map(|event| async move { event.ok() })
            .collect()
            .await
    }

    #[tokio::test]
    async fn content_arrives_between_a_start_and_a_finish() {
        let backend = MockBackend::new(MockConfig {
            script: Script::Content(vec!["a".into(), "b".into()]),
            ..MockConfig::default()
        });
        let events = collect(&backend).await;
        assert!(matches!(
            events.first(),
            Some(GenerationEvent::Started { .. })
        ));
        assert!(matches!(
            events.last(),
            Some(GenerationEvent::Finished {
                finish_reason: FinishReason::Stop,
                ..
            })
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, GenerationEvent::ContentDelta { .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn usage_counts_what_was_produced() {
        let backend = MockBackend::new(MockConfig {
            script: Script::Content(vec!["a".into(), "b".into(), "c".into()]),
            prompt_tokens: 20,
            ..MockConfig::default()
        });
        let events = collect(&backend).await;
        let Some(GenerationEvent::Finished { usage, .. }) = events.last() else {
            panic!("no finish event");
        };
        assert_eq!(usage.prompt_tokens, 20);
        assert_eq!(usage.completion_tokens, 3);
        assert_eq!(usage.total_tokens, 23);
    }

    #[tokio::test]
    async fn an_empty_script_still_finishes() {
        // A stream that produces nothing must still terminate properly, or the
        // layer above cannot tell "the model said nothing" from "the engine
        // hung".
        let backend = MockBackend::new(MockConfig {
            script: Script::Empty,
            ..MockConfig::default()
        });
        let events = collect(&backend).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, GenerationEvent::ContentDelta { .. }))
        );
        assert!(matches!(
            events.last(),
            Some(GenerationEvent::Finished { .. })
        ));
    }

    #[tokio::test]
    async fn a_tool_call_sends_its_id_and_name_exactly_once() {
        let backend = MockBackend::new(MockConfig {
            script: Script::ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                argument_fragments: vec!["{\"path\":".into(), "\"a.txt\"}".into()],
            },
            ..MockConfig::default()
        });
        let events = collect(&backend).await;
        let ids: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                GenerationEvent::ToolCallDelta { id, .. } => id.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["call_1".to_owned()]);
        assert!(matches!(
            events.last(),
            Some(GenerationEvent::Finished {
                finish_reason: FinishReason::ToolCalls,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_mid_stream_failure_reaches_the_caller_after_the_content() {
        let backend = MockBackend::new(MockConfig {
            script: Script::FailMidStream {
                content: vec!["partial".into()],
                error: "engine gave up".into(),
            },
            ..MockConfig::default()
        });
        let loaded = backend.make_resident(ModelId::new("mock@4k"), 4096).await;
        let stream = backend
            .generate(
                loaded.instance,
                GenerationRequest::new(vec![ChatMessage::user("hi")]),
                CancellationToken::new(),
            )
            .await
            .expect("stream");
        let results: Vec<_> = stream.collect().await;
        assert!(results.iter().any(|result| result.is_err()));
    }

    #[tokio::test]
    async fn the_backend_records_what_it_was_asked_for() {
        // How a test proves that an option the client sent survived every
        // layer above, rather than being dropped somewhere polite about it.
        let backend = MockBackend::default();
        let loaded = backend.make_resident(ModelId::new("mock@4k"), 4096).await;
        let mut request = GenerationRequest::new(vec![ChatMessage::user("hi")]).without_reasoning();
        request
            .template_options
            .insert("enable_thinking".into(), serde_json::json!(false));

        let _ = backend
            .generate(loaded.instance, request, CancellationToken::new())
            .await
            .expect("stream");

        let seen = backend
            .last_request()
            .await
            .expect("a request was recorded");
        assert_eq!(
            seen.reasoning,
            hermes_inference::generation::ReasoningControl::Disabled
        );
        assert_eq!(seen.template_options["enable_thinking"], false);
    }

    #[tokio::test]
    async fn generating_against_a_stale_instance_is_refused() {
        // The case that matters after a reload: a request queued against a
        // model that is no longer loaded must not silently run against a
        // different one.
        let backend = MockBackend::default();
        let first = backend.make_resident(ModelId::new("a@4k"), 4096).await;
        let _second = backend.make_resident(ModelId::new("b@4k"), 4096).await;
        let err = backend
            .generate(
                first.instance,
                GenerationRequest::new(vec![ChatMessage::user("hi")]),
                CancellationToken::new(),
            )
            .await
            .err()
            .expect("a stale instance must be refused");
        assert!(matches!(err, BackendError::NoModelLoaded));
    }

    #[tokio::test]
    async fn cancelling_ends_an_endless_stream() {
        let backend = MockBackend::new(MockConfig {
            script: Script::Endless {
                fragment: "tick ".into(),
                interval: Duration::from_millis(5),
            },
            ..MockConfig::default()
        });
        let loaded = backend.make_resident(ModelId::new("mock@4k"), 4096).await;
        let cancel = CancellationToken::new();
        let mut stream = backend
            .generate(
                loaded.instance,
                GenerationRequest::new(vec![ChatMessage::user("hi")]),
                cancel.clone(),
            )
            .await
            .expect("stream");

        assert!(stream.next().await.is_some());
        cancel.cancel();
        // Bounded: if cancellation did not take effect this would hang, and a
        // hanging test is the symptom of a leaked generation.
        let ended = tokio::time::timeout(Duration::from_secs(5), async {
            while stream.next().await.is_some() {}
        })
        .await;
        assert!(ended.is_ok(), "cancellation did not end the stream");
    }
}
