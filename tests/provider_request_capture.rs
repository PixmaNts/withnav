use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod common;
use common::deadline::Deadline;
use common::sse::{
    http_sse, spawn_capturing_provider, spawn_gated_provider, spawn_holding_provider,
    spawn_streaming_provider, sse_body, text_chunk,
};

async fn connect(url: &str) -> tokio::net::TcpStream {
    let address = url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    tokio::net::TcpStream::connect(address)
        .bounded(file!(), line!())
        .await
        .unwrap()
}

async fn cancelled_connections(url: &str) {
    for partial in [
        "",
        "POST /v1/chat/completions HTTP/1.1\r\nContent-Len",
        "POST /v1/chat/completions HTTP/1.1\r\nContent-Length: 10\r\n\r\n{}",
    ] {
        let mut stream = connect(url).await;
        stream
            .write_all(partial.as_bytes())
            .bounded(file!(), line!())
            .await
            .unwrap();
        stream.shutdown().bounded(file!(), line!()).await.unwrap();
    }
}

async fn complete_connection(url: &str) -> tokio::net::TcpStream {
    let mut stream = connect(url).await;
    stream
        .write_all(b"POST /v1/chat/completions HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}")
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream
}

#[tokio::test]
async fn partial_connections_do_not_consume_captured_response_or_hold_indices() {
    for kind in 0..3 {
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let reached = std::sync::Arc::new(tokio::sync::Notify::new());
        let response = http_sse(&sse_body(&[&text_chunk("first complete request")]));
        let provider = match kind {
            0 => spawn_capturing_provider(vec![response]).await,
            1 => spawn_gated_provider(vec![response], release.clone()).await,
            _ => {
                spawn_holding_provider(1, 0, reached.clone(), release.clone(), move |index, _| {
                    assert_eq!(index, 0);
                    response.clone()
                })
                .await
            }
        };
        cancelled_connections(&provider.url).await;
        let mut stream = complete_connection(&provider.url).await;
        if kind == 2 {
            reached.notified().bounded(file!(), line!()).await;
        }
        release.notify_one();
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .bounded(file!(), line!())
            .await
            .unwrap();
        assert!(response.contains("first complete request"));
        assert_eq!(provider.requests().len(), 1);
        assert_eq!(provider.bodies(), vec!["{}"]);
    }
}

#[tokio::test]
async fn streaming_provider_never_forwards_cancelled_partial_requests() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    cancelled_connections(&provider.url).await;
    let mut stream = complete_connection(&provider.url).await;
    let (request, chunks) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(request.split_once("\r\n\r\n").unwrap().1, "{}");
    drop(chunks);
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(response.contains("[DONE]"));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), provider.requests.recv())
            .await
            .is_err()
    );
}
