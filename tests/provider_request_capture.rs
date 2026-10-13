use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn mcp_definitions_reach_provider_with_safe_names_and_normalized_schema() {
    use serde_json::{json, Value};
    use std::collections::BTreeMap;
    use withnav::{
        mcp::McpManager,
        provider::{
            client::ModelClient,
            config::ProviderConfig,
            openai::{ChatMessage, ClientCache},
        },
        settings::Settings,
        tools::Registry,
    };
    let mut settings = Settings::default();
    settings.set_dotted("mcp", json!({"fake":{"command":["python3",concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/mcp_server.py")],"env":{"MALFORMED_SCHEMA":"1"}}}));
    let manager = McpManager::start(&settings);
    tokio::time::timeout(Duration::from_secs(5), async {
        while manager.snapshot().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut registry = Registry::new();
    for tool in manager.snapshot() {
        registry.register(Box::new(tool));
    }
    let server = spawn_capturing_provider(vec![http_sse(&sse_body(&[&text_chunk("ok")]))]).await;
    let provider = ProviderConfig {
        base_url: server.url.clone(),
        api_key_env: None,
        headers: BTreeMap::new(),
        connect_timeout_ms: 2000,
        request_timeout_ms: 5000,
        catalog_format: None,
    };
    let client = ModelClient::new("fake", &provider, "k".into(), &ClientCache::new()).unwrap();
    let request = withnav::agent::prompt::build_request(
        "fake",
        None,
        None,
        None,
        vec![ChatMessage::User {
            content: "hello".into(),
            provenance: Default::default(),
        }],
        registry.schemas(),
        Some(256),
    );
    client
        .chat(request, None, &Default::default(), &mut |_| {})
        .await
        .unwrap();
    let captured: Value = serde_json::from_str(&server.bodies()[0]).unwrap();
    let tools = captured["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 8);
    assert!(!tools
        .iter()
        .any(|tool| tool["function"]["name"] == "mcp__fake__malformed"));
    assert!(manager.statuses()[0]
        .skipped_tools
        .iter()
        .any(|reason| reason.contains("malformed") && reason.contains("minLength")));
    for tool in tools {
        let function = &tool["function"];
        let name = function["name"].as_str().unwrap();
        assert!(
            name.len() <= 64
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        );
        assert!(function["description"]
            .as_str()
            .unwrap()
            .starts_with("[mcp:fake]"));
        assert_eq!(function["parameters"]["type"], "object");
    }
    let reference = tools
        .iter()
        .find(|t| t["function"]["name"] == "mcp__fake__reference")
        .unwrap();
    assert_eq!(
        reference["function"]["parameters"]["properties"]["text"]["type"],
        "string"
    );
    assert!(!reference.to_string().contains("$ref"));
    manager.shutdown().await;
}

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
