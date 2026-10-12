//! The context budget through the service: the snapshot after a scripted
//! turn, `context.updated`, `GET /v1/sessions/{id}/context`, `/context`, and
//! a window too small to hold its own reserve.

use axum::http::StatusCode;
use serde_json::{json, Value};

use withnav::server::{app, AppState};

mod common;
use common::server::{
    collect_until, post_message_ok, request_json, spawn_server, TestServerConfig,
};
use common::sse::{http_sse, spawn_capturing_provider, sse_body, text_chunk, tool_call_chunk};

fn tmp_ws() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("ws");
    (tmp, ws)
}

fn usage_chunk(prompt: u64, completion: u64) -> String {
    json!({"choices": [], "usage": {
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
    }})
    .to_string()
}

fn estimate_of(snapshot: &Value) -> u64 {
    snapshot["estimate_total"].as_u64().expect("estimate_total")
}

fn category_sum(snapshot: &Value) -> u64 {
    ["system", "rules", "skills_catalog", "tools", "messages"]
        .iter()
        .map(|k| snapshot["breakdown"][k].as_u64().expect(k))
        .sum()
}

/// A session on a 1M window whose model reads a file, then answers, with
/// the provider reporting usage on both steps. Returns the state, the id and
/// the events up to the turn end.
async fn scripted_turn(
    tmp: &std::path::Path,
    ws: &std::path::Path,
) -> (
    AppState,
    ulid::Ulid,
    Vec<withnav::bus::event::EventEnvelope>,
) {
    let notes = ws.join("notes.txt");
    let call = tool_call_chunk(
        "call_1",
        "read",
        &json!({"path": notes.display().to_string()}),
    );
    let first_usage = usage_chunk(1200, 40);
    let done = text_chunk("all read");
    let second_usage = usage_chunk(1500, 5);
    let provider = spawn_capturing_provider(vec![
        http_sse(&sse_body(&[&call, &first_usage])),
        http_sse(&sse_body(&[&done, &second_usage])),
    ])
    .await;
    std::fs::write(&notes, "x".repeat(4000)).expect("notes");
    let state = TestServerConfig::mock(tmp, &provider.url)
        .chat_tools(1_000_000)
        .state();
    let (addr, _task) = spawn_server(state.clone()).await;
    let id = state.create_session(ws, None, false).await.expect("create");
    let sub = state.subscribe(id, None).await.expect("sub");
    post_message_ok(&format!("http://{addr}"), &id.to_string(), "read the notes").await;
    let events = collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;
    // The server task dies with the test; the provider mock is consumed.
    (state, id, events)
}

// Given a fresh session
// When its context is read before any message
// Then it holds the fixed parts only and the budget of its window
#[tokio::test]
async fn a_fresh_session_has_a_snapshot_with_the_fixed_parts() {
    let (tmp, ws) = tmp_ws();
    let state = TestServerConfig::unreachable(tmp.path())
        .chat_tools(1_000_000)
        .state();
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
    let snap = serde_json::to_value(state.context(id).await.expect("context")).unwrap();
    assert_eq!(snap["model"], "mock/mock-model");
    assert_eq!(snap["gateway"], "mock");
    assert_eq!(snap["upstream_id"], "mock-model");
    assert!(snap.get("alias").is_none());
    assert_eq!(snap["window"], 1_000_000);
    assert_eq!(snap["window_source"], "models.json");
    assert_eq!(snap["revision"], 0);
    assert_eq!(snap["breakdown"]["messages"], 0);
    assert!(snap["breakdown"]["system"].as_u64().unwrap() > 0);
    assert!(snap["breakdown"]["tools"].as_u64().unwrap() > 0);
    assert_eq!(estimate_of(&snap), category_sum(&snap));
    assert_eq!(snap["output_reserve"], 8192);
    assert_eq!(snap["margin"], 50_000);
    assert_eq!(snap["input_budget"], 1_000_000 - 8192 - 50_000);
    assert!(snap.get("last_usage").is_none());
}

