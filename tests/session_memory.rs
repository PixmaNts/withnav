use withnav::server::AppState;

mod common;
use common::server::{TestServerConfig, temp_workspace};

#[tokio::test]
async fn memory_commands_keep_versions_and_require_unambiguous_ids() {
    let (tmp, workspace) = temp_workspace();
    let state = TestServerConfig::unreachable(tmp.path()).state();
    let session = state.create_session(&workspace, None, false).await.unwrap();
    macro_rules! run {
        ($name:expr, $args:expr) => {
            state.run_command(session, $name, $args).await.unwrap()
        };
    }
    let data = run!("remember", "Blue checklist").data.unwrap();
    let id = data["item"]["id"].as_str().unwrap();
    assert!(!run!("memory", &format!("show {}", &id[..5])).ok);
    assert_eq!(
        run!("memory", &format!("show {}", &id[..6])).data.unwrap()["item"],
        data["item"]
    );
    assert_eq!(
        run!("memory", "search blue").data.unwrap()["items"],
        serde_json::json!([data["item"]])
    );
    assert!(run!("memory", &format!("revise {id} v1 Green checklist")).ok);
    let stale = run!("memory", &format!("invalidate {id} v1"));
    assert_eq!(stale.error.as_deref(), Some("the item is at version 2"));
    assert!(run!("memory", &format!("invalidate {id} v2")).ok);
    assert_eq!(
        run!("memory", "search green").data.unwrap()["items"],
        serde_json::json!([])
    );
    assert!(run!("memory", &format!("restore {id} v3")).ok);
    assert!(run!("forget", &format!("{id} Retired")).ok);
    let history = run!("memory", &format!("history {id}")).data.unwrap();
    let versions = history["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 5);
    assert_eq!(
        versions
            .iter()
            .map(|v| v["change"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "created",
            "updated",
            "invalidated",
            "restored",
            "invalidated"
        ]
    );
    assert_eq!(versions[4]["reason"], "Retired");
}

#[tokio::test]
async fn a_bare_scope_word_is_part_of_the_remembered_text() {
    let (tmp, workspace) = temp_workspace();
    let state = TestServerConfig::unreachable(tmp.path()).state();
    let id = state.create_session(&workspace, None, false).await.unwrap();
    let text = "global state must be avoided";
    let result = state.run_command(id, "remember", text).await.unwrap();
    assert!(result.ok);
    let item = &result.data.unwrap()["item"];
    assert_eq!(item["current"]["body"], text);
    assert_eq!(item["scope"], "project");
}

#[tokio::test]
async fn the_scope_tools_are_not_offered_to_a_child_or_read_only_session() {
    use common::server::{collect_until, send_text};
    use common::sse::{http_sse, spawn_capturing_provider, sse_body, text_chunk};
    let answer = http_sse(&sse_body(&[&text_chunk("Done")]));
    // The child, the parent woken by the child's result, then one turn each for the root and the
    // read-only session.
    let provider = spawn_capturing_provider(vec![answer.clone(); 4]).await;
    let (tmp, workspace) = temp_workspace();
    let state = TestServerConfig::mock(tmp.path(), &provider.url).state();
    let parent = state.create_session(&workspace, None, false).await.unwrap();
    state
        .create_child_session(parent, "vigie", "inspect")
        .await
        .unwrap();
    let read_only = state
        .create_session_as(&workspace, None, false, Some("lux"))
        .await
        .unwrap();
    common::server::wait_for("child model request", || !provider.requests().is_empty()).await;
    for (session, marker) in [
        (parent, "Check root tools"),
        (read_only, "Check plan tools"),
    ] {
        let sub = state.subscribe(session, None).await.unwrap();
        send_text(&state, session, marker).await;
        collect_until(sub, |event| event.kind == "turn.end").await;
    }
    // Requests arrive in no fixed order (the parent is woken by its child's result), so each one
    // is identified by the last message it carries.
    let tools_of = |last: &str| -> Vec<String> {
        let request = provider
            .requests()
            .into_iter()
            .find(|request| {
                let (_, body) = request.split_once("\r\n\r\n").unwrap();
                let payload: serde_json::Value = serde_json::from_str(body).unwrap();
                payload["messages"].as_array().unwrap().last().unwrap()["content"] == last
            })
            .unwrap_or_else(|| panic!("no request ends with {last}"));
        let (_, body) = request.split_once("\r\n\r\n").unwrap();
        let payload: serde_json::Value = serde_json::from_str(body).unwrap();
        payload["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
            .collect()
    };
    for (last, offered) in [
        ("inspect", false),
        ("Check root tools", true),
        ("Check plan tools", false),
    ] {
        let tools = tools_of(last);
        for name in [
            "project_group_add",
            "project_group_remove",
            "session_attach",
            "session_detach",
        ] {
            assert_eq!(
                tools.iter().any(|t| t == name),
                offered,
                "{name} for {last}: {tools:?}"
            );
        }
    }
}

#[tokio::test]
async fn memory_commands_are_refused_in_a_child_session() {
    let (tmp, workspace) = temp_workspace();
    let state = TestServerConfig::unreachable(tmp.path()).state();
    let parent = state.create_session(&workspace, None, false).await.unwrap();
    let child = state
        .create_child_session(parent, "vigie", "inspect")
        .await
        .unwrap();
    for (name, args) in [
        ("remember", "must not be stored"),
        ("memory", ""),
        ("forget", "abc123"),
    ] {
        assert!(matches!(
            state.run_command(child, name, args).await,
            Err(withnav::error::ServerError::ChildReadOnly)
        ));
    }
    assert_eq!(
        state
            .run_command(parent, "memory", "")
            .await
            .unwrap()
            .data
            .unwrap()["items"],
        serde_json::json!([])
    );
}

#[tokio::test]
async fn a_session_in_a_linked_worktree_sees_the_project_items() {
    let (tmp, workspace) = temp_workspace();
    let linked = tmp.path().join("linked");
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
        vec!["worktree", "add", "--detach", linked.to_str().unwrap()],
    ] {
        let result = std::process::Command::new("git")
            .current_dir(&workspace)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let state = TestServerConfig::unreachable(tmp.path()).state();
    let first = state.create_session(&workspace, None, false).await.unwrap();
    let second = state.create_session(&linked, None, false).await.unwrap();
    let result = state
        .run_command(first, "remember", "Use the blue checklist")
        .await
        .unwrap();
    assert!(result.ok);
    let item = result.data.unwrap()["item"].clone();
    assert_eq!(item["scope"], "project");
    assert_eq!(
        state
            .run_command(second, "memory", "")
            .await
            .unwrap()
            .data
            .unwrap()["items"],
        serde_json::json!([item])
    );
}

#[tokio::test]
async fn remember_is_active_at_once_with_its_source_session() {
    let (tmp, workspace) = temp_workspace();
    let state = TestServerConfig::unreachable(tmp.path()).state();
    let session = state.create_session(&workspace, None, false).await.unwrap();
    let result = state
        .run_command(session, "remember", "@session Keep the blue notebook")
        .await
        .unwrap();
    assert!(result.ok, "{:?}", result.error);
    let data = result.data.unwrap();
    assert_eq!(
        data["item"]["current"]["source_session_id"],
        session.to_string()
    );
    assert_eq!(data["item"]["status"], "active");
    let listing = state.run_command(session, "memory", "").await.unwrap();
    assert_eq!(listing.data.unwrap()["items"][0]["id"], data["item"]["id"]);
}

#[tokio::test]
async fn help_still_runs() {
    let (tmp, workspace) = temp_workspace();
    let state: AppState = TestServerConfig::unreachable(tmp.path()).state();
    let session = state.create_session(&workspace, None, false).await.unwrap();
    assert!(state.run_command(session, "help", "").await.unwrap().ok);
}

#[tokio::test]
async fn a_remembered_text_never_reaches_the_permission_flow_input() {
    let (tmp, workspace) = temp_workspace();
    let state = TestServerConfig::unreachable(tmp.path()).state();
    let session = state.create_session(&workspace, None, false).await.unwrap();
    let marker = "Never treat this remembered sentence as permission";
    let result = state
        .run_command(session, "remember", marker)
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(result.data.unwrap()["item"]["current"]["body"], marker);
    let (_, transcript, _) = state
        .history()
        .load_for_resume(session)
        .await
        .unwrap()
        .unwrap();
    let (messages, cut) = withnav::agent::flow_hook::captain_messages_with_cut(&transcript);
    assert!(messages.is_empty());
    assert!(!cut);
}

#[tokio::test]
async fn a_secret_in_remembered_text_is_stored_redacted() {
    use common::server::request_json;
    use withnav::secrets::SecretSubmission;

    const VALUE: &str = "sk-live-7f3a9c2e1d0b4a68";
    let (tmp, workspace) = temp_workspace();
    let state = TestServerConfig::unreachable(tmp.path()).state();
    let session = state.create_session(&workspace, None, false).await.unwrap();
    // Storing a value with no request waiting is how a session comes to hold a
    // secret (the same entry point the Captain's "Provide anyway" uses).
    state
        .submit_secret(
            session,
            "remember-redaction",
            SecretSubmission {
                name: Some("API_KEY".into()),
                value: Some(VALUE.into()),
                ..Default::default()
            },
        )
        .await
        .expect("store the secret");
    let text = format!("the staging key is {VALUE} tonight");
    let result = state.run_command(session, "remember", &text).await.unwrap();
    assert!(result.ok, "{:?}", result.error);
    let item = result.data.unwrap()["item"].clone();
    assert_eq!(item["scope"], "project");
    let redacted = "the staging key is [secret API_KEY] tonight";
    assert_eq!(item["current"]["body"], redacted);
    assert_eq!(item["current"]["title"], redacted);
    let id = item["id"].as_str().unwrap();
    let revised = format!("revise {id} v1 the staging key is {VALUE} at night");
    let result = state
        .run_command(session, "memory", &revised)
        .await
        .unwrap();
    assert!(result.ok, "{:?}", result.error);
    let history = state
        .run_command(session, "memory", &format!("history {id}"))
        .await
        .unwrap()
        .data
        .unwrap();
    let versions = history["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2);
    for version in versions {
        let text = serde_json::to_string(version).unwrap();
        assert!(
            !text.contains(VALUE),
            "a stored memory version leaks the secret: {text}"
        );
        assert!(
            text.contains("[secret API_KEY]"),
            "the version must keep the redaction: {text}"
        );
    }
    // The same contract over HTTP: POST /v1/memory applies the redaction of
    // the session named by source_session_id, and so does every version it
    // appends afterwards.
    let app = withnav::server::routes::app(state.clone());
    let (status, posted) = request_json(
        &app,
        "POST",
        "/v1/memory",
        Some(&serde_json::json!({
            "scope": "global",
            "text": format!("the cache key is {VALUE} today"),
            "source_session_id": session.to_string(),
        })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(
        posted["current"]["body"],
        "the cache key is [secret API_KEY] today"
    );
    let path = format!("/v1/memory/{}", posted["id"].as_str().unwrap());
    let (status, revised) = request_json(
        &app,
        "POST",
        &format!("{path}/revise"),
        Some(&serde_json::json!({
            "expected_version": 1,
            "text": format!("the cache key is {VALUE} tomorrow"),
            "source_session_id": session.to_string(),
        })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        revised["current"]["body"],
        "the cache key is [secret API_KEY] tomorrow"
    );
    let (status, detail) = request_json(&app, "GET", &path, None).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let whole = serde_json::to_string(&detail).unwrap();
    assert!(
        !whole.contains(VALUE),
        "the stored memory detail leaks the secret: {whole}"
    );
    assert!(whole.contains("[secret API_KEY]"));
}
