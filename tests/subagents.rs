//! Background child session ownership, tool policy and durable notices.

use std::path::{Path, PathBuf};

use navai::bus::event::EventKind;
use navai::server::AppState;
use serde_json::json;

mod common;
use common::server::TestServerConfig;
use common::sse::{http_sse, spawn_gated_provider, sse_body, text_chunk};

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    ws: PathBuf,
    state: AppState,
}

fn fixture() -> Fx {
    fixture_at(common::server::UNREACHABLE_PROVIDER_URL)
}

fn fixture_at(provider_url: &str) -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let state = AppState::new(
        TestServerConfig::mock(&root, provider_url)
            .chat_tools(128_000)
            .config(),
    )
    .unwrap();
    Fx {
        _tmp: tmp,
        root,
        ws,
        state,
    }
}

fn install_agents(root: &Path, ws: &Path) {
    let home = root.join("home");
    let cfg = root.join("cfg/agents");
    let project = ws.join(".navai/agents");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("writer.md"),
        "---\nmode: subagent\naccess: full\n---\nWrite the requested change.",
    )
    .unwrap();
    std::fs::write(
        project.join("filtered.md"),
        "---\nmode: subagent\naccess: full\ntools:\n  read: false\n  grep: false\n---\nNo reads.",
    )
    .unwrap();
}

async fn events(fx: &Fx, id: ulid::Ulid) -> Vec<navai::bus::event::EventEnvelope> {
    fx.state.history().flush().await.unwrap();
    match fx.state.subscribe(id, None).await.unwrap().replay {
        navai::bus::ResumeOutcome::Replay(events) => events,
        other => panic!("expected replay: {other:?}"),
    }
}

#[tokio::test]
async fn child_policy_depth_metadata_and_terminal_notices() {
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = spawn_gated_provider(
        vec![http_sse(&sse_body(&[&text_chunk("child result")]))],
        release.clone(),
    )
    .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("default"), None)
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "inspect sources")
        .await
        .unwrap();
    for _ in 0..100 {
        if !provider.requests().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        !provider.requests().is_empty(),
        "child's first turn reached its provider"
    );
    let summary = fx.state.session_summary(child).await.unwrap();
    assert_eq!(
        summary.parent_session_id.as_deref(),
        Some(parent.to_string().as_str())
    );
    assert_eq!(summary.promptable, false);
    let listed = fx.state.list(&fx.ws.display().to_string()).await.unwrap();
    let child_row = listed.iter().find(|entry| entry.id == child).unwrap();
    assert_eq!(
        child_row.parent_session_id.as_deref(),
        Some(parent.to_string().as_str())
    );
    assert_eq!(child_row.child_status.as_deref(), Some("running"));
    assert!(!child_row.promptable);
    assert_eq!(
        fx.state
            .create_child_session(child, "explore", "nested")
            .await
            .unwrap_err()
            .to_string(),
        "child sessions are read-only to clients"
    );
    assert_eq!(
        fx.state.session_summary(child).await.unwrap().agent,
        "explore"
    );
    fx.state.close_session(child).await.unwrap();
    release.notify_one();
    let link = fx.state.child_metadata(child).await.unwrap().unwrap();
    assert!(matches!(link.status.as_str(), "stopped" | "failed"));
    let stored = fx.state.session_summary(child).await.unwrap();
    assert_eq!(
        stored.parent_session_id.as_deref(),
        Some(parent.to_string().as_str())
    );
    assert!(!stored.promptable);
    let parent_events = events(&fx, parent).await;
    assert!(parent_events
        .iter()
        .any(|e| e.kind == EventKind::SubagentSpawned.as_str()));
    assert!(parent_events
        .iter()
        .any(|e| e.kind == EventKind::SubagentDone.as_str()));
}

#[tokio::test]
async fn filtered_agent_declared_false_tool_is_removed() {
    let fx = fixture();
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("default"), None)
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "filtered", "inspect")
        .await
        .unwrap();
    assert_eq!(
        fx.state.session_summary(child).await.unwrap().agent,
        "filtered"
    );
    fx.state.close_session(child).await.unwrap();
}

#[tokio::test]
async fn capacity_error_has_clear_machine_and_model_message() {
    let fx = fixture();
    install_agents(&fx.root, &fx.ws);
    std::fs::write(
        fx.root.join("cfg/config.json"),
        json!({"subagents":{"max_concurrent":1}}).to_string(),
    )
    .unwrap();
    let parent = fx
        .state
        .create_session(&fx.ws, Some("default"), None)
        .await
        .unwrap();
    let _child = fx
        .state
        .create_child_session(parent, "explore", "first")
        .await
        .unwrap();
    let err = fx
        .state
        .create_child_session(parent, "explore", "second")
        .await
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("subagent capacity reached: 1 children are running"));
}

#[tokio::test]
async fn child_stop_http_is_allowed_but_chat_command_and_resume_are_forbidden() {
    let fx = fixture();
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("default"), None)
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "read")
        .await
        .unwrap();
    let app = navai::server::app(fx.state.clone());
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let message = app.clone().oneshot(axum::http::Request::post(format!("/v1/sessions/{child}/messages")).header("content-type", "application/json").body(Body::from(json!({"target_actor_id":"agent:main","content":[{"type":"text","text":"hello"}]}).to_string())).unwrap()).await.unwrap();
    assert_eq!(message.status(), 403);
    assert_eq!(
        message
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        br#"{"error":"child sessions are read-only to clients","reason":"child_read_only"}"#
    );
    let command = app
        .clone()
        .oneshot(
            axum::http::Request::post(format!("/v1/sessions/{child}/commands"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"name":"help"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(command.status(), 403);
    let resume = app
        .clone()
        .oneshot(
            axum::http::Request::post("/v1/sessions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"workspace":fx.ws.display().to_string(),"resume":child.to_string()})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resume.status(), 403);
    let stop = app
        .oneshot(
            axum::http::Request::post(format!("/v1/sessions/{child}/stop"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stop.status(), 200);
    assert!(matches!(
        fx.state
            .child_metadata(child)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_str(),
        "stopped" | "failed"
    ));
}
