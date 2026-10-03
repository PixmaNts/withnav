use std::time::Duration;

use navai::bus::command::Command;
use navai::provider::openai::ChatMessage;
use navai::server::AppState;
use serde_json::{json, Value};

mod common;

#[tokio::test]
async fn mirrored_child_permission_is_resolved_after_parent_stop_and_resume() {
    let mut provider = common::sse::spawn_streaming_provider().await;
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
    let parent = state.create_session(&ws, None, None).await.unwrap();
    let child = state
        .create_child_session(parent, "writer", "write")
        .await
        .unwrap();
    let (_, stream) = provider.requests.recv().await.unwrap();
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
            if let navai::bus::ResumeOutcome::Replay(events) =
                state.subscribe(parent, None).await.unwrap().replay
            {
                if let Some(e) = events
                    .into_iter()
                    .find(|e| e.kind == "permission.requested")
                {
                    break e.payload["request_id"].clone();
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    state.close_session(parent).await.unwrap();
    state
        .create_or_resume_session(&ws, None, None, Some(parent))
        .await
        .unwrap();
    let navai::bus::ResumeOutcome::Replay(events) =
        state.subscribe(parent, None).await.unwrap().replay
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
    state.close_all().await;
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
        .await
        .unwrap();
    store
        .record_session(child, &ws.display().to_string(), "default", None)
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
    store.flush().await.unwrap();
    drop(store);
    let state = AppState::new(config).unwrap();
    state
        .create_or_resume_session(&ws, None, None, Some(parent))
        .await
        .unwrap();
    let navai::bus::ResumeOutcome::Replay(events) =
        state.subscribe(parent, None).await.unwrap().replay
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
    state.close_all().await;
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
        .await
        .unwrap();
    first
        .history()
        .record_session(child, "/ws", "default", None)
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
        .await
        .unwrap();
    let second = AppState::new(config.clone()).unwrap();
    assert_eq!(
        second.child_metadata(child).await.unwrap().unwrap().status,
        "running"
    );
    assert!(second
        .history()
        .pending_completions(parent)
        .await
        .unwrap()
        .is_empty());
    drop(first);
    let third = AppState::new(config).unwrap();
    let link = third.child_metadata(child).await.unwrap().unwrap();
    assert_eq!(link.status, "stopped");
    assert_eq!(link.terminal_reason.as_deref(), Some("restart"));
    assert_eq!(
        third
            .history()
            .pending_completions(parent)
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
                .await
                .unwrap()
                .len()
                == count
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn outputs(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::User { content } => serde_json::from_str::<Value>(content)
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
    let mut provider = spawn_streaming_provider().await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state.create_session(&ws, None, None).await.unwrap();
    let child = state
        .create_child_session(parent, "explore", "inspect")
        .await
        .unwrap();
    let (_, stream) = provider.requests.recv().await.unwrap();
    stream.send(text_chunk("a useful answer")).unwrap();
    drop(stream);
    let (request, stream) = tokio::time::timeout(Duration::from_secs(2), provider.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let result = request_outputs(&request);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["child_session_id"], child.to_string());
    assert_eq!(result[0]["result"], "a useful answer");
    notice_count(&state, parent, 0).await;
    assert_eq!(
        outputs(
            &state
                .history()
                .load(parent)
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
    state.close_session(parent).await.unwrap();
    state
        .create_or_resume_session(&ws, None, None, Some(parent))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), provider.requests.recv())
            .await
            .is_err()
    );
    assert_eq!(
        outputs(
            &state
                .history()
                .load(parent)
                .await
                .unwrap()
                .unwrap()
                .transcript
        )
        .len(),
        1
    );
    state.close_all().await;
}

#[tokio::test]
async fn busy_parent_consumes_several_notices_in_order_at_next_boundary() {
    let mut provider = spawn_streaming_provider().await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state.create_session(&ws, None, None).await.unwrap();
    state
        .send(Command::child_prompt(parent, "parent working".into()))
        .await
        .unwrap();
    let (_, parent_stream) = provider.requests.recv().await.unwrap();
    let first = state
        .create_child_session(parent, "explore", "first")
        .await
        .unwrap();
    let (_, first_stream) = provider.requests.recv().await.unwrap();
    let second = state
        .create_child_session(parent, "explore", "second")
        .await
        .unwrap();
    let (_, second_stream) = provider.requests.recv().await.unwrap();
    first_stream.send(text_chunk("first answer")).unwrap();
    drop(first_stream);
    notice_count(&state, parent, 1).await;
    second_stream
        .send(text_chunk(&"\\\n".repeat(20000)))
        .unwrap();
    drop(second_stream);
    notice_count(&state, parent, 2).await;
    for text in ["older navigator wake", "latest navigator wake"] {
        let mut command = Command::child_prompt(parent, text.into());
        command.source_actor_id = "navigator:conversation".into();
        state.send(command).await.unwrap();
    }
    assert!(outputs(
        &state
            .history()
            .load(parent)
            .await
            .unwrap()
            .unwrap()
            .transcript
    )
    .is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), provider.requests.recv())
            .await
            .is_err()
    );
    parent_stream.send(text_chunk("parent finished")).unwrap();
    drop(parent_stream);
    let (request, stream) = tokio::time::timeout(Duration::from_secs(2), provider.requests.recv())
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
    notice_count(&state, parent, 0).await;
    state.close_all().await;
    drop(stream);
}

#[tokio::test]
async fn unconsumed_notice_survives_child_delete_and_service_recreation() {
    let mut provider = spawn_streaming_provider().await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let config = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .config();
    let state = AppState::new(config.clone()).unwrap();
    let parent = state.create_session(&ws, None, None).await.unwrap();
    let child = state
        .create_child_session(parent, "explore", "inspect")
        .await
        .unwrap();
    let (_, stream) = provider.requests.recv().await.unwrap();
    state.close_session(parent).await.unwrap();
    state.delete(child).await.unwrap();
    drop(stream);
    notice_count(&state, parent, 1).await;
    drop(state);
    let state = AppState::new(config).unwrap();
    state
        .create_or_resume_session(&ws, None, None, Some(parent))
        .await
        .unwrap();
    let (request, stream) = tokio::time::timeout(Duration::from_secs(2), provider.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let result = request_outputs(&request);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["child_session_id"], child.to_string());
    state.close_all().await;
    drop(stream);
}

#[tokio::test]
async fn parent_delete_removes_child_tree_and_notices() {
    let mut provider = spawn_streaming_provider().await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state.create_session(&ws, None, None).await.unwrap();
    let child = state
        .create_child_session(parent, "explore", "inspect")
        .await
        .unwrap();
    let (_, stream) = provider.requests.recv().await.unwrap();
    state.delete(parent).await.unwrap();
    assert!(!state.session_has_runtime(child));
    assert!(!state.history().exists(child).await.unwrap());
    assert!(!state.history().exists(parent).await.unwrap());
    assert!(state.child_metadata(child).await.unwrap().is_none());
    assert!(state
        .history()
        .unconsumed_completions(parent)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(state.reserved_child_count(parent), 0);
    drop(stream);
}

#[tokio::test]
async fn recovery_terminalizes_unfinished_child_without_tool_replay() {
    let mut provider = spawn_streaming_provider().await;
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
        .await
        .unwrap();
    history
        .record_session(child, &ws.display().to_string(), "default", None)
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
        .await
        .unwrap();
    drop(history);
    let state = AppState::new(config).unwrap();
    let link = state.child_metadata(child).await.unwrap().unwrap();
    assert_eq!(link.status, "stopped");
    assert_eq!(link.terminal_reason.as_deref(), Some("restart"));
    assert_eq!(state.reserved_child_count(parent), 0);
    state
        .create_or_resume_session(&ws, None, None, Some(parent))
        .await
        .unwrap();
    let (request, stream) = tokio::time::timeout(Duration::from_secs(2), provider.requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request_outputs(&request)[0]["outcome"], "stopped");
    assert!(!state.session_has_runtime(child));
    state.close_all().await;
    drop(stream);
}

#[tokio::test]
async fn child_permission_is_answered_by_owner_without_parent_interaction() {
    let mut provider = spawn_streaming_provider().await;
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
    let parent = state.create_session(&ws, None, None).await.unwrap();
    let other = state.create_session(&ws, None, None).await.unwrap();
    let child = state
        .create_child_session(parent, "writer", "write")
        .await
        .unwrap();
    let (_, stream) = provider.requests.recv().await.unwrap();
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
            state.history().flush().await.unwrap();
            let replay = state.subscribe(parent, None).await.unwrap().replay;
            if let navai::bus::ResumeOutcome::Replay(events) = replay {
                if let Some(e) = events
                    .into_iter()
                    .find(|e| e.kind == "permission.requested")
                {
                    break e;
                }
            }
            tokio::task::yield_now().await;
        }
    })
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
            .await,
        Err(navai::error::ServerError::ChildReadOnly)
    ));
    state
        .answer_permission(
            parent,
            id,
            navai::agent::permissions::PermissionDecision::AllowOnce,
        )
        .await
        .unwrap();
    assert!(state
        .answer_permission(
            parent,
            id,
            navai::agent::permissions::PermissionDecision::AllowOnce
        )
        .await
        .is_err());
    let (_, stream) = provider.requests.recv().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(ws.join("approval-count")).unwrap(),
        "x"
    );
    stream.send(text_chunk("written")).unwrap();
    drop(stream);
    let (_, stream) = provider.requests.recv().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(ws.join("approval-count")).unwrap(),
        "x"
    );
    state.close_all().await;
    drop(stream);
}

