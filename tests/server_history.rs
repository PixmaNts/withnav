//! Conversation history (navigator, server half): persistence at the HTTP
//! seam with a scripted provider — create with `history_db` set, converse,
//! list, resume in a FRESH AppState on the same database file, the resumed
//! model request contains the earlier messages, the SSE stream of the
//! resumed session replays the stored events; delete; the list is
//! per-workspace; without `history_db` nothing is written.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use rust_agent::bus::event::EventEnvelope;
use rust_agent::server::{AppState, ServerConfig};

// ---- fixtures (same technique as server_messages.rs) ----

fn write_test_models(dir: &Path, base_url: &str) {
    std::fs::write(
        dir.join("models.json"),
        json!({
            "providers": {
                "mock": {
                    "base_url": base_url,
                    "api_key_env": "RUST_AGENT_TEST_HISTORY_KEY",
                    "headers": {},
                    "connect_timeout_ms": 2000,
                    "request_timeout_ms": 600000
                }
            },
            "models": {
                "cognition": {
                    "provider": "mock",
                    "model": "mock-opus",
                    "reasoning_history": "preserve"
                }
            }
        })
        .to_string(),
    )
    .expect("write models.json");
}

fn make_state(tmp: &Path, base_url: &str, history_db: Option<PathBuf>) -> AppState {
    write_test_models(tmp, base_url);
    let config_dir = tmp.join("cfg");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let auth_path = config_dir.join("auth.json");
    std::fs::write(&auth_path, json!({ "mock": { "api_key": "test-key" } }).to_string())
        .expect("write auth.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 600");
    }
    AppState::new(ServerConfig {
        bind: "127.0.0.1:0".parse().expect("loopback bind"),
        models_path: Some(tmp.join("models.json")),
        config_dir,
        allow_non_loopback: false,
        bearer_token: None,
        history_db,
    })
}

async fn spawn_server(state: AppState) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral bind");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        let _ = rust_agent::server::routes::serve_state_on(state, listener).await;
    });
    let mut up = false;
    for _ in 0..100 {
        if reqwest::get(format!("http://{addr}/v1/health")).await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(up, "server never came up");
    (addr, task)
}

fn sse_body(events: &[&str]) -> String {
    let mut b = String::new();
    for e in events {
        b.push_str("data: ");
        b.push_str(e);
        b.push_str("\n\n");
    }
    b.push_str("data: [DONE]\n\n");
    b
}

