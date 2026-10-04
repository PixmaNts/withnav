//! Scripted-provider helpers shared by the in-crate unit tests.

/// A bounded captured chat provider with a qualified metacognition assignment.
pub(crate) async fn layer_chat_fixture(
    variant: Option<serde_json::Value>,
    runtime: bool,
    answer: &str,
) -> (
    tempfile::TempDir,
    wiremock::MockServer,
    super::config::ModelsConfig,
) {
    use serde_json::json;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let chunk = json!({"choices":[{"index":0,"delta":{"content":answer},"finish_reason":"stop"}]})
        .to_string();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse_body(&[&chunk]), "text/event-stream"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"combo/chat","context_length":128000,"capabilities":{"effort_tiers":["low","high"]}}]})))
        .mount(&server).await;
    let mut model = json!({"provider":"gateway","model":"combo/chat","default_variant":"high"});
    if !runtime {
        model["context_window"] = json!(128000);
        model["variants"] =
            json!({"low":{"reasoning_effort":"low"},"high":{"reasoning_effort":"high"}});
    }
    let models = super::config::ModelsConfig::parse(&json!({"providers":{"gateway":{"base_url":format!("{}/v1",server.uri()),"catalog_format":"omniroute","request_timeout_ms":2000}},"models":{"unrelated-key":model}}).to_string()).unwrap();
    let mut assignment = json!({"model":"gateway/combo/chat"});
    if let Some(variant) = variant {
        assignment["variant"] = variant;
    }
    std::fs::write(
        dir.path().join("config.json"),
        json!({"layers":{"metacognition":assignment}}).to_string(),
    )
    .unwrap();
    super::auth::set_key(dir.path(), "gateway", "test-key").unwrap();
    (dir, server, models)
}

pub(crate) async fn assert_layer_body(server: &wiremock::MockServer, effort: Option<&str>) {
    let requests = server.received_requests().await.unwrap();
    let posts: Vec<_> = requests.iter().filter(|r| r.method == "POST").collect();
    assert_eq!(posts.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&posts[0].body).unwrap();
    assert_eq!(body["model"], "combo/chat");
    match effort {
        Some(effort) => assert_eq!(body["reasoning_effort"], effort),
        None => assert!(body.get("reasoning_effort").is_none()),
    }
}

/// A chat-completion SSE body: one `data:` frame per event, then `[DONE]`.
pub(crate) fn sse_body(events: &[&str]) -> String {
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
pub(crate) fn http_sse(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}
