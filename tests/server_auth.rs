//! Bearer-token gate on the HTTP surface: when `ServerConfig.bearer_token`
//! is set, every route except `/v1/health` answers 401 without the exact
//! `Authorization: Bearer <token>` header; with the header, behaviour is
//! unchanged. Without a token, nothing is gated.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use withnav::server::{AppState, app};

mod common;
use common::server::TestServerConfig;

fn test_state(tmp: &std::path::Path, bearer_token: Option<String>) -> AppState {
    let mut config = TestServerConfig::unreachable(tmp);
    if let Some(token) = &bearer_token {
        config = config.bearer_token(token);
    }
    config.state()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("json")
}

async fn request_with_auth(
    state: &AppState,
    auth_header: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(header) = auth_header {
        builder = builder.header("authorization", header);
    }
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request");
    app(state.clone())
        .oneshot(request)
        .await
        .expect("infallible")
}

// Given a server configured with a bearer token
// When POST /v1/sessions arrives WITHOUT an Authorization header
// Then 401 before any session work happens (no session is minted)
#[tokio::test]
async fn with_a_token_missing_header_is_401() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = test_state(tmp.path(), Some("secret-token".to_string()));

    let response = request_with_auth(
        &state,
        None,
        "POST",
        "/v1/sessions",
        Some(json!({ "workspace": tmp.path().join("ws").to_str().expect("utf8") })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(state.session_count(), 0, "401 happens before the handler");
}

// Given a server configured with a bearer token
// When the same POST arrives WITH the exact `Bearer <token>` header
// Then 200 with the frozen create body (the gate is transparent)
#[tokio::test]
async fn with_a_token_the_bearer_header_is_accepted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("ws dir");
    let state = test_state(tmp.path(), Some("secret-token".to_string()));

    let response = request_with_auth(
        &state,
        Some("Bearer secret-token"),
        "POST",
        "/v1/sessions",
        Some(json!({ "workspace": ws.to_str().expect("utf8") })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["root_actor_id"], "agent:main");
    assert_eq!(state.session_count(), 1);
}

// Given a server configured with a bearer token
// When the header carries the WRONG token (or a non-Bearer scheme)
// Then 401 — the comparison is exact, not prefix
#[tokio::test]
async fn with_a_token_a_wrong_or_malformed_header_is_401() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = test_state(tmp.path(), Some("secret-token".to_string()));

    for header in [
        "Bearer wrong",
        "secret-token",
        "bearer secret-token",
        "Bearer ",
    ] {
        let response = request_with_auth(
            &state,
            Some(header),
            "POST",
            "/v1/sessions",
            Some(json!({})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{header:?}");
    }
}

// Given a server configured with a bearer token
// When GET /v1/health arrives WITHOUT any header
// Then 200: health stays open so liveness probes need no secret
#[tokio::test]
async fn with_a_token_health_answers_without_the_header() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = test_state(tmp.path(), Some("secret-token".to_string()));

    let response = request_with_auth(&state, None, "GET", "/v1/health", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body, json!({ "status": "ok" }));
}

// Given a server with NO token configured
// When POST /v1/sessions arrives without any header
// Then 200 as before: the gate is opt-in
#[tokio::test]
async fn without_a_token_behaviour_is_unchanged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("ws dir");
    let state = test_state(tmp.path(), None);

    let response = request_with_auth(
        &state,
        None,
        "POST",
        "/v1/sessions",
        Some(json!({ "workspace": ws.to_str().expect("utf8") })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(state.session_count(), 1);
}
