//! Persisted workspace visibility and bearer-protected workspace removal.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::path::Path;
use tower::ServiceExt;
use withnav::server::{app, AppState};

mod common;
use common::server::TestServerConfig;

fn state(root: &Path) -> AppState {
    TestServerConfig::unreachable(root)
        .bearer_token("secret")
        .state()
}

async fn request(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Value,
    auth: bool,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if auth {
        req = req.header("authorization", "Bearer secret");
    }
    let response = app(state.clone())
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn create(state: &AppState, path: &Path) -> String {
    let (status, body) = request(
        state,
        "POST",
        "/v1/sessions",
        json!({"workspace": path}),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body["session_id"].as_str().unwrap().into()
}

fn uri(path: &Path) -> String {
    let mut url = reqwest::Url::parse("http://localhost/v1/workspaces").unwrap();
    url.query_pairs_mut()
        .append_pair("path", path.to_str().unwrap());
    format!("{}?{}", url.path(), url.query().unwrap())
}

async fn listed(state: &AppState) -> Vec<Value> {
    request(state, "GET", "/v1/workspaces", Value::Null, true)
        .await
        .1["workspaces"]
        .as_array()
        .unwrap()
        .clone()
}

#[cfg(unix)]
#[tokio::test]
async fn hidden_paths_fold_survive_restart_and_reopen_explicitly_or_on_creation() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let alias = tmp.path().join("alias");
    std::os::unix::fs::symlink(&workspace, &alias).unwrap();
    let first = state(tmp.path());
    create(&first, &workspace).await;
    create(&first, &alias).await;
    assert_eq!(listed(&first).await[0]["session_count"], 2);
    assert_eq!(
        request(&first, "DELETE", &uri(&alias), Value::Null, true)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert!(listed(&first).await.is_empty());
    first.close_all().await;
    drop(first);
    let restarted = state(tmp.path());
    assert!(listed(&restarted).await.is_empty());
    for _ in 0..2 {
        assert_eq!(
            request(&restarted, "POST", &uri(&workspace), Value::Null, true)
                .await
                .0,
            StatusCode::NO_CONTENT
        );
    }
    assert_eq!(listed(&restarted).await[0]["session_count"], 2);
    request(&restarted, "DELETE", &uri(&alias), Value::Null, true).await;
    create(&restarted, &alias).await;
    assert_eq!(listed(&restarted).await[0]["session_count"], 3);
    restarted.close_all().await;
}

#[tokio::test]
async fn workspace_mutations_require_auth_and_existing_absolute_directories_and_protect_launches() {
    let tmp = tempfile::tempdir().unwrap();
    let state = TestServerConfig::unreachable(tmp.path())
        .bearer_token("secret")
        .web(vec![tmp.path().to_path_buf()])
        .state();
    let missing = tmp.path().join("missing");
    for (case, method, path, authenticated, expected) in [
        (
            "unauthenticated reopen",
            "POST",
            tmp.path(),
            false,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "relative reopen",
            "POST",
            Path::new("relative"),
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "missing reopen",
            "POST",
            missing.as_path(),
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "unauthenticated hide",
            "DELETE",
            tmp.path(),
            false,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "relative hide",
            "DELETE",
            Path::new("relative"),
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "missing hide",
            "DELETE",
            missing.as_path(),
            true,
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let (status, body) = request(&state, method, &uri(path), Value::Null, authenticated).await;
        assert_eq!(status, expected, "{case}: {body}");
    }
    let (status, body) = request(
        &state,
        "DELETE",
        &format!("{}&sessions=delete", uri(tmp.path())),
        Value::Null,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["reason"], "launched");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("opened by nav web on the command line"));
    state.close_all().await;
}

#[tokio::test]
async fn destructive_removal_deletes_folded_root_sessions_children_and_events() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let state = state(tmp.path());
    let root = create(&state, &workspace).await;
    create(&state, &workspace.join("..").join("project")).await;
    let child = ulid::Ulid::generate();
    state
        .history()
        .record_session(child, workspace.to_str().unwrap(), "mock", false)
        .await
        .unwrap();
    state.history().flush().await.unwrap();
    let conn = rusqlite::Connection::open(tmp.path().join("history.db")).unwrap();
    conn.execute("INSERT INTO child_links(parent_id,child_id,agent,goal,policy_snapshot,status,created_at) VALUES (?1,?2,'vigie','task','{}','done','now')", rusqlite::params![root, child.to_string()]).unwrap();
    let (status, body) = request(
        &state,
        "DELETE",
        &format!("{}&sessions=delete", uri(&workspace)),
        Value::Null,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"deleted":2}));
    for table in ["sessions", "events", "child_links"] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    assert!(listed(&state).await.is_empty());
    state.close_all().await;
}
