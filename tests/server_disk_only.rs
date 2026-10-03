//! A conversation that only the store holds (after a restart): a fresh
//! `AppState` on the same database file answers GET, SSE and POST over HTTP
//! from the stored history, and an unknown or deleted id stays a 404.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use navai::bus::event::EventEnvelope;
use navai::server::AppState;

mod common;
use common::server::{post_message, post_message_ok, spawn_server, TestServerConfig};
use common::sse::{http_sse, read_sse, spawn_mock_provider, sse_body, SseEnd, SseFrame};

/// One finished conversation on a first server, then a second, fresh server
/// on the same database file. Returns the second server's base URL, the
/// conversation id, the events the first server served for it and the
/// second server's state; the temp root and the server task stay alive.
struct Restarted {
    base: String,
    id: String,
    events: Vec<EventEnvelope>,
    state: AppState,
    _root: tempfile::TempDir,
    _task: tokio::task::JoinHandle<()>,
}

async fn restarted_after_one_turn() -> Restarted {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let mock = spawn_mock_provider(vec![http_sse(&sse_body(&[done]))]).await;
    let root = tempfile::tempdir().expect("tempdir");
    let ws = root.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");
    let config = TestServerConfig::mock(root.path(), &mock.url);

    let first = config.state();
    let (addr, first_task) = spawn_server(first.clone()).await;
    let base = format!("http://{addr}");
    let id = common::server::create_session(&base, &ws).await;
    let parsed: ulid::Ulid = id.parse().expect("ulid");
    post_message_ok(&base, &id, "go").await;
    let mut events = Vec::new();
    for _ in 0..100 {
        events = stored_events(&first, parsed).await;
        if events.iter().any(|e| e.kind == "turn.end") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(events.iter().any(|e| e.kind == "turn.end"), "{events:?}");
    let stop = reqwest::Client::new()
        .post(format!("{base}/v1/sessions/{id}/stop"))
        .send()
        .await
        .expect("stop");
    assert_eq!(stop.status(), 200);
    let events = stored_events(&first, parsed).await;
    assert_eq!(events.last().map(|e| e.kind.as_str()), Some("session.end"));
    first_task.abort();
    drop(first);

    let second = config.state();
    let (addr, task) = spawn_server(second.clone()).await;
    Restarted {
        base: format!("http://{addr}"),
        id,
        events,
        state: second,
        _root: root,
        _task: task,
    }
}

async fn stored_events(state: &AppState, id: ulid::Ulid) -> Vec<EventEnvelope> {
    let sub = state.subscribe(id, None).await.expect("subscribe");
    match sub.replay {
        navai::bus::ResumeOutcome::Replay(events) => events,
        other => panic!("expected a replay: {other:?}"),
    }
}

/// The SSE frames of `GET /events`; the server must end the stream itself.
async fn sse_frames(base: &str, id: &str, last_event_id: Option<u64>) -> Vec<SseFrame> {
    let read = read_sse(base, id, last_event_id, |_| false).await;
    assert_eq!(read.end, SseEnd::Eof, "the stream ends: {:?}", read.frames);
    read.frames
}

fn seqs(frames: &[SseFrame]) -> Vec<u64> {
    frames.iter().map(|f| f.seq().expect("seq")).collect()
}

// Given a session that ran a turn and was stopped in this same process
// When the registry is inspected and GET, SSE and POST ask for the id
// Then the registry no longer holds it, and all three answer from the store:
//      GET 200 stopped at the stored last_seq, SSE the stored replay ending
//      with no live stream, POST 409 resume_required
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_history_survives_registry_removal() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let mock = spawn_mock_provider(vec![http_sse(&sse_body(&[done]))]).await;
    let root = tempfile::tempdir().expect("tempdir");
    let ws = root.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");
    let state = TestServerConfig::mock(root.path(), &mock.url).state();
    let (addr, task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");

    let id = common::server::create_session(&base, &ws).await;
    let parsed: ulid::Ulid = id.parse().expect("ulid");
    post_message_ok(&base, &id, "go").await;
    for _ in 0..100 {
        let events = stored_events(&state, parsed).await;
        if events.iter().any(|e| e.kind == "turn.end") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    state.close_session(parsed).await.expect("close");

    assert_eq!(state.session_count(), 0, "the registry holds no entry");
    let stored = stored_events(&state, parsed).await;
    let last = stored.last().expect("stored events");
    assert_eq!(last.kind, "session.end", "session.end is stored");

    let summary: Value = reqwest::get(format!("{base}/v1/sessions/{id}"))
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(summary["actor_state"], "stopped");
    assert_eq!(summary["current_seq"], last.seq);

    let frames = sse_frames(&base, &id, None).await;
    let all: Vec<u64> = stored.iter().map(|e| e.seq).collect();
    assert_eq!(seqs(&frames), all, "the stored history replays, then ends");
    let sub = state.subscribe(parsed, None).await.expect("subscribe");
    assert!(sub.live.is_none(), "no live stream after the stop");

    let post = post_message(&base, &id, "late").await;
    assert_eq!(post.status(), 409);
    let body: Value = post.json().await.expect("json");
    assert_eq!(body["reason"], "resume_required", "{body}");
    assert_eq!(state.session_count(), 0, "answering made no entry");
    task.abort();
}

// Given a stored conversation and a restarted server
// When GET /v1/sessions/{id} asks for it
// Then 200, stopped, current_seq == the stored last_seq
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restarted_server_answers_get_for_a_stored_conversation() {
    let restarted = restarted_after_one_turn().await;
    let last = restarted.events.last().expect("events").seq;

    let response = reqwest::get(format!("{}/v1/sessions/{}", restarted.base, restarted.id))
        .await
        .expect("get");
    assert_eq!(response.status(), 200);
    let summary: Value = response.json().await.expect("json");
    assert_eq!(summary["actor_state"], "stopped");
    assert_eq!(summary["current_seq"], last);
    assert_eq!(summary["session_id"], restarted.id.as_str());
    assert_eq!(summary["agent"], "build", "a legacy row runs as build");
    let mut keys: Vec<&str> = summary
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "actor_state",
            "agent",
            "current_seq",
            "model",
            "session_id",
            "workspace"
        ]
    );
    assert_eq!(restarted.state.session_count(), 0, "no memory slot made");
}

