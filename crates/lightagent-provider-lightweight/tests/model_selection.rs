//! Model routing over real loopback HTTP, including the placeholder written by init.
use futures_util::StreamExt;
use lightagent_core::provider::{AgentProvider, ProviderMessage, ProviderRequest};
use lightagent_provider_lightweight::{LightweightProvider, ProviderConfig};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const ANSWER: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

async fn server(
    replies: Vec<(&'static str, String)>,
) -> (String, tokio::task::JoinHandle<Vec<(String, Value)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (content_type, body) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let (header_end, content_length) = loop {
                let mut buffer = [0; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, length)| length.trim().parse::<usize>().unwrap())
                        .unwrap_or(0);
                    break (end + 4, length);
                }
            };
            while bytes.len() < header_end + content_length {
                let mut buffer = [0; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            let headers = String::from_utf8_lossy(&bytes[..header_end]).to_string();
            let received = serde_json::from_slice(&bytes[header_end..]).unwrap_or(Value::Null);
            requests.push((headers, received));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
        requests
    });
    (format!("http://{address}"), task)
}

fn models(ids: &[&str]) -> (&'static str, String) {
    (
        "application/json",
        json!({"data": ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>()} ).to_string(),
    )
}

async fn generate(provider: &LightweightProvider) -> Result<(), String> {
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.stream(
            ProviderRequest::new("default", vec![ProviderMessage::user("hello")]),
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .map_err(|error| error.to_string())?;
    let events = stream.collect::<Vec<_>>().await;
    assert!(events.iter().all(Result::is_ok));
    Ok(())
}

#[tokio::test]
async fn default_uses_the_loaded_model_and_follows_model_changes() {
    let (base, task) = server(vec![
        models(&["minicpm5-1b-q4_k_m@16k"]),
        ("text/event-stream", ANSWER.into()),
        models(&["another-model@8k"]),
        ("text/event-stream", ANSWER.into()),
    ])
    .await;
    let provider =
        LightweightProvider::new(ProviderConfig::new(base, "default").with_api_key("test-key"))
            .unwrap();
    generate(&provider).await.unwrap();
    generate(&provider).await.unwrap();
    let requests = task.await.unwrap();
    assert!(requests[0].0.starts_with("GET /v1/models "));
    assert!(requests[1].0.starts_with("POST /v1/chat/completions "));
    assert_eq!(requests[1].1["model"], "minicpm5-1b-q4_k_m@16k");
    assert_eq!(requests[3].1["model"], "another-model@8k");
    assert!(requests.iter().all(|(headers, _)| {
        headers
            .to_lowercase()
            .contains("authorization: bearer test-key")
    }));
}

#[tokio::test]
async fn an_available_explicit_model_is_preserved() {
    let (base, task) = server(vec![
        models(&["other-model", "pinned-model"]),
        ("text/event-stream", ANSWER.into()),
    ])
    .await;
    let provider = LightweightProvider::new(ProviderConfig::new(base, "pinned-model")).unwrap();
    generate(&provider).await.unwrap();
    let requests = task.await.unwrap();
    assert!(requests[0].0.starts_with("GET /v1/models "));
    assert_eq!(requests[1].1["model"], "pinned-model");
}

#[tokio::test]
async fn a_stale_explicit_model_follows_the_gateway_only_loaded_model() {
    let (base, task) = server(vec![
        models(&["newly-selected-model@16k"]),
        ("text/event-stream", ANSWER.into()),
    ])
    .await;
    let provider = LightweightProvider::new(ProviderConfig::new(base, "old-model@8k")).unwrap();
    generate(&provider).await.unwrap();
    let requests = task.await.unwrap();
    assert_eq!(requests[1].1["model"], "newly-selected-model@16k");
}

#[tokio::test]
async fn missing_and_ambiguous_models_fail_before_generation() {
    for (ids, expected) in [
        (vec![], "No model is loaded"),
        (vec!["one", "two"], "multiple models"),
    ] {
        let (base, task) = server(vec![models(&ids)]).await;
        let provider = LightweightProvider::new(ProviderConfig::new(base, "default")).unwrap();
        assert!(generate(&provider).await.unwrap_err().contains(expected));
        assert_eq!(task.await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn a_real_default_model_is_used_when_advertised() {
    let (base, task) = server(vec![
        models(&["one", "default"]),
        ("text/event-stream", ANSWER.into()),
    ])
    .await;
    let provider = LightweightProvider::new(ProviderConfig::new(base, "default")).unwrap();
    generate(&provider).await.unwrap();
    assert_eq!(task.await.unwrap()[1].1["model"], "default");
}
