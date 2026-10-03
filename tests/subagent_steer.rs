use navai::server::AppState;
use serde_json::{json, Value};
mod common;
use common::deadline::Deadline;
use common::server::TestServerConfig;
use common::sse::{spawn_streaming_provider, text_chunk, tool_call_chunk};

fn body(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

#[tokio::test]
async fn accepted_guidance_prevents_obsolete_final_response_terminalization() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
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
        .steer_child(parent, child, "follow up before finishing", false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream.send(text_chunk("obsolete final")).unwrap();
    drop(stream);
    let (request, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(request.contains("follow up before finishing"));
    assert_eq!(
        state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    assert_eq!(state.reserved_child_count(parent), 1);
    stream.send(text_chunk("revised final")).unwrap();
    drop(stream);
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(
        state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .result_text
            .as_deref(),
        Some("revised final")
    );
    assert!(state
        .steer_child(parent, child, "late", false)
        .bounded(file!(), line!())
        .await
        .is_err());
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn steer_during_provider_request_lands_after_the_complete_tool_batch() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
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
        .steer_child(parent, child, "parent guidance", false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream
        .send(tool_call_chunk("glob", "glob", &json!({"pattern":"*"})))
        .unwrap();
    drop(stream);
    let (request, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let b = body(&request);
    let messages = b["messages"].as_array().unwrap();
    let tool = messages.iter().position(|m| m["role"] == "tool").unwrap();
    let input = messages
        .iter()
        .position(|m| m["content"] == "parent guidance")
        .unwrap();
    assert!(tool < input);
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn interrupt_provider_request_keeps_child_session_and_starts_a_fresh_turn() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state: AppState = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
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
    let (_, old) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .steer_child(parent, child, "replace task", true)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (request, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(request.contains("replace task"));
    assert_eq!(
        state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    stream.send(text_chunk("finished")).unwrap();
    drop(stream);
    drop(old);
    state.close_all().bounded(file!(), line!()).await;
}

async fn replay(state: &AppState, id: ulid::Ulid) -> Vec<navai::bus::event::EventEnvelope> {
    state
        .history()
        .flush()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let navai::bus::ResumeOutcome::Replay(events) = state
        .subscribe(id, None)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .replay
    else {
        panic!("replay");
    };
    events
}

#[tokio::test]
async fn parked_permission_queues_steer_and_interrupt_cancels_without_running_call() {
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
        .chat_tools(128000)
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
        .create_child_session(parent, "writer", "inspect")
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
            "held",
            "bash",
            &json!({"command":"touch forbidden"}),
        ))
        .unwrap();
    drop(stream);
    let requested = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(e) = replay(&state, child)
                .await
                .into_iter()
                .find(|e| e.kind == "permission.requested")
            {
                break e;
            }
            tokio::task::yield_now().await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    assert!(state
        .steer_child(other, child, "foreign", false)
        .bounded(file!(), line!())
        .await
        .is_err());
    assert!(state
        .run_command(child, "compact", "")
        .bounded(file!(), line!())
        .await
        .is_err());
    assert!(state
        .run_command(child, "model", "default")
        .bounded(file!(), line!())
        .await
        .is_err());
    state
        .steer_child(parent, child, "queued steer", false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(50),
        provider.requests.recv()
    )
    .bounded(file!(), line!())
    .await
    .is_err());
    state
        .steer_child(parent, child, "replace pending", true)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (request, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(request.contains("queued steer") && request.contains("replace pending"));
    assert!(!ws.join("forbidden").exists());
    let messages = body(&request)["messages"].as_array().unwrap().clone();
    assert_eq!(
        messages
            .iter()
            .filter(|m| m["role"] == "tool" && m["tool_call_id"] == "held")
            .count(),
        1
    );
    let events = replay(&state, child).bounded(file!(), line!()).await;
    assert!(events.iter().any(|e| e.kind == "permission.resolved"
        && e.payload["request_id"] == requested.payload["request_id"]
        && e.payload["resolution"] == "cancelled"));
    state
        .close_session(child)
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(state
        .steer_child(parent, child, "too late", false)
        .bounded(file!(), line!())
        .await
        .is_err());
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn result_delivery_steer_is_mid_turn_and_interrupt_restarts_parent() {
    for mode in ["steer", "interrupt"] {
        let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        let state = TestServerConfig::mock(tmp.path(), &provider.url)
            .chat_tools(128000)
            .state();
        let parent = state
            .create_session(&ws, None, None)
            .bounded(file!(), line!())
            .await
            .unwrap();
        state
            .send(navai::bus::command::Command::child_prompt(
                parent,
                "driver instruction".into(),
            ))
            .bounded(file!(), line!())
            .await
            .unwrap();
        let (_, parent_stream) = provider
            .requests
            .recv()
            .bounded(file!(), line!())
            .await
            .unwrap();
        let child = state
            .create_child_with_delivery(parent, "explore", "inspect", mode)
            .bounded(file!(), line!())
            .await
            .unwrap();
        let (_, child_stream) = provider
            .requests
            .recv()
            .bounded(file!(), line!())
            .await
            .unwrap();
        child_stream.send(text_chunk("result delivered")).unwrap();
        drop(child_stream);
        let mut parent_stream = Some(parent_stream);
        if mode == "steer" {
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while state
                    .child_metadata(child)
                    .bounded(file!(), line!())
                    .await
                    .unwrap()
                    .unwrap()
                    .status
                    == "running"
                {
                    tokio::task::yield_now().await;
                }
            })
            .bounded(file!(), line!())
            .await
            .unwrap();
            parent_stream
                .as_ref()
                .unwrap()
                .send(tool_call_chunk("glob", "glob", &json!({"pattern":"*"})))
                .unwrap();
            drop(parent_stream.take());
        }
        let (request, stream) = provider
            .requests
            .recv()
            .bounded(file!(), line!())
            .await
            .unwrap();
        assert!(request.contains("result delivered"));
        let events = replay(&state, parent).bounded(file!(), line!()).await;
        let starts = events.iter().filter(|e| e.kind == "turn.start").count();
        if mode == "steer" {
            assert_eq!(starts, 1);
        } else {
            assert_eq!(starts, 2);
            assert!(events
                .iter()
                .any(|e| e.kind == "turn.end" && e.payload["outcome"] == "interrupted"));
        }
        state.close_all().bounded(file!(), line!()).await;
        drop(stream);
        drop(parent_stream);
    }
}

#[tokio::test]
async fn interrupt_result_waits_for_parent_permission_and_queued_driver_goes_first() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let parent = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .send(navai::bus::command::Command::child_prompt(
            parent,
            "driver instruction".into(),
        ))
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
            "held",
            "bash",
            &json!({"command":"touch forbidden"}),
        ))
        .unwrap();
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !replay(&state, parent)
            .await
            .iter()
            .any(|e| e.kind == "permission.requested")
        {
            tokio::task::yield_now().await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    let child = state
        .create_child_with_delivery(parent, "explore", "inspect", "interrupt")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream.send(text_chunk("result")).unwrap();
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status
            == "running"
        {
            tokio::task::yield_now().await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(50),
        provider.requests.recv()
    )
    .bounded(file!(), line!())
    .await
    .is_err());
    assert!(!replay(&state, parent)
        .bounded(file!(), line!())
        .await
        .iter()
        .any(|e| e.kind == "permission.resolved"));
    state
        .send(navai::bus::command::Command::child_prompt(
            parent,
            "driver correction".into(),
        ))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (request, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let b = body(&request);
    let messages = b["messages"].as_array().unwrap();
    let driver = messages
        .iter()
        .position(|m| m["content"] == "driver correction")
        .unwrap();
    let result = messages
        .iter()
        .position(|m| {
            m["content"]
                .as_str()
                .is_some_and(|s| s.contains("untrusted_subagent_output"))
        })
        .unwrap();
    assert!(driver < result);
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn interrupt_during_running_tool_preserves_result_pairing_and_capability_ceiling() {
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
        .chat_tools(128000)
        .state();
    let parent = state
        .create_session(&ws, None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = state
        .create_child_session(parent, "writer", "inspect")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (first, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let schemas = body(&first)["tools"].as_array().unwrap().clone();
    assert!(!schemas.iter().any(|tool| matches!(
        tool["function"]["name"].as_str(),
        Some("task" | "ask" | "steer_subagent")
    )));
    stream
        .send(tool_call_chunk(
            "long-tool",
            "bash",
            &json!({"command":"touch started; sleep 30"}),
        ))
        .unwrap();
    drop(stream);
    let id = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(e) = replay(&state, child)
                .await
                .into_iter()
                .find(|e| e.kind == "permission.requested")
            {
                break e.payload["request_id"].as_str().unwrap().parse().unwrap();
            }
            tokio::task::yield_now().await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    state
        .answer_permission(
            parent,
            id,
            navai::agent::permissions::PermissionDecision::AllowOnce,
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !ws.join("started").exists() {
            tokio::task::yield_now().await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    state
        .steer_child(parent, child, "abort slow work", true)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (request, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let messages = body(&request)["messages"].as_array().unwrap().clone();
    assert_eq!(
        messages
            .iter()
            .filter(|m| m["role"] == "tool" && m["tool_call_id"] == "long-tool")
            .count(),
        1
    );
    assert!(request.contains("abort slow work"));
    assert_eq!(
        state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}
