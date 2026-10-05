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
async fn budget_parked_child_claims_interrupt_once_after_owner_continue() {
    let mut provider = spawn_streaming_provider().await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let parent = state.create_session(&ws, None, false).await.unwrap();
    let child = state
        .create_child_session(parent, "explore", "original delegated task")
        .await
        .unwrap();
    for step in 0..navai::agent::run_loop::max_steps() {
        let (_, stream) = provider
            .requests
            .recv()
            .bounded(file!(), line!())
            .await
            .unwrap();
        stream
            .send(tool_call_chunk(
                &format!("glob-{step}"),
                "glob",
                &json!({"pattern":"*"}),
            ))
            .unwrap();
        drop(stream);
    }
    common::server::collect_until(state.subscribe(child, None).await.unwrap(), |e| {
        e.kind == "turn.end"
    })
    .await;
    state
        .steer_child(parent, child, "accepted budget replacement", true)
        .await
        .unwrap();
    assert!(
        provider.requests.try_recv().is_err(),
        "interrupt cannot renew budget"
    );
    state.continue_child(parent, child).await.unwrap();
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
            .filter(|m| m["content"] == "accepted budget replacement")
            .count(),
        1
    );
    stream.send(text_chunk("finished replacement")).unwrap();
    drop(stream);
    common::server::collect_until(state.subscribe(child, None).await.unwrap(), |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;
    state.close_all().await;
}

#[tokio::test]
async fn completion_after_negative_eligibility_cannot_overtake_queued_driver() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let parent = state
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .send(common::server::driver_command(parent, "original".into()))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, parent_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let _child = state
        .create_child_with_delivery(parent, "explore", "inspect", "steer")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, child_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .send(common::server::driver_command(
            parent,
            "driver correction".into(),
        ))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let barrier = navai::session::ChildPreparationBarrier::default();
    state
        .pause_negative_result_eligibility(parent, barrier.clone())
        .unwrap();
    parent_stream
        .send(tool_call_chunk("glob", "glob", &json!({"pattern":"*"})))
        .unwrap();
    drop(parent_stream);
    barrier.reached.notified().bounded(file!(), line!()).await;
    child_stream.send(text_chunk("late child result")).unwrap();
    drop(child_stream);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while state
            .history()
            .unconsumed_completions(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    barrier.release.notify_one();
    let (request, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(!request.contains("driver correction"));
    assert!(!request.contains("late child result"));
    stream.send(text_chunk("natural boundary")).unwrap();
    drop(stream);
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
                .is_some_and(|s| s.contains("late child result"))
        })
        .unwrap();
    assert!(driver < result);
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn two_interrupts_deliver_both_inputs_in_admission_order() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let parent = state
        .create_session(&ws, None, false)
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
        .steer_child(parent, child, "interrupt first", true)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .steer_child(parent, child, "interrupt second", true)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let mut streams = Vec::new();
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (request, stream) = provider.requests.recv().await.unwrap();
            streams.push(stream);
            if request.contains("interrupt second") {
                break request;
            }
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    let b = body(&request);
    let messages = b["messages"].as_array().unwrap();
    let first = messages
        .iter()
        .position(|m| m["content"] == "interrupt first")
        .unwrap();
    let second = messages
        .iter()
        .position(|m| m["content"] == "interrupt second")
        .unwrap();
    assert!(first < second);
    assert_eq!(
        messages
            .iter()
            .filter(|m| m["content"] == "interrupt first")
            .count(),
        1
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| m["content"] == "interrupt second")
            .count(),
        1
    );
    state.close_all().bounded(file!(), line!()).await;
    drop(old);
    drop(streams);
}

#[tokio::test]
async fn result_interrupt_replacement_uses_accepted_deferred_model() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let config = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .config();
    let path = config.models_path.as_ref().unwrap();
    let mut models: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    models["models"]["mock/replacement-model"] = models["models"]["mock/mock-model"].clone();
    models["models"]["mock/replacement-model"]["model"] = json!("replacement-model");
    std::fs::write(path, models.to_string()).unwrap();
    let state = AppState::new(config).unwrap();
    let parent = state
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .send(common::server::driver_command(parent, "driver".into()))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, old) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let _child = state
        .create_child_with_delivery(parent, "explore", "inspect", "interrupt")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, child_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let command = state
        .run_command(parent, "model", "mock/replacement-model")
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(command.ok);
    child_stream.send(text_chunk("result")).unwrap();
    drop(child_stream);
    let (request, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(body(&request)["model"], "replacement-model");
    assert!(request.contains("untrusted_subagent_output"));
    state.close_all().bounded(file!(), line!()).await;
    drop(old);
    drop(stream);
}

