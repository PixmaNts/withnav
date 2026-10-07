//! Background child session ownership, tool policy and durable notices.

use std::path::{Path, PathBuf};

use navai::bus::event::EventKind;
use navai::server::AppState;
use navai::tools::Tool;
use serde_json::{json, Value};

mod common;
use common::deadline::Deadline;
#[tokio::test]
async fn child_delete_waits_for_registration_before_removing_owner() {
    child_lifecycle_during_publication(true, false)
        .bounded(file!(), line!())
        .await;
}

#[tokio::test]
async fn child_stop_waits_for_registration_and_runtime_exit() {
    child_lifecycle_during_publication(false, false)
        .bounded(file!(), line!())
        .await;
}

#[tokio::test]
async fn child_delete_waits_for_failed_start_cleanup() {
    child_lifecycle_during_publication(true, true)
        .bounded(file!(), line!())
        .await;
}

async fn child_lifecycle_during_publication(delete: bool, cleanup: bool) {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let barrier = navai::session::ChildPreparationBarrier::default();
    if cleanup {
        fx.state.fail_child_start(false);
        fx.state.pause_child_cleanup(barrier.clone());
    } else {
        fx.state.pause_child_registration(barrier.clone());
    }
    let service = fx.state.service();
    let spawn = tokio::spawn(async move {
        service
            .create_child_session(parent, "explore", "inspect")
            .bounded(file!(), line!())
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        barrier.reached.notified(),
    )
    .bounded(file!(), line!())
    .await
    .unwrap();
    let replay = events(&fx, parent).bounded(file!(), line!()).await;
    let child = replay
        .iter()
        .find(|e| e.kind == "subagent.spawned")
        .unwrap()
        .payload["child_session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(fx
        .state
        .history()
        .exists(child)
        .bounded(file!(), line!())
        .await
        .unwrap());
    assert_eq!(fx.state.session_has_runtime(child), cleanup);
    let service = fx.state.service();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let lifecycle = tokio::spawn(async move {
        entered_tx.send(()).unwrap();
        if delete {
            service.delete(child).bounded(file!(), line!()).await
        } else {
            service.close_session(child).bounded(file!(), line!()).await
        }
    });
    entered_rx.bounded(file!(), line!()).await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), async {
            while !lifecycle.is_finished() {
                tokio::task::yield_now().bounded(file!(), line!()).await;
            }
        })
        .bounded(file!(), line!())
        .await
        .is_err(),
        "child lifecycle must wait for its owned operation"
    );
    assert!(fx
        .state
        .history()
        .exists(child)
        .bounded(file!(), line!())
        .await
        .unwrap());
    barrier.release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), spawn)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.is_err(), cleanup);
    tokio::time::timeout(std::time::Duration::from_secs(2), lifecycle)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!fx.state.session_has_runtime(child));
    assert_eq!(fx.state.reserved_child_count(parent), 0);
    if delete {
        assert!(!fx
            .state
            .history()
            .exists(child)
            .bounded(file!(), line!())
            .await
            .unwrap());
        assert!(fx
            .state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .is_none());
    } else {
        assert_eq!(
            fx.state
                .child_metadata(child)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .unwrap()
                .status,
            "stopped"
        );
    }
    let notices = events(&fx, parent)
        .bounded(file!(), line!())
        .await
        .into_iter()
        .filter(|e| e.kind == "subagent.done" && e.payload["child_session_id"] == child.to_string())
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].payload["outcome"],
        if cleanup { "failed" } else { "stopped" }
    );
    assert!(fx
        .state
        .history()
        .pending_completions(parent)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .is_empty());
    fx.state.close_all().bounded(file!(), line!()).await;
    provider.requests.close();
}
use common::server::TestServerConfig;
use common::sse::{
    http_sse, spawn_capturing_provider, spawn_gated_provider, spawn_holding_provider, sse_body,
    text_chunk, tool_call_chunk,
};

#[tokio::test]
async fn stopping_parent_during_task_preparation_does_not_wait_or_leak() {
    paused_child_admission(false)
        .bounded(file!(), line!())
        .await;
}

