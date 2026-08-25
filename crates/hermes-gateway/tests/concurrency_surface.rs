//! More than one client at a time, over real sockets.
//!
//! Every contention test this project had before M9 manufactured its
//! contention in-process, by taking the scheduler's only permit from the test
//! body. That proves the queue's arithmetic and says nothing about whether two
//! actual clients can be told apart, served at once, or told where they stand —
//! which is the whole of what "multiple clients" means.
//!
//! The peer address is what the scheduler keys fairness on, and it is observed
//! from the connection rather than claimed by the client. Making two clients
//! that are genuinely different therefore means two source addresses: these
//! tests bind one client to `127.0.0.2`, which Linux routes on loopback without
//! configuration. Where that is unavailable the tests say so and skip rather
//! than asserting something weaker under the same name.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hermes_backend_mock::{MockBackend, MockConfig, Script};
use hermes_core::ModelId;
use hermes_gateway::catalog::{Catalog, ResidentModel};
use hermes_gateway::{GatewayConfig, GatewayState};
use hermes_inference::InferenceBackend;
use serde_json::{Value, json};

const N_CTX: u32 = 4096;

/// A gateway bound to an ephemeral loopback port.
struct Harness {
    base: String,
    state: Arc<GatewayState>,
    _server: tokio::task::JoinHandle<()>,
}

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
            param_count: None,
            quantization: None,
            model_max_context_length: None,
            ram_verdict: None,
            backend: Some("mock".into()),
            model_path: "/mock/model.gguf".into(),
            effective: hermes_core::RuntimeParams::default(),
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
            // Through `service`, exactly as `hermes serve` does: without it
            // every request would arrive with the same scheduling key and
            // these tests would pass while proving nothing.
            let _ = axum::serve(listener, hermes_gateway::service(app)).await;
        });

        Self {
            base: format!("http://127.0.0.1:{port}"),
            state,
            _server: server,
        }
    }
}

/// Install the pinned rustls provider.
///
/// `reqwest` panics rather than erroring when none is installed, even for a
/// plain `http://` request, and this workspace pins `ring` because the default
/// provider needs cmake.
fn ensure_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A client that connects from a chosen source address.
///
/// `None` is the ordinary client, which the operating system gives the default
/// loopback source. `Some` is how a second, genuinely distinct peer is made.
fn client_from(source: Option<Ipv4Addr>) -> reqwest::Client {
    let builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
    match source {
        Some(address) => builder.local_address(IpAddr::V4(address)),
        None => builder,
    }
    .build()
    .expect("client")
}

/// Whether this machine routes a second loopback address.
fn second_loopback() -> Option<Ipv4Addr> {
    let address = Ipv4Addr::new(127, 0, 0, 2);
    std::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(address), 0))
        .ok()
        .map(|_| address)
}

fn body(max_tokens: u32) -> Value {
    json!({
        "model": "mock-model@4k",
        "messages": [{"role": "user", "content": "hello"}],
        "max_tokens": max_tokens,
        "stream": true,
    })
}