fn http_sse(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

/// A scripted provider that answers EVERY connection with the given SSE
/// response (and records the request bodies).
struct MockProvider {
    url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

async fn spawn_mock_provider(response: Vec<u8>) -> MockProvider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
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
    MockProvider {
        url: format!("http://{addr}/v1"),
        requests,
    }
}

async fn create_session(base: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/sessions"))
        .json(&body)
        .send()
        .await
        .expect("create")
}

async fn post_message(base: &str, id: &str, text: &str) {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/sessions/{id}/messages"))
        .json(&json!({
            "target_actor_id": "agent:main",
            "mode": "next-turn",
            "content": [{ "type": "text", "text": text }]
        }))
        .send()
        .await
        .expect("post message");
    assert_eq!(resp.status(), 200);
}

async fn list_sessions(base: &str, workspace: &Path) -> Value {
    reqwest::Client::new()
        .get(format!(
            "{base}/v1/sessions?workspace={}",
            workspace.display()
        ))
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json")
}

/// Collects replay + live until `pred` matches, then settles.
async fn collect_until(
    sub: rust_agent::bus::SubscribeReply,
    pred: impl Fn(&EventEnvelope) -> bool,
) -> Vec<EventEnvelope> {
    let mut out = Vec::new();
    if let rust_agent::bus::ResumeOutcome::Replay(replay) = sub.replay {
        out.extend(replay);
    }
    if let Some(mut live) = sub.live {
        while let Some(env) =
            tokio::time::timeout(Duration::from_secs(5), live.recv()).await.ok().flatten()
        {
            let matched = pred(&env);
            out.push(env);
            if matched {
                tokio::time::sleep(Duration::from_millis(50)).await;
                while let Ok(env) = live.try_recv() {
                    out.push(env);
                }
                break;
            }
        }
    }
    out
}

/// Waits until the writer thread has drained (observed via the store's own
/// read path through a fresh HTTP list).
async fn wait_for_list(base: &str, workspace: &Path, min: usize) -> Vec<Value> {
    for _ in 0..100 {
        let body = list_sessions(base, workspace).await;
        let sessions = body["sessions"].as_array().cloned().unwrap_or_default();
        if sessions.len() >= min {
            return sessions;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("history writer never caught up");
}

fn done_response(text: &str) -> Vec<u8> {
    let chunk = format!(
        r#"{{"choices":[{{"index":0,"delta":{{"content":"{text}"}},"finish_reason":"stop"}}]}}"#
    );
    http_sse(&sse_body(&[&chunk]))
}

// ---- the acceptance tests ----

struct ServerA {
    base: String,
    state: AppState,
    ws: PathBuf,
    _mock: MockProvider,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ServerA {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The tempdir is returned SEPARATELY: a resume test drops the first server
/// (and its state) while keeping the database file on disk.
async fn spawn_history_server() -> (ServerA, PathBuf, tempfile::TempDir) {
    let mock = spawn_mock_provider(done_response("OK")).await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let db = root.join("sessions.db");
    let state = make_state(&root, &mock.url, Some(db.clone()));
    let (addr, task) = spawn_server(state.clone()).await;
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("workspace dir");
    (
        ServerA {
            base: format!("http://{addr}"),
            state,
            ws: ws.clone(),
            _mock: mock,
            task,
        },
        db,
        tmp,
    )
}

// Given a session created with history_db set against a scripted provider
// When a message completes its turn
// Then the store has the session with a title from the first driver message,
//      the events are stored (minus llm.delta/session.end), and the
//      transcript snapshot is saved
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn converse_persists_events_title_and_transcript() {
    let (server, db, _keep) = spawn_history_server().await;
    let created = create_session(
        &server.base,
        json!({ "workspace": server.ws, "profile": "navigator" }),
    )
    .await;
    assert_eq!(created.status(), 200);
    let created: Value = created.json().await.expect("json");
    let id = created["session_id"].as_str().expect("id").to_string();

    let handle = server.state.handle(id.parse().unwrap()).expect("live");
    let sub = handle.sequencer.subscribe_since(None).await.expect("sub");
    let msg = "Remember the word: pineapple<editor-context>{\"file\":\"a.rs\"}</editor-context>";
    post_message(&server.base, &id, msg).await;
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    assert!(
        events.iter().any(|e| e.kind == "turn.end" && e.payload["outcome"] == "done"),
        "turn completed"
    );

    let listed = wait_for_list(&server.base, &server.ws, 1).await;
    assert_eq!(listed[0]["id"], id.as_str());
    assert_eq!(listed[0]["title"], "Remember the word: pineapple");
    assert_eq!(listed[0]["message_count"], 1);
    assert_eq!(listed[0]["live"], true);

    // Direct store probe: events stored except llm.delta; no session.end
    // while live; transcript snapshot present.
    let store = rust_agent::server::store::HistoryStore::open(&db).unwrap();
    let loaded = tokio::time::timeout(Duration::from_secs(5), store.load(id.parse().unwrap()))
        .await
        .expect("load")
        .expect("store load")
        .expect("row");
    let kinds: Vec<&str> = loaded.envelopes.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"session.start"));
    assert!(kinds.contains(&"message.received"));
    assert!(kinds.contains(&"llm.response"));
    assert!(kinds.contains(&"turn.end"));
    assert!(!kinds.contains(&"llm.delta"), "deltas are never stored");
    assert!(!kinds.contains(&"session.end"), "no session.end stored");
    assert!(
        !loaded.transcript.is_empty(),
        "transcript snapshot after turn end"
    );
}

// Given a persisted conversation in one AppState
// When a FRESH AppState opens the same database file and resumes the id
// Then the SAME id is returned, the resumed model request contains the
//      earlier messages, and the SSE stream replays the stored events
//      before any new one (no new session.start)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_in_a_fresh_state_replays_and_remembers() {
    let (server, db, _keep) = spawn_history_server().await;
    let created = create_session(
        &server.base,
        json!({ "workspace": server.ws, "profile": "navigator" }),
    )
    .await;
    let created: Value = created.json().await.expect("json");
    let id = created["session_id"].as_str().expect("id").to_string();

    // Turn 1 in the ORIGINAL state.
    let handle = server.state.handle(id.parse().unwrap()).expect("live");
    let sub = handle.sequencer.subscribe_since(None).await.expect("sub");
    post_message(&server.base, &id, "Remember the word: pineapple").await;
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    let original_seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    let original_max = *original_seqs.iter().max().expect("seqs");
    wait_for_list(&server.base, &server.ws, 1).await;
    // Kill the first server (drop state: its runtime tasks go with it).
    // The tempdir stays alive through `_keep`: the db file must survive.
    server.task.abort();
    drop(server);

    // Turn 2 in a FRESH AppState over the same file.
    let mock2 = spawn_mock_provider(done_response("OK, the word was pineapple")).await;
    let tmp2 = tempfile::tempdir().expect("tempdir 2");
    let root2 = tmp2.path().to_path_buf();
    // Same workspace path content-wise: reuse a copy of the same ws string.
    let ws2 = root2.join("ws");
    std::fs::create_dir_all(&ws2).expect("ws2");
    let state2 = make_state(&root2, &mock2.url, Some(db.clone()));
    // The resume keys on the STORED row (workspace/model/profile are the
    // stored ones); the request's workspace only guides models.json lookup.
    let (resumed_id, resumed) = state2
        .create_or_resume_session(&ws2, None, None, Some(id.parse().unwrap()))
        .await
        .expect("resume");
    assert_eq!(resumed_id.to_string(), id, "same id returned");
    assert!(resumed, "flagged as resumed");
    let (addr2, task2) = spawn_server(state2.clone()).await;
    let base2 = format!("http://{addr2}");

    // A fresh subscriber (no cursor) sees the WHOLE past conversation.
    let handle2 = state2.handle(id.parse().unwrap()).expect("live 2");
    let sub2 = handle2.sequencer.subscribe_since(None).await.expect("sub2");
    if let rust_agent::bus::ResumeOutcome::Replay(replay) = &sub2.replay {
        assert!(
            replay.iter().any(|e| e.kind == "session.start"),
            "the original session.start replays"
        );
        assert!(
            replay.iter().any(|e| e.kind == "message.received"),
            "the original driver message replays"
        );
        assert!(
            !replay.iter().any(|e| e.kind == "session.end"),
            "a resumed stream must not look closed"
        );
        let replay_max = replay.iter().map(|e| e.seq).max().unwrap_or(0);
        assert!(
            replay_max <= original_max,
            "stored seqs preserved: {replay_max} <= {original_max}"
        );
    } else {
        panic!("expected replay, got {:?}", sub2.replay);
    }

    // Drive the next turn: the model request must contain the earlier
    // messages (the remembered word).
    post_message(&base2, &id, "What word did I ask you to remember?").await;
    let events2 = collect_until(sub2, |e| e.kind == "turn.end").await;
    assert!(events2.iter().any(|e| e.payload["outcome"] == "done"));
    let new_events: Vec<&EventEnvelope> =
        events2.iter().filter(|e| e.seq > original_max).collect();
    assert!(!new_events.is_empty(), "new events were sequenced");
    let new_min = new_events.iter().map(|e| e.seq).min().unwrap();
    assert_eq!(
        new_min,
        original_max + 1,
        "the resumed sequencer continues at last_seq + 1 (new {new_min}, stored max {original_max})"
    );
    assert!(
        !new_events
            .iter()
            .any(|e| e.kind == "session.start"),
        "NO new session.start on resume"
    );
    let new_message = new_events
        .iter()
        .find(|e| e.kind == "message.received")
        .expect("the new driver message was sequenced after the stored head");
    assert_eq!(
        new_message.payload["content"],
        "What word did I ask you to remember?"
    );

    // The provider saw the earlier conversation.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let requests = mock2.requests.lock().unwrap().clone();
    assert!(!requests.is_empty(), "provider called");
    let body: Value = serde_json::from_str(
        requests
            .last()
            .unwrap()
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default(),
    )
    .expect("request json");
    let contents: Vec<String> = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        contents.iter().any(|c| c.contains("pineapple")),
        "the earlier messages ride along: {contents:?}"
    );
    task2.abort();
}

// Given a live session and its history
// When DELETE /v1/sessions/{id} runs
// Then 204, the store no longer lists it, and a resume of that id 404s
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_removes_from_memory_and_store() {
    let (server, _db, _keep) = spawn_history_server().await;
    let created: Value = create_session(
        &server.base,
        json!({ "workspace": server.ws, "profile": "navigator" }),
    )
    .await
    .json()
    .await
    .expect("json");
    let id = created["session_id"].as_str().expect("id").to_string();
    let handle = server.state.handle(id.parse().unwrap()).expect("live");
    let sub = handle.sequencer.subscribe_since(None).await.expect("sub");
    post_message(&server.base, &id, "hello").await;
    let _ = collect_until(sub, |e| e.kind == "turn.end").await;
    wait_for_list(&server.base, &server.ws, 1).await;

    let resp = reqwest::Client::new()
        .delete(format!("{}/v1/sessions/{id}", server.base))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 204);
    assert!(server.state.handle(id.parse().unwrap()).is_none());
    assert!(server.state.slot(id.parse().unwrap()).is_none());

    // Store is empty for the workspace.
    for _ in 0..50 {
        let body = list_sessions(&server.base, &server.ws).await;
        if body["sessions"].as_array().map(|a| a.is_empty()).unwrap_or(false) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let body = list_sessions(&server.base, &server.ws).await;
    assert_eq!(body["sessions"].as_array().map(|a| a.len()), Some(0));

    // A disk-only id that never existed 404s.
    let bogus = ulid::Ulid::new().to_string();
    let resp = reqwest::Client::new()
        .delete(format!("{}/v1/sessions/{bogus}", server.base))
        .send()
        .await
        .expect("delete bogus");
    assert_eq!(resp.status(), 404);

    // Resuming the deleted id 404s too.
    let resp = create_session(
        &server.base,
        json!({ "workspace": server.ws, "profile": "navigator", "resume": id }),
    )
    .await;
    assert_eq!(resp.status(), 404);
}

// Given two workspaces with different conversations
// When GET /v1/sessions?workspace=... runs for each
// Then each list only contains its own workspace's sessions
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_is_per_workspace() {
    let (server, _db, _keep) = spawn_history_server().await;
    let other = server.ws.parent().unwrap().join("other-ws");
    std::fs::create_dir_all(&other).expect("other ws");

    for (ws, text) in [(&server.ws, "in the first workspace"), (&other, "in the second")] {
        let created: Value = create_session(
            &server.base,
            json!({ "workspace": ws, "profile": "navigator" }),
        )
        .await
        .json()
        .await
        .expect("json");
        let id = created["session_id"].as_str().expect("id").to_string();
        let handle = server.state.handle(id.parse().unwrap()).expect("live");
        let sub = handle.sequencer.subscribe_since(None).await.expect("sub");
        post_message(&server.base, &id, text).await;
        let _ = collect_until(sub, |e| e.kind == "turn.end").await;
    }

    let first = wait_for_list(&server.base, &server.ws, 1).await;
    assert_eq!(first.len(), 1, "only the first workspace's session");
    assert_eq!(first[0]["title"], "in the first workspace");
    let second = wait_for_list(&server.base, &other, 1).await;
    assert_eq!(second.len(), 1, "only the second workspace's session");
    assert_eq!(second[0]["title"], "in the second");
}

// Given a server WITHOUT history_db
// When a full conversation turn completes
// Then nothing is written anywhere and the list is empty (no store at all)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_history_db_nothing_is_written() {
    let mock = spawn_mock_provider(done_response("OK")).await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let state = make_state(&root, &mock.url, None);
    let (addr, task) = spawn_server(state.clone()).await;
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("ws");

    let created: Value = create_session(
        &format!("http://{addr}"),
        json!({ "workspace": ws, "profile": "navigator" }),
    )
    .await
    .json()
    .await
    .expect("json");
    let id = created["session_id"].as_str().expect("id").to_string();
    let handle = state.handle(id.parse().unwrap()).expect("live");
    let sub = handle.sequencer.subscribe_since(None).await.expect("sub");
    post_message(&format!("http://{addr}"), &id, "no persistence").await;
    let _ = collect_until(sub, |e| e.kind == "turn.end").await;

    let body = list_sessions(&format!("http://{addr}"), &ws).await;
    assert_eq!(body["sessions"].as_array().map(|a| a.len()), Some(0));
    // And no file was created anywhere in the tempdir.
    let entries: Vec<_> = std::fs::read_dir(&root)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !entries.iter().any(|name| name.ends_with(".db")),
        "no database file: {entries:?}"
    );
    task.abort();
}