#[tokio::test]
async fn stop_cancels_parent_before_waiting_for_child_publication() {
    let fx = fixture();
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let barrier = navai::session::ChildPreparationBarrier::default();
    fx.state.pause_child_publication(barrier.clone());
    let service = fx.state.service();
    let spawn = tokio::spawn(async move {
        service
            .create_child_session(parent, "explore", "inspect")
            .bounded(file!(), line!())
            .await
    });
    barrier.reached.notified().bounded(file!(), line!()).await;
    let service = fx.state.service();
    let stop = tokio::spawn(async move {
        service
            .close_session(parent)
            .bounded(file!(), line!())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !fx.state.session_is_cancelled(parent) {
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    assert!(!stop.is_finished());
    barrier.release.notify_one();
    assert!(spawn.bounded(file!(), line!()).await.unwrap().is_err());
    tokio::time::timeout(std::time::Duration::from_secs(2), stop)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(fx.state.reserved_child_count(parent), 0);
    assert_eq!(
        fx.state
            .list(&fx.ws.display().to_string())
            .bounded(file!(), line!())
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
            .create_session(&fx.ws, Some("mock/mock-model"), false)
            .bounded(file!(), line!())
            .await
            .unwrap();
        fx.state.fail_child_start(publication);
        assert!(fx
            .state
            .create_child_session(parent, "explore", "inspect")
            .bounded(file!(), line!())
            .await
            .is_err());
        assert_eq!(fx.state.reserved_child_count(parent), 0);
        assert_eq!(
            fx.state
                .history()
                .active_child_count(parent)
                .bounded(file!(), line!())
                .await
                .unwrap(),
            0
        );
        let replay = events(&fx, parent).bounded(file!(), line!()).await;
        let spawn = replay
            .iter()
            .find(|e| e.kind == "subagent.spawned")
            .unwrap();
        let child = spawn.payload["child_session_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let link = fx
            .state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(link.status, "failed");
        assert!(link.terminal_reason.unwrap().contains("injected child"));
        assert!(matches!(
            fx.state
                .send(common::server::driver_command(child, "retry".into()))
                .bounded(file!(), line!())
                .await,
            Err(navai::error::ServerError::ResumeRequired)
        ));
        assert_eq!(
            replay.iter().filter(|e| e.kind == "subagent.done").count(),
            1
        );
        fx.state.close_all().bounded(file!(), line!()).await;
    }
}

#[tokio::test]
async fn inherited_child_keeps_parent_headers_and_output_limit_after_alias_edit() {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    let path = fx.root.join("models.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["providers"]["mock"]["headers"] = json!({"x-inherited":"parent"});
    config["models"]["mock/mock-model"]["max_output_tokens"] = json!(4321);
    std::fs::write(&path, config.to_string()).unwrap();
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    fx.state
        .send(common::server::driver_command(
            parent,
            "parent request".into(),
        ))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (parent_body, parent_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    config["providers"]["mock"]["headers"] = json!({"x-inherited":"changed"});
    config["providers"]["mock"]["base_url"] = json!(common::server::UNREACHABLE_PROVIDER_URL);
    config["models"]["mock/mock-model"]["max_output_tokens"] = json!(8765);
    std::fs::write(&path, config.to_string()).unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "child request")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (child_body, child_stream) =
        tokio::time::timeout(std::time::Duration::from_secs(2), provider.requests.recv())
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap();
    assert!(parent_body.to_lowercase().contains("x-inherited: parent"));
    assert!(child_body.to_lowercase().contains("x-inherited: parent"));
    for request in [parent_body, child_body] {
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["max_tokens"], 4321);
    }
    fx.state
        .close_session(child)
        .bounded(file!(), line!())
        .await
        .unwrap();
    fx.state.close_all().bounded(file!(), line!()).await;
    drop((parent_stream, child_stream));
}

#[tokio::test]
async fn cascaded_child_stop_is_pending_until_parent_resume() {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "inspect")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    fx.state
        .close_session(parent)
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream.send(text_chunk("finished offline")).unwrap();
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fx.state.reserved_child_count(parent) != 0 {
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    assert_eq!(
        fx.state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "stopped"
    );
    assert_eq!(
        fx.state
            .history()
            .pending_completions(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .len(),
        1
    );
    fx.state
        .create_or_resume_session(&fx.ws, None, false, Some(parent))
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(fx
        .state
        .history()
        .pending_completions(parent)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        events(&fx, parent)
            .bounded(file!(), line!())
            .await
            .iter()
            .filter(|e| e.kind == "subagent.done")
            .count(),
        1
    );
    fx.state
        .close_session(parent)
        .bounded(file!(), line!())
        .await
        .unwrap();
    fx.state
        .create_or_resume_session(&fx.ws, None, false, Some(parent))
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(
        events(&fx, parent)
            .bounded(file!(), line!())
            .await
            .iter()
            .filter(|e| e.kind == "subagent.done")
            .count(),
        1
    );
    fx.state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn pending_completion_at_resume_snapshot_has_no_sequence_collision() {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "inspect")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    fx.state
        .close_session(parent)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let before = fx
        .state
        .history()
        .meta(parent)
        .bounded(file!(), line!())
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
            .create_or_resume_session(&ws, None, false, Some(parent))
            .bounded(file!(), line!())
            .await
    });
    barrier.reached.notified().bounded(file!(), line!()).await;
    stream.send(text_chunk("finished during resume")).unwrap();
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fx.state.reserved_child_count(parent) != 0 {
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    assert_eq!(
        fx.state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "stopped"
    );
    assert_eq!(
        fx.state
            .history()
            .meta(parent)
            .bounded(file!(), line!())
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
            .bounded(file!(), line!())
            .await
            .unwrap()
            .len(),
        1
    );
    barrier.release.notify_one();
    resume.bounded(file!(), line!()).await.unwrap().unwrap();
    let replay = events(&fx, parent).bounded(file!(), line!()).await;
    let notices = replay
        .iter()
        .filter(|e| e.kind == "subagent.done")
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1);
    // The resume's own `session.bypass` (off) comes first, then the notice.
    assert_eq!(notices[0].seq, before + 2);
    let mut seqs = replay.iter().map(|e| e.seq).collect::<Vec<_>>();
    let count = seqs.len();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.len(), count);
    fx.state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn resumed_parent_rejects_spawn_from_previous_incarnation() {
    paused_child_admission(true).bounded(file!(), line!()).await;
}

async fn paused_child_admission(resume: bool) {
    let provider = spawn_capturing_provider(vec![http_sse(&sse_body(&[&tool_call_chunk(
        "paused-task",
        "task",
        &json!({"agent":"explore","prompt":"inspect"}),
    )]))])
    .bounded(file!(), line!())
    .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    add_cognition_model(&fx.root);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let barrier = navai::session::ChildPreparationBarrier::default();
    fx.state.pause_child_preparation(barrier.clone());
    fx.state
        .send(common::server::driver_command(
            parent,
            "spawn a child".into(),
        ))
        .bounded(file!(), line!())
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        barrier.reached.notified(),
    )
    .bounded(file!(), line!())
    .await
    .unwrap();
    assert_eq!(fx.state.reserved_child_count(parent), 1);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        fx.state.close_session(parent),
    )
    .bounded(file!(), line!())
    .await
    .unwrap()
    .unwrap();
    if resume {
        fx.state
            .create_or_resume_session(&fx.ws, None, false, Some(parent))
            .bounded(file!(), line!())
            .await
            .unwrap();
    }
    barrier.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fx.state.reserved_child_count(parent) != 0 {
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    assert_eq!(
        fx.state
            .history()
            .active_child_count(parent)
            .bounded(file!(), line!())
            .await
            .unwrap(),
        0
    );
    let listed = fx
        .state
        .list(&fx.ws.display().to_string())
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(listed.len(), 1, "no prepared child row was stored");
    assert_eq!(listed[0].id, parent);
    assert!(!events(&fx, parent)
        .bounded(file!(), line!())
        .await
        .iter()
        .any(|event| event.kind == "subagent.spawned"));
    fx.state.close_all().bounded(file!(), line!()).await;
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
    let models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(models["models"]["mock/mock-model"].is_object());
    std::fs::write(path, models.to_string()).unwrap();
}

#[tokio::test]
async fn writable_child_inherits_disabled_or_invalid_review_despite_settings_removal() {
    for value in [json!(false), json!("invalid")] {
        let provider = spawn_capturing_provider(vec![http_sse(&sse_body(&[&tool_call_chunk(
            "permission",
            "bash",
            &json!({"command":"true"}),
        )]))])
        .await;
        let fx = fixture_at(&provider.url);
        install_agents(&fx.root, &fx.ws);
        let settings = fx.root.join("cfg/config.json");
        std::fs::write(
            &settings,
            json!({"agents":{"build":{"model":"mock/mock-model"}},"permissions":{"automatic_review":value}}).to_string(),
        )
        .unwrap();
        let parent = fx
            .state
            .create_session(&fx.ws, Some("mock/mock-model"), false)
            .await
            .unwrap();
        std::fs::remove_file(settings).unwrap();
        let child = fx
            .state
            .create_child_session(parent, "writer", "the driver approved this")
            .await
            .unwrap();
        let mut requested = None;
        for _ in 0..200 {
            requested = events(&fx, child)
                .await
                .into_iter()
                .find(|event| event.kind == "permission.requested");
            if requested.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(requested.unwrap().payload["reason"]
            .as_str()
            .unwrap()
            .contains("disabled"));
        assert_eq!(
            fx.state
                .history()
                .latest_driver_message(child)
                .await
                .unwrap(),
            None
        );
        fx.state.close_session(parent).await.unwrap();
    }
}

async fn events(fx: &Fx, id: ulid::Ulid) -> Vec<navai::bus::event::EventEnvelope> {
    fx.state
        .history()
        .flush()
        .bounded(file!(), line!())
        .await
        .unwrap();
    match fx
        .state
        .subscribe(id, None)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .replay
    {
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
    .bounded(file!(), line!())
    .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    std::fs::write(
        fx.root.join("cfg/config.json"),
        json!({"agents":{"build":{"model":"mock/mock-model"}},"subagents":{"max_concurrent":1}})
            .to_string(),
    )
    .unwrap();
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "writer", "write")
        .bounded(file!(), line!())
        .await
        .unwrap();
    for _ in 0..200 {
        if events(&fx, child)
            .bounded(file!(), line!())
            .await
            .iter()
            .any(|e| e.kind == "permission.requested")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5))
            .bounded(file!(), line!())
            .await;
    }
    assert!(events(&fx, child)
        .bounded(file!(), line!())
        .await
        .iter()
        .any(|e| e.kind == "permission.requested"));
    assert_eq!(
        fx.state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    assert!(!events(&fx, parent)
        .bounded(file!(), line!())
        .await
        .iter()
        .any(|e| e.kind == "subagent.done"));
    assert!(matches!(
        fx.state
            .create_child_session(parent, "writer", "another")
            .bounded(file!(), line!())
            .await,
        Err(navai::error::ServerError::SubagentCapacity { .. })
    ));
    fx.state
        .close_session(child)
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(
        fx.state
            .child_metadata(child)
            .bounded(file!(), line!())
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
    .bounded(file!(), line!())
    .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    std::fs::write(
        fx.ws.join(".navai/agents/writer.md"),
        "---\nmode: subagent\ntools:\n  write: false\n---\nResearch without writes.",
    )
    .unwrap();
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "writer", "try forbidden tools")
        .bounded(file!(), line!())
        .await
        .unwrap();
    for _ in 0..200 {
        if fx
            .state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status
            != "running"
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5))
            .bounded(file!(), line!())
            .await;
    }
    assert!(!fx.ws.join("forbidden.txt").exists());
    let replay = events(&fx, child).bounded(file!(), line!()).await;
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
            .bounded(file!(), line!())
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
    .bounded(file!(), line!())
    .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "inspect sources")
        .bounded(file!(), line!())
        .await
        .unwrap();
    for _ in 0..100 {
        if !provider.requests().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5))
            .bounded(file!(), line!())
            .await;
    }
    assert!(
        !provider.requests().is_empty(),
        "child's first turn reached its provider"
    );
    let summary = fx
        .state
        .session_summary(child)
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(
        summary.parent_session_id.as_deref(),
        Some(parent.to_string().as_str())
    );
    assert!(!summary.promptable);
    let child_events = events(&fx, child).bounded(file!(), line!()).await;
    assert!(child_events
        .iter()
        .any(|e| e.kind == EventKind::SubagentSpawned.as_str()));
    let listed = fx
        .state
        .list(&fx.ws.display().to_string())
        .bounded(file!(), line!())
        .await
        .unwrap();
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
            .bounded(file!(), line!())
            .await
            .unwrap_err()
            .to_string(),
        "child sessions are read-only to clients"
    );
    assert_eq!(
        fx.state
            .session_summary(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .agent,
        "explore"
    );
    fx.state
        .close_session(child)
        .bounded(file!(), line!())
        .await
        .unwrap();
    release.notify_one();
    let link = fx
        .state
        .child_metadata(child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link.status, "stopped");
    let stored = fx
        .state
        .session_summary(child)
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(
        stored.parent_session_id.as_deref(),
        Some(parent.to_string().as_str())
    );
    assert!(!stored.promptable);
    let parent_events = events(&fx, parent).bounded(file!(), line!()).await;
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
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "filtered", "inspect")
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(
        fx.state
            .session_summary(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .agent,
        "filtered"
    );
    fx.state
        .close_session(child)
        .bounded(file!(), line!())
        .await
        .unwrap();
}

