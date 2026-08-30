//! The gateway as a client sees it: over a real socket, with real HTTP.
//!
//! These tests deliberately do not call the handlers directly. Half of what
//! spec section 12 promises is framing — headers, chunk boundaries, the
//! terminal `[DONE]`, a body that stops when the client goes away — and none of
//! that exists until the response has been through hyper.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use lightweight_backend_mock::{MockBackend, MockConfig, Script};
use lightweight_core::{ModelId, SseDecoder, SseEvent};
use lightweight_gateway::catalog::{Catalog, ResidentModel};
use lightweight_gateway::scheduler::{Band, PeerKey};
use lightweight_gateway::{AuthPolicy, GatewayConfig, GatewayState};
use lightweight_inference::InferenceBackend;
use lightweight_inference::generation::{Prompt, ToolChoice};
use serde_json::{Value, json};

/// A gateway bound to an ephemeral loopback port.
struct Harness {
    base: String,
    backend: Arc<MockBackend>,
    state: Arc<GatewayState>,
    _server: tokio::task::JoinHandle<()>,
}

const N_CTX: u32 = 4096;

impl Harness {
    async fn start(config: MockConfig, gateway: GatewayConfig) -> Self {
        let backend = Arc::new(MockBackend::new(config));
        let model = ModelId::with_context("mock-model", N_CTX);
        let loaded = backend.make_resident(model.clone(), N_CTX).await;
        let catalog = Arc::new(Catalog::with_resident(ResidentModel {
            id: model,
            instance: loaded.instance,
            n_ctx: N_CTX,
            architecture: "mock".into(),
            param_count: Some(1_200_000_000),
            quantization: Some("Q4_K_M".into()),
            model_max_context_length: Some(131_072),
            ram_verdict: Some("safe".into()),
            backend: Some("mock".into()),
            model_path: "/mock/model.gguf".into(),
            effective: lightweight_core::RuntimeParams::default(),
        }));

        let state = Arc::new(GatewayState::new(
            Arc::clone(&backend) as Arc<dyn InferenceBackend>,
            catalog,
            gateway,
        ));
        let app = lightweight_gateway::app(Arc::clone(&state));
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, lightweight_gateway::service(app)).await;
        });

        Self {
            base: format!("http://127.0.0.1:{port}"),
            backend,
            state,
            _server: server,
        }
    }

    async fn default() -> Self {
        Self::start(MockConfig::default(), GatewayConfig::default()).await
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("client")
    }

    async fn post_chat(&self, body: Value) -> reqwest::Response {
        Self::client()
            .post(format!("{}/v1/chat/completions", self.base))
            // Exactly what the client sends when no key is configured.
            .header("Authorization", "Bearer no-key-required")
            .json(&body)
            .send()
            .await
            .expect("request")
    }

    async fn post_completions(&self, body: Value) -> reqwest::Response {
        Self::client()
            .post(format!("{}/v1/completions", self.base))
            .header("Authorization", "Bearer no-key-required")
            .json(&body)
            .send()
            .await
            .expect("request")
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        Self::client()
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .expect("request")
    }

    async fn get_json(&self, path: &str) -> Value {
        self.get(path).await.json().await.expect("json body")
    }
}

/// Read a whole SSE response into decoded events.
async fn read_stream(response: reqwest::Response) -> Vec<SseEvent> {
    let body = response.text().await.expect("body");
    let mut decoder = SseDecoder::new();
    decoder.feed(body.as_bytes()).expect("decode");
    assert!(
        !decoder.has_pending(),
        "the stream ended mid-frame: {body:?}"
    );
    decoder.drain()
}

fn chunk(event: &SseEvent) -> Value {
    serde_json::from_str(&event.data).expect("chunk json")
}

fn ensure_provider() {
    // `reqwest` panics rather than erroring when no rustls provider has been
    // installed, even for a plain http:// request.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[tokio::test]
async fn a_streamed_completion_arrives_in_the_contracted_order() {
    ensure_provider();
    let harness = Harness::start(
        MockConfig {
            script: Script::Content(vec!["Hello".into(), ", ".into(), "world".into()]),
            prompt_tokens: 12,
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "stream_options": {"include_usage": true}
        }))
        .await;

    assert_eq!(response.status(), 200);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
    // Without these a chunk can sit in a buffer until the next one pushes it
    // out, which on a slow CPU means tokens arriving in bursts.
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-cache")
    );
    assert_eq!(
        response
            .headers()
            .get("x-accel-buffering")
            .and_then(|value| value.to_str().ok()),
        Some("no")
    );

    let events = read_stream(response).await;
    assert_eq!(
        chunk(&events[0])["choices"][0]["delta"]["role"],
        "assistant"
    );
    let content: String = events[1..4]
        .iter()
        .map(|event| {
            chunk(event)["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(content, "Hello, world");
    assert_eq!(chunk(&events[4])["choices"][0]["finish_reason"], "stop");

    let usage = chunk(&events[5]);
    assert_eq!(usage["choices"], json!([]));
    assert_eq!(usage["usage"]["prompt_tokens"], 12);
    assert_eq!(usage["usage"]["completion_tokens"], 3);
    assert!(events[6].is_done());

    // Every chunk carries the identity the client reads back, and the model is
    // our catalog id rather than an engine path.
    for event in &events[..events.len() - 1] {
        let chunk = chunk(event);
        assert_eq!(chunk["model"], "mock-model@4k");
        assert_eq!(chunk["object"], "chat.completion.chunk");
        assert!(
            chunk["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("chatcmpl-"))
        );
    }
}

#[tokio::test]
async fn a_non_streamed_completion_always_has_choices() {
    ensure_provider();
    // An absent or empty `choices` is rejected outright by the client
    // (transports/chat_completions.py:1010), so this is not a formality.
    let harness = Harness::default().await;
    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await;

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["role"], "assistant");
    assert_eq!(body["choices"][0]["message"]["content"], "Hello, world");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["model"], "mock-model@4k");
    assert!(body["usage"]["total_tokens"].as_u64().is_some());
}

#[tokio::test]
async fn unknown_request_fields_are_accepted() {
    ensure_provider();
    // The client sends `reasoning_effort`, `think` and `options.num_ctx`, and
    // will send more in versions we have not seen.
    let harness = Harness::default().await;
    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high",
            "think": true,
            "options": {"num_ctx": 8192},
            "parallel_tool_calls": false,
            "a_field_from_2027": {"nested": true}
        }))
        .await;
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn the_clients_default_max_tokens_is_clamped_not_refused() {
    ensure_provider();
    // Hermes defaults `max_tokens` to 65536, far beyond any context we load.
    let harness = Harness::default().await;
    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 65536
        }))
        .await;
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn an_overlong_prompt_is_a_parsable_400() {
    ensure_provider();
    // The alternative is an empty stream, which the client retries blindly —
    // the exact failure this pre-flight exists to prevent.
    let harness = Harness::start(
        MockConfig {
            prompt_tokens: N_CTX + 100,
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "a very long conversation"}],
            "stream": true
        }))
        .await;

    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "context_length_exceeded");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["param"], "messages");
    let message = body["error"]["message"].as_str().expect("message");
    assert!(
        message.contains("maximum context length is 4096"),
        "the client parses the limit out of this text: {message}"
    );
    // Refused before the engine was asked to do anything.
    assert_eq!(harness.backend.generation_count(), 0);
}

