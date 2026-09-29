//! Todo-9 integration tests: GET /v1/sessions/:id/ws over a REAL HTTP server
//! on an ephemeral loopback socket, driven by a real tokio-tungstenite WS
//! client. The WS wraps the SAME event + command mechanics as SSE and POST
//! /messages: a `message.send` frame is acked, event frames carry the same
//! durable history an SSE client sees, and a rejected send is an `error`
//! frame (not a socket close). Sessions run against a scripted mock provider.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use rust_agent::server::{AppState, ServerConfig};

fn write_test_models(dir: &Path, base_url: &str) -> PathBuf {
    let path = dir.join("models.json");
    std::fs::write(
        &path,
        json!({
            "providers": {
                "mock": {
                    "base_url": base_url,
                    "api_key_env": "RUST_AGENT_TEST_WS_KEY",
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
    addr: SocketAddr,
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
        addr,
        ws,
        _mock: mock,
        task,
        _root: tmp,
    }
}

// ---- WS helpers ----

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_ws(addr: SocketAddr, id: &str) -> WsStream {
    let url = format!("ws://{addr}/v1/sessions/{id}/ws");
    let (stream, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect");
    stream
}

/// Collect parsed server frames until `pred` matches one, or the budget
/// elapses. Returns everything collected (including the matching frame).
async fn collect_frames(
    ws: &mut WsStream,
    budget_ms: u64,
    pred: impl Fn(&Value) -> bool,
) -> Vec<Value> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(budget_ms);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    let matched = pred(&v);
                    out.push(v);
                    if matched {
                        break;
                    }
                }
            }
            Ok(Some(Ok(WsMessage::Close(_)))) | Ok(None) => break,
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) => break,
            Err(_) => break,
        }
    }
    out
}

fn message_send(request_id: &str, text: &str) -> String {
    json!({
        "type": "message.send",
        "request_id": request_id,
        "payload": {
            "target_actor_id": "agent:main",
            "mode": "next-turn",
            "content": [{ "type": "text", "text": text }]
        }
    })
    .to_string()
}

fn durable_seqs(frames: &[Value]) -> Vec<(u64, String)> {
    frames
        .iter()
        .filter(|f| f["type"] == "event")
        .filter_map(|f| {
            let env = &f["envelope"];
            Some((env["seq"].as_u64()?, env["kind"].as_str()?.to_string()))
        })
        .filter(|(_, k)| k != "llm.delta")
        .collect()
}

// ---- (a) message.send -> ack echoing request_id + command_id, then events ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_message_send_is_acked_and_drives_events() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;
    let mut ws = connect_ws(server.addr, &id).await;

    // Send a message.send frame.
    ws.send(WsMessage::Text(message_send("req-1", "go")))
        .await
        .expect("send");

    // First: an ack echoing our request_id + a real command_id (a ULID).
    let acked = collect_frames(&mut ws, 2000, |v| v["type"] == "ack").await;
    let ack = acked
        .iter()
        .find(|v| v["type"] == "ack")
        .expect("ack frame");
    assert_eq!(ack["request_id"], "req-1", "ack echoes request_id");
    assert_eq!(ack["accepted"], true);
    let ack_command_id = ack["command_id"]
        .as_str()
        .expect("ack carries a command_id");
    assert!(
        ack_command_id.parse::<ulid::Ulid>().is_ok(),
        "ack command_id is a real ULID: {ack_command_id}"
    );

    // Then: subsequent event frames drive the turn to turn.end{done}.
    let frames = collect_frames(&mut ws, 2500, |v| {
        v["type"] == "event"
            && v["envelope"]["kind"] == "turn.end"
            && v["envelope"]["payload"]["outcome"] == "done"
    })
    .await;
    let events: Vec<&Value> = frames.iter().filter(|f| f["type"] == "event").collect();
    let kinds: Vec<String> = events
        .iter()
        .filter_map(|f| f["envelope"]["kind"].as_str().map(str::to_string))
        .collect();
    assert!(
        kinds.contains(&"turn.end".to_string()),
        "the turn ran to completion: {kinds:?}"
    );
    // The ack's command_id correlates with the message.received envelope.
    let received = events
        .iter()
        .find(|f| f["envelope"]["kind"] == "message.received")
        .expect("message.received sequenced");
    assert_eq!(
        received["envelope"]["payload"]["command_id"], ack_command_id,
        "ack command_id == the sequenced message.received command_id"
    );
    assert_eq!(
        received["envelope"]["payload"]["source"], "user:http",
        "WS command source is server-assigned user:http (no impersonation)"
    );
}