#[tokio::test]
async fn capacity_error_has_clear_machine_and_model_message() {
    let reached = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = spawn_holding_provider(1, 0, reached.clone(), release.clone(), |_, _| {
        http_sse(&sse_body(&[&text_chunk("child result")]))
    })
    .bounded(file!(), line!())
    .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    std::fs::write(
        fx.root.join("cfg/config.json"),
        json!({"agents":{"build":{"model":"mock/mock-model"}},"subagents":{"max_concurrent":1}})
            .to_string(),
    )
    .unwrap();
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "first")
        .bounded(file!(), line!())
        .await
        .unwrap();
    // Hold the first turn at the provider, rather than letting a dead endpoint
    // fail it and release its slot before the second admission is attempted.
    reached.notified().bounded(file!(), line!()).await;
    let bodies = provider.bodies();
    assert_eq!(bodies.len(), 1);
    let request: Value = serde_json::from_str(&bodies[0]).unwrap();
    assert_eq!(request["model"], "mock-model");
    assert!(request["messages"].to_string().contains("first"));
    assert_eq!(
        fx.state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    assert_eq!(fx.state.reserved_child_count(parent), 1);
    let err = fx
        .state
        .create_child_session(parent, "explore", "second")
        .bounded(file!(), line!())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        navai::error::ServerError::SubagentCapacity { capacity: 1 }
    ));
    assert_eq!(
        err.to_string(),
        "subagent capacity reached: 1 children are running; wait for one to finish"
    );
    assert_eq!(provider.requests().len(), 1);
    fx.state
        .close_session(child)
        .bounded(file!(), line!())
        .await
        .unwrap();
    release.notify_one();
    fx.state.close_all().bounded(file!(), line!()).await;
    assert_eq!(fx.state.reserved_child_count(parent), 0);
}

