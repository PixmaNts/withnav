mod common;

#[tokio::test]
async fn bypass_http_sends_only_the_desired_boolean_and_reads_the_watermark() {
    let body = r#"{"enabled":true,"changed":true,"bypass_seq":42}"#;
    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let server = common::sse::spawn_capturing_provider(vec![response.into_bytes()]).await;
    let client = withnav::tui::client::Client::new(server.url.trim_end_matches("/v1"), None);
    let reply = client.set_bypass("root", true).await.unwrap();
    assert!(reply.enabled);
    assert_eq!(reply.bypass_seq, 42);
    assert!(server.requests()[0].starts_with("POST /v1/sessions/root/bypass "));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&server.bodies()[0]).unwrap(),
        serde_json::json!({"enabled":true})
    );
}
