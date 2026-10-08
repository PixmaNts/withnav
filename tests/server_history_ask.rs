//! A conversation resumed while the cognition was waiting on the Captain:
//! the Captain's next message must answer the dangling `ask` as its TOOL
//! result, not start a new user turn.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use withnav::bus::event::EventEnvelope;

mod common;
use common::server::{post_create_session, post_message_ok, spawn_server, TestServerConfig};
use common::sse::{http_sse, sse_body};

type Requests = Arc<Mutex<Vec<String>>>;

/// Answers every connection with `response`, recording the raw requests.
async fn spawn_mock(response: Vec<u8>) -> (String, Requests) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    let requests: Requests = Arc::default();
    let seen = requests.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = vec![0u8; 65536];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..n]).into_owned());
            let _ = sock.write_all(&response).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });
    (format!("http://{addr}/v1"), requests)
}

fn state(tmp: &Path, url: &str, db: &Path) -> withnav::server::AppState {
    TestServerConfig::mock(tmp, url)
        .model_alias("cognition", "mock-opus")
        .history_db(db.to_path_buf())
        .assigned_navigator_state()
}

async fn collect_until(
    sub: withnav::session::Subscription,
    pred: impl Fn(&EventEnvelope) -> bool,
) -> Vec<EventEnvelope> {
    let mut out = Vec::new();
    if let withnav::bus::ResumeOutcome::Replay(replay) = sub.replay {
        out.extend(replay);
    }
    if let Some(mut live) = sub.live {
        while let Some(env) = tokio::time::timeout(Duration::from_secs(5), live.recv())
            .await
            .ok()
            .flatten()
        {
            let matched = pred(&env);
            out.push(env);
            if matched {
                break;
            }
        }
    }
    out
}

// Given a cognition that yielded on `ask` and a conversation resumed in a
//      fresh AppState on the same database
// When the Captain sends a message
// Then the model request carries it as the `tool` message answering the ask's
//      id, and no user message other than the first
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_ask_is_answered_as_a_tool_result() {
    let ask = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"ask_1","function":{"name":"ask","arguments":"{\"question\":\"which scope?\"}"}}]},"finish_reason":"tool_calls"}]}"#;
    let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let db = root.join("sessions.db");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("ws");

    // Session 1: the cognition asks.
    let (url1, _) = spawn_mock(http_sse(&sse_body(&[ask]))).await;
    let state1 = state(root, &url1, &db);
    let (addr1, task1) = spawn_server(state1.clone()).await;
    let base1 = format!("http://{addr1}");
    let created: Value = post_create_session(&base1, &json!({ "workspace": ws, "editor": true }))
        .await
        .json()
        .await
        .expect("json");
    let id = created["session_id"].as_str().expect("id").to_string();
    let sub = state1
        .subscribe(id.parse().unwrap(), None)
        .await
        .expect("sub");
    post_message_ok(&base1, &id, "remove an auth scope").await;
    let events = collect_until(sub, |e| {
        e.kind == "agent.state" && e.payload["to"] == "yielded"
    })
    .await;
    assert!(events.iter().any(|e| e.kind == "agent.yield"), "asked");

    // Wait until the turn-end transcript snapshot is stored.
    let store = withnav::session::store::HistoryStore::open(&db).unwrap();
    let mut saved = false;
    for _ in 0..100 {
        let loaded = store.load(id.parse().unwrap()).await.unwrap().unwrap();
        if !loaded.transcript.is_empty() {
            saved = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(saved, "transcript snapshot stored");
    task1.abort();
    drop(state1);

    // Session 2: fresh state, same database.
    let (url2, requests2) = spawn_mock(http_sse(&sse_body(&[done]))).await;
    let tmp2 = tempfile::tempdir().expect("tempdir 2");
    let state2 = state(tmp2.path(), &url2, &db);
    // A resume refuses another endpoint; this test is about the pending ask.
    common::server::move_saved_endpoint(&db, &id, &url2);
    let (resumed_id, resumed) = state2
        .create_or_resume_session(&ws, None, false, Some(id.parse().unwrap()))
        .await
        .expect("resume");
    assert!(resumed);
    let (addr2, _task2) = spawn_server(state2.clone()).await;
    let base2 = format!("http://{addr2}");
    let sub2 = state2.subscribe(resumed_id, None).await.expect("sub2");
    post_message_ok(&base2, &id, "the local scope").await;
    let events2 = collect_until(sub2, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;
    assert!(
        events2
            .iter()
            .any(|e| e.kind == "turn.end" && e.payload["outcome"] == "done"),
        "resumed turn completed"
    );

    let requests = requests2.lock().unwrap().clone();
    let body: Value = serde_json::from_str(
        requests
            .last()
            .expect("provider called")
            .split("\r\n\r\n")
            .nth(1)
            .expect("body"),
    )
    .expect("request json");
    let messages = body["messages"].as_array().expect("messages");
    let tool = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap_or_else(|| panic!("no tool message: {messages:?}"));
    assert_eq!(tool["tool_call_id"], "ask_1");
    assert_eq!(tool["content"], "the local scope");
    let users: Vec<&Value> = messages.iter().filter(|m| m["role"] == "user").collect();
    assert_eq!(users.len(), 1, "no stray user message: {users:?}");
    assert_eq!(users[0]["content"], "remove an auth scope");
}