#[tokio::test]
async fn child_depth_has_no_task_registry_entry() {
    let fx = fixture();
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "writer", "do work")
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(matches!(
        fx.state
            .create_child_session(child, "writer", "nested")
            .bounded(file!(), line!())
            .await,
        Err(navai::error::ServerError::ChildReadOnly)
    ));
    let metadata = fx
        .state
        .child_metadata(child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&metadata.policy_snapshot).unwrap()["task"],
        false
    );
}

#[tokio::test]
async fn parent_sse_replay_contains_spawned_and_done_after_completion() {
    let provider =
        spawn_capturing_provider(vec![http_sse(&sse_body(&[&text_chunk("child result")]))])
            .bounded(file!(), line!())
            .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "review")
        .bounded(file!(), line!())
        .await
        .unwrap();
    for _ in 0..100 {
        if fx
            .state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .is_some_and(|link| link.status != "running")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5))
            .bounded(file!(), line!())
            .await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !events(&fx, parent)
            .bounded(file!(), line!())
            .await
            .iter()
            .any(|e| e.kind == "subagent.done")
        {
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    let replay = events(&fx, parent).bounded(file!(), line!()).await;
    let kinds: Vec<&str> = replay.iter().map(|event| event.kind.as_str()).collect();
    assert!(kinds.contains(&EventKind::SubagentSpawned.as_str()));
    assert!(kinds.contains(&EventKind::SubagentDone.as_str()));
}

