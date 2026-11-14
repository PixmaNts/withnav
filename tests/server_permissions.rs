//! In-process tests of the permission routes (`/v1/permissions/...`) against `AppState`.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

mod common;
use common::server::{TestServerConfig, UNREACHABLE_PROVIDER_URL};

#[tokio::test]
async fn permission_routes_require_authentication() {
    let tmp = tempfile::tempdir().unwrap();
    let token = "secret-owner-token";
    let cfg = TestServerConfig::mock(tmp.path(), UNREACHABLE_PROVIDER_URL).bearer_token(token);
    let state = cfg.state();

    let routes = [
        ("GET", "/v1/permissions/rules", Body::empty()),
        (
            "POST",
            "/v1/permissions/rules",
            Body::from(
                json!({
                    "action": "bash",
                    "resource": "*",
                    "effect": "allow",
                    "scope": "global",
                })
                .to_string(),
            ),
        ),
        ("DELETE", "/v1/permissions/rules/1", Body::empty()),
        ("GET", "/v1/permissions/history", Body::empty()),
        ("GET", "/v1/permissions/validations", Body::empty()),
        (
            "GET",
            "/v1/permissions/validation/check?kind=flow&location=%2Fflow&hash=sha256%3Aabc",
            Body::empty(),
        ),
        ("DELETE", "/v1/permissions/validations/1", Body::empty()),
        (
            "POST",
            "/v1/permissions/validate",
            Body::from(
                json!({
                    "kind": "flow",
                    "name": "test_flow",
                })
                .to_string(),
            ),
        ),
    ];

    for (method, path, body) in routes {
        let app = withnav::server::routes::app(state.clone());
        let mut req_builder = Request::builder().method(method).uri(path);
        if method == "POST" {
            req_builder = req_builder.header("content-type", "application/json");
        }
        let req = req_builder.body(body).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "route {method} {path} must require bearer token"
        );
    }
}

#[tokio::test]
async fn validation_check_reads_without_migrating_database() {
    let tmp = tempfile::tempdir().unwrap();
    let token = "read-only-token";
    let cfg = TestServerConfig::mock(tmp.path(), UNREACHABLE_PROVIDER_URL).bearer_token(token);
    let state = cfg.state();
    let db = tmp.path().join("data/withnav.db");
    let before = std::fs::read(&db).ok();
    let app = withnav::server::routes::app(state);
    let req = Request::builder()
        .method("GET")
        .uri("/v1/permissions/validation/check?kind=flow&location=%2Fflow&hash=sha256%3Aabc")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(std::fs::read(&db).ok(), before);
    assert!(
        !db.exists(),
        "read-only validation lookup must not create the database"
    );
}

#[tokio::test]
async fn permission_rules_crud_and_history_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let token = "test-token";
    let cfg = TestServerConfig::mock(tmp.path(), UNREACHABLE_PROVIDER_URL).bearer_token(token);
    let state = cfg.state();

    let auth_header = format!("Bearer {token}");

    // 1. Initial list of rules is empty
    let app = withnav::server::routes::app(state.clone());
    let req = Request::builder()
        .method("GET")
        .uri("/v1/permissions/rules")
        .header("authorization", &auth_header)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let rules: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert!(rules.is_empty());

    // 2. Add rule
    let app = withnav::server::routes::app(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/v1/permissions/rules")
        .header("authorization", &auth_header)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "action": "bash",
                "resource": "cargo test *",
                "effect": "allow",
                "scope": "global",
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let added: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let rule_id = added["id"].as_i64().unwrap();
    assert_eq!(added["action"], "bash");

    // 3. List rules includes added rule
    let app = withnav::server::routes::app(state.clone());
    let req = Request::builder()
        .method("GET")
        .uri("/v1/permissions/rules")
        .header("authorization", &auth_header)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let rules: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["id"], rule_id);

    // 4. Revoke rule
    let app = withnav::server::routes::app(state.clone());
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/permissions/rules/{rule_id}"))
        .header("authorization", &auth_header)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 5. Revoke again returns 404
    let app = withnav::server::routes::app(state.clone());
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/permissions/rules/{rule_id}"))
        .header("authorization", &auth_header)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 6. Active list is empty, but --all includes revoked
    let app = withnav::server::routes::app(state.clone());
    let req = Request::builder()
        .method("GET")
        .uri("/v1/permissions/rules?all=true")
        .header("authorization", &auth_header)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let rules: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(rules.len(), 1);
    assert!(rules[0]["revoked_at"].is_string());

    // 7. Decision history
    let app = withnav::server::routes::app(state.clone());
    let req = Request::builder()
        .method("GET")
        .uri("/v1/permissions/history?limit=10")
        .header("authorization", &auth_header)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let history: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert!(history.is_empty());
}

#[tokio::test]
async fn validations_and_flow_validation_routes() {
    let tmp = tempfile::tempdir().unwrap();
    let token = "test-token";
    let cfg = TestServerConfig::mock(tmp.path(), UNREACHABLE_PROVIDER_URL).bearer_token(token);
    let state = cfg.state();

    let auth_header = format!("Bearer {token}");

    // 1. Validations list initially empty
    let app = withnav::server::routes::app(state.clone());
    let req = Request::builder()
        .method("GET")
        .uri("/v1/permissions/validations")
        .header("authorization", &auth_header)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let validations: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert!(validations.is_empty());

    // 2. Revoke non-existent validation returns 404
    let app = withnav::server::routes::app(state.clone());
    let req = Request::builder()
        .method("DELETE")
        .uri("/v1/permissions/validations/99999")
        .header("authorization", &auth_header)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 3. Validate non-existent flow returns 404 or bad request
    let app = withnav::server::routes::app(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/v1/permissions/validate")
        .header("authorization", &auth_header)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "kind": "flow",
                "name": "non_existent_flow",
                "workspace": tmp.path(),
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status() == StatusCode::NOT_FOUND || resp.status() == StatusCode::BAD_REQUEST,
        "validating non-existent flow should fail, got status {}",
        resp.status()
    );
}

#[tokio::test]
async fn server_permissions_rejects_relative_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let token = "test-token";
    let cfg = TestServerConfig::mock(tmp.path(), UNREACHABLE_PROVIDER_URL).bearer_token(token);
    let state = cfg.state();
    let auth_header = format!("Bearer {token}");

    // 1. GET /v1/permissions/rules?project=. -> 400
    let app = withnav::server::routes::app(state.clone());
    let req = Request::builder()
        .method("GET")
        .uri("/v1/permissions/rules?project=.")
        .header("authorization", &auth_header)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // 2. POST /v1/permissions/rules with relative project scope -> 400
    let app = withnav::server::routes::app(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/v1/permissions/rules")
        .header("authorization", &auth_header)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "action": "bash",
                "resource": "*",
                "effect": "allow",
                "scope": {"project": "relative/path"},
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // 3. DELETE /v1/permissions/validations/1?workspace=. -> 400
    let app = withnav::server::routes::app(state.clone());
    let req = Request::builder()
        .method("DELETE")
        .uri("/v1/permissions/validations/1?workspace=.")
        .header("authorization", &auth_header)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // 4. POST /v1/permissions/validate with relative workspace -> 400
    let app = withnav::server::routes::app(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/v1/permissions/validate")
        .header("authorization", &auth_header)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "kind": "flow",
                "name": "any_flow",
                "workspace": "relative/workspace",
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
