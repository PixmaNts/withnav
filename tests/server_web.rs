//! The browser client's server surface: the embedded static files (only for a `nav web`
//! server, never behind the token), `GET /v1/workspaces` and `GET /v1/intent`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use withnav::server::{app, AppState};

mod common;
use common::server::TestServerConfig;

const TOKEN: &str = "web-token";

async fn get(
    state: &AppState,
    uri: &str,
    token: bool,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut request = Request::builder().method("GET").uri(uri);
    if token {
        request = request.header("authorization", format!("Bearer {TOKEN}"));
    }
    let response = app(state.clone())
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, headers, body.to_vec())
}

async fn get_json(state: &AppState, uri: &str) -> (StatusCode, Value) {
    let (status, _, body) = get(state, uri, true).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn web_state(tmp: &std::path::Path, launch: Vec<std::path::PathBuf>) -> AppState {
    TestServerConfig::unreachable(tmp)
        .bearer_token(TOKEN)
        .web(launch)
        .state()
}

async fn create_session(state: &AppState, workspace: &std::path::Path) {
    let response = app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sessions")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::from(
                    json!({ "workspace": workspace.to_str().expect("utf8") }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::OK);
}

// Given a server started for the web client, with a token
// When `/` is requested without the token
// Then the page is served with the security headers
#[tokio::test]
async fn the_page_is_served_without_a_token_with_security_headers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = web_state(tmp.path(), vec![]);
    let (status, headers, body) = get(&state, "/", false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    let csp = headers["content-security-policy"].to_str().expect("csp");
    assert!(csp.contains("default-src 'self'") && csp.contains("frame-ancestors 'none'"));
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["referrer-policy"], "no-referrer");
    assert_eq!(headers["cache-control"], "no-cache");
    assert!(String::from_utf8_lossy(&body).contains("<html") || !body.is_empty());
}

// Given a client-side route that is not a file
// When it is requested
// Then index.html answers, identical to `/`
#[tokio::test]
async fn an_unknown_page_path_falls_back_to_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = web_state(tmp.path(), vec![]);
    let (_, _, index) = get(&state, "/", false).await;
    let (status, headers, body) = get(&state, "/workspace/some/session", false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, index);
    assert!(headers.contains_key("content-security-policy"));
}

// Given a built asset under assets/
// When it is requested
// Then it has its own content type and a long immutable cache
#[tokio::test]
async fn hashed_assets_are_cached_for_good() {
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("webui/dist/assets");
    let Some(file) = std::fs::read_dir(&dist)
        .ok()
        .and_then(|mut entries| entries.find_map(|e| e.ok()))
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x == "js" || x == "css")
        })
    else {
        return;
    };
    let name = file.file_name().to_string_lossy().into_owned();
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = web_state(tmp.path(), vec![]);
    let (status, headers, _) = get(&state, &format!("/assets/{name}"), false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers["cache-control"],
        "public, max-age=31536000, immutable"
    );
    let expected = if name.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else {
        "text/css; charset=utf-8"
    };
    assert_eq!(headers["content-type"], expected);
}

// Given a web server with a token
// When an API path is requested without it, or an unknown API path with it
// Then 401 for the first and a JSON 404 for the second, never the page
#[tokio::test]
async fn api_paths_keep_the_token_and_never_return_the_page() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = web_state(tmp.path(), vec![]);
    for uri in ["/v1/workspaces", "/v1/intent?workspace=/tmp", "/v1/models"] {
        let (status, _, _) = get(&state, uri, false).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    let (status, _, _) = get(&state, "/v1/unknown", false).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, headers, body) = get(&state, "/v1/unknown", true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!headers.contains_key("content-security-policy"));
    assert!(serde_json::from_slice::<Value>(&body).is_ok());
    let (status, _, _) = get(&state, "/v1/health", false).await;
    assert_eq!(status, StatusCode::OK);
}

