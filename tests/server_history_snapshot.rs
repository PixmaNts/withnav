//! The transcript is saved on EVERY turn end, a /stop-cancelled turn
//! included, and lands in the store behind that turn's events.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use rust_agent::provider::openai::ChatMessage;
use rust_agent::server::store::HistoryStore;

mod common;
use common::server::{post_create_session, post_message_ok, spawn_server, TestServerConfig};

/// A provider whose stream stalls mid-turn: one content delta, then silence
/// with the connection held open.
async fn spawn_stalling_provider() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = vec![0u8; 65536];
            let _ = sock.read(&mut buf).await;
            let chunk = r#"{"choices":[{"index":0,"delta":{"content":"partial"}}]}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {chunk}\n\n"
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.flush().await;
            held.push(sock);
        }
    });
    format!("http://{addr}/v1")
}

// Given a turn stalled mid-stream
// When the session is stopped and its history is loaded from the database
// Then the transcript holds the driver's message of the interrupted turn and
//      the stored events end with that turn's turn.end and the session.end
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_mid_turn_session_keeps_its_transcript() {
    let url = spawn_stalling_provider().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = tmp.path().join("sessions.db");
    let state = TestServerConfig::mock(tmp.path(), &url)
        .model_alias("cognition", "mock-opus")
        .history_db(db.clone())
        .state();
    let (addr, task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("ws");

    let created: Value =
        post_create_session(&base, &json!({ "workspace": ws, "profile": "navigator" }))
            .await
            .json()
            .await
            .expect("json");
    let id = created["session_id"].as_str().expect("id").to_string();
    let ulid: ulid::Ulid = id.parse().unwrap();

    let handle = state.handle(ulid).expect("live");
    let sub = handle.sequencer.subscribe_since(None).await.expect("sub");
    let mut live = sub.live.expect("live queue");
    post_message_ok(&base, &id, "remember pineapple").await;
    // The turn is in flight once its first delta streamed.
    loop {
        let env = tokio::time::timeout(Duration::from_secs(5), live.recv())
            .await
            .expect("delta in time")
            .expect("open");
        if env.kind == "llm.delta" {
            break;
        }
    }

    state.close_session(ulid).await.expect("stop");
    drop(state);
    task.abort();

    // A fresh store handle on the same file (the writer drains before load
    // observes a settled row).
    let store = HistoryStore::open(&db).expect("open");
    let mut loaded = None;
    for _ in 0..100 {
        let l = store.load(ulid).await.expect("load").expect("row");
        let has_end = l.envelopes.iter().any(|e| e.kind == "session.end");
        if has_end && !l.transcript.is_empty() {
            loaded = Some(l);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let loaded = loaded.expect("transcript saved after the stop");
    assert!(
        loaded.transcript.iter().any(|m| matches!(
            m,
            ChatMessage::User { content, .. } if format!("{content:?}").contains("remember pineapple")
        )),
        "interrupted turn's driver message is in the transcript: {:?}",
        loaded.transcript
    );
    let kinds: Vec<&str> = loaded.envelopes.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"turn.end"), "turn.end stored: {kinds:?}");
    assert_eq!(kinds.last(), Some(&"session.end"));
    assert_eq!(
        loaded.meta.last_seq,
        loaded.envelopes.last().expect("events").seq,
        "every event, session.end included, is stored"
    );
}
