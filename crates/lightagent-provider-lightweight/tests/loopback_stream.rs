//! End-to-end offline test of the adapter against a hand-written SSE server.
//!
//! A `tokio::net::TcpListener` on `127.0.0.1:0` accepts one connection, reads
//! the request, and writes a `text/event-stream` response reproducing the whole
//! contract: a role chunk with empty content, a reasoning delta, two-index
//! tool-call deltas with split arguments and id-once, a mid-stream keep-alive
//! comment, a finish chunk with `tool_calls`, an empty-`choices` usage chunk,
//! and `[DONE]`. The test asserts the ordered `ProviderEvent`s and that the
//! comment frame produced none. No network beyond loopback; runs in the default
//! `cargo test` tier.

use futures_util::StreamExt;
use lightagent_core::provider::{
    AgentProvider, FinishReason, ProviderEvent, ProviderMessage, ProviderRequest,
};
use lightagent_provider_lightweight::{LightweightProvider, ProviderConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// The canned stream body: the full contract, frame by frame.
const STREAM_BODY: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n\n",
    ": ping\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.txt\\\"}\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":1,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"datetime.now\",\"arguments\":\"{}\"}}]}}]}\n\n",
    ": queued position=0 waited=0s\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":36,\"completion_tokens\":8,\"total_tokens\":44}}\n\n",
    "data: [DONE]\n\n",
);

async fn serve_once(listener: TcpListener) {
    if let Ok((mut socket, _)) = listener.accept().await {
        // Read the request until the header terminator; ignore the body.
        let mut buf = [0u8; 4096];
        // A single read is enough to get past the headers on loopback.
        let _ = socket.read(&mut buf).await;

        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/event-stream\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\
             \r\n\
             {STREAM_BODY}"
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.flush().await;
        let _ = socket.shutdown().await;
    }
}

#[tokio::test]
async fn a_full_stream_decodes_to_ordered_provider_events() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(serve_once(listener));

    let provider = LightweightProvider::new(ProviderConfig::new(format!("http://{addr}"), "m@8k"))
        .expect("provider");

    let request = ProviderRequest::new("m@8k", vec![ProviderMessage::user("hi")]);
    let stream = provider
        .stream(request, CancellationToken::new())
        .await
        .expect("stream");
    let events: Vec<ProviderEvent> = stream
        .map(|item| item.expect("no stream error"))
        .collect()
        .await;

    server.await.expect("server task");

    // The exact ordered contract. The two comment frames contribute nothing.
    assert_eq!(events[0], ProviderEvent::RoleStarted);
    assert_eq!(events[1], ProviderEvent::Reasoning("thinking".into()));
    assert_eq!(
        events[2],
        ProviderEvent::ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("read_file".into()),
            arguments: Some(String::new()),
        }
    );
    assert_eq!(
        events[3],
        ProviderEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments: Some("{\"path\":".into()),
        }
    );
    assert_eq!(
        events[4],
        ProviderEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments: Some("\"a.txt\"}".into()),
        }
    );
    assert_eq!(
        events[5],
        ProviderEvent::ToolCallDelta {
            index: 1,
            id: Some("call_2".into()),
            name: Some("datetime.now".into()),
            arguments: Some("{}".into()),
        }
    );
    match &events[6] {
        ProviderEvent::Finished { reason, usage } => {
            assert_eq!(*reason, FinishReason::ToolCalls);
            let usage = usage.expect("usage from the empty-choices chunk");
            assert_eq!(usage.total_tokens, 44);
            assert_eq!(usage.completion_tokens, 8);
        }
        other => panic!("expected Finished, got {other:?}"),
    }
    assert_eq!(events.len(), 7, "no spurious events from comment frames");

    // Reconstruct the split arguments, proving id-once accumulation works
    // end to end.
    let mut acc = lightagent_core::ToolCallAccumulator::new();
    for event in &events {
        if let ProviderEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments,
        } = event
        {
            acc.push(*index, id.clone(), name.clone(), arguments.clone());
        }
    }
    let calls = acc.into_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].arguments, "{\"path\":\"a.txt\"}");
    assert_eq!(calls[1].id, "call_2");
}
