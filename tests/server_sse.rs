//! Todo-8 integration tests: GET /v1/sessions/:id/events over a REAL HTTP
//! server on an ephemeral loopback socket. The SSE `id:` field is the event
//! `seq`; `Last-Event-ID` resumes gaplessly; a rejected cursor or a stopped
//! session yields a control frame / clean termination, never a hang or a
//! 404 mid-race. Sessions are created against a scripted MOCK PROVIDER so a
//! posted message drives real events.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use rust_agent::server::{AppState, ServerConfig};

fn write_test_models(dir: &Path, base_url: &str) -> PathBuf {
    let path = dir.join("models.json");
    std::fs::write(
        &path,
        json!({
            "providers": {
                "mock": {
                    "base_url": base_url,
                    "api_key_env": "RUST_AGENT_TEST_SSE_KEY",
                    "headers": {},
                    "connect_timeout_ms": 2000,
                    "request_timeout_ms": 600000
                }
            },
            "models": {
                "default": {
                    "provider": "mock",
                    "model": "mock-model",
                    "reasoning_history": "preserve"
                }
            }
        })
        .to_string(),
    )
    .expect("write models.json");
    path
}

fn test_config(tmp: &Path, base_url: &str) -> ServerConfig {
    write_test_models(tmp, base_url);
    let config_dir = tmp.join("cfg");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let auth_path = config_dir.join("auth.json");
    std::fs::write(
        &auth_path,
        json!({ "mock": { "api_key": "test-key" } }).to_string(),
    )
    .expect("write auth.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 600");
    }
    ServerConfig {
        bind: "127.0.0.1:0".parse().expect("loopback bind"),
        models_path: Some(tmp.join("models.json")),
        config_dir,
        allow_non_loopback: false,
        bearer_token: None,
    }
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
        if reqwest::get(format!("http://{addr}/v1/health"))
            .await
            .is_ok()
        {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(up, "server never came up");
    (addr, task)
}

async fn create_session(base: &str, ws: &Path) -> String {
    let created: Value = reqwest::Client::new()
        .post(format!("{base}/v1/sessions"))
        .json(&json!({ "workspace": ws.to_str().expect("utf8 ws") }))
        .send()
        .await
        .expect("create")
        .json()
        .await
        .expect("create json");
    created["session_id"]
        .as_str()
        .expect("session id")
        .to_string()
}

async fn post_message(base: &str, id: &str, text: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/sessions/{id}/messages"))
        .json(&json!({
            "target_actor_id": "agent:main",
            "mode": "next-turn",
            "content": [{ "type": "text", "text": text }]
        }))
        .send()
        .await
        .expect("post message")
}

// ---- scripted mock provider ----

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

struct MockProvider {
    url: String,
}

async fn spawn_mock_provider(responses: Vec<Vec<u8>>) -> MockProvider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        for resp in responses {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = vec![0u8; 16384];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(&resp).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });
    MockProvider {
        url: format!("http://{addr}/v1"),
    }
}

struct TestServer {
    base: String,
    ws: PathBuf,
    _mock: MockProvider,
    task: tokio::task::JoinHandle<()>,
    _root: tempfile::TempDir,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spawn_test_server(mock_responses: Vec<Vec<u8>>) -> TestServer {
    let mock = spawn_mock_provider(mock_responses).await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let config = test_config(&root, &mock.url);
    let state = AppState::new(config);
    let (addr, task) = spawn_server(state.clone()).await;
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("workspace dir");
    TestServer {
        base: format!("http://{addr}"),
        ws,
        _mock: mock,
        task,
        _root: tmp,
    }
}

// ---- SSE frame parsing over a raw HTTP GET ----

/// One parsed SSE frame: its `id:` (if any), `event:` name (default
/// "message"), and the concatenated `data:` payload.
#[derive(Debug, Clone)]
struct SseFrame {
    id: Option<u64>,
    event: String,
    data: String,
}

/// Why the SSE read loop stopped — so a termination test can distinguish a
/// clean server-side EOF from a transport error or the client's own budget.
#[derive(Debug, PartialEq, Eq)]
enum SseEnd {
    /// The server closed the stream cleanly (body EOF).
    Eof,
    /// The client's read budget elapsed while the stream was still open.
    Timeout,
    /// The transport errored mid-read.
    Error,
}

struct SseRead {
    frames: Vec<SseFrame>,
    end: SseEnd,
}

/// Read the SSE body until the budget elapses OR the server ends the stream,
/// parsing complete `\n\n`-delimited frames incrementally, and REPORT how it
/// ended so callers can assert a clean EOF vs a mere timeout.
async fn read_sse(base: &str, id: &str, last_event_id: Option<u64>, budget_ms: u64) -> SseRead {
    let url = format!("{base}/v1/sessions/{id}/events");
    let mut req = reqwest::Client::new().get(&url);
    if let Some(k) = last_event_id {
        req = req.header("last-event-id", k.to_string());
    }
    let resp = req.send().await.expect("sse connect");
    assert_eq!(resp.status(), 200, "sse status");
    let mut stream = resp;
    let mut buf = String::new();
    let mut frames = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(budget_ms);
    let end = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break SseEnd::Timeout;
        }
        match tokio::time::timeout(remaining, stream.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(idx) = buf.find("\n\n") {
                    let raw = buf[..idx].to_string();
                    buf.drain(..idx + 2);
                    if let Some(frame) = parse_frame(&raw) {
                        frames.push(frame);
                    }
                }
            }
            Ok(Ok(None)) => break SseEnd::Eof,
            Ok(Err(_)) => break SseEnd::Error,
            Err(_) => break SseEnd::Timeout,
        }
    };
    SseRead { frames, end }
}

