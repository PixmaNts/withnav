use axum::http::StatusCode;
use serde_json::json;

mod common;
use common::server::{TestServerConfig, request_json, temp_workspace};

async fn group_tool_fixture(
    add: bool,
    foreign: bool,
) -> (tempfile::TempDir, axum::Router, String, String) {
    use common::server::{collect_until, send_text};
    use common::sse::{
        http_sse, spawn_responding_provider, sse_body, text_chunk, tool_calls_chunk,
    };
    let (tmp, workspace) = temp_workspace();
    let ids = std::sync::Arc::new(std::sync::OnceLock::<(String, String)>::new());
    let inputs = ids.clone();
    let tool = if add {
        "project_group_add"
    } else {
        "project_group_remove"
    };
    let provider = spawn_responding_provider(2, move |index, _| {
        let (project, group) = inputs.get().unwrap();
        let chunk = if index == 0 {
            tool_calls_chunk(&[(
                "group-change",
                tool,
                json!({"project_id":project,"group_id":group}),
            )])
        } else {
            text_chunk("Done")
        };
        http_sse(&sse_body(&[&chunk]))
    })
    .await;
    let state = TestServerConfig::mock(tmp.path(), &provider.url).state();
    let session = state.create_session(&workspace, None, false).await.unwrap();
    let app = withnav::server::routes::app(state.clone());
    let (_, before) = request_json(&app, "GET", "/v1/projects", None).await;
    let own = before["projects"][0]["id"].as_str().unwrap().to_owned();
    let project = if foreign {
        let other = tmp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        state.create_session(&other, None, false).await.unwrap();
        let (_, rows) = request_json(&app, "GET", "/v1/projects", None).await;
        rows["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|p| p["id"].as_str().filter(|id| *id != own))
            .unwrap()
            .to_owned()
    } else {
        own
    };
    let (status, group) =
        request_json(&app, "POST", "/v1/groups", Some(&json!({"name":"Shared"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let group = group["id"].as_str().unwrap().to_owned();
    if !add {
        let (status, _) = request_json(
            &app,
            "PUT",
            &format!("/v1/projects/{project}/groups/{group}"),
            Some(&json!({"confirm":true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    ids.set((project.clone(), group.clone())).unwrap();
    let sub = state.subscribe(session, None).await.unwrap();
    send_text(&state, session, "Change memory membership").await;
    let events = collect_until(sub, |event| event.kind == "turn.end").await;
    assert!(
        events
            .iter()
            .all(|event| event.kind != "permission.request")
    );
    (tmp, app, project, group)
}

#[tokio::test]
async fn the_group_tool_proposes_and_the_captain_confirms() {
    let (_tmp, app, project, group) = group_tool_fixture(true, false).await;
    let (_, before) = request_json(&app, "GET", "/v1/projects", None).await;
    assert_eq!(before["projects"][0]["groups"], json!([]));
    assert_eq!(history_row(&before, "agent", "add")["status"], "proposed");
    let (_, pending) = request_json(&app, "GET", "/v1/memory/needs-you", None).await;
    let row = &pending["changes"][0];
    assert_eq!(row["actor"], "agent");
    assert_eq!(row["project_id"], project);
    assert_eq!(row["group_id"], group);
    let id = row["id"].as_i64().unwrap();
    let (status, applied) = request_json(
        &app,
        "POST",
        &format!("/v1/memory/needs-you/{id}/accept"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(applied["status"], "applied");
    let (_, after) = request_json(&app, "GET", "/v1/projects", None).await;
    assert_eq!(after["projects"][0]["groups"], json!([group]));
    let row = history_row(&after, "agent", "add");
    assert_eq!(row["id"], id);
    assert_eq!(row["status"], "applied");
    assert!(row["decided_at"].is_string());
}

#[tokio::test]
async fn a_group_removal_by_the_agent_applies_and_is_in_the_history() {
    let (_tmp, app, project, group) = group_tool_fixture(false, false).await;
    let (_, after) = request_json(&app, "GET", "/v1/projects", None).await;
    assert_eq!(after["projects"][0]["groups"], json!([]));
    let removed = history_row(&after, "agent", "remove");
    for (field, value) in [
        ("actor", "agent"),
        ("status", "applied"),
        ("change", "remove"),
        ("project_id", &project),
        ("group_id", &group),
    ] {
        assert_eq!(removed[field], value);
    }
    let (_, pending) = request_json(&app, "GET", "/v1/memory/needs-you", None).await;
    assert_eq!(pending["changes"], json!([]));
}

fn history_row<'a>(
    body: &'a serde_json::Value,
    actor: &str,
    change: &str,
) -> &'a serde_json::Value {
    body["history"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["actor"] == actor && row["change"] == change)
        .unwrap()
}

#[tokio::test]
async fn removing_another_projects_group_requires_confirmation() {
    let (_tmp, app, project, group) = group_tool_fixture(false, true).await;
    let (_, before) = request_json(&app, "GET", "/v1/projects", None).await;
    let row = history_row(&before, "agent", "remove");
    assert_eq!(row["project_id"], project);
    assert_eq!(row["group_id"], group);
    assert_eq!(row["status"], "proposed");
    assert_eq!(before["groups"][0]["members"], json!([project]));
    let (_, pending) = request_json(&app, "GET", "/v1/memory/needs-you", None).await;
    assert_eq!(pending["changes"], json!([row]));
    let id = row["id"].as_i64().unwrap();
    let (status, _) = request_json(
        &app,
        "POST",
        &format!("/v1/memory/needs-you/{id}/accept"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, after) = request_json(&app, "GET", "/v1/projects", None).await;
    assert_eq!(after["groups"][0]["members"], json!([]));
    assert_eq!(history_row(&after, "agent", "remove")["status"], "applied");
}

#[tokio::test]
async fn memory_routes_need_the_token() {
    use tower::ServiceExt;
    let (tmp, _) = temp_workspace();
    let app = withnav::server::routes::app(
        TestServerConfig::unreachable(tmp.path())
            .bearer_token("test-token")
            .state(),
    );
    for path in ["/v1/memory", "/v1/projects", "/v1/memory/needs-you"] {
        for (token, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("Bearer test-token"), StatusCode::OK),
        ] {
            let mut builder = axum::http::Request::builder().uri(path);
            if let Some(token) = token {
                builder = builder.header("authorization", token);
            }
            let reply = app
                .clone()
                .oneshot(builder.body(axum::body::Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(reply.status(), expected, "{path}");
        }
    }
}

#[tokio::test]
async fn widening_a_scope_without_confirmation_lists_the_affected_projects() {
    let (tmp, workspace) = temp_workspace();
    let state = TestServerConfig::unreachable(tmp.path()).state();
    let session = state.create_session(&workspace, None, false).await.unwrap();
    let app = withnav::server::routes::app(state.clone());
    let (_, projects) = request_json(&app, "GET", "/v1/projects", None).await;
    let id = projects["projects"][0]["id"].as_str().unwrap();
    let result = state
        .run_command(session, "remember", "Share the checklist")
        .await
        .unwrap();
    let data = result.data.unwrap();
    let item = data["item"]["id"].as_str().unwrap();
    let (status, body) = request_json(
        &app,
        "POST",
        &format!("/v1/memory/{item}/rescope"),
        Some(&json!({"expected_version":1,"scope":"global"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["reason"], "widening_requires_confirmation");
    assert!(body["affected_projects"].is_array());
    let (_, unchanged) = request_json(&app, "GET", &format!("/v1/memory/{item}"), None).await;
    assert_eq!(unchanged["scope_ref"], id);
    assert_eq!(unchanged["current_version"], 1);
}

#[tokio::test]
async fn revise_with_a_stale_version_answers_409_and_changes_nothing() {
    let (tmp, _) = temp_workspace();
    let app = withnav::server::routes::app(TestServerConfig::unreachable(tmp.path()).state());
    use tower::ServiceExt;
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/memory")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let (status, item) = request_json(
        &app,
        "POST",
        "/v1/memory",
        Some(&json!({"scope":"global","text":"Keep the checklist"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let path = format!("/v1/memory/{}", item["id"].as_str().unwrap());
    let (status, body) = request_json(
        &app,
        "POST",
        &format!("{path}/revise"),
        Some(&json!({"expected_version":0,"text":"wrong"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body,
        json!({"reason":"version_conflict","current_version":1})
    );
    let (status, after) = request_json(&app, "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after["current"], item["current"]);
    assert_eq!(after["versions"].as_array().unwrap().len(), 1);
}