#[tokio::test]
async fn child_slot_is_released_after_terminal_result() {
    let response = http_sse(&sse_body(&[&text_chunk("completed")]));
    let provider = common::sse::spawn_capturing_provider(vec![response])
        .bounded(file!(), line!())
        .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    std::fs::write(
        fx.root.join("cfg/config.json"),
        json!({"agents":{"build":{"model":"mock/mock-model"}},"subagents":{"max_concurrent":1}})
            .to_string(),
    )
    .unwrap();
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "finish")
        .bounded(file!(), line!())
        .await
        .unwrap();
    for _ in 0..100 {
        if fx
            .state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .is_some_and(|link| link.status == "done" || link.status == "failed")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5))
            .bounded(file!(), line!())
            .await;
    }
    let link = fx
        .state
        .child_metadata(child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(link.status.as_str(), "done" | "failed"));
    let next = fx
        .state
        .create_child_session(parent, "explore", "second task")
        .bounded(file!(), line!())
        .await
        .unwrap();
    fx.state
        .close_session(next)
        .bounded(file!(), line!())
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_children_reuse_exactly_two_completed_capacity_slots() {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    std::fs::write(
        fx.root.join("cfg/config.json"),
        json!({"agents":{"build":{"model":"mock/mock-model"}},"subagents":{"max_concurrent":4}})
            .to_string(),
    )
    .unwrap();
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let children = futures::future::join_all(
        (0..4).map(|_| fx.state.create_child_session(parent, "explore", "inspect")),
    )
    .bounded(file!(), line!())
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert_eq!(children.len(), 4);
    let mut streams = Vec::new();
    for _ in 0..4 {
        let (_, stream) =
            tokio::time::timeout(std::time::Duration::from_secs(2), provider.requests.recv())
                .bounded(file!(), line!())
                .await
                .unwrap()
                .unwrap();
        streams.push(stream);
    }
    assert!(matches!(
        fx.state
            .create_child_session(parent, "explore", "fifth")
            .bounded(file!(), line!())
            .await,
        Err(navai::error::ServerError::SubagentCapacity { capacity: 4 })
    ));
    let finished = streams.drain(..2).collect::<Vec<_>>();
    futures::future::join_all(finished.into_iter().map(|stream| async move {
        stream.send(text_chunk("complete")).unwrap();
        drop(stream);
    }))
    .bounded(file!(), line!())
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if fx
                .state
                .history()
                .active_child_count(parent)
                .bounded(file!(), line!())
                .await
                .unwrap()
                == 2
                && fx.state.reserved_child_count(parent) == 2
            {
                break;
            }
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    let replacements = futures::future::join_all((0..2).map(|_| {
        fx.state
            .create_child_session(parent, "explore", "replacement")
    }))
    .bounded(file!(), line!())
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert_eq!(replacements.len(), 2);
    assert_eq!(
        fx.state
            .history()
            .active_child_count(parent)
            .bounded(file!(), line!())
            .await
            .unwrap(),
        4
    );
    assert_eq!(fx.state.reserved_child_count(parent), 4);
    assert!(matches!(
        fx.state
            .create_child_session(parent, "explore", "fifth again")
            .bounded(file!(), line!())
            .await,
        Err(navai::error::ServerError::SubagentCapacity { capacity: 4 })
    ));
    fx.state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn child_prompt_is_not_attributed_as_driver_task_or_transcript_inherited() {
    let fx = fixture();
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    fx.state
        .send(common::server::driver_command(
            parent,
            "parent history".into(),
        ))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "only child prompt")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let loaded = fx
        .state
        .history()
        .load(child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert!(loaded.transcript.iter().all(|message| {
        !serde_json::to_string(message)
            .unwrap()
            .contains("parent history")
    }));
    let child_events = events(&fx, child).bounded(file!(), line!()).await;
    let received = child_events
        .iter()
        .find(|event| event.kind == "message.received")
        .unwrap();
    assert_eq!(received.payload["source"], "parent:agent");
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
    .bounded(file!(), line!())
    .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    add_cognition_model(&fx.root);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let task = navai::tools::task::TaskTool::new(fx.state.service(), parent);
    let cx = navai::tools::ToolContext {
        binding: None,
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
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(result.output.contains("\"status\":\"running\""));
    for _ in 0..100 {
        if events(&fx, parent)
            .bounded(file!(), line!())
            .await
            .iter()
            .any(|e| e.kind == EventKind::SubagentSpawned.as_str())
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5))
            .bounded(file!(), line!())
            .await;
    }
    let parent_events = events(&fx, parent).bounded(file!(), line!()).await;
    let spawned = parent_events
        .iter()
        .find(|e| e.kind == EventKind::SubagentSpawned.as_str())
        .unwrap_or_else(|| panic!("no spawn event: {parent_events:#?}"));
    let child_id: ulid::Ulid = spawned.payload["child_session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let meta = fx
        .state
        .child_metadata(child_id)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(meta.status, "running");
    assert_eq!(meta.goal, "inspect");
    assert!(
        !fx.state
            .session_summary(child_id)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .promptable
    );
    release.notify_one();
    for _ in 0..100 {
        if fx
            .state
            .child_metadata(child_id)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status
            != "running"
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5))
            .bounded(file!(), line!())
            .await;
    }
    let done = fx
        .state
        .child_metadata(child_id)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(done.status.as_str(), "done" | "failed"));
}