/// Frame-only convenience for tests that don't care how the stream ended.
async fn read_sse_frames(
    base: &str,
    id: &str,
    last_event_id: Option<u64>,
    budget_ms: u64,
) -> Vec<SseFrame> {
    read_sse(base, id, last_event_id, budget_ms).await.frames
}

fn parse_frame(raw: &str) -> Option<SseFrame> {
    let mut id = None;
    let mut event = "message".to_string();
    let mut data = String::new();
    let mut saw_field = false;
    for line in raw.lines() {
        if let Some(v) = line.strip_prefix("id:") {
            id = v.trim().parse::<u64>().ok();
            saw_field = true;
        } else if let Some(v) = line.strip_prefix("event:") {
            event = v.trim().to_string();
            saw_field = true;
        } else if let Some(v) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(v.trim());
            saw_field = true;
        }
        // ':' comment lines (keep-alive) are ignored.
    }
    if saw_field {
        Some(SseFrame { id, event, data })
    } else {
        None
    }
}

fn kind_of(frame: &SseFrame) -> Option<String> {
    serde_json::from_str::<Value>(&frame.data)
        .ok()
        .and_then(|v| v["kind"].as_str().map(str::to_string))
}

// ---- (a) each durable frame carries id == seq, monotonic ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_frames_carry_id_equal_to_seq_and_are_monotonic() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;

    // Connect FIRST (captures session.start@0), then drive a turn.
    let base = server.base.clone();
    let id2 = id.clone();
    let reader = tokio::spawn(async move { read_sse_frames(&base, &id2, None, 2500).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let resp = post_message(&server.base, &id, "go").await;
    assert_eq!(resp.status(), 200);

    let frames = reader.await.expect("reader");
    let durable: Vec<&SseFrame> = frames.iter().filter(|f| kind_of(f).is_some()).collect();
    assert!(!durable.is_empty(), "durable frames present: {frames:?}");
    // Each durable frame's id == its envelope seq, and ids strictly increase.
    let mut last: Option<u64> = None;
    for f in &durable {
        let seq = f.id.expect("durable frame has id");
        let payload: Value = serde_json::from_str(&f.data).expect("json");
        assert_eq!(seq, payload["seq"].as_u64().expect("seq"), "id == seq");
        if let Some(prev) = last {
            assert!(seq > prev, "monotonic ids: {seq} after {prev}");
        }
        last = Some(seq);
    }
    // session.start is seq 0 and is present (absent Last-Event-ID replays it).
    assert_eq!(durable[0].id, Some(0), "first durable is session.start@0");
    assert_eq!(kind_of(durable[0]).as_deref(), Some("session.start"));
}

// ---- (b) reconnect with Last-Event-ID: K resumes gaplessly ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_with_last_event_id_resumes_without_gap_or_dup() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;

    // Drive a full turn so the ring has several durable events.
    let base = server.base.clone();
    let id2 = id.clone();
    let reader = tokio::spawn(async move { read_sse_frames(&base, &id2, None, 2500).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    post_message(&server.base, &id, "go").await;
    let first = reader.await.expect("reader");
    // The ring retains only DURABLE kinds; `llm.delta` is live-only and
    // legitimately absent on replay, so the resume contract is over the
    // durable subsequence (a dropped-delta seq gap is expected + legal).
    let durable = |fs: &[SseFrame]| -> Vec<u64> {
        fs.iter()
            .filter_map(|f| Some((f.id?, kind_of(f)?)))
            .filter(|(_, k)| k != "llm.delta")
            .map(|(seq, _)| seq)
            .collect()
    };
    let first_durable = durable(&first);
    assert!(
        first_durable.len() >= 3,
        "several durable events: {first_durable:?}"
    );

    let k = first_durable[1];
    let resumed = read_sse_frames(&server.base, &id, Some(k), 1500).await;
    let resumed_durable = durable(&resumed);
    let expected: Vec<u64> = first_durable.iter().copied().filter(|s| *s > k).collect();
    assert_eq!(
        resumed_durable, expected,
        "gapless resume of durable seq > {k}"
    );
    assert!(
        !resumed_durable.contains(&k),
        "no duplicate of the cursor event"
    );
}

// ---- (c) absent Last-Event-ID replays full history incl. seq 0 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_last_event_id_replays_from_seq_zero() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;
    post_message(&server.base, &id, "go").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A fresh connection with NO Last-Event-ID must replay session.start@0.
    let frames = read_sse_frames(&server.base, &id, None, 1200).await;
    let first_durable = frames
        .iter()
        .find(|f| kind_of(f).is_some())
        .expect("a durable frame");
    assert_eq!(
        first_durable.id,
        Some(0),
        "replayed from seq 0, not cursor-0-skip"
    );
    assert_eq!(kind_of(first_durable).as_deref(), Some("session.start"));
}

// ---- (d) a future cursor is a resync control frame, never silent caught-up ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn future_cursor_yields_resync_control_frame() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;

    // Cursor far beyond head -> InvalidCursor -> EXACTLY one "resync" control
    // frame, no durable events, then a clean server EOF (never a silent empty
    // "caught up" stream, never a hang).
    let read = read_sse(&server.base, &id, Some(9_999_999), 1200).await;
    assert_eq!(
        read.end,
        SseEnd::Eof,
        "resync stream ends cleanly (EOF), not a timeout"
    );
    assert_eq!(
        read.frames.len(),
        1,
        "exactly one resync frame: {:?}",
        read.frames
    );
    assert_eq!(
        read.frames[0].event, "resync",
        "the frame is a resync control frame"
    );
    assert!(
        kind_of(&read.frames[0]).is_none(),
        "resync frame carries no durable envelope"
    );
}

