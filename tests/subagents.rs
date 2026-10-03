//! Background child session ownership, tool policy and durable notices.

use std::path::{Path, PathBuf};

use navai::bus::event::EventKind;
use navai::server::AppState;
use navai::tools::Tool;
use serde_json::{json, Value};

mod common;
use common::server::TestServerConfig;
use common::sse::{
    http_sse, spawn_capturing_provider, spawn_gated_provider, spawn_holding_provider, sse_body,
    text_chunk, tool_call_chunk,
};

#[tokio::test]
async fn stopping_parent_during_task_preparation_does_not_wait_or_leak() {
    paused_child_admission(false).await;
}

#[tokio::test]
async fn stop_cancels_parent_before_waiting_for_child_publication() {
    let fx = fixture();
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("default"), None)
        .await
        .unwrap();
    let barrier = navai::session::ChildPreparationBarrier::default();
    fx.state.pause_child_publication(barrier.clone());
    let service = fx.state.service();
    let spawn = tokio::spawn(async move {
        service
            .create_child_session(parent, "explore", "inspect")
            .await
    });
    barrier.reached.notified().await;
    let service = fx.state.service();
    let stop = tokio::spawn(async move { service.close_session(parent).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !fx.state.session_is_cancelled(parent) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!stop.is_finished());
    barrier.release.notify_one();
    assert!(spawn.await.unwrap().is_err());
    tokio::time::timeout(std::time::Duration::from_secs(2), stop)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(fx.state.reserved_child_count(parent), 0);
    assert_eq!(
        fx.state
            .list(&fx.ws.display().to_string())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn committed_child_is_terminalized_when_publication_or_start_fails() {
    for publication in [true, false] {
        let fx = fixture();
        install_agents(&fx.root, &fx.ws);
        let parent = fx
            .state
            .create_session(&fx.ws, Some("default"), None)
            .await
            .unwrap();
        fx.state.fail_child_start(publication);
        assert!(fx
            .state
            .create_child_session(parent, "explore", "inspect")
            .await
            .is_err());
        assert_eq!(fx.state.reserved_child_count(parent), 0);
        assert_eq!(
            fx.state.history().active_child_count(parent).await.unwrap(),
            0
        );
        let replay = events(&fx, parent).await;
        let spawn = replay
            .iter()
            .find(|e| e.kind == "subagent.spawned")
            .unwrap();
        let child = spawn.payload["child_session_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let link = fx.state.child_metadata(child).await.unwrap().unwrap();
        assert_eq!(link.status, "failed");
        assert!(link.terminal_reason.unwrap().contains("injected child"));
        assert!(matches!(
            fx.state
                .send(navai::bus::command::Command::child_prompt(
                    child,
                    "retry".into()
                ))
                .await,
            Err(navai::error::ServerError::ResumeRequired)
        ));
        assert_eq!(
            replay.iter().filter(|e| e.kind == "subagent.done").count(),
            1
        );
        fx.state.close_all().await;
    }
}

#[tokio::test]
async fn child_completion_after_parent_close_is_pending_until_resume() {
    let mut provider = common::sse::spawn_streaming_provider().await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("default"), None)
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "inspect")
        .await
        .unwrap();
    let (_, stream) = provider.requests.recv().await.unwrap();
    fx.state.close_session(parent).await.unwrap();
    stream.send(text_chunk("finished offline")).unwrap();
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fx.state.reserved_child_count(parent) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fx.state
            .child_metadata(child)
            .await
            .unwrap()
            .unwrap()
            .status,
        "done"
    );
    assert_eq!(
        fx.state
            .history()
            .pending_completions(parent)
            .await
            .unwrap()
            .len(),
        1
    );
    fx.state
        .create_or_resume_session(&fx.ws, None, None, Some(parent))
        .await
        .unwrap();
    assert!(fx
        .state
        .history()
        .pending_completions(parent)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        events(&fx, parent)
            .await
            .iter()
            .filter(|e| e.kind == "subagent.done")
            .count(),
        1
    );
    fx.state.close_session(parent).await.unwrap();
    fx.state
        .create_or_resume_session(&fx.ws, None, None, Some(parent))
        .await
        .unwrap();
    assert_eq!(
        events(&fx, parent)
            .await
            .iter()
            .filter(|e| e.kind == "subagent.done")
            .count(),
        1
    );
    fx.state.close_all().await;
}