#[tokio::test]
async fn a_request_with_no_messages_is_refused_with_a_reason() {
    ensure_provider();
    let harness = Harness::default().await;
    let response = harness
        .post_chat(json!({"model": "mock-model@4k", "messages": []}))
        .await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["param"], "messages");
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn a_model_we_do_not_have_is_a_404_that_names_what_we_do() {
    ensure_provider();
    let harness = Harness::default().await;
    let response = harness
        .post_chat(json!({
            "model": "llama-3.2-3b",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await;
    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.expect("json");
    let message = body["error"]["message"].as_str().expect("message");
    assert!(message.contains("mock-model@4k"), "{message}");
}

#[tokio::test]
async fn a_stale_context_suffix_is_still_served() {
    ensure_provider();
    // Our own naming policy produces this: the id carries the context, so a
    // client that cached `@8k` while we now serve `@4k` is holding a name we
    // invented.
    let harness = Harness::default().await;
    let response = harness
        .post_chat(json!({
            "model": "mock-model@8k",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    // And the response says what actually answered.
    assert_eq!(body["model"], "mock-model@4k");
}

#[tokio::test]
async fn models_advertises_the_effective_context() {
    ensure_provider();
    let harness = Harness::default().await;
    let body: Value = harness.get("/v1/models").await.json().await.expect("json");
    assert_eq!(body["object"], "list");
    let row = &body["data"][0];
    assert_eq!(row["id"], "mock-model@4k");
    for key in ["context_length", "n_ctx", "max_tokens", "max_output_tokens"] {
        assert_eq!(row[key], N_CTX, "{key}");
    }
    // The model's real ceiling is reported, under a name the client's context
    // scanner does not recognize.
    assert_eq!(row["hermes"]["model_max_context_length"], 131_072);
}

#[tokio::test]
async fn props_and_models_agree_about_the_context() {
    ensure_provider();
    // Two endpoints disagreeing about the window is worse than one of them
    // being absent: the client probes both.
    let harness = Harness::default().await;
    let props: Value = harness.get("/props").await.json().await.expect("json");
    let models: Value = harness.get("/v1/models").await.json().await.expect("json");
    assert_eq!(
        props["default_generation_settings"]["n_ctx"],
        models["data"][0]["context_length"]
    );
    assert_eq!(props["total_slots"], 1);
}

#[tokio::test]
async fn health_reports_the_loaded_model() {
    ensure_provider();
    let harness = Harness::default().await;
    let response = harness.get("/health").await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["model"], "mock-model@4k");
    assert_eq!(body["backend"], "mock");
}

#[tokio::test]
async fn a_missing_authorization_header_is_accepted_on_loopback() {
    ensure_provider();
    let harness = Harness::default().await;
    let response = Harness::client()
        .post(format!("{}/v1/chat/completions", harness.base))
        .json(&json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn a_configured_key_is_enforced() {
    ensure_provider();
    let harness = Harness::start(
        MockConfig::default(),
        GatewayConfig {
            auth: AuthPolicy::Required {
                key: "shared-secret".into(),
            },
            ..GatewayConfig::default()
        },
    )
    .await;

    let unauthenticated = Harness::client()
        .post(format!("{}/v1/chat/completions", harness.base))
        .json(&json!({"messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("request");
    assert_eq!(unauthenticated.status(), 401);
    let body: Value = unauthenticated.json().await.expect("json");
    assert_eq!(body["error"]["type"], "authentication_error");

    let authenticated = Harness::client()
        .post(format!("{}/v1/chat/completions", harness.base))
        .header("Authorization", "Bearer shared-secret")
        .json(&json!({"messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("request");
    assert_eq!(authenticated.status(), 200);
}

#[tokio::test]
async fn a_generation_that_fails_midway_ends_the_stream_cleanly() {
    ensure_provider();
    let harness = Harness::start(
        MockConfig {
            script: Script::FailMidStream {
                content: vec!["partial answer".into()],
                error: "the engine stopped".into(),
            },
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }))
        .await;
    assert_eq!(response.status(), 200);

    let events = read_stream(response).await;
    let error_chunk = chunk(&events[events.len() - 2]);
    assert_eq!(error_chunk["choices"][0]["finish_reason"], "error");
    assert_eq!(error_chunk["error"]["code"], "generation_failed");
    assert!(
        events.last().expect("frames").is_done(),
        "a failed stream must still terminate with [DONE]"
    );
}

#[tokio::test]
async fn a_failure_before_the_first_byte_is_an_http_status() {
    ensure_provider();
    let harness = Harness::start(
        MockConfig {
            script: Script::Fail("no slot available".into()),
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;
    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }))
        .await;
    assert_eq!(response.status(), 500);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "generation_failed");
}

#[tokio::test]
async fn a_client_that_disconnects_mid_stream_frees_the_slot() {
    ensure_provider();
    // The property the whole cancellation design rests on. If the slot leaked,
    // the next request would wait out the queue timeout instead of being
    // served.
    let harness = Harness::start(
        MockConfig {
            script: Script::Endless {
                fragment: "tick ".into(),
                interval: Duration::from_millis(10),
            },
            ..MockConfig::default()
        },
        GatewayConfig {
            queue_timeout: Duration::from_secs(2),
            ..GatewayConfig::default()
        },
    )
    .await;

    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }))
        .await;
    assert_eq!(response.status(), 200);
    // Drop the response without reading it to the end: this is a client
    // walking away mid-generation.
    drop(response);

    // The slot must come back. Poll briefly rather than sleeping a fixed time,
    // so a slow machine does not turn this into a flake.
    let freed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(permit) = harness
                .state
                .acquire_slot(Band::Bulk, PeerKey::default())
                .await
            {
                return permit;
            }
        }
    })
    .await;
    assert!(freed.is_ok(), "the slot was not released on disconnect");

    harness
        .backend
        .set_script(Script::Content(vec!["ok".into()]))
        .await;
}

#[tokio::test]
async fn an_empty_generation_still_produces_a_terminated_stream() {
    ensure_provider();
    // The model saying nothing is a real outcome. We never fabricate a token
    // to hide it — but the stream must still be well formed, or the client
    // cannot tell an empty answer from a broken connection.
    let harness = Harness::start(
        MockConfig {
            script: Script::Empty,
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;
    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "stream_options": {"include_usage": true}
        }))
        .await;
    let events = read_stream(response).await;
    assert_eq!(
        chunk(&events[0])["choices"][0]["delta"]["role"],
        "assistant"
    );
    assert_eq!(chunk(&events[1])["choices"][0]["finish_reason"], "stop");
    assert!(chunk(&events[2])["usage"]["completion_tokens"] == 0);
    assert!(events.last().expect("frames").is_done());
}

#[tokio::test]
async fn tool_call_deltas_reach_the_client_ready_to_accumulate() {
    ensure_provider();
    let harness = Harness::start(
        MockConfig {
            script: Script::ToolCall {
                id: "call_abc".into(),
                name: "read_file".into(),
                argument_fragments: vec!["{\"path\"".into(), ":\"a.txt\"}".into()],
            },
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "read a.txt"}],
            "stream": true
        }))
        .await;
    let events = read_stream(response).await;

    // Exactly one id, at one index, and the arguments concatenate to valid
    // JSON — which is what the client's accumulator produces.
    let mut ids = Vec::new();
    let mut arguments = String::new();
    for event in &events {
        if event.is_done() {
            continue;
        }
        let chunk = chunk(event);
        let Some(calls) = chunk["choices"][0]["delta"]["tool_calls"].as_array() else {
            continue;
        };
        for call in calls {
            assert_eq!(call["index"], 0);
            if let Some(id) = call["id"].as_str() {
                ids.push(id.to_owned());
            }
            if let Some(fragment) = call["function"]["arguments"].as_str() {
                arguments.push_str(fragment);
            }
        }
    }
    assert_eq!(ids, vec!["call_abc".to_owned()]);
    assert_eq!(
        serde_json::from_str::<Value>(&arguments).expect("valid JSON arguments")["path"],
        "a.txt"
    );
}