// ---- (e) reconnect-after-stop replays through final_seq then terminates
//         (no 404 mid-race, no hang) ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_after_stop_replays_through_final_seq_then_terminates() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;

    // Observe the live history first so the expected replay set is grounded
    // in what actually happened (not a guess about teardown timing).
    let base = server.base.clone();
    let id2 = id.clone();
    let reader = tokio::spawn(async move { read_sse_frames(&base, &id2, None, 2500).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    post_message(&server.base, &id, "go").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stop = reqwest::Client::new()
        .post(format!("{}/v1/sessions/{id}/stop", server.base))
        .send()
        .await
        .expect("stop");
    assert_eq!(stop.status(), 200);

    let live = reader.await.expect("reader");
    let dur = |fs: &[SseFrame]| -> Vec<(u64, String)> {
        fs.iter()
            .filter_map(|f| Some((f.id?, kind_of(f)?)))
            .filter(|(_, k)| k != "llm.delta")
            .collect()
    };
    let expected = dur(&live);
    assert!(
        !expected.is_empty(),
        "observed a durable history: {expected:?}"
    );

    // GET /events on the tombstone: 200 (not 404), replays the durable
    // history through final_seq (session.end LAST), then ends with a clean
    // server EOF — not a client-budget timeout, not a hang.
    let read = read_sse(&server.base, &id, None, 3000).await;
    assert_eq!(read.end, SseEnd::Eof, "clean server EOF, not timeout/error");
    let replayed = dur(&read.frames);
    // Everything observed live is contained, in order (the observer's 2500ms
    // budget can end before stop, so its tail is a prefix of the truth).
    let mut it = replayed.iter();
    for pair in &expected {
        assert!(
            it.any(|r| r == pair),
            "tombstone replay missing/misordered {pair:?}: got {replayed:?}"
        );
    }
    let seqs: Vec<u64> = replayed.iter().map(|(s, _)| *s).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] > w[0]),
        "tombstone replay is seq-ordered: {seqs:?}"
    );
    let last = replayed.last().expect("tombstone replayed something");
    assert_eq!(
        last.1, "session.end",
        "session.end (final_seq) is the LAST replayed event: {replayed:?}"
    );
    let payload: Value = serde_json::from_str(
        &read
            .frames
            .iter()
            .rev()
            .find(|f| kind_of(f).as_deref() == Some("session.end"))
            .expect("session.end frame")
            .data,
    )
    .expect("json");
    assert_eq!(
        last.0,
        payload["seq"].as_u64().expect("seq"),
        "last frame id == session.end seq"
    );
    assert_eq!(
        last.0, payload["payload"]["final_seq"],
        "final_seq == session.end seq"
    );
}