#[tokio::test]
async fn completion_between_resume_snapshot_and_registration_has_no_sequence_collision() {
    let mut provider = common::sse::spawn_streaming_provider().await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("default"), None)
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "inspect")
        .await
        .unwrap();
    let (_, stream) = provider.requests.recv().await.unwrap();
    fx.state.close_session(parent).await.unwrap();
    let before = fx
        .state
        .history()
        .meta(parent)
        .await
        .unwrap()
        .unwrap()
        .last_seq;
    let barrier = navai::session::ChildPreparationBarrier::default();
    fx.state.pause_resume_registration(barrier.clone());
    let service = fx.state.service();
    let ws = fx.ws.clone();
    let resume = tokio::spawn(async move {
        service
            .create_or_resume_session(&ws, None, None, Some(parent))
            .await
    });
    barrier.reached.notified().await;
    stream.send(text_chunk("finished during resume")).unwrap();
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fx.state.reserved_child_count(parent) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fx.state
            .child_metadata(child)
            .await
            .unwrap()
            .unwrap()
            .status,
        "done"
    );
    assert_eq!(
        fx.state
            .history()
            .meta(parent)
            .await
            .unwrap()
            .unwrap()
            .last_seq,
        before
    );
    assert_eq!(
        fx.state
            .history()
            .pending_completions(parent)
            .await
            .unwrap()
            .len(),
        1
    );
    barrier.release.notify_one();
    resume.await.unwrap().unwrap();
    let replay = events(&fx, parent).await;
    let notices = replay
        .iter()
        .filter(|e| e.kind == "subagent.done")
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].seq, before + 1);
    let mut seqs = replay.iter().map(|e| e.seq).collect::<Vec<_>>();
    let count = seqs.len();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.len(), count);
    fx.state.close_all().await;
}

#[tokio::test]
async fn resumed_parent_rejects_spawn_from_previous_incarnation() {
    paused_child_admission(true).await;
}

async fn paused_child_admission(resume: bool) {
    let provider = spawn_capturing_provider(vec![http_sse(&sse_body(&[&tool_call_chunk(
        "paused-task",
        "task",
        &json!({"agent":"explore","prompt":"inspect"}),
    )]))])
    .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    add_cognition_model(&fx.root);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("cognition"), None)
        .await
        .unwrap();
    let barrier = navai::session::ChildPreparationBarrier::default();
    fx.state.pause_child_preparation(barrier.clone());
    fx.state
        .send(navai::bus::command::Command::child_prompt(
            parent,
            "spawn a child".into(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        barrier.reached.notified(),
    )
    .await
    .unwrap();
    assert_eq!(fx.state.reserved_child_count(parent), 1);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        fx.state.close_session(parent),
    )
    .await
    .unwrap()
    .unwrap();
    if resume {
        fx.state
            .create_or_resume_session(&fx.ws, None, None, Some(parent))
            .await
            .unwrap();
    }
    barrier.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fx.state.reserved_child_count(parent) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fx.state.history().active_child_count(parent).await.unwrap(),
        0
    );
    let listed = fx.state.list(&fx.ws.display().to_string()).await.unwrap();
    assert_eq!(listed.len(), 1, "no prepared child row was stored");
    assert_eq!(listed[0].id, parent);
    assert!(!events(&fx, parent)
        .await
        .iter()
        .any(|event| event.kind == "subagent.spawned"));
    fx.state.close_all().await;
}

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

fn add_cognition_model(root: &Path) {
    let path = root.join("models.json");
    let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    models["models"]["cognition"] = models["models"]["default"].clone();
    std::fs::write(path, models.to_string()).unwrap();
}

