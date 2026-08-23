//! The gateway as a client sees it: over a real socket, with real HTTP.
//!
//! These tests deliberately do not call the handlers directly. Half of what
//! spec section 12 promises is framing — headers, chunk boundaries, the
//! terminal `[DONE]`, a body that stops when the client goes away — and none of
//! that exists until the response has been through hyper.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hermes_backend_mock::{MockBackend, MockConfig, Script};
use hermes_core::{ModelId, SseDecoder, SseEvent};
use hermes_gateway::catalog::{Catalog, ResidentModel};
use hermes_gateway::{AuthPolicy, GatewayConfig, GatewayState};
use hermes_inference::InferenceBackend;
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
        }));

        let state = Arc::new(GatewayState::new(
            Arc::clone(&backend) as Arc<dyn InferenceBackend>,
            catalog,
            gateway,
        ));
        let app = hermes_gateway::app(Arc::clone(&state));
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
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

    async fn get(&self, path: &str) -> reqwest::Response {
        Self::client()
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .expect("request")
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
            if let Some(permit) = harness.state.acquire_slot().await {
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
        hermes_inference::generation::ReasoningControl::Disabled
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
        hermes_inference::generation::ReasoningControl::Effort("high".into())
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
        hermes_inference::generation::ReasoningControl::Default
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
