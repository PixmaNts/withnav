//! The scripted chat provider the layer tests share: it writes a `config.json` layer assignment,
//! so it stays with the code that reads one.

use withnav_provider::test_support::sse_body;

/// A bounded captured chat provider with a qualified metacognition assignment.
pub(crate) async fn layer_chat_fixture(
    variant: Option<serde_json::Value>,
    runtime: bool,
    answer: &str,
) -> (
    tempfile::TempDir,
    wiremock::MockServer,
    withnav_provider::config::ModelsConfig,
) {
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
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
    let models = withnav_provider::config::ModelsConfig::parse(&json!({"providers":{"gateway":{"base_url":format!("{}/v1",server.uri()),"catalog_format":"omniroute","request_timeout_ms":2000}},"models":{"gateway/combo/chat":model}}).to_string()).unwrap();
    let mut assignment = json!({"model":"gateway/combo/chat"});
    if let Some(variant) = variant {
        assignment["variant"] = variant;
    }
    std::fs::write(
        dir.path().join("config.json"),
        json!({"layers":{"metacognition":assignment}}).to_string(),
    )
    .unwrap();
    withnav_provider::auth::set_key(dir.path(), "gateway", "test-key").unwrap();
    (dir, server, models)
}