async fn events(fx: &Fx, id: ulid::Ulid) -> Vec<navai::bus::event::EventEnvelope> {
    fx.state.history().flush().await.unwrap();
    match fx.state.subscribe(id, None).await.unwrap().replay {
        navai::bus::ResumeOutcome::Replay(events) => events,
        other => panic!("expected replay: {other:?}"),
    }
}

#[tokio::test]
async fn permission_parked_child_keeps_capacity_until_stopped() {
    let provider = spawn_capturing_provider(vec![http_sse(&sse_body(&[&tool_call_chunk(
        "permission",
        "bash",
        &json!({"command":"true"}),
    )]))])
    .await;
    let fx = fixture_at(&provider.url);
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
    let child = fx
        .state
        .create_child_session(parent, "writer", "write")
        .await
        .unwrap();
    for _ in 0..200 {
        if events(&fx, child)
            .await
            .iter()
            .any(|e| e.kind == "permission.requested")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(events(&fx, child)
        .await
        .iter()
        .any(|e| e.kind == "permission.requested"));
    assert_eq!(
        fx.state
            .child_metadata(child)
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    assert!(!events(&fx, parent)
        .await
        .iter()
        .any(|e| e.kind == "subagent.done"));
    assert!(matches!(
        fx.state
            .create_child_session(parent, "writer", "another")
            .await,
        Err(navai::error::ServerError::SubagentCapacity { .. })
    ));
    fx.state.close_session(child).await.unwrap();
    assert_eq!(
        fx.state
            .child_metadata(child)
            .await
            .unwrap()
            .unwrap()
            .status,
        "stopped"
    );
}

#[tokio::test]
async fn forbidden_child_calls_are_denied_before_permissions() {
    let provider = spawn_capturing_provider(vec![
        http_sse(&sse_body(&[&tool_call_chunk(
            "write-denied",
            "write",
            &json!({"path":"forbidden.txt","content":"bad"}),
        )])),
        http_sse(&sse_body(&[&tool_call_chunk(
            "ask-denied",
            "ask",
            &json!({"question":"approve?"}),
        )])),
        http_sse(&sse_body(&[&text_chunk("finished")])),
    ])
    .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    std::fs::write(
        fx.ws.join(".navai/agents/build.md"),
        "---\nmode: primary\ntools:\n  write: false\n---\nBuild.",
    )
    .unwrap();
    let parent = fx
        .state
        .create_session(&fx.ws, Some("default"), None)
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "writer", "try forbidden tools")
        .await
        .unwrap();
    for _ in 0..200 {
        if fx
            .state
            .child_metadata(child)
            .await
            .unwrap()
            .unwrap()
            .status
            != "running"
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(!fx.ws.join("forbidden.txt").exists());
    let replay = events(&fx, child).await;
    assert!(!replay
        .iter()
        .any(|e| e.kind == "permission.requested" || e.kind == "agent.yield"));
    let posts: Vec<_> = replay
        .iter()
        .filter(|e| e.kind == "tool.post_call")
        .collect();
    assert_eq!(posts.len(), 2, "{replay:#?}");
    for post in posts {
        assert_eq!(post.payload["status"], "error");
    }
    assert_eq!(
        fx.state
            .child_metadata(child)
            .await
            .unwrap()
            .unwrap()
            .status,
        "done"
    );
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
    assert!(!summary.promptable);
    let child_events = events(&fx, child).await;
    assert!(child_events
        .iter()
        .any(|e| e.kind == EventKind::SubagentSpawned.as_str()));
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
    assert_eq!(link.status, "stopped");
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
async fn child_depth_has_no_task_registry_entry() {
    let fx = fixture();
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("default"), None)
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "writer", "do work")
        .await
        .unwrap();
    assert!(matches!(
        fx.state
            .create_child_session(child, "writer", "nested")
            .await,
        Err(navai::error::ServerError::ChildReadOnly)
    ));
    let metadata = fx.state.child_metadata(child).await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&metadata.policy_snapshot).unwrap()["task"],
        false
    );
}

