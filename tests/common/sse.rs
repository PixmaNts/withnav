//! Scripted chat-completion streams: SSE bodies and wiremock responses.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::ResponseTemplate;

/// A chat-completion SSE body: one `data:` frame per event, then `[DONE]`.
pub fn sse_body(events: &[&str]) -> String {
    let mut b = String::new();
    for e in events {
        b.push_str("data: ");
        b.push_str(e);
        b.push_str("\n\n");
    }
    b.push_str("data: [DONE]\n\n");
    b
}

/// The raw HTTP/1.1 response a scripted TCP provider writes for `body`.
pub fn http_sse(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

/// The wiremock answer carrying [`sse_body`] for `events`.
pub fn sse_response(events: &[&str]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(sse_body(events), "text/event-stream")
}

/// A scripted TCP provider; `url` is the `base_url` to put in `models.json`.
pub struct MockProvider {
    pub url: String,
}

/// Serves `responses` one per accepted connection, in order, then stops
/// accepting. Each response is written after reading one request chunk.
pub async fn spawn_mock_provider(responses: Vec<Vec<u8>>) -> MockProvider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        for resp in responses {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = vec![0u8; 16384];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(&resp).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });
    MockProvider {
        url: format!("http://{addr}/v1"),
    }
}