// Given a stored conversation and a restarted server
// When SSE connects with no cursor, with a cursor inside the history, at the
//      last seq, and past it
// Then the stored history replays (all / after k / nothing) and the stream
//      ends; a cursor past the end is the invalid-cursor frame
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restarted_server_replays_the_stored_history_over_sse() {
    let restarted = restarted_after_one_turn().await;
    let all: Vec<u64> = restarted.events.iter().map(|e| e.seq).collect();
    let last = *all.last().expect("events");

    let frames = sse_frames(&restarted.base, &restarted.id, None).await;
    assert_eq!(seqs(&frames), all, "everything, seq 0 included");
    let replayed: Vec<EventEnvelope> = frames
        .iter()
        .map(|f| serde_json::from_str(&f.data).expect("envelope"))
        .collect();
    assert_eq!(replayed, restarted.events);

    let frames = sse_frames(&restarted.base, &restarted.id, Some(1)).await;
    assert_eq!(seqs(&frames), all[2..].to_vec(), "after the cursor");

    let frames = sse_frames(&restarted.base, &restarted.id, Some(last)).await;
    assert!(frames.is_empty(), "nothing after the last seq: {frames:?}");

    let frames = sse_frames(&restarted.base, &restarted.id, Some(last + 1)).await;
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].event, "resync");
}

// Given a stored conversation, a resumed-then-stopped one and an unknown id
// When a message is posted to each
// Then the stored and the stopped ones answer 409 with a structured
//      resume-first body, and the unknown id answers 404
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn posting_to_a_stopped_conversation_says_to_resume_first() {
    let restarted = restarted_after_one_turn().await;

    let stored = post_message(&restarted.base, &restarted.id, "late").await;
    assert_eq!(stored.status(), 409);
    let body: Value = stored.json().await.expect("json");
    assert_eq!(body["reason"], "resume_required", "{body}");
    assert!(
        body["error"].as_str().is_some_and(|e| e.contains("resume")),
        "{body}"
    );

    let unknown = post_message(&restarted.base, &ulid::Ulid::new().to_string(), "x").await;
    assert_eq!(unknown.status(), 404);

    // Resumed and stopped again: the store answers the same way.
    let resumed: Value = common::server::post_create_session(
        &restarted.base,
        &json!({ "workspace": Path::new("/ignored"), "resume": restarted.id }),
    )
    .await
    .json()
    .await
    .expect("json");
    assert_eq!(resumed["session_id"], restarted.id.as_str());
    let stop = reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{}/stop",
            restarted.base, restarted.id
        ))
        .send()
        .await
        .expect("stop");
    assert_eq!(stop.status(), 200);
    let stopped = post_message(&restarted.base, &restarted.id, "late").await;
    assert_eq!(stopped.status(), 409);
    let body: Value = stopped.json().await.expect("json");
    assert_eq!(body["reason"], "resume_required", "{body}");
}

// Given a stored conversation that is then deleted, and an id never stored
// When GET, SSE and POST ask for them
// Then every endpoint answers 404
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_and_deleted_ids_stay_404_everywhere() {
    let restarted = restarted_after_one_turn().await;
    let client = reqwest::Client::new();
    let deleted = client
        .delete(format!("{}/v1/sessions/{}", restarted.base, restarted.id))
        .send()
        .await
        .expect("delete");
    assert_eq!(deleted.status(), 204);

    for id in [restarted.id.clone(), ulid::Ulid::new().to_string()] {
        let get = reqwest::get(format!("{}/v1/sessions/{id}", restarted.base))
            .await
            .expect("get");
        assert_eq!(get.status(), 404, "GET {id}");
        let sse = reqwest::get(format!("{}/v1/sessions/{id}/events", restarted.base))
            .await
            .expect("sse");
        assert_eq!(sse.status(), 404, "SSE {id}");
        assert_eq!(post_message(&restarted.base, &id, "x").await.status(), 404);
    }
}
