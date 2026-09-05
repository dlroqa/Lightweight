//! A live test of `EmbeddingClient` against a raw loopback HTTP server that
//! answers the OpenAI `/v1/embeddings` shape.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::time::Duration;

use lightagent_provider_lightweight::EmbeddingClient;

fn spawn_embeddings_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            // Two inputs in the request → two 3-dim embeddings back, in order.
            let body = r#"{"data":[{"embedding":[0.1,0.2,0.3]},{"embedding":[0.4,0.5,0.6]}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

#[tokio::test]
async fn embeds_over_http() {
    let port = spawn_embeddings_server();
    let client = EmbeddingClient::new(format!("http://127.0.0.1:{port}"), None).unwrap();
    let vectors = tokio::time::timeout(
        Duration::from_secs(5),
        client.embed("test-model", &["hello".to_owned(), "world".to_owned()]),
    )
    .await
    .expect("no timeout")
    .expect("embeddings");
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0], vec![0.1, 0.2, 0.3]);
    assert_eq!(vectors[1], vec![0.4, 0.5, 0.6]);
}