#[tokio::test]
async fn a_non_streamed_tool_call_assembles_into_one_call() {
    ensure_provider();
    // Both modes must produce the same call, or an agent behaves differently
    // depending on a transport detail.
    let harness = Harness::start(
        MockConfig {
            script: Script::ToolCall {
                id: "call_abc".into(),
                name: "read_file".into(),
                argument_fragments: vec!["{\"path\"".into(), ":\"a.txt\"}".into()],
            },
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;
    let body: Value = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "read a.txt"}]
        }))
        .await
        .json()
        .await
        .expect("json");

    let call = &body["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["id"], "call_abc");
    assert_eq!(call["type"], "function");
    assert_eq!(call["function"]["name"], "read_file");
    assert_eq!(
        serde_json::from_str::<Value>(call["function"]["arguments"].as_str().expect("arguments"))
            .expect("valid JSON")["path"],
        "a.txt"
    );
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
}

#[tokio::test]
async fn reasoning_is_streamed_separately_from_content() {
    ensure_provider();
    // Merging them would put the model's private reasoning into the visible
    // answer.
    let harness = Harness::start(
        MockConfig {
            script: Script::Reasoning {
                reasoning: vec!["thinking".into()],
                content: vec!["answer".into()],
            },
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;
    let events = read_stream(
        harness
            .post_chat(json!({
                "model": "mock-model@4k",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": true
            }))
            .await,
    )
    .await;

    let reasoning: Vec<String> = events
        .iter()
        .filter(|event| !event.is_done())
        .filter_map(|event| {
            chunk(event)["choices"][0]["delta"]["reasoning_content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(reasoning, vec!["thinking".to_owned()]);

    let content: Vec<String> = events
        .iter()
        .filter(|event| !event.is_done())
        .filter_map(|event| {
            chunk(event)["choices"][0]["delta"]["content"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(content, vec!["answer".to_owned()]);
}

#[tokio::test]
async fn malformed_json_is_refused_in_the_shape_a_client_can_read() {
    ensure_provider();
    let harness = Harness::default().await;
    let response = Harness::client()
        .post(format!("{}/v1/chat/completions", harness.base))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "invalid_json");
}

#[tokio::test]
async fn metadata_endpoints_stay_complete_when_auth_is_disabled() {
    ensure_provider();
    // Today's only mode. Nothing about a loopback gateway changes because the
    // redaction path exists.
    let harness = Harness::default().await;
    let props: Value = harness.get("/props").await.json().await.expect("json");
    assert_eq!(props["model_path"], "/mock/model.gguf");
    let health: Value = harness.get("/health").await.json().await.expect("json");
    assert_eq!(health["model"], "mock-model@4k");
    assert_eq!(health["engine"]["state"], "ready");
}

#[tokio::test]
async fn an_unauthenticated_caller_is_told_the_context_but_not_the_path() {
    ensure_provider();
    // The compromise that keeps both promises: a client resolving a model's
    // context probes `/props` and must still find `n_ctx`, while a stranger on
    // the network learns nothing about this filesystem.
    let harness = Harness::start(
        MockConfig::default(),
        GatewayConfig {
            auth: AuthPolicy::Required {
                key: "shared-secret".into(),
            },
            ..GatewayConfig::default()
        },
    )
    .await;

    let props: Value = harness.get("/props").await.json().await.expect("json");
    assert_eq!(props["default_generation_settings"]["n_ctx"], N_CTX);
    assert_eq!(props["total_slots"], 1);
    assert!(
        props.get("model_path").is_none(),
        "the model path leaked to an unauthenticated caller: {props}"
    );

    // A health check with no credentials still gets a usable answer.
    let health: Value = harness.get("/health").await.json().await.expect("json");
    assert_eq!(health["status"], "ok");
    assert!(health.get("model").is_none(), "{health}");
    assert!(health.get("engine").is_none(), "{health}");
}

#[tokio::test]
async fn the_engines_own_properties_reach_an_authorized_caller_and_no_one_else() {
    ensure_provider();
    // The engine answers with a per-slot context and its own model path. The
    // number is worth publishing to whoever is diagnosing a disagreement
    // between what was asked for and what is being served; the path is the one
    // thing redaction exists to remove, so the whole object travels only with
    // a key.
    let harness = Harness::start(
        MockConfig {
            engine_props: Some(serde_json::json!({
                "default_generation_settings": {"n_ctx": 1024},
                "total_slots": 4,
                "model_path": "/engine/side/model.gguf",
            })),
            ..MockConfig::default()
        },
        GatewayConfig {
            auth: AuthPolicy::Required {
                key: "shared-secret".into(),
            },
            ..GatewayConfig::default()
        },
    )
    .await;

    let anonymous: Value = harness.get("/props").await.json().await.expect("json");
    assert!(
        anonymous.get("hermes").is_none(),
        "the engine's own props reached an unauthenticated caller: {anonymous}"
    );

    let authorized: Value = Harness::client()
        .get(format!("{}/props", harness.base))
        .header("Authorization", "Bearer shared-secret")
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    // Ours stays ours: the advertised context is the gateway's, and the
    // engine's is beside it under our own key rather than in place of it.
    assert_eq!(authorized["default_generation_settings"]["n_ctx"], N_CTX);
    assert_eq!(authorized["total_slots"], 1);
    assert_eq!(
        authorized["hermes"]["engine"]["default_generation_settings"]["n_ctx"],
        1024
    );
    assert_eq!(authorized["hermes"]["engine"]["total_slots"], 4);
}

#[tokio::test]
async fn an_authenticated_caller_sees_everything() {
    ensure_provider();
    let harness = Harness::start(
        MockConfig::default(),
        GatewayConfig {
            auth: AuthPolicy::Required {
                key: "shared-secret".into(),
            },
            ..GatewayConfig::default()
        },
    )
    .await;

    let authorized = |path: &str| {
        Harness::client()
            .get(format!("{}{path}", harness.base))
            .header("Authorization", "Bearer shared-secret")
            .send()
    };

    let props: Value = authorized("/props")
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    assert_eq!(props["model_path"], "/mock/model.gguf");

    let health: Value = authorized("/health")
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    assert_eq!(health["model"], "mock-model@4k");
}

#[tokio::test]
async fn the_api_key_cannot_be_printed_by_accident() {
    ensure_provider();
    // `GatewayState` is the thing most likely to be written into a log line as
    // `?state`, and it prints its config, which holds the key.
    let harness = Harness::start(
        MockConfig::default(),
        GatewayConfig {
            auth: AuthPolicy::Required {
                key: "a-key-that-must-not-be-logged".into(),
            },
            ..GatewayConfig::default()
        },
    )
    .await;
    let rendered = format!("{:?}", harness.state);
    assert!(
        !rendered.contains("a-key-that-must-not-be-logged"),
        "the key reached a debug rendering: {rendered}"
    );
}

#[tokio::test]
async fn a_reasoning_control_reaches_the_backend() {
    ensure_provider();
    // Tolerating a request field and acting on it are different things. This
    // asserts the second: what the client sent arrived at the engine boundary.
    let harness = Harness::default().await;
    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "none",
            "chat_template_kwargs": {"enable_thinking": false}
        }))
        .await;
    assert_eq!(response.status(), 200);

    let seen = harness
        .backend
        .last_request()
        .await
        .expect("the backend was asked to generate");
    assert_eq!(
        seen.reasoning,
        lightweight_inference::generation::ReasoningControl::Disabled
    );
    assert_eq!(seen.template_options["enable_thinking"], false);
}

#[tokio::test]
async fn an_effort_level_is_passed_through_rather_than_interpreted() {
    ensure_provider();
    let harness = Harness::default().await;
    let _ = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high"
        }))
        .await;

    let seen = harness.backend.last_request().await.expect("a request");
    assert_eq!(
        seen.reasoning,
        lightweight_inference::generation::ReasoningControl::Effort("high".into())
    );
}

#[tokio::test]
async fn a_request_that_says_nothing_about_reasoning_leaves_it_to_the_model() {
    ensure_provider();
    // Defaulting either way would change what every model produces.
    let harness = Harness::default().await;
    let _ = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await;

    let seen = harness.backend.last_request().await.expect("a request");
    assert_eq!(
        seen.reasoning,
        lightweight_inference::generation::ReasoningControl::Default
    );
    assert!(seen.template_options.is_empty());
}

#[tokio::test]
async fn a_reasoning_only_completion_is_reported_honestly() {
    ensure_provider();
    // A reasoning model that spends its whole budget thinking produces no
    // content. We do not invent any: the stream carries the reasoning it
    // actually produced, terminates properly, and the client decides.
    let harness = Harness::start(
        MockConfig {
            script: Script::Reasoning {
                reasoning: vec!["thinking hard".into()],
                content: Vec::new(),
            },
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    let events = read_stream(
        harness
            .post_chat(json!({
                "model": "mock-model@4k",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": true,
                "stream_options": {"include_usage": true}
            }))
            .await,
    )
    .await;

    let reasoning: String = events
        .iter()
        .filter(|event| !event.is_done())
        .filter_map(|event| {
            chunk(event)["choices"][0]["delta"]["reasoning_content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(reasoning, "thinking hard");
    assert!(events.last().expect("frames").is_done());

    // And the non-streaming form says the same thing.
    let body: Value = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(body["choices"][0]["message"]["content"], "");
    assert_eq!(
        body["choices"][0]["message"]["reasoning_content"],
        "thinking hard"
    );
}

// ---------------------------------------------------------------------------
// M4: tools on the way *in*. The output side was already covered above; what
// was missing was the half that makes any of it happen — a gateway that
// accepts `tools` and does not forward them tells the model nothing, so the
// model never calls anything and the agent loop never starts.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn declared_tools_reach_the_backend() {
    ensure_provider();
    let harness = Harness::default().await;
    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "what is the weather?"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get the weather for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string"}},
                        "required": ["city"],
                    },
                },
            }],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
        }))
        .await;
    assert_eq!(response.status(), 200);

    let seen = harness
        .backend
        .last_request()
        .await
        .expect("the backend saw a request");
    assert_eq!(seen.tools.len(), 1, "the tool never reached the backend");
    assert_eq!(seen.tools[0].name, "get_weather");
    assert_eq!(
        seen.tools[0].description.as_deref(),
        Some("Get the weather for a city")
    );
    // Carried, not rewritten: these become the tokens the template renders.
    assert_eq!(seen.tools[0].parameters["required"][0], "city");
    assert_eq!(seen.tool_choice, ToolChoice::Auto);
    assert_eq!(seen.parallel_tool_calls, Some(false));
}