#[tokio::test]
async fn full_access_child_of_plan_parent_has_no_write_dispatch() {
    let provider = spawn_capturing_provider(vec![http_sse(&sse_body(&[&text_chunk("unused")]))])
        .bounded(file!(), line!())
        .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session_as(&fx.ws, Some("mock/mock-model"), false, Some("plan"))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "writer", "do an edit")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let link = fx
        .state
        .child_metadata(child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    let policy: Value = serde_json::from_str(&link.policy_snapshot).unwrap();
    for tool in ["write", "edit", "bash"] {
        assert_eq!(policy[tool], false, "{tool}");
    }
    assert_eq!(
        fx.state
            .session_summary(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .agent,
        "writer"
    );
}

#[tokio::test]
async fn declared_read_only_child_tool_denial_is_reflected_in_saved_policy() {
    let fx = fixture();
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "filtered", "inspect")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let policy: Value = serde_json::from_str(
        &fx.state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .policy_snapshot,
    )
    .unwrap();
    assert_eq!(policy["read"], false);
    assert_eq!(policy["grep"], false);
    fx.state
        .close_session(child)
        .bounded(file!(), line!())
        .await
        .unwrap();
}

#[tokio::test]
async fn child_stop_http_is_allowed_but_chat_command_and_resume_are_forbidden() {
    let fx = fixture();
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "explore", "read")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let app = navai::server::app(fx.state.clone());
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let message = app.clone().oneshot(axum::http::Request::post(format!("/v1/sessions/{child}/messages")).header("content-type", "application/json").body(Body::from(json!({"target_actor_id":"agent:main","content":[{"type":"text","text":"hello"}]}).to_string())).unwrap()).bounded(file!(), line!()).await.unwrap();
    assert_eq!(message.status(), 403);
    assert_eq!(
        message
            .into_body()
            .collect()
            .bounded(file!(), line!())
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
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(command.status(), 403);
    let direct = fx
        .state
        .run_command(child, "help", "")
        .bounded(file!(), line!())
        .await
        .unwrap_err();
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
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(resume.status(), 403);
    let stop = app
        .oneshot(
            axum::http::Request::post(format!("/v1/sessions/{child}/stop"))
                .body(Body::empty())
                .unwrap(),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(stop.status(), 200);
    assert!(matches!(
        fx.state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status
            .as_str(),
        "stopped" | "failed"
    ));
}

// ---- session bypass and children ----

async fn bypass_events(fx: &Fx, id: ulid::Ulid) -> Vec<navai::bus::event::EventEnvelope> {
    events(fx, id)
        .bounded(file!(), line!())
        .await
        .into_iter()
        .filter(|e| e.kind == "session.bypass")
        .collect()
}

async fn bypass_of(fx: &Fx, id: ulid::Ulid) -> bool {
    fx.state
        .session_summary(id)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .bypass
}

async fn set_bypass(fx: &Fx, id: ulid::Ulid, enabled: bool) {
    fx.state
        .set_bypass(id, enabled, navai::bus::runtime::bypass::BypassOrigin::Http)
        .bounded(file!(), line!())
        .await
        .unwrap();
}

/// A parent and a child that stays running (its model answer is held open).
async fn parent_with_running_child(
    fx: &Fx,
    provider: &mut common::sse::StreamingProvider,
    parent_on: bool,
) -> (
    ulid::Ulid,
    ulid::Ulid,
    tokio::sync::mpsc::UnboundedSender<String>,
) {
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    if parent_on {
        set_bypass(fx, parent, true).await;
    }
    let child = fx
        .state
        .create_child_session(parent, "writer", "write")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, hold) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    (parent, child, hold)
}

