//! Route-level integration tests for the MCP UI HTTP endpoints:
//! `GET /v1/mcp/{server}/resource`, `POST /v1/mcp/{server}/call`, and `GET /v1/mcp`.
//! Exercises real socket handling, query parsing, body deserialization, bearer authentication,
//! and MCP service error translation.

use std::path::Path;
use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{Value, json};

use withnav::server::AppState;

mod common;
use common::server::{TestServerConfig, create_session, spawn_server};

fn fixture_state(tmp: &Path, bearer_token: Option<&str>) -> AppState {
    let mut builder = TestServerConfig::unreachable(tmp);
    if let Some(token) = bearer_token {
        builder = builder.bearer_token(token);
    }
    let cfg = builder.config();
    let mut settings = withnav::settings::load(&cfg.config_dir).expect("settings");
    settings.set_dotted(
        "mcp",
        json!({
            "fixture": {
                "command": [
                    common::python(),
                    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_server.py"),
                ],
                "call_timeout_ms": 5000,
            }
        }),
    );
    withnav::settings::save(&cfg.config_dir, &settings).expect("save settings");
    AppState::new(cfg).expect("state")
}

async fn wait_for_mcp_ready(state: &AppState) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if state
                .mcp_statuses()
                .iter()
                .any(|s| s.server_id == "fixture" && s.state == "ready")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture MCP server must initialize within deadline");
}

// Given a running server with an initialized fixture MCP server and a created session
// When GET /v1/mcp/fixture/resource and POST /v1/mcp/fixture/call are requested with valid inputs
// Then resource read returns 200 with the HTML content and tool call returns 200 with the echoed JSON payload
#[tokio::test]
async fn valid_mcp_resource_read_and_tool_call_over_socket() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace directory");
    let state = fixture_state(tmp.path(), None);
    wait_for_mcp_ready(&state).await;

    let (addr, server_task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");
    let _session_id = create_session(&base, &ws).await;

    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base}/v1/mcp/fixture/resource"))
        .query(&[("uri", "ui://fixture/app.html")])
        .send()
        .await
        .expect("get resource response");
    assert_eq!(response.status(), StatusCode::OK);
    let resource_body: Value = response.json().await.expect("resource json");
    assert_eq!(resource_body["mime_type"], "text/html;profile=mcp-app");
    assert_eq!(resource_body["text"], "<!doctype html><title>App</title>");
    assert_eq!(resource_body["uri"], "ui://fixture/app.html");

    let response = client
        .post(format!("{base}/v1/mcp/fixture/call"))
        .json(&json!({
            "name": "echo",
            "arguments": { "text": "hi" }
        }))
        .send()
        .await
        .expect("post call response");
    assert_eq!(response.status(), StatusCode::OK);
    let call_body: Value = response.json().await.expect("call json");
    let content_text = call_body["content"][0]["text"]
        .as_str()
        .expect("tool content text string");
    let parsed_text: Value = serde_json::from_str(content_text).expect("parsed echoed json");
    assert_eq!(parsed_text, json!({ "text": "hi" }));

    state.close_all().await;
    server_task.abort();
}