#[tokio::test]
async fn a_request_with_no_tools_declares_none() {
    // An empty declaration is not the same as no declaration: a template
    // renders the "you may call these" preamble either way, which costs prompt
    // tokens and changes what the model does.
    ensure_provider();
    let harness = Harness::default().await;
    harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .await;
    let seen = harness.backend.last_request().await.expect("a request");
    assert!(seen.tools.is_empty());
    assert_eq!(seen.tool_choice, ToolChoice::Unspecified);
}

#[tokio::test]
async fn an_unusable_tool_declaration_is_a_400_that_names_the_entry() {
    ensure_provider();
    let harness = Harness::default().await;
    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"type": "function", "function": {"name": "fine"}},
                {"type": "function", "function": {"description": "no name"}},
            ],
        }))
        .await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "invalid_tools");
    assert_eq!(body["error"]["param"], "tools");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("tools[1]")),
        "the refusal must name the entry: {body}"
    );
}

#[tokio::test]
async fn a_tool_choice_naming_an_undeclared_function_is_a_400() {
    // The rename trap: renaming a tool but not its `tool_choice` would
    // otherwise reach the model as a demand for a function it never received.
    ensure_provider();
    let harness = Harness::default().await;
    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "read_file"}}],
            "tool_choice": {"type": "function", "function": {"name": "read_fil"}},
        }))
        .await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "invalid_tool_choice");
    assert_eq!(body["error"]["param"], "tool_choice");
}