/// The first queue notice a streamed response carries.
///
/// A queued request is answered immediately with headers and then says where it
/// stands in an SSE comment, so this reads the body incrementally and stops at
/// the first one. Reading the body to completion instead would wait for a
/// generation that has not started yet.
///
/// **The response is borrowed, never consumed.** Dropping it disconnects that
/// client, which withdraws it from the queue and moves everybody behind it — so
/// a reader that took ownership would change the queue it was measuring the
/// moment it returned, and the position another client read a moment later
/// would depend on which read happened to finish first.
async fn first_queue_notice(response: &mut reqwest::Response) -> Option<String> {
    let mut body = String::new();
    let read = async {
        while let Ok(Some(chunk)) = response.chunk().await {
            body.push_str(&String::from_utf8_lossy(&chunk));
            if let Some(line) = body
                .lines()
                .find(|line| line.starts_with(": queued position="))
            {
                return Some(line.to_owned());
            }
        }
        None
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .ok()
        .flatten()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_are_told_apart_by_the_connection_they_arrived_on() {
    ensure_provider();
    let Some(second) = second_loopback() else {
        eprintln!("skipping: this machine does not route 127.0.0.2");
        return;
    };

    // One slot, and notices often enough that a test need not wait a quarter
    // of a minute to read one.
    let harness = Harness::start(
        MockConfig::default(),
        GatewayConfig {
            max_concurrent_requests: 1,
            queue_notice_interval: Duration::from_millis(50),
            ..GatewayConfig::default()
        },
    )
    .await;

    // Held from the test body: what is being measured here is the order two
    // real clients are placed in, not who holds the slot.
    let busy = harness
        .state
        .try_acquire_slot()
        .expect("the gateway starts idle");

    // The busy client queues two requests, then the quiet one queues its
    // first. Under plain arrival order the quiet client would be third.
    let first = client_from(None);
    let queued_a = first
        .post(format!("{}/v1/chat/completions", harness.base))
        .json(&body(4000))
        .send()
        .await
        .expect("request");
    let mut queued_b = first
        .post(format!("{}/v1/chat/completions", harness.base))
        .json(&body(4000))
        .send()
        .await
        .expect("request");

    let quiet = client_from(Some(second));
    let mut queued_c = quiet
        .post(format!("{}/v1/chat/completions", harness.base))
        .json(&body(4000))
        .send()
        .await
        .expect("request");

    // Read together, not one after the other. A notice is read by consuming
    // the response, and consuming it disconnects that client - which withdraws
    // it from the queue and moves everybody behind it. Sequential reads would
    // measure the queue as it was left by the previous read.
    let (notice_b, notice_c) = tokio::join!(
        first_queue_notice(&mut queued_b),
        first_queue_notice(&mut queued_c)
    );
    let notice_b = notice_b.expect("a queue notice");
    let notice_c = notice_c.expect("a queue notice");

    assert!(
        notice_c.contains("position=1"),
        "the second client should sit behind one request, not two: {notice_c}"
    );
    assert!(
        notice_b.contains("position=2"),
        "the busy client's second request should give way to the newcomer: {notice_b}"
    );

    drop(queued_a);
    drop(busy);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_client_sending_twice_keeps_the_order_it_sent_in() {
    ensure_provider();
    // The other half of the claim above: fairness between clients must not
    // reorder one client's own requests. Same shape, one peer.
    let harness = Harness::start(
        MockConfig::default(),
        GatewayConfig {
            max_concurrent_requests: 1,
            queue_notice_interval: Duration::from_millis(50),
            ..GatewayConfig::default()
        },
    )
    .await;
    let busy = harness
        .state
        .try_acquire_slot()
        .expect("the gateway starts idle");

    let client = client_from(None);
    let mut queued_a = client
        .post(format!("{}/v1/chat/completions", harness.base))
        .json(&body(4000))
        .send()
        .await
        .expect("request");
    let mut queued_b = client
        .post(format!("{}/v1/chat/completions", harness.base))
        .json(&body(4000))
        .send()
        .await
        .expect("request");

    // Together, for the reason the two-client test gives: reading one notice
    // disconnects that client and reorders the queue behind it.
    let (notice_a, notice_b) = tokio::join!(
        first_queue_notice(&mut queued_a),
        first_queue_notice(&mut queued_b)
    );
    let notice_a = notice_a.expect("a queue notice");
    let notice_b = notice_b.expect("a queue notice");
    assert!(notice_a.contains("position=0"), "{notice_a}");
    assert!(notice_b.contains("position=1"), "{notice_b}");

    drop(busy);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_are_served_at_once_when_the_gateway_has_two_slots() {
    ensure_provider();
    // The point of raising the slot count: both clients generate, neither
    // waits. Asserted from the engine's side - two generations in flight at
    // the same moment - rather than from wall-clock timing.
    let harness = Harness::start(
        MockConfig {
            // Long enough that the first request is unmistakably still running
            // when the second is admitted, and ended by cancellation rather
            // than by a token budget.
            script: Script::Endless {
                fragment: "tick ".into(),
                interval: Duration::from_millis(20),
            },
            ..MockConfig::default()
        },
        GatewayConfig {
            max_concurrent_requests: 2,
            ..GatewayConfig::default()
        },
    )
    .await;

    let client = client_from(None);
    let one = client
        .post(format!("{}/v1/chat/completions", harness.base))
        .json(&body(4000))
        .send()
        .await
        .expect("request");
    let two = client
        .post(format!("{}/v1/chat/completions", harness.base))
        .json(&body(4000))
        .send()
        .await
        .expect("request");

    assert_eq!(one.status(), 200);
    assert_eq!(two.status(), 200);

    // Both are generating, and neither is queued: the scheduler says so.
    let queue = harness.state.scheduler().snapshot();
    assert_eq!(queue.capacity, 2);
    assert_eq!(queue.running, 2, "both clients should hold a slot");
    assert_eq!(queue.waiting, 0, "neither client should be waiting");
    assert_eq!(queue.queued, 0, "and neither should have had to queue");

    // Going away returns both slots, through the same `Drop` path a single
    // client has always used.
    drop(one);
    drop(two);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(harness.state.scheduler().snapshot().running, 0);
}
