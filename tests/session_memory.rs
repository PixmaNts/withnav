use withnav::server::AppState;

mod common;
use common::server::{TestServerConfig, temp_workspace};

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
        .run_command(session, "remember", "session Keep the blue notebook")
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