// Given a turn with a tool call and provider usage on each step
// When it ends
// Then the snapshot has the grown history, the provider's last count and a higher revision
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_snapshot_after_a_scripted_turn_keeps_estimate_and_usage_apart() {
    let (tmp, ws) = tmp_ws();
    let fresh = TestServerConfig::unreachable(tmp.path())
        .chat_tools(1_000_000)
        .state();
    let fresh_id = fresh
        .create_session(&ws, None, false)
        .await
        .expect("create");
    let before = serde_json::to_value(fresh.context(fresh_id).await.unwrap()).unwrap();

    let tmp2 = tempfile::tempdir().unwrap();
    let (state, id, events) = scripted_turn(tmp2.path(), &ws).await;
    let snap = serde_json::to_value(state.context(id).await.expect("context")).unwrap();

    assert_eq!(estimate_of(&snap), category_sum(&snap));
    assert!(
        snap["breakdown"]["messages"].as_u64().unwrap() > 1000,
        "the 4000-byte file output is in the history: {snap}"
    );
    assert_eq!(snap["breakdown"]["system"], before["breakdown"]["system"]);
    assert_eq!(snap["breakdown"]["tools"], before["breakdown"]["tools"]);
    assert!(estimate_of(&snap) > estimate_of(&before));
    // The provider's count for the LAST step, not a sum and not the estimate.
    assert_eq!(snap["last_usage"]["prompt_tokens"], 1500);
    assert_eq!(snap["last_usage"]["completion_tokens"], 5);
    assert_eq!(snap["last_usage"]["model"], "mock/mock-model");
    assert_eq!(snap["last_usage"]["gateway"], "mock");
    assert_eq!(snap["last_usage"]["upstream_id"], "mock-model");
    assert!(snap["last_usage"].get("alias").is_none());
    assert_ne!(snap["last_usage"]["prompt_tokens"], snap["estimate_total"]);
    let last_response = events
        .iter()
        .rev()
        .find(|e| e.kind == "llm.response")
        .expect("llm.response");
    assert_eq!(snap["last_usage"]["step"], last_response.payload["step_id"]);
    assert!(snap["revision"].as_u64().unwrap() >= 3);
    assert_eq!(snap["percent"], 0);
}

// Given a scripted turn
// When its events are read
// Then every context.updated carries a snapshot, revisions rise, the last equals the endpoint, and none is stored
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_updated_is_emitted_at_boundaries_and_never_stored() {
    let (tmp, ws) = tmp_ws();
    let (state, id, events) = scripted_turn(tmp.path(), &ws).await;
    let updates: Vec<&withnav::bus::event::EventEnvelope> = events
        .iter()
        .filter(|e| e.kind == "context.updated")
        .collect();
    assert!(updates.len() >= 3, "{updates:?}");
    let revisions: Vec<u64> = updates
        .iter()
        .map(|e| e.payload["revision"].as_u64().unwrap())
        .collect();
    assert!(revisions.windows(2).all(|w| w[1] > w[0]), "{revisions:?}");
    for e in &updates {
        assert_eq!(
            e.payload["estimate_total"].as_u64().unwrap(),
            category_sum(&e.payload)
        );
        assert_eq!(e.payload["window"], 1_000_000);
    }
    // The first update of the turn already counts the user message.
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let turn_start = kinds.iter().position(|k| *k == "turn.start").unwrap();
    let first_update = kinds.iter().position(|k| *k == "context.updated").unwrap();
    let first_request = kinds.iter().position(|k| *k == "llm.delta").unwrap();
    assert!(turn_start < first_update && first_update < first_request);

    let snap = serde_json::to_value(state.context(id).await.unwrap()).unwrap();
    assert_eq!(&snap, &updates.last().unwrap().payload);

    // Live-only: the stored history has no context.updated.
    state.history().flush().await.expect("flush");
    let sub = state.subscribe(id, None).await.expect("subscribe");
    match sub.replay {
        withnav::bus::ResumeOutcome::Replay(replayed) => {
            assert!(replayed.iter().all(|e| e.kind != "context.updated"));
            assert!(replayed.iter().any(|e| e.kind == "turn.end"));
        }
        other => panic!("expected replay, got {other:?}"),
    }
}

