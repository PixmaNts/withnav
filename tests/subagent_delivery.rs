use std::time::Duration;

use navai::bus::command::Command;
use navai::provider::openai::ChatMessage;
use navai::server::AppState;
use serde_json::{json, Value};

mod common;
use common::deadline::Deadline;

#[tokio::test]
async fn owner_continue_admits_one_budget_renewal_and_child_finishes() {
    use tower::ServiceExt;
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let other = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = state
        .create_child_session(parent, "explore", "inspect")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, first) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(matches!(
        state
            .continue_child(parent, child)
            .bounded(file!(), line!())
            .await,
        Err(navai::error::ServerError::ChildNotParked)
    ));
    assert!(matches!(
        state
            .continue_child(other, child)
            .bounded(file!(), line!())
            .await,
        Err(navai::error::ServerError::ChildReadOnly)
    ));
    let steps = navai::agent::run_loop::max_steps();
    let mut stream = Some(first);
    for index in 0..steps {
        let tx = stream.take().unwrap();
        tx.send(tool_call_chunk(
            &format!("read-{index}"),
            "glob",
            &json!({"pattern":"*"}),
        ))
        .unwrap();
        drop(tx);
        if index + 1 < steps {
            stream = Some(
                provider
                    .requests
                    .recv()
                    .bounded(file!(), line!())
                    .await
                    .unwrap()
                    .1,
            );
        }
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let navai::bus::ResumeOutcome::Replay(events) = state
                .subscribe(child, None)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .replay
            {
                if events
                    .iter()
                    .any(|e| e.kind == "agent.yield" && e.payload["kind"] == "budget")
                {
                    break;
                }
            }
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    let app = navai::server::app(state.clone());
    let request = || {
        axum::http::Request::post(format!("/v1/sessions/{parent}/children/{child}/continue"))
            .body(axum::body::Body::empty())
            .unwrap()
    };
    let (a, b) = tokio::join!(
        app.clone().oneshot(request()),
        app.clone().oneshot(request())
    );
    let mut statuses = vec![a.unwrap().status().as_u16(), b.unwrap().status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, vec![200, 409]);
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream.send(text_chunk("child complete")).unwrap();
    drop(stream);
    let (_, stream) = tokio::time::timeout(Duration::from_secs(2), provider.requests.recv())
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "done"
    );
    assert_eq!(state.reserved_child_count(parent), 0);
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn queued_driver_correction_precedes_completion_only_wake() {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .send(Command::child_prompt(parent, "original instruction".into()))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, parent_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .create_child_session(parent, "explore", "inspect")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, child_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    child_stream.send(text_chunk("child answer")).unwrap();
    drop(child_stream);
    notice_count(&state, parent, 1)
        .bounded(file!(), line!())
        .await;
    state
        .send(Command::child_prompt(parent, "driver correction".into()))
        .bounded(file!(), line!())
        .await
        .unwrap();
    parent_stream.send(text_chunk("old turn done")).unwrap();
    drop(parent_stream);
    let (request, stream) = tokio::time::timeout(Duration::from_secs(2), provider.requests.recv())
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let driver = messages
        .iter()
        .position(|m| m["content"] == "driver correction")
        .unwrap();
    let notice = messages
        .iter()
        .position(|m| {
            m["content"]
                .as_str()
                .is_some_and(|s| s.contains("untrusted_subagent_output"))
        })
        .unwrap();
    assert!(driver < notice);
    assert_eq!(request_outputs(&request).len(), 1);
    stream.send(text_chunk("corrected")).unwrap();
    drop(stream);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), provider.requests.recv())
            .bounded(file!(), line!())
            .await
            .is_err()
    );
    state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn mirrored_child_permission_is_resolved_after_parent_stop_and_resume() {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir_all(ws.join(".navai/agents")).unwrap();
    std::fs::write(
        ws.join(".navai/agents/writer.md"),
        "---\nmode: subagent\naccess: full\n---\nWrite.",
    )
    .unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = state
        .create_child_session(parent, "writer", "write")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream
        .send(tool_call_chunk(
            "pending",
            "bash",
            &json!({"command":"true"}),
        ))
        .unwrap();
    drop(stream);
    let request = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let navai::bus::ResumeOutcome::Replay(events) = state
                .subscribe(parent, None)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .replay
            {
                if let Some(e) = events
                    .into_iter()
                    .find(|e| e.kind == "permission.requested")
                {
                    break e.payload["request_id"].clone();
                }
            }
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    state
        .close_session(parent)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .create_or_resume_session(&ws, None, None, Some(parent))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let navai::bus::ResumeOutcome::Replay(events) = state
        .subscribe(parent, None)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .replay
    else {
        panic!("replay");
    };
    let resolved = events
        .iter()
        .filter(|e| e.kind == "permission.resolved" && e.payload["request_id"] == request)
        .collect::<Vec<_>>();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["child_session_id"], child.to_string());
    assert_eq!(resolved[0].payload["resolution"], "cancelled");
    state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn abandoned_mirrored_permission_is_expired_on_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let config = TestServerConfig::unreachable(tmp.path())
        .chat_tools(128_000)
        .config();
    let store = navai::session::store::HistoryStore::open(&config.history_db).unwrap();
    let parent = ulid::Ulid::new();
    let child = ulid::Ulid::new();
    let request = ulid::Ulid::new();
    store
        .record_session(parent, &ws.display().to_string(), "default", None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    store
        .record_session(child, &ws.display().to_string(), "default", None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    store
        .create_child_link(
            child,
            "writer".into(),
            "write".into(),
            "{}".into(),
            4,
            navai::bus::event::EventEnvelope::new(
                parent,
                0,
                navai::bus::event::EventKind::SubagentSpawned,
                json!({}),
            ),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    store.append_event(
        parent,
        navai::bus::event::EventEnvelope::new(
            parent,
            1,
            navai::bus::event::EventKind::PermissionRequested,
            json!({"request_id":request,"child_session_id":child,"tool_call_id":"pending"}),
        ),
    );
    store.flush().bounded(file!(), line!()).await.unwrap();
    drop(store);
    let state = AppState::new(config).unwrap();
    state
        .create_or_resume_session(&ws, None, None, Some(parent))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let navai::bus::ResumeOutcome::Replay(events) = state
        .subscribe(parent, None)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .replay
    else {
        panic!("replay");
    };
    let resolved = events
        .iter()
        .filter(|e| {
            e.kind == "permission.resolved" && e.payload["request_id"] == request.to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["resolution"], "expired");
    state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn recovery_leaves_live_instance_children_and_recovers_stale_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let config = TestServerConfig::unreachable(tmp.path())
        .chat_tools(128_000)
        .config();
    let first = AppState::new(config.clone()).unwrap();
    let parent = ulid::Ulid::new();
    let child = ulid::Ulid::new();
    first
        .history()
        .record_session(parent, "/ws", "default", None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    first
        .history()
        .record_session(child, "/ws", "default", None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    first
        .history()
        .create_child_link(
            child,
            "explore".into(),
            "goal".into(),
            "{}".into(),
            4,
            navai::bus::event::EventEnvelope::new(
                parent,
                0,
                navai::bus::event::EventKind::SubagentSpawned,
                json!({}),
            ),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    let mut other_config = config.clone();
    other_config.data_dir = tmp.path().join("another-data-directory");
    let alias = tmp.path().join("database-alias.db");
    std::os::unix::fs::symlink(&config.history_db, &alias).unwrap();
    other_config.history_db = alias;
    let second = AppState::new(other_config).unwrap();
    assert_eq!(
        second
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    assert!(second
        .history()
        .pending_completions(parent)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .is_empty());
    drop(first);
    let third = AppState::new(config).unwrap();
    let link = third
        .child_metadata(child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link.status, "stopped");
    assert_eq!(link.terminal_reason.as_deref(), Some("restart"));
    assert_eq!(
        third
            .history()
            .pending_completions(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(third.reserved_child_count(parent), 0);
}
use common::server::TestServerConfig;
use common::sse::{spawn_streaming_provider, text_chunk, tool_call_chunk};

async fn notice_count(state: &AppState, parent: ulid::Ulid, count: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if state
                .history()
                .unconsumed_completions(parent)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .len()
                == count
            {
                break;
            }
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
}

fn outputs(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::User { content, .. } => serde_json::from_str::<Value>(content)
                .ok()
                .filter(|v| v["untrusted_subagent_output"] == true),
            _ => None,
        })
        .collect()
}

fn request_outputs(request: &str) -> Vec<Value> {
    let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| {
            serde_json::from_str::<Value>(message["content"].as_str()?)
                .ok()
                .filter(|v| v["untrusted_subagent_output"] == true)
        })
        .collect()
}

#[tokio::test]
async fn finished_child_wakes_idle_parent_and_consumes_once() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = state
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
    stream.send(text_chunk("a useful answer")).unwrap();
    drop(stream);
    let (request, stream) = tokio::time::timeout(Duration::from_secs(2), provider.requests.recv())
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    let result = request_outputs(&request);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["child_session_id"], child.to_string());
    assert_eq!(result[0]["result"], "a useful answer");
    notice_count(&state, parent, 0)
        .bounded(file!(), line!())
        .await;
    assert_eq!(
        outputs(
            &state
                .history()
                .load(parent)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .unwrap()
                .transcript
        )
        .len(),
        1
    );
    stream.send(text_chunk("processed")).unwrap();
    drop(stream);
    state
        .close_session(parent)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .create_or_resume_session(&ws, None, None, Some(parent))
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), provider.requests.recv())
            .bounded(file!(), line!())
            .await
            .is_err()
    );
    assert_eq!(
        outputs(
            &state
                .history()
                .load(parent)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .unwrap()
                .transcript
        )
        .len(),
        1
    );
    state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn busy_parent_consumes_several_notices_in_order_at_next_boundary() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .send(Command::child_prompt(parent, "parent working".into()))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, parent_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let first = state
        .create_child_session(parent, "explore", "first")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, first_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let second = state
        .create_child_session(parent, "explore", "second")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, second_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    first_stream.send(text_chunk("first answer")).unwrap();
    drop(first_stream);
    notice_count(&state, parent, 1)
        .bounded(file!(), line!())
        .await;
    second_stream
        .send(text_chunk(&"\\\n".repeat(20000)))
        .unwrap();
    drop(second_stream);
    notice_count(&state, parent, 2)
        .bounded(file!(), line!())
        .await;
    for text in ["older navigator wake", "latest navigator wake"] {
        let mut command = Command::child_prompt(parent, text.into());
        command.source_actor_id = "navigator:conversation".into();
        state.send(command).bounded(file!(), line!()).await.unwrap();
    }
    assert!(outputs(
        &state
            .history()
            .load(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .transcript
    )
    .is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), provider.requests.recv())
            .bounded(file!(), line!())
            .await
            .is_err()
    );
    parent_stream.send(text_chunk("parent finished")).unwrap();
    drop(parent_stream);
    let (request, stream) = tokio::time::timeout(Duration::from_secs(2), provider.requests.recv())
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    let result = request_outputs(&request);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0]["child_session_id"], first.to_string());
    assert_eq!(result[1]["child_session_id"], second.to_string());
    let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    for m in body["messages"].as_array().unwrap() {
        if m["content"]
            .as_str()
            .is_some_and(|s| s.contains("untrusted_subagent_output"))
        {
            assert!(m["content"].as_str().unwrap().len() <= 16 * 1024);
        }
    }
    notice_count(&state, parent, 0)
        .bounded(file!(), line!())
        .await;
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn unconsumed_notice_survives_child_delete_and_service_recreation() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let config = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .config();
    let state = AppState::new(config.clone()).unwrap();
    let parent = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = state
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
    state
        .close_session(parent)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state.delete(child).bounded(file!(), line!()).await.unwrap();
    drop(stream);
    notice_count(&state, parent, 1)
        .bounded(file!(), line!())
        .await;
    drop(state);
    let state = AppState::new(config).unwrap();
    state
        .create_or_resume_session(&ws, None, None, Some(parent))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (request, stream) = tokio::time::timeout(Duration::from_secs(2), provider.requests.recv())
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    let result = request_outputs(&request);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["child_session_id"], child.to_string());
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn parent_delete_removes_child_tree_and_notices() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = state
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
    state
        .delete(parent)
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(!state.session_has_runtime(child));
    assert!(!state
        .history()
        .exists(child)
        .bounded(file!(), line!())
        .await
        .unwrap());
    assert!(!state
        .history()
        .exists(parent)
        .bounded(file!(), line!())
        .await
        .unwrap());
    assert!(state
        .child_metadata(child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .is_none());
    assert!(state
        .history()
        .unconsumed_completions(parent)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(state.reserved_child_count(parent), 0);
    drop(stream);
}

#[tokio::test]
async fn recovery_terminalizes_unfinished_child_without_tool_replay() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let config = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .config();
    let history = navai::session::store::HistoryStore::open(config.history_db.clone()).unwrap();
    let parent = ulid::Ulid::new();
    let child = ulid::Ulid::new();
    history
        .record_session(parent, &ws.display().to_string(), "default", None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    history
        .record_session(child, &ws.display().to_string(), "default", None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    history
        .create_child_link(
            child,
            "explore".into(),
            "inspect".into(),
            "{}".into(),
            4,
            navai::bus::event::EventEnvelope::new(
                parent,
                0,
                navai::bus::event::EventKind::SubagentSpawned,
                json!({}),
            ),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    drop(history);
    let state = AppState::new(config).unwrap();
    let link = state
        .child_metadata(child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link.status, "stopped");
    assert_eq!(link.terminal_reason.as_deref(), Some("restart"));
    assert_eq!(state.reserved_child_count(parent), 0);
    state
        .create_or_resume_session(&ws, None, None, Some(parent))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (request, stream) = tokio::time::timeout(Duration::from_secs(2), provider.requests.recv())
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request_outputs(&request)[0]["outcome"], "stopped");
    assert!(!state.session_has_runtime(child));
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn child_permission_is_answered_by_owner_without_parent_interaction() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir_all(ws.join(".navai/agents")).unwrap();
    std::fs::write(
        ws.join(".navai/agents/writer.md"),
        "---\nmode: subagent\naccess: full\n---\nWrite.",
    )
    .unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let other = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = state
        .create_child_session(parent, "writer", "write")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream
        .send(tool_call_chunk(
            "write-once",
            "bash",
            &json!({"command":"printf x >> approval-count"}),
        ))
        .unwrap();
    drop(stream);
    let request = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            state
                .history()
                .flush()
                .bounded(file!(), line!())
                .await
                .unwrap();
            let replay = state
                .subscribe(parent, None)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .replay;
            if let navai::bus::ResumeOutcome::Replay(events) = replay {
                if let Some(e) = events
                    .into_iter()
                    .find(|e| e.kind == "permission.requested")
                {
                    break e;
                }
            }
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    assert_eq!(request.payload["child_session_id"], child.to_string());
    let id = request.payload["request_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(matches!(
        state
            .answer_permission(
                other,
                id,
                navai::agent::permissions::PermissionDecision::AllowOnce
            )
            .bounded(file!(), line!())
            .await,
        Err(navai::error::ServerError::ChildReadOnly)
    ));
    assert!(matches!(
        state
            .answer_permission(
                child,
                id,
                navai::agent::permissions::PermissionDecision::AllowOnce
            )
            .bounded(file!(), line!())
            .await,
        Err(navai::error::ServerError::ChildReadOnly)
    ));
    state
        .answer_permission(
            parent,
            id,
            navai::agent::permissions::PermissionDecision::AllowOnce,
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(state
        .answer_permission(
            parent,
            id,
            navai::agent::permissions::PermissionDecision::AllowOnce
        )
        .bounded(file!(), line!())
        .await
        .is_err());
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(ws.join("approval-count")).unwrap(),
        "x"
    );
    stream.send(text_chunk("written")).unwrap();
    drop(stream);
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(ws.join("approval-count")).unwrap(),
        "x"
    );
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn child_permission_and_result_do_not_answer_parent_ask() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir_all(ws.join(".navai/agents")).unwrap();
    std::fs::write(
        ws.join(".navai/agents/writer.md"),
        "---\nmode: subagent\naccess: full\n---\nWrite.",
    )
    .unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .send(Command::child_prompt(parent, "ask me".into()))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream
        .send(tool_call_chunk(
            "parent-ask",
            "ask",
            &json!({"question":"Choose?"}),
        ))
        .unwrap();
    drop(stream);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let navai::bus::ResumeOutcome::Replay(events) = state
                .subscribe(parent, None)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .replay
            {
                if events.iter().any(|e| e.kind == "agent.yield") {
                    break;
                }
            }
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    let child = state
        .create_child_session(parent, "writer", "write")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream
        .send(tool_call_chunk(
            "child-bash",
            "bash",
            &json!({"command":"true"}),
        ))
        .unwrap();
    drop(stream);
    let request_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let navai::bus::ResumeOutcome::Replay(events) = state
                .subscribe(parent, None)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .replay
            {
                if let Some(e) = events.iter().find(|e| {
                    e.kind == "permission.requested"
                        && e.payload["child_session_id"] == child.to_string()
                }) {
                    break e.payload["request_id"].as_str().unwrap().parse().unwrap();
                }
            }
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    state
        .answer_permission(
            parent,
            request_id,
            navai::agent::permissions::PermissionDecision::AllowOnce,
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream.send(text_chunk("child finished")).unwrap();
    drop(stream);
    notice_count(&state, parent, 1)
        .bounded(file!(), line!())
        .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), provider.requests.recv())
            .bounded(file!(), line!())
            .await
            .is_err()
    );
    let transcript = state
        .history()
        .load(parent)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap()
        .transcript;
    assert!(!transcript.iter().any(
        |m| matches!(m, ChatMessage::Tool { tool_call_id, .. } if tool_call_id == "parent-ask")
    ));
    state
        .send(Command::child_prompt(parent, "my answer".into()))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (request, stream) =
        match tokio::time::timeout(Duration::from_secs(5), provider.requests.recv())
            .bounded(file!(), line!())
            .await
        {
            Ok(request) => request.unwrap(),
            Err(_) => panic!(
                "parent answer never reached provider; events={:?}; transcript={:?}; notices={:?}",
                state
                    .subscribe(parent, None)
                    .bounded(file!(), line!())
                    .await,
                state.history().load(parent).bounded(file!(), line!()).await,
                state
                    .history()
                    .unconsumed_completions(parent)
                    .bounded(file!(), line!())
                    .await
            ),
        };
    assert_eq!(request_outputs(&request).len(), 1);
    assert!(request.contains("my answer"));
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn published_notice_is_consumed_after_crash_and_never_consumed_twice() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("history.db");
    let parent = ulid::Ulid::new();
    let child = ulid::Ulid::new();
    let store = navai::session::store::HistoryStore::open(&path).unwrap();
    store
        .record_session(parent, "/ws", "default", None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    store
        .record_session(child, "/ws", "default", None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    store
        .create_child_link(
            child,
            "explore".into(),
            "goal".into(),
            "{}".into(),
            4,
            navai::bus::event::EventEnvelope::new(
                parent,
                0,
                navai::bus::event::EventKind::SubagentSpawned,
                json!({}),
            ),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    store
        .finish_child_link(child, "done".into(), None, "answer".into())
        .bounded(file!(), line!())
        .await
        .unwrap();
    let payload = store
        .pending_completions(parent)
        .bounded(file!(), line!())
        .await
        .unwrap()[0]
        .1
        .clone();
    store
        .deliver_completion(
            child,
            navai::bus::event::EventEnvelope::new(
                parent,
                1,
                navai::bus::event::EventKind::SubagentDone,
                payload,
            ),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    drop(store);
    let store = navai::session::store::HistoryStore::open(&path).unwrap();
    assert!(store
        .pending_completions(parent)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .unconsumed_completions(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()[0]
            .2,
        "answer"
    );
    let transcript = vec![ChatMessage::User {
        provenance: Default::default(),
        content: "untrusted answer".into(),
    }];
    assert!(store
        .consume_completions(parent, vec![child, ulid::Ulid::new()], transcript.clone())
        .bounded(file!(), line!())
        .await
        .is_err());
    assert_eq!(
        store
            .unconsumed_completions(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(store
        .load(parent)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap()
        .transcript
        .is_empty());
    store
        .consume_completions(parent, vec![child], transcript.clone())
        .bounded(file!(), line!())
        .await
        .unwrap();
    drop(store);
    let store = navai::session::store::HistoryStore::open(&path).unwrap();
    assert!(store
        .unconsumed_completions(parent)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .consume_completions(parent, vec![child], vec![])
        .bounded(file!(), line!())
        .await
        .is_err());
    assert_eq!(
        serde_json::to_value(
            store
                .load(parent)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .unwrap()
                .transcript
        )
        .unwrap(),
        serde_json::to_value(transcript).unwrap()
    );
}