#[tokio::test]
async fn parent_sse_replay_contains_spawned_and_done_after_completion() {
    let provider =
        spawn_capturing_provider(vec![http_sse(&sse_body(&[&text_chunk("child result")]))]).await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("default"), None)
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "review")
        .await
        .unwrap();
    for _ in 0..100 {
        if fx
            .state
            .child_metadata(child)
            .await
            .unwrap()
            .is_some_and(|link| link.status != "running")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let replay = events(&fx, parent).await;
    let kinds: Vec<&str> = replay.iter().map(|event| event.kind.as_str()).collect();
    assert!(kinds.contains(&EventKind::SubagentSpawned.as_str()));
    assert!(kinds.contains(&EventKind::SubagentDone.as_str()));
}

#[tokio::test]
async fn child_slot_is_released_after_terminal_result() {
    let response = http_sse(&sse_body(&[&text_chunk("completed")]));
    let provider = common::sse::spawn_capturing_provider(vec![response]).await;
    let fx = fixture_at(&provider.url);
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
    let child = fx
        .state
        .create_child_session(parent, "explore", "finish")
        .await
        .unwrap();
    for _ in 0..100 {
        if fx
            .state
            .child_metadata(child)
            .await
            .unwrap()
            .is_some_and(|link| link.status == "done" || link.status == "failed")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let link = fx.state.child_metadata(child).await.unwrap().unwrap();
    assert!(matches!(link.status.as_str(), "done" | "failed"));
    let next = fx
        .state
        .create_child_session(parent, "explore", "second task")
        .await
        .unwrap();
    fx.state.close_session(next).await.unwrap();
}

#[tokio::test]
async fn concurrent_children_reuse_exactly_two_completed_capacity_slots() {
    let mut provider = common::sse::spawn_streaming_provider().await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    std::fs::write(
        fx.root.join("cfg/config.json"),
        json!({"subagents":{"max_concurrent":4}}).to_string(),
    )
    .unwrap();
    let parent = fx
        .state
        .create_session(&fx.ws, Some("default"), None)
        .await
        .unwrap();
    let children = futures::future::join_all(
        (0..4).map(|_| fx.state.create_child_session(parent, "explore", "inspect")),
    )
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert_eq!(children.len(), 4);
    let mut streams = Vec::new();
    for _ in 0..4 {
        let (_, stream) =
            tokio::time::timeout(std::time::Duration::from_secs(2), provider.requests.recv())
                .await
                .unwrap()
                .unwrap();
        streams.push(stream);
    }
    assert!(matches!(
        fx.state
            .create_child_session(parent, "explore", "fifth")
            .await,
        Err(navai::error::ServerError::SubagentCapacity { capacity: 4 })
    ));
    let finished = streams.drain(..2).collect::<Vec<_>>();
    futures::future::join_all(finished.into_iter().map(|stream| async move {
        stream.send(text_chunk("complete")).unwrap();
        drop(stream);
    }))
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if fx.state.history().active_child_count(parent).await.unwrap() == 2
                && fx.state.reserved_child_count(parent) == 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let replacements = futures::future::join_all((0..2).map(|_| {
        fx.state
            .create_child_session(parent, "explore", "replacement")
    }))
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert_eq!(replacements.len(), 2);
    assert_eq!(
        fx.state.history().active_child_count(parent).await.unwrap(),
        4
    );
    assert_eq!(fx.state.reserved_child_count(parent), 4);
    assert!(matches!(
        fx.state
            .create_child_session(parent, "explore", "fifth again")
            .await,
        Err(navai::error::ServerError::SubagentCapacity { capacity: 4 })
    ));
    fx.state.close_all().await;
}

#[tokio::test]
async fn child_prompt_is_not_attributed_as_driver_task_or_transcript_inherited() {
    let fx = fixture();
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("default"), None)
        .await
        .unwrap();
    fx.state
        .send(navai::bus::command::Command::child_prompt(
            parent,
            "parent history".into(),
        ))
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "only child prompt")
        .await
        .unwrap();
    let loaded = fx.state.history().load(child).await.unwrap().unwrap();
    assert!(loaded.transcript.iter().all(|message| {
        !serde_json::to_string(message)
            .unwrap()
            .contains("parent history")
    }));
    let child_events = events(&fx, child).await;
    let received = child_events
        .iter()
        .find(|event| event.kind == "message.received")
        .unwrap();
    assert_eq!(received.payload["source"], "user:subagent");
    assert_eq!(received.payload["content"], "only child prompt");
}