// Given a running server with an initialized fixture MCP server
// When POST /v1/mcp/fixture/call receives unparseable JSON or JSON missing the required name field
// Then the endpoint returns 400 mcp_invalid_request with a fixed message, never the body quoted back
#[tokio::test]
async fn mcp_call_rejects_malformed_json_and_invalid_shape() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = fixture_state(tmp.path(), None);
    wait_for_mcp_ready(&state).await;

    let (addr, server_task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{base}/v1/mcp/fixture/call"))
        .header("content-type", "application/json")
        .body("not json")
        .send()
        .await
        .expect("post malformed response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("error json");
    assert_eq!(
        body,
        json!({"error": "invalid MCP App request", "reason": "mcp_invalid_request"})
    );

    let response = client
        .post(format!("{base}/v1/mcp/fixture/call"))
        .json(&json!({
            "arguments": { "text": "hi" }
        }))
        .send()
        .await
        .expect("post missing name response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("error json");
    assert_eq!(
        body,
        json!({"error": "invalid MCP App request", "reason": "mcp_invalid_request"})
    );

    state.close_all().await;
    server_task.abort();
}

// Given a server configured with a bearer authentication token
// When requests arrive at MCP routes without the required Authorization header
// Then 401 Unauthorized is returned before route handlers execute, while valid bearer tokens pass
#[tokio::test]
async fn mcp_routes_require_bearer_auth_when_configured() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let token = "test-mcp-secret-token";
    let state = fixture_state(tmp.path(), Some(token));
    wait_for_mcp_ready(&state).await;

    let (addr, server_task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base}/v1/mcp/fixture/resource"))
        .query(&[("uri", "ui://fixture/app.html")])
        .send()
        .await
        .expect("unauthorized resource response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = client
        .post(format!("{base}/v1/mcp/fixture/call"))
        .json(&json!({
            "name": "echo",
            "arguments": { "text": "hi" }
        }))
        .send()
        .await
        .expect("unauthorized call response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = client
        .get(format!("{base}/v1/mcp"))
        .send()
        .await
        .expect("unauthorized status response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = client
        .get(format!("{base}/v1/mcp/fixture/resource"))
        .query(&[("uri", "ui://fixture/app.html")])
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("authorized resource response");
    assert_eq!(response.status(), StatusCode::OK);

    let response = client
        .post(format!("{base}/v1/mcp/fixture/call"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "name": "echo",
            "arguments": { "text": "hi" }
        }))
        .send()
        .await
        .expect("authorized call response");
    assert_eq!(response.status(), StatusCode::OK);

    let response = client
        .get(format!("{base}/v1/mcp"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("authorized status response");
    assert_eq!(response.status(), StatusCode::OK);

    state.close_all().await;
    server_task.abort();
}

// Given a running server without an MCP server named "nosuch"
// When GET /v1/mcp/nosuch/resource and POST /v1/mcp/nosuch/call are requested
// Then both return 404 mcp_unknown_server
#[tokio::test]
async fn mcp_routes_reject_unknown_server() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = fixture_state(tmp.path(), None);
    wait_for_mcp_ready(&state).await;

    let (addr, server_task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base}/v1/mcp/nosuch/resource"))
        .query(&[("uri", "ui://x")])
        .send()
        .await
        .expect("unknown server resource response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("error json");
    assert_eq!(body["reason"], "mcp_unknown_server", "{body}");

    let response = client
        .post(format!("{base}/v1/mcp/nosuch/call"))
        .json(&json!({
            "name": "echo",
            "arguments": { "text": "hi" }
        }))
        .send()
        .await
        .expect("unknown server call response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("error json");
    assert_eq!(body["reason"], "mcp_unknown_server", "{body}");

    state.close_all().await;
    server_task.abort();
}

// Given a server whose fixture MCP server is shut down after initialization
// When GET resource and POST call are requested for the disconnected server
// Then both return 503 mcp_unavailable
#[tokio::test]
async fn mcp_routes_report_unavailable_server_after_shutdown() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = fixture_state(tmp.path(), None);
    wait_for_mcp_ready(&state).await;

    let (addr, server_task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    state.close_all().await;

    let response = client
        .get(format!("{base}/v1/mcp/fixture/resource"))
        .query(&[("uri", "ui://fixture/app.html")])
        .send()
        .await
        .expect("disconnected resource response");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.expect("error json");
    assert_eq!(body["reason"], "mcp_unavailable", "{body}");

    let response = client
        .post(format!("{base}/v1/mcp/fixture/call"))
        .json(&json!({
            "name": "echo",
            "arguments": { "text": "hi" }
        }))
        .send()
        .await
        .expect("unavailable call response");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.expect("error json");
    assert_eq!(body["reason"], "mcp_unavailable", "{body}");

    server_task.abort();
}

// Given a running server with an initialized fixture MCP server
// When GET /v1/mcp is requested
// Then 200 OK is returned with a snapshot listing the ready fixture server and its tools (including echo)
#[tokio::test]
async fn mcp_status_route_returns_server_snapshot_with_tools() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = fixture_state(tmp.path(), None);
    wait_for_mcp_ready(&state).await;

    let (addr, server_task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base}/v1/mcp"))
        .send()
        .await
        .expect("status response");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("status json");

    assert!(body.get("incarnation").is_some());
    let servers = body["servers"].as_array().expect("servers array");
    let fixture = servers
        .iter()
        .find(|s| s["server_id"] == "fixture")
        .expect("fixture server present in snapshot");
    assert_eq!(fixture["state"], "ready");
    assert_eq!(fixture["transport"], "stdio");
    assert!(
        fixture["tool_count"].as_u64().unwrap_or(0) > 0,
        "tool_count must be positive for ready fixture: {fixture}"
    );

    let call_response = client
        .post(format!("{base}/v1/mcp/fixture/call"))
        .json(&json!({
            "name": "echo",
            "arguments": { "verified": true }
        }))
        .send()
        .await
        .expect("call echo response");
    assert_eq!(call_response.status(), StatusCode::OK);
    let call_body: Value = call_response.json().await.expect("call json");
    let raw_text = call_body["content"][0]["text"]
        .as_str()
        .expect("content text");
    let parsed: Value = serde_json::from_str(raw_text).expect("parsed echo json");
    assert_eq!(parsed, json!({ "verified": true }));

    state.close_all().await;
    server_task.abort();
}