// Given an ordinary server (not started by `nav web`)
// When `/` is requested
// Then nothing is served there, token or not
#[tokio::test]
async fn an_ordinary_server_serves_no_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = TestServerConfig::unreachable(tmp.path())
        .bearer_token(TOKEN)
        .state();
    let (status, _, _) = get(&state, "/", false).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = get(&state, "/", true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let no_token = TestServerConfig::unreachable(tmp.path()).state();
    let (status, _, _) = get(&no_token, "/", false).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// Given launch workspaces, history in another workspace, and one whose directory is gone
// When /v1/workspaces is read
// Then launched ones come first in argument order, then history by recency, and the missing is skipped
#[tokio::test]
async fn workspaces_list_launched_first_then_history_and_skip_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonical root");
    let launched_b = root.join("beta");
    let launched_a = root.join("alpha");
    let history = root.join("history-only");
    let gone = root.join("gone");
    for dir in [&launched_a, &launched_b, &history, &gone] {
        std::fs::create_dir_all(dir).expect("workspace dir");
    }
    let state = web_state(&root, vec![launched_b.clone(), launched_a.clone()]);
    create_session(&state, &history).await;
    create_session(&state, &history).await;
    create_session(&state, &gone).await;
    create_session(&state, &launched_a).await;
    std::fs::remove_dir_all(&gone).expect("remove workspace");

    let (status, body) = get_json(&state, "/v1/workspaces").await;
    assert_eq!(status, StatusCode::OK);
    let list = body["workspaces"].as_array().expect("array");
    let paths: Vec<&str> = list
        .iter()
        .map(|w| w["path"].as_str().expect("path"))
        .collect();
    assert_eq!(
        paths,
        [
            launched_b.to_str().unwrap(),
            launched_a.to_str().unwrap(),
            history.to_str().unwrap()
        ]
    );
    assert_eq!(list[0]["name"], "beta");
    assert_eq!(list[0]["launched"], true);
    assert_eq!(list[0]["session_count"], 0);
    assert_eq!(list[0]["updated_at"], Value::Null);
    assert_eq!(list[1]["session_count"], 1);
    assert!(list[1]["updated_at"].is_string());
    assert_eq!(list[2]["launched"], false);
    assert_eq!(list[2]["session_count"], 2);
    for entry in list {
        for key in ["branch", "dirty"] {
            assert!(entry.get(key).is_some(), "{key} is always present");
        }
    }
}

// Given a server that was not started by `nav web`
// When /v1/workspaces is read
// Then only the history's workspaces are listed, none launched
#[tokio::test]
async fn workspaces_without_launch_list_history_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonical root");
    let ws = root.join("proj");
    std::fs::create_dir_all(&ws).expect("workspace dir");
    let state = TestServerConfig::unreachable(&root)
        .bearer_token(TOKEN)
        .state();
    create_session(&state, &ws).await;
    let (status, body) = get_json(&state, "/v1/workspaces").await;
    assert_eq!(status, StatusCode::OK);
    let list = body["workspaces"].as_array().expect("array");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["launched"], false);
    assert_eq!(list[0]["name"], "proj");
}

// Given a workspace with an intent record, one without, and no query
// When /v1/intent is read
// Then the sections, `exists:false` with nulls, and a 400
#[tokio::test]
async fn intent_reports_present_absent_and_missing_query() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let with = tmp.path().join("with");
    let without = tmp.path().join("without");
    std::fs::create_dir_all(with.join(".withnav")).expect("dir");
    std::fs::create_dir_all(&without).expect("dir");
    std::fs::write(
        with.join(".withnav/intent.md"),
        "## Intent\nShip the web client\n\n## Plan\n1. build\n",
    )
    .expect("intent");
    let state = web_state(tmp.path(), vec![]);

    let uri = format!("/v1/intent?workspace={}", with.to_str().unwrap());
    let (status, body) = get_json(&state, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["exists"], true);
    assert_eq!(body["intent"], "Ship the web client");
    assert_eq!(body["plan"], "1. build");
    assert_eq!(body["decisions"], Value::Null);
    assert_eq!(body["active_spec"], Value::Null);
    assert!(body["raw"].as_str().unwrap().contains("## Plan"));

    let uri = format!("/v1/intent?workspace={}", without.to_str().unwrap());
    let (status, body) = get_json(&state, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["exists"], false);
    assert_eq!(body["raw"], Value::Null);

    let (status, body) = get_json(&state, "/v1/intent").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
}