// ---- (b) event frames carry id==seq envelopes incl. session.start@0 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_events_carry_full_envelopes_from_seq_zero() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;
    let mut ws = connect_ws(server.addr, &id).await;

    // Absent last_seq -> full replay incl. session.start@0.
    ws.send(WsMessage::Text(message_send("r", "go")))
        .await
        .expect("send");
    let frames = collect_frames(&mut ws, 2500, |v| {
        v["type"] == "event" && v["envelope"]["kind"] == "turn.end"
    })
    .await;
    let dur = durable_seqs(&frames);
    assert!(!dur.is_empty(), "durable events present");
    assert_eq!(dur[0], (0, "session.start".to_string()), "starts at seq 0");
    // Monotonic durable seqs.
    let mut last: Option<u64> = None;
    for (seq, _) in &dur {
        if let Some(prev) = last {
            assert!(*seq > prev, "monotonic durable seqs: {seq} after {prev}");
        }
        last = Some(*seq);
    }
}

// ---- (c) WS and SSE clients observe the identical durable history ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_and_sse_observe_identical_durable_history() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;

    // A WS client and an SSE client, both connected BEFORE the turn is driven.
    let mut ws = connect_ws(server.addr, &id).await;

    let sse_base = server.base.clone();
    let sse_id = id.clone();
    let sse_task = tokio::spawn(async move {
        let resp = reqwest::Client::new()
            .get(format!("{sse_base}/v1/sessions/{sse_id}/events"))
            .send()
            .await
            .expect("sse");
        let mut stream = resp;
        let mut buf = String::new();
        let mut out: Vec<(u64, String)> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(3000);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, stream.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    buf.push_str(&String::from_utf8_lossy(&chunk));
                    while let Some(idx) = buf.find("\n\n") {
                        let raw = buf[..idx].to_string();
                        buf.drain(..idx + 2);
                        for line in raw.lines() {
                            if let Some(v) = line.strip_prefix("data:") {
                                if let Ok(env) = serde_json::from_str::<Value>(v.trim()) {
                                    if let (Some(seq), Some(kind)) =
                                        (env["seq"].as_u64(), env["kind"].as_str())
                                    {
                                        if kind != "llm.delta" {
                                            let is_end = kind == "turn.end";
                                            out.push((seq, kind.to_string()));
                                            if is_end {
                                                return out;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break,
            }
        }
        out
    });

    tokio::time::sleep(Duration::from_millis(150)).await;
    // Drive the turn via the WS command intake.
    ws.send(WsMessage::Text(message_send("r", "go")))
        .await
        .expect("send");

    let ws_frames = collect_frames(&mut ws, 3000, |v| {
        v["type"] == "event" && v["envelope"]["kind"] == "turn.end"
    })
    .await;
    let ws_dur = durable_seqs(&ws_frames);
    let sse_dur = sse_task.await.expect("sse task");

    assert_eq!(
        ws_dur, sse_dur,
        "WS and SSE observe identical durable histories"
    );
    assert!(!ws_dur.is_empty(), "non-empty durable history");
}

// ---- (d) a malformed / bad-target send is an error frame, not a close ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_bad_send_is_an_error_frame_not_a_close() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;
    let mut ws = connect_ws(server.addr, &id).await;

    // A message.send to an unknown target_actor_id -> error frame (400-class).
    let bad = json!({
        "type": "message.send",
        "request_id": "bad-1",
        "payload": {
            "target_actor_id": "agent:nope",
            "mode": "next-turn",
            "content": [{ "type": "text", "text": "hi" }]
        }
    })
    .to_string();
    ws.send(WsMessage::Text(bad)).await.expect("send");

    let frames = collect_frames(&mut ws, 1500, |v| v["type"] == "error").await;
    let err = frames
        .iter()
        .find(|v| v["type"] == "error")
        .expect("error frame");
    assert_eq!(err["request_id"], "bad-1", "error echoes request_id");
    assert!(
        err["message"].as_str().unwrap_or("").contains("target"),
        "error explains the bad target: {err:?}"
    );

    // The socket is still open: a valid send now gets acked.
    ws.send(WsMessage::Text(message_send("ok-1", "go")))
        .await
        .expect("send after error");
    let acked = collect_frames(&mut ws, 2000, |v| v["type"] == "ack").await;
    assert!(
        acked
            .iter()
            .any(|v| v["type"] == "ack" && v["request_id"] == "ok-1"),
        "socket stayed open; a later valid send is acked: {acked:?}"
    );
}

// ---- (e) unknown session -> the WS upgrade is refused (404) ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_unknown_session_upgrade_is_refused() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let unknown = ulid::Ulid::new().to_string();
    let url = format!("ws://{}/v1/sessions/{unknown}/ws", server.addr);
    let result = tokio_tungstenite::connect_async(url).await;
    assert!(
        result.is_err(),
        "WS upgrade to an unknown session must be refused (non-101)"
    );
}

// ---- (f) a future ?last_seq= cursor yields a resync error frame then closes ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_future_last_seq_yields_resync_error_frame() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;

    let url = format!("ws://{}/v1/sessions/{id}/ws?last_seq=9999999", server.addr);
    let (mut ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect");

    let frames = collect_frames(&mut ws, 1500, |v| v["type"] == "error").await;
    let err = frames
        .iter()
        .find(|v| v["type"] == "error")
        .expect("resync error frame for a future cursor");
    assert!(
        err["message"].as_str().unwrap_or("").contains("resync"),
        "future cursor gets a client-visible resync signal, not a silent close: {err:?}"
    );
    assert!(
        frames.iter().all(|v| v["type"] != "event"),
        "the resync stream carries no event frames"
    );
}

// ---- (g) a valid ?last_seq= cursor resumes gaplessly (WS mirrors SSE) ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_valid_last_seq_resumes_gaplessly() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;

    // Drive a full turn so the ring has several durable events.
    let mut ws = connect_ws(server.addr, &id).await;
    ws.send(WsMessage::Text(message_send("r", "go")))
        .await
        .expect("send");
    let first = collect_frames(&mut ws, 2500, |v| {
        v["type"] == "event" && v["envelope"]["kind"] == "turn.end"
    })
    .await;
    let durable = durable_seqs(&first);
    assert!(durable.len() >= 3, "several durable events: {durable:?}");

    // Reconnect from a mid-stream durable K; expect exactly durable seq > K.
    let k = durable[1].0;
    let url = format!("ws://{}/v1/sessions/{id}/ws?last_seq={k}", server.addr);
    let (mut ws2, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws reconnect");
    let resumed = collect_frames(&mut ws2, 1500, |v| {
        v["type"] == "event" && v["envelope"]["kind"] == "turn.end"
    })
    .await;
    let resumed_durable = durable_seqs(&resumed);
    let expected: Vec<(u64, String)> = durable.iter().filter(|(s, _)| *s > k).cloned().collect();
    assert_eq!(
        resumed_durable, expected,
        "WS resume from last_seq={k} is gapless (durable seq > K)"
    );
}

// ---- (h) a malformed JSON frame is an error frame; the socket stays open ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_malformed_frame_is_an_error_and_socket_survives() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;
    let mut ws = connect_ws(server.addr, &id).await;

    ws.send(WsMessage::Text("{ this is not valid json".to_string()))
        .await
        .expect("send garbage");
    let frames = collect_frames(&mut ws, 1500, |v| v["type"] == "error").await;
    assert!(
        frames.iter().any(|v| v["type"] == "error"),
        "malformed frame yields an error frame: {frames:?}"
    );

    // The socket survived: a valid send now gets acked.
    ws.send(WsMessage::Text(message_send("ok-1", "go")))
        .await
        .expect("send after malformed");
    let acked = collect_frames(&mut ws, 2000, |v| v["type"] == "ack").await;
    assert!(
        acked
            .iter()
            .any(|v| v["type"] == "ack" && v["request_id"] == "ok-1"),
        "socket stayed open after a malformed frame: {acked:?}"
    );
}