#[tokio::test]
async fn child_inherits_the_parents_current_bypass_before_its_first_prompt() {
    for parent_on in [true, false] {
        let mut provider = common::sse::spawn_streaming_provider()
            .bounded(file!(), line!())
            .await;
        let fx = fixture_at(&provider.url);
        let (parent, child, _hold) = parent_with_running_child(&fx, &mut provider, parent_on).await;
        assert_eq!(bypass_of(&fx, child).await, parent_on);
        let toggles = bypass_events(&fx, child).await;
        if parent_on {
            assert_eq!(toggles.len(), 1);
            assert_eq!(
                toggles[0].payload,
                json!({"enabled":true,"by":format!("parent:{parent}")})
            );
            let first_prompt = events(&fx, child)
                .bounded(file!(), line!())
                .await
                .into_iter()
                .find(|e| e.kind == "message.received")
                .expect("the child's first prompt");
            assert!(toggles[0].seq < first_prompt.seq, "ON before the prompt");
        } else {
            assert!(toggles.is_empty());
        }
        fx.state.close_all().bounded(file!(), line!()).await;
    }
}

#[tokio::test]
async fn parent_changes_reach_live_children_both_ways_and_no_unrelated_root() {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let fx = fixture_at(&provider.url);
    let (parent, child, _hold) = parent_with_running_child(&fx, &mut provider, false).await;
    let other = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    set_bypass(&fx, parent, true).await;
    assert!(bypass_of(&fx, parent).await && bypass_of(&fx, child).await);
    assert!(!bypass_of(&fx, other).await);
    set_bypass(&fx, parent, false).await;
    assert!(!bypass_of(&fx, parent).await && !bypass_of(&fx, child).await);
    let toggles = bypass_events(&fx, child).await;
    let by = format!("parent:{parent}");
    assert_eq!(
        toggles
            .iter()
            .map(|e| e.payload.clone())
            .collect::<Vec<_>>(),
        vec![
            json!({"enabled":true,"by":by}),
            json!({"enabled":false,"by":by})
        ]
    );
    assert!(bypass_events(&fx, other).await.is_empty());
    // A child cannot be toggled by hand.
    assert!(matches!(
        fx.state
            .set_bypass(child, true, navai::bus::runtime::bypass::BypassOrigin::Http)
            .bounded(file!(), line!())
            .await,
        Err(navai::error::ServerError::ChildReadOnly)
    ));
    // A child that ended is simply not followed.
    fx.state
        .close_session(child)
        .bounded(file!(), line!())
        .await
        .unwrap();
    set_bypass(&fx, parent, true).await;
    assert!(bypass_of(&fx, parent).await);
    fx.state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn a_toggle_during_child_registration_is_never_missed() {
    // Paused before the child is registered: the toggle completes without it
    // and the child reads the parent's value when it registers.
    // Paused inside the bypass gate: the toggle waits and then reaches the child.
    for inside_gate in [false, true] {
        let mut provider = common::sse::spawn_streaming_provider()
            .bounded(file!(), line!())
            .await;
        let fx = fixture_at(&provider.url);
        install_agents(&fx.root, &fx.ws);
        let parent = fx
            .state
            .create_session(&fx.ws, Some("mock/mock-model"), false)
            .bounded(file!(), line!())
            .await
            .unwrap();
        let barrier = navai::session::ChildPreparationBarrier::default();
        if inside_gate {
            fx.state.pause_child_bypass_gate(barrier.clone());
        } else {
            fx.state.pause_child_registration(barrier.clone());
        }
        let service = fx.state.service();
        let spawn = tokio::spawn(async move {
            service
                .create_child_session(parent, "writer", "write")
                .bounded(file!(), line!())
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            barrier.reached.notified(),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
        let service = fx.state.service();
        let toggle = tokio::spawn(async move {
            service
                .set_bypass(
                    parent,
                    true,
                    navai::bus::runtime::bypass::BypassOrigin::Http,
                )
                .bounded(file!(), line!())
                .await
        });
        if inside_gate {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert!(!toggle.is_finished(), "the toggle waits for the child");
        } else {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !toggle.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .bounded(file!(), line!())
            .await
            .unwrap();
        }
        barrier.release.notify_one();
        toggle.bounded(file!(), line!()).await.unwrap().unwrap();
        let child = spawn.bounded(file!(), line!()).await.unwrap().unwrap();
        assert!(bypass_of(&fx, child).await, "inside_gate={inside_gate}");
        let (_, _hold) = provider
            .requests
            .recv()
            .bounded(file!(), line!())
            .await
            .unwrap();
        fx.state.close_all().bounded(file!(), line!()).await;
    }
}

#[tokio::test]
async fn failed_durable_toggle_enables_nothing_in_the_tree() {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let fx = fixture_at(&provider.url);
    let (parent, child, _hold) = parent_with_running_child(&fx, &mut provider, false).await;
    fx.state.fail_bypass_commit();
    assert!(fx
        .state
        .set_bypass(
            parent,
            true,
            navai::bus::runtime::bypass::BypassOrigin::Http
        )
        .bounded(file!(), line!())
        .await
        .is_err());
    assert!(!bypass_of(&fx, parent).await && !bypass_of(&fx, child).await);
    assert!(bypass_events(&fx, parent).await.is_empty());
    assert!(bypass_events(&fx, child).await.is_empty());
    set_bypass(&fx, parent, true).await;
    assert!(bypass_of(&fx, parent).await && bypass_of(&fx, child).await);
    fx.state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn parent_enabling_bypass_answers_the_childs_ordinary_pending_request() {
    let provider = spawn_capturing_provider(vec![
        http_sse(&sse_body(&[&tool_call_chunk(
            "child-bash",
            "bash",
            &json!({"command":"true"}),
        )])),
        http_sse(&sse_body(&[&text_chunk("child done")])),
    ])
    .bounded(file!(), line!())
    .await;
    let fx = fixture_at(&provider.url);
    install_agents(&fx.root, &fx.ws);
    let parent = fx
        .state
        .create_session(&fx.ws, Some("mock/mock-model"), false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = fx
        .state
        .create_child_session(parent, "writer", "run it")
        .bounded(file!(), line!())
        .await
        .unwrap();
    for _ in 0..400 {
        if events(&fx, child)
            .bounded(file!(), line!())
            .await
            .iter()
            .any(|e| e.kind == "permission.requested")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    set_bypass(&fx, parent, true).await;
    let mut resolved = Vec::new();
    for _ in 0..400 {
        resolved = events(&fx, child)
            .bounded(file!(), line!())
            .await
            .into_iter()
            .filter(|e| e.kind == "permission.resolved")
            .collect();
        if !resolved.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["approved_by"], "bypass");
    fx.state.close_all().bounded(file!(), line!()).await;
}