#[tokio::test]
async fn a_field_of_the_wrong_type_is_not_reported_as_invalid_json() {
    // The engine answers 500 to `"tools": "nope"`, which is a client mistake
    // reported as a server fault. It is a 400 here — and specifically not one
    // that sends the client hunting for a syntax error that is not there.
    ensure_provider();
    let harness = Harness::default().await;
    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": "nope",
        }))
        .await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "invalid_request_body");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        !message.contains("not valid JSON"),
        "the body was valid JSON; only a field was unreadable: {message}"
    );
}

// ---------------------------------------------------------------------------
// M4: /v1/completions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_text_completion_has_the_text_completion_shape() {
    ensure_provider();
    let harness = Harness::start(
        MockConfig {
            script: Script::Content(vec![" Paris".into(), ".".into()]),
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    let response = harness
        .post_completions(json!({
            "model": "mock-model@4k",
            "prompt": "The capital of France is",
            "max_tokens": 8,
        }))
        .await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");

    assert_eq!(body["object"], "text_completion");
    assert_eq!(body["model"], "mock-model@4k");
    assert_eq!(body["choices"][0]["text"], " Paris.");
    assert_eq!(body["choices"][0]["index"], 0);
    // Present and null rather than absent: clients index it.
    assert!(body["choices"][0]["logprobs"].is_null());
    assert!(
        body["choices"][0]
            .as_object()
            .is_some_and(|choice| choice.contains_key("logprobs")),
        "logprobs must be present: {body}"
    );
    // A completion is not a conversation: no message, no chat template.
    assert!(body["choices"][0].get("message").is_none());
    assert_eq!(body["usage"]["prompt_tokens"], 11);

    let seen = harness.backend.last_request().await.expect("a request");
    match &seen.prompt {
        Prompt::Text(text) => assert_eq!(text.reveal(), "The capital of France is"),
        Prompt::Chat(_) => panic!("a completion reached the backend as a conversation"),
    }
}

#[tokio::test]
async fn a_streamed_text_completion_ends_with_usage_then_done() {
    ensure_provider();
    let harness = Harness::start(
        MockConfig {
            script: Script::Content(vec![" Pa".into(), "ris".into()]),
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    let response = harness
        .post_completions(json!({
            "model": "mock-model@4k",
            "prompt": "The capital of France is",
            "stream": true,
            "stream_options": {"include_usage": true},
        }))
        .await;
    assert_eq!(response.status(), 200);
    let events = read_stream(response).await;

    assert!(
        events.last().is_some_and(SseEvent::is_done),
        "a stream must end with [DONE]"
    );
    let chunks: Vec<Value> = events
        .iter()
        .filter(|event| !event.is_done())
        .map(chunk)
        .collect();
    assert!(
        chunks.iter().all(|c| c["object"] == "text_completion"),
        "every chunk carries the endpoint's object: {chunks:?}"
    );

    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["text"].as_str())
        .collect();
    assert_eq!(text, " Paris");

    // The finish chunk names the choice it closes.
    // Content chunks carry finish_reason: null, so the closing one is the
    // chunk where it becomes a string.
    assert!(
        chunks
            .iter()
            .filter(|c| !c["choices"].as_array().is_some_and(Vec::is_empty))
            .all(|c| c["choices"][0]
                .as_object()
                .is_some_and(|choice| { choice.contains_key("finish_reason") })),
        "finish_reason must be present on every choice: {chunks:?}"
    );
    let finish = chunks
        .iter()
        .find(|c| c["choices"][0]["finish_reason"].is_string())
        .expect("a finish chunk");
    assert_eq!(finish["choices"][0]["finish_reason"], "stop");
    assert_eq!(finish["choices"][0]["index"], 0);

    // OpenAI's shape, not the engine's: usage rides a chunk with no choices.
    let usage = chunks.last().expect("a last chunk");
    assert_eq!(usage["choices"].as_array().map(Vec::len), Some(0));
    assert_eq!(usage["usage"]["prompt_tokens"], 11);
}

#[tokio::test]
async fn an_array_prompt_yields_one_choice_per_prompt() {
    // What the endpoint has always meant. Refusing it because this machine is
    // slow would bake this machine into the product.
    ensure_provider();
    let harness = Harness::start(
        MockConfig {
            script: Script::Content(vec!["out".into()]),
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    let response = harness
        .post_completions(json!({
            "model": "mock-model@4k",
            "prompt": ["alpha", "beta", "gamma"],
        }))
        .await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");

    let choices = body["choices"].as_array().expect("choices");
    assert_eq!(choices.len(), 3);
    for (index, choice) in choices.iter().enumerate() {
        assert_eq!(choice["index"], index);
        assert_eq!(choice["text"], "out");
    }
    // One request, one set of numbers, covering all three generations.
    assert_eq!(body["usage"]["prompt_tokens"], 33);
    assert_eq!(harness.backend.generation_count(), 3);
}

#[tokio::test]
async fn a_streamed_array_prompt_indexes_every_choice() {
    ensure_provider();
    let harness = Harness::start(
        MockConfig {
            script: Script::Content(vec!["x".into()]),
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    let response = harness
        .post_completions(json!({
            "model": "mock-model@4k",
            "prompt": ["one", "two"],
            "stream": true,
        }))
        .await;
    let events = read_stream(response).await;
    let chunks: Vec<Value> = events
        .iter()
        .filter(|event| !event.is_done())
        .map(chunk)
        .collect();

    let finished: Vec<u64> = chunks
        .iter()
        .filter(|c| c["choices"][0]["finish_reason"].is_string())
        .filter_map(|c| c["choices"][0]["index"].as_u64())
        .collect();
    assert_eq!(finished, vec![0, 1], "each choice closes exactly once");
}

#[tokio::test]
async fn echo_repeats_the_prompt_at_the_head_of_the_completion() {
    ensure_provider();
    let harness = Harness::start(
        MockConfig {
            script: Script::Content(vec![" Paris".into()]),
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    let response = harness
        .post_completions(json!({
            "model": "mock-model@4k",
            "prompt": "The capital of France is",
            "echo": true,
        }))
        .await;
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["choices"][0]["text"], "The capital of France is Paris");
}

#[tokio::test]
async fn a_token_prompt_is_refused_by_name() {
    // Refused, not mis-parsed: a client that sent token ids and was told "not
    // valid JSON" would have nothing to go on.
    ensure_provider();
    let harness = Harness::default().await;
    let response = harness
        .post_completions(json!({"model": "mock-model@4k", "prompt": [1, 2, 3]}))
        .await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "invalid_prompt");
    assert_eq!(body["error"]["param"], "prompt");
}

#[tokio::test]
async fn a_completion_parameter_we_cannot_honour_is_refused_by_name() {
    // Ignoring these would return a well-formed reply to a different request.
    ensure_provider();
    let harness = Harness::default().await;
    for (param, body) in [
        ("logprobs", json!({"prompt": "x", "logprobs": 5})),
        ("best_of", json!({"prompt": "x", "best_of": 4})),
        ("suffix", json!({"prompt": "x", "suffix": "</code>"})),
    ] {
        let response = harness.post_completions(body).await;
        assert_eq!(response.status(), 400, "{param} was not refused");
        let body: Value = response.json().await.expect("json");
        assert_eq!(body["error"]["code"], "unsupported_parameter");
        assert_eq!(body["error"]["param"], param);
    }
}

#[tokio::test]
async fn an_overlong_completion_prompt_is_a_400_before_anything_streams() {
    // The same guarantee the chat endpoint gives: a prompt that cannot fit is
    // a parsable status, not an empty stream the client retries verbatim.
    ensure_provider();
    let harness = Harness::start(
        MockConfig {
            prompt_tokens: N_CTX + 10,
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    let response = harness
        .post_completions(json!({
            "model": "mock-model@4k",
            "prompt": "far too long",
            "stream": true,
        }))
        .await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "context_length_exceeded");
    assert_eq!(body["error"]["param"], "prompt");
    assert_eq!(
        harness.backend.generation_count(),
        0,
        "nothing may be generated once the prompt is known not to fit"
    );
}

#[tokio::test]
async fn a_completion_for_a_model_we_do_not_have_is_a_404() {
    ensure_provider();
    let harness = Harness::default().await;
    let response = harness
        .post_completions(json!({"model": "something-else", "prompt": "x"}))
        .await;
    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "model_not_found");
}

#[tokio::test]
async fn a_completion_that_fails_midway_ends_the_stream_cleanly() {
    // Once headers are out an error cannot become a status, so it becomes a
    // terminal chunk and a proper [DONE] rather than a dropped connection.
    ensure_provider();
    let harness = Harness::start(
        MockConfig {
            script: Script::FailMidStream {
                content: vec!["par".into()],
                error: "the engine gave up".into(),
            },
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    let response = harness
        .post_completions(json!({
            "model": "mock-model@4k",
            "prompt": "x",
            "stream": true,
        }))
        .await;
    assert_eq!(response.status(), 200);
    let events = read_stream(response).await;
    assert!(events.last().is_some_and(SseEvent::is_done));

    let error_chunk = events
        .iter()
        .filter(|event| !event.is_done())
        .map(chunk)
        .find(|c| c.get("error").is_some())
        .expect("a terminal error chunk");
    assert!(
        error_chunk["error"]["code"].is_string(),
        "the error chunk must carry a code: {error_chunk}"
    );
}

// ---------------------------------------------------------------------------
// The scheduler, the queue and the metrics, as a client sees them.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_queued_streamed_request_is_told_where_it_stands() {
    ensure_provider();
    // The failure this replaces: a client whose request is behind a
    // multi-minute generation receives *nothing at all* — no headers, no
    // bytes — until the request ahead of it finishes, which every client's read
    // timeout eventually reads as a hung server.
    let harness = Harness::start(
        MockConfig::default(),
        GatewayConfig {
            queue_notice_interval: Duration::from_millis(50),
            ..GatewayConfig::default()
        },
    )
    .await;

    // Hold the only slot, exactly as a long generation would.
    let permit = harness.state.try_acquire_slot().expect("the free slot");
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        drop(permit);
    });

    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }))
        .await;
    // Answered immediately, while still queued: this is the whole point.
    assert_eq!(response.status(), 200);

    let body = response.text().await.expect("body");
    assert!(
        body.contains(": queued position=0"),
        "the client was never told it was queued: {body}"
    );
    assert!(
        body.contains("Hello"),
        "the request never ran after being queued: {body}"
    );
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
    release.await.expect("release");
}

#[tokio::test]
async fn a_queued_streamed_request_that_waits_too_long_is_told_so() {
    ensure_provider();
    // Once headers are out a refusal cannot be a status code, so it has to be
    // the terminal error chunk — carrying the same `server_busy` code the
    // non-streamed path returns with its 503, because which side of the headers
    // the client happened to be on is our detail, not theirs.
    let harness = Harness::start(
        MockConfig::default(),
        GatewayConfig {
            queue_timeout: Duration::from_millis(100),
            queue_notice_interval: Duration::from_millis(20),
            ..GatewayConfig::default()
        },
    )
    .await;
    let _permit = harness.state.try_acquire_slot().expect("the free slot");

    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }))
        .await;
    assert_eq!(response.status(), 200);
    let events = read_stream(response).await;
    let error = events
        .iter()
        .filter(|event| !event.is_done())
        .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok())
        .find(|chunk| chunk.get("error").is_some())
        .expect("an error chunk");
    assert_eq!(error["error"]["code"], "server_busy");
    assert_eq!(error["choices"][0]["finish_reason"], "error");
    assert!(events.last().expect("frames").is_done());
}

