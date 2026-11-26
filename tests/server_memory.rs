use axum::http::StatusCode;
use serde_json::{Value, json};

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

#[tokio::test]
async fn put_session_project_attaches_and_detaches() {
    let (tmp, workspace) = temp_workspace();
    let state = TestServerConfig::unreachable(tmp.path()).state();
    let session = state.create_session(&workspace, None, false).await.unwrap();
    let other = tmp.path().join("elsewhere");
    std::fs::create_dir(&other).unwrap();
    // A second workspace resolves to its own project to attach to.
    let foreign = state.create_session(&other, None, false).await.unwrap();
    let app = withnav::server::routes::app(state.clone());
    let (_, projects) = request_json(&app, "GET", "/v1/projects", None).await;
    // Projects are listed by name, so each one is identified by its session
    // links, never by its position in the list.
    let active = |session: &str, body: &Value| {
        body["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| {
                p["sessions"].as_array().is_some_and(|links| {
                    links
                        .iter()
                        .any(|l| l["session_id"] == session && l["active"] == Value::Bool(true))
                })
            })
            .expect("the session's project")["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let project = active(&session.to_string(), &projects);
    let path = format!("/v1/sessions/{foreign}/project");
    // Detach the session from the project its workspace gave it, then attach
    // it to another project: one active link per session, at once.
    let (status, change) =
        request_json(&app, "PUT", &path, Some(&json!({"project_id": null}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(change["status"], "applied");
    assert_eq!(change["change"], "remove");
    assert_eq!(change["linked_session_id"], foreign.to_string());
    let (status, change) =
        request_json(&app, "PUT", &path, Some(&json!({"project_id": project}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(change["status"], "applied");
    assert_eq!(change["change"], "add");
    assert_eq!(change["project_id"], project);
    let (_, projects) = request_json(&app, "GET", "/v1/projects", None).await;
    assert_eq!(active(&foreign.to_string(), &projects), project);
    // A project id that is neither a string nor null is refused.
    let (status, body) = request_json(&app, "PUT", &path, Some(&json!({"project_id": 42}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["reason"], "invalid_memory_request");
    assert_eq!(body["message"], "project_id must be a string or null");
    // The refusal changed nothing: the session still works in `project`.
    let (_, projects) = request_json(&app, "GET", "/v1/projects", None).await;
    assert_eq!(active(&foreign.to_string(), &projects), project);
}

#[tokio::test]
async fn the_group_question_can_be_answered_later() {
    let (tmp, workspace) = temp_workspace();
    let state = TestServerConfig::unreachable(tmp.path()).state();
    let _session = state.create_session(&workspace, None, false).await.unwrap();
    let app = withnav::server::routes::app(state);
    let (_, projects) = request_json(&app, "GET", "/v1/projects", None).await;
    let project = projects["projects"][0]["id"].as_str().unwrap().to_owned();
    let path = format!("/v1/projects/{project}/group-answer");
    // "Later" leaves the question open; no membership is added.
    let (status, answered) = request_json(
        &app,
        "POST",
        &path,
        Some(&json!({"group_id": null, "later": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answered["group_answered"], false);
    assert_eq!(answered["groups"], json!([]));
    // Naming a group adds the membership and closes the question at once.
    let (status, group) =
        request_json(&app, "POST", "/v1/groups", Some(&json!({"name": "Shared"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let group = group["id"].as_str().unwrap().to_owned();
    let (status, answered) =
        request_json(&app, "POST", &path, Some(&json!({"group_id": group}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answered["group_answered"], true);
    assert_eq!(answered["groups"], json!([group]));
    let (_, projects) = request_json(&app, "GET", "/v1/projects", None).await;
    let row = history_row(&projects, "captain", "add");
    assert_eq!(row["group_id"], group);
    assert_eq!(row["status"], "applied");
}

#[tokio::test]
async fn a_rejected_proposal_changes_nothing() {
    // The fixture proposes adding its own project to the group, from the agent.
    let (_tmp, app, project, _group) = group_tool_fixture(true, false).await;
    let (_, pending) = request_json(&app, "GET", "/v1/memory/needs-you", None).await;
    let row = pending["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["actor"] == "agent" && row["change"] == "add")
        .unwrap();
    let id = row["id"].as_i64().unwrap();
    let (status, decided) = request_json(
        &app,
        "POST",
        &format!("/v1/memory/needs-you/{id}/reject"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decided["status"], "rejected");
    let (_, after) = request_json(&app, "GET", "/v1/projects", None).await;
    // The project stayed out of the group, and its own add history shows the
    // proposal and the refusal as one row that was decided in place.
    let home = after["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == project)
        .expect("the fixture's home project");
    assert_eq!(home["groups"], json!([]));
    let add = history_row(&after, "agent", "add");
    assert_eq!(add["id"], id);
    assert_eq!(add["status"], "rejected");
    assert!(add["decided_at"].is_string());
    let (_, pending) = request_json(&app, "GET", "/v1/memory/needs-you", None).await;
    assert_eq!(pending["changes"], json!([]));
}

/// Drives one agent turn that calls a session scope tool through the scripted
/// provider, as the group tool fixture does. `respond` builds the tool call's
/// arguments from the session and project ids.
async fn session_scope_fixture(
    tool: &'static str,
    respond: impl Fn(&str, &str) -> Value + Send + Sync + Copy + 'static,
) -> (tempfile::TempDir, axum::Router, String, String) {
    use common::server::{collect_until, send_text};
    use common::sse::{
        http_sse, spawn_responding_provider, sse_body, text_chunk, tool_calls_chunk,
    };
    let (tmp, workspace) = temp_workspace();
    let ids = std::sync::Arc::new(std::sync::OnceLock::<(String, String)>::new());
    let inputs = ids.clone();
    let provider = spawn_responding_provider(2, move |index, _| {
        let (session, project) = inputs.get().unwrap();
        let chunk = if index == 0 {
            tool_calls_chunk(&[("scope-change", tool, respond(session, project))])
        } else {
            text_chunk("Done")
        };
        http_sse(&sse_body(&[&chunk]))
    })
    .await;
    let state = TestServerConfig::mock(tmp.path(), &provider.url).state();
    // A session in the first workspace creates the project the wandering
    // session will ask for; the wandering one, in a second workspace, is the
    // session whose link the tool changes.
    let _resident = state.create_session(&workspace, None, false).await.unwrap();
    let other = tmp.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let wandering = state.create_session(&other, None, false).await.unwrap();
    let app = withnav::server::routes::app(state.clone());
    let (_, projects) = request_json(&app, "GET", "/v1/projects", None).await;
    // Projects are listed by name, never a stable order: the target project is
    // the one the wandering session does NOT work in.
    let own = projects["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| {
            p["sessions"].as_array().is_some_and(|links| {
                links.iter().any(|l| {
                    l["session_id"] == *wandering.to_string() && l["active"] == Value::Bool(true)
                })
            })
        })
        .expect("the wandering session's project");
    let target = projects["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] != own["id"])
        .expect("another project")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    ids.set((wandering.to_string(), target.clone())).unwrap();
    let sub = state.subscribe(wandering, None).await.unwrap();
    send_text(&state, wandering, "Change my project").await;
    let events = collect_until(sub, |event| event.kind == "turn.end").await;
    assert!(
        events
            .iter()
            .all(|event| event.kind != "permission.request")
    );
    (tmp, app, wandering.to_string(), target)
}

#[tokio::test]
async fn session_attach_proposes_and_waits_for_the_captain() {
    let (_tmp, app, session, project) = session_scope_fixture(
        "session_attach",
        |_, project| json!({"project_id": project}),
    )
    .await;
    // A pending change is visible in needs-you right away…
    let (_, pending) = request_json(&app, "GET", "/v1/memory/needs-you", None).await;
    let row = pending["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["actor"] == "agent" && row["relation"] == "session_project")
        .expect("the proposed session link");
    assert_eq!(row["status"], "proposed");
    assert_eq!(row["change"], "add");
    assert_eq!(row["linked_session_id"], session);
    assert_eq!(row["project_id"], project);
    // …and the session's active project did not move until the Captain accepts.
    let (_, projects) = request_json(&app, "GET", "/v1/projects", None).await;
    let active = projects["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| {
            p["sessions"].as_array().is_some_and(|links| {
                links
                    .iter()
                    .any(|l| l["session_id"] == session && l["active"] == Value::Bool(true))
            })
        })
        .expect("the session still works in its first project");
    assert_ne!(active["id"], project);
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
    let (_, projects) = request_json(&app, "GET", "/v1/projects", None).await;
    let active = projects["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == project)
        .unwrap();
    assert!(
        active["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| { l["session_id"] == session && l["active"] == true })
    );
}

#[tokio::test]
async fn session_detach_applies_with_history() {
    let (_tmp, app, session, project) =
        session_scope_fixture("session_detach", |_, _| json!({})).await;
    let _ = project;
    // The link is gone at once, with no permission request and no pending
    // change for it…
    let (_, projects) = request_json(&app, "GET", "/v1/projects", None).await;
    let linked = projects["projects"].as_array().unwrap().iter().find(|p| {
        p["sessions"].as_array().is_some_and(|links| {
            links
                .iter()
                .any(|l| l["session_id"] == session && l["active"] == Value::Bool(true))
        })
    });
    assert!(
        linked.is_none(),
        "the session must no longer work in any project"
    );
    let (_, pending) = request_json(&app, "GET", "/v1/memory/needs-you", None).await;
    assert_eq!(pending["changes"], json!([]));
    // …and the history row records the removal as applied by the agent.
    let removal = projects["history"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| {
            row["actor"] == "agent"
                && row["change"] == "remove"
                && row["relation"] == "session_project"
                && row["linked_session_id"] == session
        })
        .expect("the detach history row");
    assert_eq!(removal["status"], "applied");
    assert!(removal["decided_at"].is_string());
}