// Given the HTTP surface
// When the context is read
// Then it is the snapshot; an unknown id is 404 and a stopped conversation 409
#[tokio::test]
async fn the_http_endpoint_serves_the_snapshot() {
    let (tmp, ws) = tmp_ws();
    let state = TestServerConfig::unreachable(tmp.path())
        .chat_tools(200_000)
        .state();
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
    let router = app(state.clone());
    let path = format!("/v1/sessions/{id}/context");
    let (status, body) = request_json(&router, "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["model"], "mock/mock-model");
    assert_eq!(body["window"], 200_000);
    assert_eq!(body["input_budget"], 200_000 - 8192 - 10_000);
    assert_eq!(estimate_of(&body), category_sum(&body));

    let (status, _) = request_json(
        &router,
        "GET",
        &format!("/v1/sessions/{}/context", ulid::Ulid::generate()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = request_json(&router, "GET", "/v1/sessions/not-an-id/context", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    state.close_session(id).await.expect("close");
    state.history().flush().await.expect("flush");
    let (status, _) = request_json(&router, "GET", &path, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

// Given a session
// When /context runs
// Then it answers the snapshot and renders the gauge line and the breakdown
#[tokio::test]
async fn the_context_command_renders_the_gauge_and_the_breakdown() {
    let (tmp, ws) = tmp_ws();
    let state = TestServerConfig::unreachable(tmp.path())
        .chat_tools(1_000_000)
        .state();
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
    let out = state.run_command(id, "/context", "").await.expect("run");
    assert!(out.ok, "{out:?}");
    let data = out.data.clone().expect("data");
    assert_eq!(data["window"], 1_000_000);
    let text = out.render_text();
    let first = text.lines().next().unwrap();
    assert!(
        first.contains(" / 1,000k · 0% · input budget 942k"),
        "{first}"
    );
    for name in ["system", "rules", "skills catalog", "tools", "messages"] {
        assert!(text.contains(name), "{text}");
    }
}

// Given a conversation stopped after a high revision
// When it is resumed under the same id
// Then its snapshot is a new incarnation with revisions starting again
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_session_has_a_new_incarnation() {
    let (tmp, ws) = tmp_ws();
    let (state, id, _) = scripted_turn(tmp.path(), &ws).await;
    let before = serde_json::to_value(state.context(id).await.unwrap()).unwrap();
    assert!(before["revision"].as_u64().unwrap() >= 3);
    state.close_session(id).await.expect("close");
    state.history().flush().await.expect("flush");
    let (resumed_id, resumed) = state
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .expect("resume");
    assert!(resumed && resumed_id == id);
    let after = serde_json::to_value(state.context(id).await.unwrap()).unwrap();
    assert_eq!(after["revision"], 0);
    assert_ne!(after["incarnation"], before["incarnation"]);
    // The history is back in the estimate.
    assert!(after["breakdown"]["messages"].as_u64().unwrap() > 1000);
}

// Given a session
// When /context gets an argument
// Then it is refused with its usage, like every argument-free command
#[tokio::test]
async fn the_context_command_takes_no_arguments() {
    let (tmp, ws) = tmp_ws();
    let state = TestServerConfig::unreachable(tmp.path())
        .chat_tools(1_000_000)
        .state();
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
    let out = state.run_command(id, "context", "now").await.expect("run");
    assert!(!out.ok);
    assert_eq!(out.error.as_deref(), Some("usage: /context"));
}

// Given a window that cannot hold its output reserve and margin
// When a session is created
// Then it is refused with a clear error naming models.json
#[tokio::test]
async fn an_impossible_budget_refuses_the_session() {
    let (tmp, ws) = tmp_ws();
    let state = TestServerConfig::unreachable(tmp.path())
        .chat_tools(4096)
        .max_output(4096)
        .state();
    let err = state
        .create_session(&ws, None, false)
        .await
        .expect_err("refused");
    assert!(
        matches!(err, withnav::error::ServerError::ImpossibleBudget { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("models.json"), "{err}");
    let router = app(state);
    let (status, body) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({"workspace": ws.display().to_string()})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["reason"], "impossible_context_budget");
}