#[tokio::test]
async fn a_queued_request_that_cannot_stream_is_refused_with_a_status() {
    ensure_provider();
    // Unchanged from before the scheduler existed, and deliberately so: nothing
    // has been written to this client, so it can still be told in the one way
    // every client already handles.
    let harness = Harness::start(
        MockConfig::default(),
        GatewayConfig {
            queue_timeout: Duration::from_millis(50),
            ..GatewayConfig::default()
        },
    )
    .await;
    let _permit = harness.state.try_acquire_slot().expect("the free slot");

    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await;
    assert_eq!(response.status(), 503);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "server_busy");
}

#[tokio::test]
async fn a_short_request_is_served_before_a_long_one_that_was_already_waiting() {
    ensure_provider();
    // The acceptance run's failure, end to end: a small auxiliary request
    // arriving while a long turn is queued must not go to the back.
    let harness = Harness::default().await;
    let permit = harness.state.try_acquire_slot().expect("the free slot");

    // A long request queues first — a big stated output budget is what makes it
    // long, and the gateway knows that number before it admits anything.
    let long = harness.state.enqueue(
        Band::classify(11, Some(4000), harness.state.config.scheduler.interactive),
        PeerKey::default(),
    );
    let short = harness.state.enqueue(
        Band::classify(11, Some(16), harness.state.config.scheduler.interactive),
        PeerKey::default(),
    );
    assert_eq!(long.band(), Band::Bulk);
    assert_eq!(short.band(), Band::Interactive);
    assert_eq!(
        short.position(),
        Some(0),
        "the later, shorter request is next"
    );
    assert_eq!(long.position(), Some(1));

    drop(permit);
    drop(long);
    drop(short);
}