#[tokio::test]
async fn child_permission_and_result_do_not_answer_parent_ask() {
    let mut provider = spawn_streaming_provider().await;
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
    let parent = state.create_session(&ws, None, None).await.unwrap();
    state
        .send(Command::child_prompt(parent, "ask me".into()))
        .await
        .unwrap();
    let (_, stream) = provider.requests.recv().await.unwrap();
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
            if let navai::bus::ResumeOutcome::Replay(events) =
                state.subscribe(parent, None).await.unwrap().replay
            {
                if events.iter().any(|e| e.kind == "agent.yield") {
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let child = state
        .create_child_session(parent, "writer", "write")
        .await
        .unwrap();
    let (_, stream) = provider.requests.recv().await.unwrap();
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
            if let navai::bus::ResumeOutcome::Replay(events) =
                state.subscribe(parent, None).await.unwrap().replay
            {
                if let Some(e) = events.iter().find(|e| {
                    e.kind == "permission.requested"
                        && e.payload["child_session_id"] == child.to_string()
                }) {
                    break e.payload["request_id"].as_str().unwrap().parse().unwrap();
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    state
        .answer_permission(
            parent,
            request_id,
            navai::agent::permissions::PermissionDecision::AllowOnce,
        )
        .await
        .unwrap();
    let (_, stream) = provider.requests.recv().await.unwrap();
    stream.send(text_chunk("child finished")).unwrap();
    drop(stream);
    notice_count(&state, parent, 1).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), provider.requests.recv())
            .await
            .is_err()
    );
    let transcript = state
        .history()
        .load(parent)
        .await
        .unwrap()
        .unwrap()
        .transcript;
    assert!(!transcript.iter().any(
        |m| matches!(m, ChatMessage::Tool { tool_call_id, .. } if tool_call_id == "parent-ask")
    ));
    state
        .send(Command::child_prompt(parent, "my answer".into()))
        .await
        .unwrap();
    let (request, stream) = provider.requests.recv().await.unwrap();
    assert_eq!(request_outputs(&request).len(), 1);
    assert!(request.contains("my answer"));
    state.close_all().await;
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
        .await
        .unwrap();
    store
        .record_session(child, "/ws", "default", None)
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
        .await
        .unwrap();
    store
        .finish_child_link(child, "done".into(), None, "answer".into())
        .await
        .unwrap();
    let payload = store.pending_completions(parent).await.unwrap()[0]
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
        .await
        .unwrap();
    drop(store);
    let store = navai::session::store::HistoryStore::open(&path).unwrap();
    assert!(store.pending_completions(parent).await.unwrap().is_empty());
    assert_eq!(
        store.unconsumed_completions(parent).await.unwrap()[0].2,
        "answer"
    );
    let transcript = vec![ChatMessage::User {
        content: "untrusted answer".into(),
    }];
    assert!(store
        .consume_completions(parent, vec![child, ulid::Ulid::new()], transcript.clone())
        .await
        .is_err());
    assert_eq!(store.unconsumed_completions(parent).await.unwrap().len(), 1);
    assert!(store
        .load(parent)
        .await
        .unwrap()
        .unwrap()
        .transcript
        .is_empty());
    store
        .consume_completions(parent, vec![child], transcript.clone())
        .await
        .unwrap();
    drop(store);
    let store = navai::session::store::HistoryStore::open(&path).unwrap();
    assert!(store
        .unconsumed_completions(parent)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .consume_completions(parent, vec![child], vec![])
        .await
        .is_err());
    assert_eq!(
        serde_json::to_value(store.load(parent).await.unwrap().unwrap().transcript).unwrap(),
        serde_json::to_value(transcript).unwrap()
    );
}
