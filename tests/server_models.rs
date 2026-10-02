//! `GET /v1/models`: chat models with tools from the models.json a new
//! session would load; 400 without `workspace`.

use serde_json::{json, Value};

mod common;
use common::server::{spawn_server, TestServerConfig, PROVIDER};

// Given a models.json with a chat+tools alias, a chat alias without tools
//      and a fim alias
// When GET /v1/models?workspace=... runs
// Then only the chat+tools alias is listed, with its context window, and
//      the navigator default is "cognition"; without workspace it is a 400
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_chat_models_with_tools_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = TestServerConfig::mock(tmp.path(), "http://127.0.0.1:9").navigator_state();

    std::fs::write(
        tmp.path().join("models.json"),
        json!({
            "providers": {
                PROVIDER: {
                    "base_url": "http://127.0.0.1:9",
                    "api_key_env": "NAVAI_TEST_KEY",
                    "headers": {}
                }
            },
            "models": {
                "cognition": { "provider": PROVIDER, "model": "mock-opus",
                               "supports_tools": true, "context_window": 200000 },
                "plain": { "provider": PROVIDER, "model": "mock-plain", "supports_tools": false },
                "fill": { "provider": PROVIDER, "model": "mock-fim", "api": "fim", "supports_tools": true }
            }
        })
        .to_string(),
    )
    .expect("write models.json");

    let (addr, _task) = spawn_server(state).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // With a workspace: only the chat+tools alias, and the navigator default.
    let ok = client
        .get(format!("{base}/v1/models"))
        .query(&[("workspace", tmp.path())])
        .send()
        .await
        .expect("get models");
    assert_eq!(ok.status(), 200);
    let body: Value = ok.json().await.expect("models json");
    assert_eq!(
        body["models"],
        json!([{ "alias": "cognition", "model": "mock-opus", "context_window": 200000 }])
    );
    assert_eq!(body["default"], "cognition");

    // Without a workspace: 400.
    let missing = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .expect("get models without workspace");
    assert_eq!(missing.status(), 400);
}