// ---- (f) unknown session id -> 404 (not an SSE stream) ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_session_events_is_404() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let unknown = ulid::Ulid::new().to_string();
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/sessions/{unknown}/events", server.base))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status(), 404);
}

// ---- (h) replay/live seam: the replay snapshot is COMPLETE (all durable
//         seqs, not empty) and an event published after the snapshot arrives
//         via `live` exactly once, never duplicated in replay ----

// Atomicity of subscribe-then-replay is a STRUCTURAL property of the
// sequencer: `subscribe_since` is one synchronous mailbox op that captures
// the replay-through-head AND registers the live sender with no await
// between them, so nothing can slip in between (that no-await handshake is
// covered by the bus-level exactly-once test in src/bus/mod.rs). A black-box
// test cannot inject a publish *inside* that atomic op. What this test pins
// is the observable seam contract: replay is the exact `[0, 1]` prefix (not
// silently empty), and a post-snapshot event is delivered live exactly once
// and is absent from replay (no gap, no dup at the boundary).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_then_replay_seam_is_complete_and_delivers_once() {
    use rust_agent::bus::event::EventKind;
    use rust_agent::bus::sequencer::{Candidate, Sequencer};
    use rust_agent::bus::{ResumeOutcome, SessionBus};

    let seq = Sequencer::spawn(ulid::Ulid::new(), SessionBus::new());

    seq.publish(Candidate::new(EventKind::TurnStart, json!({ "n": 0 })))
        .expect("publish 0");
    seq.publish(Candidate::new(EventKind::TurnEnd, json!({ "n": 1 })))
        .expect("publish 1");

    let reply = seq.subscribe_since(None).await.expect("subscribe");
    let replay_seqs: Vec<u64> = match &reply.replay {
        ResumeOutcome::Replay(events) => events.iter().map(|e| e.seq).collect(),
        other => panic!("expected Replay, got {other:?}"),
    };
    assert_eq!(
        replay_seqs,
        vec![0, 1],
        "replay is the complete durable prefix, not silently empty"
    );
    let mut live = reply.live.expect("live receiver for a Replay");

    seq.publish(Candidate::new(EventKind::TaskDone, json!({ "n": 2 })))
        .expect("publish 2");

    let post = tokio::time::timeout(Duration::from_secs(2), live.recv())
        .await
        .expect("live delivers within budget")
        .expect("live event present");
    assert_eq!(
        post.seq, 2,
        "seam event seq follows the replay prefix exactly"
    );
    assert!(
        !replay_seqs.contains(&post.seq),
        "seam event {} not duplicated in replay {replay_seqs:?}",
        post.seq
    );
    assert_eq!(post.kind, "task.done", "the exact post-snapshot event");
    let dup = tokio::time::timeout(Duration::from_millis(200), live.recv()).await;
    assert!(dup.is_err(), "no duplicate delivery at the seam: {dup:?}");
}

// ---- (g) two concurrent clients observe identical durable histories ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_observe_identical_durable_history() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;

    let base1 = server.base.clone();
    let id1 = id.clone();
    let base2 = server.base.clone();
    let id2 = id.clone();
    let r1 = tokio::spawn(async move { read_sse_frames(&base1, &id1, None, 2500).await });
    let r2 = tokio::spawn(async move { read_sse_frames(&base2, &id2, None, 2500).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    post_message(&server.base, &id, "go").await;

    let f1 = r1.await.expect("r1");
    let f2 = r2.await.expect("r2");
    let dur = |fs: &[SseFrame]| -> Vec<(u64, String)> {
        fs.iter()
            .filter_map(|f| Some((f.id?, kind_of(f)?)))
            .filter(|(_, k)| k != "llm.delta")
            .collect()
    };
    assert_eq!(
        dur(&f1),
        dur(&f2),
        "identical durable histories across clients"
    );
    assert!(!dur(&f1).is_empty(), "non-empty durable history");
}