#[tokio::test]
async fn metrics_report_what_the_gateway_actually_did() {
    ensure_provider();
    let harness = Harness::default().await;
    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await;
    assert_eq!(response.status(), 200);

    let scrape = harness.get("/metrics").await;
    assert_eq!(scrape.status(), 200);
    assert!(
        scrape
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/plain")),
        "a Prometheus scrape must be text"
    );
    let text = scrape.text().await.expect("body");
    assert!(
        text.contains("hermes_requests_total{endpoint=\"chat_completions\",outcome=\"ok\"} 1"),
        "{text}"
    );
    assert!(text.contains("hermes_generations_total 1"), "{text}");
    assert!(
        text.contains("hermes_finish_reason_total{reason=\"stop\"} 1"),
        "{text}"
    );
    assert!(
        text.contains("hermes_queue_slots{state=\"capacity\"} 1"),
        "{text}"
    );
    // Every metric a scraper reads must have been declared first.
    //
    // A histogram declares its `# TYPE` on the family name, and `_bucket`,
    // `_sum` and `_count` are series *within* that family rather than metrics
    // of their own - so the suffix is stripped before looking for the
    // declaration. This is the same invariant as before ("nothing is emitted
    // that a scraper was not told about"), expressed in the exposition
    // format's own terms now that a family in it has more than one series.
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        let name = line.split(['{', ' ']).next().unwrap_or_default();
        let family = ["_bucket", "_sum", "_count"]
            .iter()
            .find_map(|suffix| name.strip_suffix(*suffix))
            .unwrap_or(name);
        assert!(
            text.contains(&format!("# TYPE {name} "))
                || text.contains(&format!("# TYPE {family} histogram")),
            "{name} was emitted without a TYPE line"
        );
    }
    // The distribution, not just the mean: a p95 is unanswerable from a sum
    // and a count, and the tail is the whole complaint on a slow machine.
    assert!(
        text.contains("# TYPE hermes_time_to_first_token_seconds histogram"),
        "{text}"
    );
    assert!(
        text.contains("hermes_time_to_first_token_seconds_bucket{le=\"+Inf\"} 1"),
        "every observation must reach the overflow bucket: {text}"
    );
    assert!(
        text.contains("hermes_queue_wait_seconds_bucket{le=\"0.050\"} 1"),
        "an unqueued request waited under 50ms and must be counted there: {text}"
    );
    // The two series that existed before keep their names and their meaning.
    assert!(text.contains("hermes_queue_wait_seconds_count 1"), "{text}");
    assert!(
        !text.contains("/mock/model.gguf"),
        "a scrape must not carry the model's path: {text}"
    );
}

#[tokio::test]
async fn the_json_metrics_carry_the_same_numbers_for_our_own_ui() {
    ensure_provider();
    let harness = Harness::default().await;
    harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "stream_options": {"include_usage": true}
        }))
        .await
        .text()
        .await
        .expect("body");

    let body: Value = harness
        .get("/api/v1/metrics")
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(body["generations"], 1);
    assert_eq!(body["finish_reasons"]["stop"], 1);
    assert_eq!(body["queue"]["capacity"], 1);
    assert_eq!(body["queue"]["running"], 0, "the slot came back");
    assert_eq!(body["tokens"]["prompt"], 11);
    assert!(
        body["model"]["id"]
            .as_str()
            .is_some_and(|id| id.contains("mock-model")),
        "{body}"
    );
    assert!(
        body.to_string().find("/mock/model.gguf").is_none(),
        "the model's path must not appear in metrics"
    );
}

#[tokio::test]
async fn a_generation_the_client_abandons_is_counted_as_cancelled_not_as_an_error() {
    ensure_provider();
    // Closing a laptop lid is a normal act. Counting it in the same column as a
    // crashed engine is how an operator ends up chasing a failure that never
    // happened.
    let harness = Harness::start(
        MockConfig {
            script: Script::Endless {
                fragment: "tick".into(),
                interval: Duration::from_millis(10),
            },
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    let response = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }))
        .await;
    assert_eq!(response.status(), 200);
    drop(response);

    // The counter is written from the guard's `Drop`, which happens when the
    // body is dropped; poll briefly rather than sleeping a fixed time.
    let counted = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = harness.state.metrics_snapshot().await;
            if snapshot.finish_reasons.cancelled == 1 {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let snapshot = counted.expect("the abandoned generation was never counted");
    assert_eq!(snapshot.finish_reasons.error, 0, "and not as an error");
    assert_eq!(snapshot.queue.running, 0, "the slot came back");
}