#[tokio::test]
async fn task_ack_returns_before_child_turn_finishes() {
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = spawn_holding_provider(
        3,
        0,
        std::sync::Arc::new(tokio::sync::Notify::new()),
        release.clone(),
        |i, _| {
            if i == 0 {
                http_sse(&sse_body(&[&tool_call_chunk(
                    "spawn-1",
                    "task",
                    &json!({"agent":"explore","prompt":"inspect"}),
                )]))
            } else {
                http_sse(&sse_body(&[&text_chunk(if i == 1 {
                    "child result"
                } else {
                    "parent continued"
                })]))
            }
        },
    )
    .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    add_cognition_model(&fx.root);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("cognition"), None)
        .await
        .unwrap();
    let task = navai::tools::task::TaskTool::new(fx.state.service(), parent);
    let cx = navai::tools::ToolContext {
        workspace: fx.ws.clone(),
        writable_roots: vec![fx.ws.clone()],
        readable_roots: vec![],
    };
    let result = task
        .call(
            json!({"agent":"explore","prompt":"inspect"}),
            tokio_util::sync::CancellationToken::new(),
            &cx,
        )
        .await
        .unwrap();
    assert!(result.output.contains("\"status\":\"running\""));
    for _ in 0..100 {
        if events(&fx, parent)
            .await
            .iter()
            .any(|e| e.kind == EventKind::SubagentSpawned.as_str())
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let parent_events = events(&fx, parent).await;
    let spawned = parent_events
        .iter()
        .find(|e| e.kind == EventKind::SubagentSpawned.as_str())
        .unwrap_or_else(|| panic!("no spawn event: {parent_events:#?}"));
    let child_id: ulid::Ulid = spawned.payload["child_session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let meta = fx.state.child_metadata(child_id).await.unwrap().unwrap();
    assert_eq!(meta.status, "running");
    assert_eq!(meta.goal, "inspect");
    assert!(!fx.state.session_summary(child_id).await.unwrap().promptable);
    release.notify_one();
    for _ in 0..100 {
        if fx
            .state
            .child_metadata(child_id)
            .await
            .unwrap()
            .unwrap()
            .status
            != "running"
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let done = fx.state.child_metadata(child_id).await.unwrap().unwrap();
    assert!(matches!(done.status.as_str(), "done" | "failed"));
}

#[tokio::test]
async fn full_access_child_of_plan_parent_has_no_write_dispatch() {
    let provider =
        spawn_capturing_provider(vec![http_sse(&sse_body(&[&text_chunk("unused")]))]).await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session_as(&fx.ws, Some("default"), None, Some("plan"))
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "writer", "do an edit")
        .await
        .unwrap();
    let link = fx.state.child_metadata(child).await.unwrap().unwrap();
    let policy: Value = serde_json::from_str(&link.policy_snapshot).unwrap();
    for tool in ["write", "edit", "bash"] {
        assert_eq!(policy[tool], false, "{tool}");
    }
    assert_eq!(
        fx.state.session_summary(child).await.unwrap().agent,
        "writer"
    );
}

#[tokio::test]
async fn declared_read_only_child_tool_denial_is_reflected_in_saved_policy() {
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
    let policy: Value = serde_json::from_str(
        &fx.state
            .child_metadata(child)
            .await
            .unwrap()
            .unwrap()
            .policy_snapshot,
    )
    .unwrap();
    assert_eq!(policy["read"], false);
    assert_eq!(policy["grep"], false);
    fx.state.close_session(child).await.unwrap();
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
    let direct = fx.state.run_command(child, "help", "").await.unwrap_err();
    assert!(matches!(direct, navai::error::ServerError::ChildReadOnly));
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