#[tokio::test]
async fn interrupt_held_automatic_summary_restores_valid_checkpoint_and_input_once() {
    use common::sse::{http_sse, spawn_holding_provider, sse_body};
    let reached = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider =
        spawn_holding_provider(8, 2, reached.clone(), release.clone(), |index, request| {
            let body: Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            let text = if body["tools"]
                .as_array()
                .is_none_or(|tools| tools.is_empty())
            {
                text_chunk("Goal: inspect sources. Next: finish.")
            } else if index < 2 {
                tool_call_chunk(&format!("read-{index}"), "read", &json!({"path":"big.txt"}))
            } else {
                text_chunk("finished after interruption")
            };
            http_sse(&sse_body(&[&text]))
        })
        .bounded(file!(), line!())
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    std::fs::write(ws.join("big.txt"), "lorem ipsum ".repeat(2000)).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(20000)
        .state();
    let parent = state
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = state
        .create_child_session(parent, "explore", "inspect the file twice")
        .bounded(file!(), line!())
        .await
        .unwrap();
    reached.notified().bounded(file!(), line!()).await;
    state
        .steer_child(parent, child, "replacement after summary", true)
        .bounded(file!(), line!())
        .await
        .unwrap();
    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while state.child_metadata(child).await.unwrap().unwrap().status == "running" {
            tokio::task::yield_now().await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    state
        .history()
        .flush()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, transcript, checkpoint) = state
        .history()
        .load_for_resume(child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    // A cancelled summary may leave no checkpoint; any committed one must
    // restore against the actual paired transcript, never a partial summary.
    if let Some(checkpoint) = checkpoint {
        navai::agent::compaction::validate(&checkpoint, &transcript).unwrap();
    }
    assert_eq!(transcript.iter().filter(|m|matches!(m,navai::provider::openai::ChatMessage::User {content,..} if content=="replacement after summary")).count(),1);
    state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn queued_driver_without_result_preserves_running_turn_and_budget() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let parent = state
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .send(common::server::driver_command(parent, "original".into()))
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
        .send(common::server::driver_command(
            parent,
            "next turn driver".into(),
        ))
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
    assert!(!request.contains("next turn driver"));
    let events = replay(&state, parent).bounded(file!(), line!()).await;
    assert_eq!(events.iter().filter(|e| e.kind == "turn.start").count(), 1);
    assert!(!events.iter().any(|e| e.kind == "turn.end"));
    stream.send(text_chunk("natural finish")).unwrap();
    drop(stream);
    let (request, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(request.contains("next turn driver"));
    let events = replay(&state, parent).bounded(file!(), line!()).await;
    assert!(events
        .iter()
        .any(|e| e.kind == "turn.end" && e.payload["outcome"] == "done"));
    assert!(!events
        .iter()
        .any(|e| e.kind == "turn.end" && e.payload["outcome"] == "interrupted"));
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn queued_driver_precedes_mid_turn_result_steer() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let parent = state
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .send(common::server::driver_command(parent, "original".into()))
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
        .create_child_with_delivery(parent, "explore", "inspect", "steer")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, child_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .send(common::server::driver_command(
            parent,
            "driver correction".into(),
        ))
        .bounded(file!(), line!())
        .await
        .unwrap();
    child_stream.send(text_chunk("child result")).unwrap();
    drop(child_stream);
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
        .send(tool_call_chunk("glob", "glob", &json!({"pattern":"*"})))
        .unwrap();
    drop(parent_stream);
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
    let notice = messages
        .iter()
        .position(|m| {
            m["content"]
                .as_str()
                .is_some_and(|s| s.contains("untrusted_subagent_output"))
        })
        .unwrap();
    assert!(driver < notice);
    assert!(messages
        .iter()
        .any(|m| m["role"] == "tool" && m["tool_call_id"] == "glob"));
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
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
        .create_session(&ws, None, false)
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
        .create_session(&ws, None, false)
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
        .send(json!({"choices":[{"index":0,"finish_reason":"tool_calls","delta":{"tool_calls":[
            {"index":0,"id":"glob","function":{"name":"glob","arguments":"{\"pattern\":\"*\"}"}},
            {"index":1,"id":"glob-second","function":{"name":"glob","arguments":"{\"pattern\":\"*\"}"}}
        ]}}]}).to_string())
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
    let second = messages
        .iter()
        .position(|m| m["role"] == "tool" && m["tool_call_id"] == "glob-second")
        .unwrap();
    assert!(second < input);
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
        .create_session(&ws, None, false)
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
    let _ = old.send(text_chunk("late stale provider chunk"));
    assert!(!request.contains("late stale provider chunk"));
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
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let other = state
        .create_session(&ws, None, false)
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
            .create_session(&ws, None, false)
            .bounded(file!(), line!())
            .await
            .unwrap();
        state
            .send(common::server::driver_command(
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
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .send(common::server::driver_command(
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
        .send(common::server::driver_command(
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
        .create_session(&ws, None, false)
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