#[tokio::test]
async fn metrics_are_behind_the_key_when_one_is_configured() {
    ensure_provider();
    // Request rates, token counts and queue depth describe what this machine is
    // doing. On a bind that is reachable from elsewhere, that is not public.
    let harness = Harness::start(
        MockConfig::default(),
        GatewayConfig {
            auth: AuthPolicy::Required {
                key: "secret-key".into(),
            },
            ..GatewayConfig::default()
        },
    )
    .await;

    assert_eq!(harness.get("/metrics").await.status(), 401);
    assert_eq!(harness.get("/api/v1/metrics").await.status(), 401);

    let authorized = Harness::client()
        .get(format!("{}/metrics", harness.base))
        .header("Authorization", "Bearer secret-key")
        .send()
        .await
        .expect("request");
    assert_eq!(authorized.status(), 200);
}

#[tokio::test]
async fn a_request_counts_as_in_flight_until_its_body_is_delivered() {
    ensure_provider();
    // The gauge exists to say what the gateway is doing right now, and what it
    // is doing for almost all of a generation is *streaming a body*. A counter
    // that stopped at the response head would read zero for the whole two
    // minutes a real completion takes on this hardware.
    // A token interval, so the stream is still open when it is asked about.
    // With an instant mock the whole body arrives before the next request can
    // be made, and the test would pass or fail on scheduling luck.
    let harness = Harness::start(
        MockConfig {
            token_interval: std::time::Duration::from_millis(150),
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    // Nothing in flight before, so anything seen during the stream is this
    // request and not a leftover.
    let idle: Value = harness.get_json("/api/v1/metrics").await;
    assert_eq!(idle["in_flight"], 0);

    let mut stream = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true,
        }))
        .await;
    assert_eq!(stream.status(), 200);

    // The head has arrived and the body has not been read to the end. The
    // handler has therefore returned, and only the guard riding in the body
    // keeps this counted.
    let during: Value = harness.get_json("/api/v1/metrics").await;
    assert!(
        during["in_flight"].as_u64().unwrap_or_default() >= 1,
        "a streamed response must still count as in flight: {during}"
    );

    // Drain it, and the count comes back down.
    while stream.chunk().await.expect("read the stream").is_some() {}
    drop(stream);

    let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let body: Value = harness.get_json("/api/v1/metrics").await;
            if body["in_flight"] == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "the gauge must come back down, not drift up"
    );
}

#[tokio::test]
async fn the_event_stream_reports_a_generation_the_client_abandoned() {
    ensure_provider();
    // The live feed's whole value is showing what just happened, and the
    // hardest case is the one a publisher on the happy path would miss: a
    // client that walks away mid-stream. It is published from the same `Drop`
    // the counters are, so it cannot be missed here either.
    let harness = Harness::start(
        MockConfig {
            token_interval: std::time::Duration::from_millis(100),
            ..MockConfig::default()
        },
        GatewayConfig::default(),
    )
    .await;

    let mut events = harness.get("/api/v1/events").await;
    assert_eq!(events.status(), 200);

    let abandoned = harness
        .post_chat(json!({
            "model": "mock-model@4k",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        }))
        .await;
    assert_eq!(abandoned.status(), 200);
    // Walk away without reading it.
    drop(abandoned);

    let frame = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(chunk) = events.chunk().await.expect("read the event stream") {
            let text = String::from_utf8_lossy(&chunk).to_string();
            if text.contains("\"model\"") {
                return text;
            }
        }
        String::new()
    })
    .await
    .expect("an event within ten seconds");

    let payload = frame
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .expect("an SSE data frame");
    let event: Value = serde_json::from_str(payload).expect("the event is json");

    assert_eq!(event["model"], "mock-model@4k");
    assert!(event["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(event["at_unix_ms"].as_u64().unwrap_or_default() > 0);
    assert!(event["total_ms"].as_u64().is_some());
    // No finish reason: this generation ended because the client left, which is
    // a deliberate act and not an error.
    assert!(event["finish_reason"].is_null(), "{event}");
    // Which queue served it, in the same spelling the metrics label uses. It
    // has been measured since M8 and was never published, so a reader could
    // see that a request waited without seeing what it waited as.
    assert_eq!(event["band"], "interactive", "{event}");

    // The feed carries what the log carries, and no more.
    assert!(
        !payload.contains("hi"),
        "the prompt must not be here: {payload}"
    );
}

#[tokio::test]
async fn describing_the_machine_and_the_service_needs_the_key() {
    ensure_provider();
    // `/api/v1/system` reports this machine's processor, its memory pressure
    // and where its disks are; `/api/v1/gateway` reports where it is serving
    // and whether a key is required. Both are inventory of the host, and on a
    // bind reachable from elsewhere neither is public.
    let harness = Harness::start(
        MockConfig::default(),
        GatewayConfig {
            auth: AuthPolicy::Required {
                key: "secret-key".into(),
            },
            ..GatewayConfig::default()
        },
    )
    .await;

    assert_eq!(harness.get("/api/v1/system").await.status(), 401);
    assert_eq!(harness.get("/api/v1/gateway").await.status(), 401);

    for path in ["/api/v1/system", "/api/v1/gateway"] {
        let authorized = Harness::client()
            .get(format!("{}{path}", harness.base))
            .header("Authorization", "Bearer secret-key")
            .send()
            .await
            .expect("request");
        assert_eq!(authorized.status(), 200, "{path} with the key");
    }
}

#[tokio::test]
async fn the_gateway_description_never_carries_the_key() {
    ensure_provider();
    // The key is kept out of the log and out of the engine's argv. An endpoint
    // that returned it would undo both, so this asserts the absence directly
    // rather than trusting that nobody adds the field later.
    let harness = Harness::start(
        MockConfig::default(),
        GatewayConfig {
            auth: AuthPolicy::Required {
                key: "secret-key".into(),
            },
            ..GatewayConfig::default()
        },
    )
    .await;

    let body = Harness::client()
        .get(format!("{}/api/v1/gateway", harness.base))
        .header("Authorization", "Bearer secret-key")
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body");

    assert!(
        !body.contains("secret-key"),
        "the gateway description leaked the API key: {body}"
    );
    // The fact that a key is required is reported; the key is not.
    assert!(body.contains("\"required\":true"), "{body}");
}
