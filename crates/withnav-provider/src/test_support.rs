//! Scripted-provider helpers shared by the unit tests of this crate and of the crates that use it.

/// Asserts the one chat POST a layer sent: the upstream model name and the `reasoning_effort` it carried.
pub async fn assert_layer_body(server: &wiremock::MockServer, effort: Option<&str>) {
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
