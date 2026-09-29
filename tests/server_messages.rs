//! Todo-7 integration tests: POST /v1/sessions/:id/messages and
//! POST /v1/sessions/:id/stop over a REAL HTTP server on an ephemeral
//! loopback socket. Sessions are created against a MOCK PROVIDER (the same
//! scripted-TCP technique as the runtime tests) so a posted message drives a
//! real turn. Frozen bodies per ticket 04 Q3.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use rust_agent::bus::event::EventEnvelope;
use rust_agent::server::{AppState, ServerConfig};

/// The mock provider is scripted per test; real-socket tests (and the
/// in-process POSTs against the same AppState) must not race on SIGTERM or
/// the models.json fixture, so each test builds its own tempdir. No shared
/// lock needed: nothing here raises signals.
///
/// Writes a models.json whose provider base_url is `base_url`.
fn write_test_models(dir: &Path, base_url: &str) -> PathBuf {
    let path = dir.join("models.json");
    std::fs::write(
        &path,
        json!({
            "providers": {
                "mock": {
                    "base_url": base_url,
                    "api_key_env": "RUST_AGENT_TEST_MESSAGES_KEY",
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

/// Spawns the real server (serve_state_on) on an ephemeral socket over the
/// given state; returns the base URL. The caller drops/aborts the task.
async fn spawn_server(state: AppState) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral bind");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        let _ = rust_agent::server::routes::serve_state_on(state, listener).await;
    });
    // Wait until the socket answers health (bounded, no fixed sleep race).
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

/// Creates a session over HTTP and returns its ULID string.
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

async fn post_message(base: &str, id: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/sessions/{id}/messages"))
        .json(&body)
        .send()
        .await
        .expect("post message")
}

fn text_message(text: &str) -> Value {
    json!({
        "target_actor_id": "agent:main",
        "mode": "next-turn",
        "content": [{ "type": "text", "text": text }]
    })
}

/// Collects (kind, seq) pairs from replay + live until `pred` matches, then
/// settles (same-tick sequenced events land in the same batch) and drains
/// what is ready. Returns everything collected.
async fn collect_until(
    sub: rust_agent::bus::SubscribeReply,
    pred: impl Fn(&EventEnvelope) -> bool,
) -> Vec<EventEnvelope> {
    let mut out = Vec::new();
    if let rust_agent::bus::ResumeOutcome::Replay(replay) = sub.replay {
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

// ---- scripted mock provider (same technique as the runtime tests) ----

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

/// Scripted responses, one per accepted connection (queued FIFO).
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
    state: AppState,
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
        state,
        ws,
        _mock: mock,
        task,
        _root: tmp,
    }
}

// ---- (a) happy path: 200 {accepted:true} immediately; message reaches the agent ----

// Given a live session backed by a scripted one-turn mock provider
// When a text message is POSTed
// Then 200 with the frozen {command_id, accepted:true, actor_state} body and
//      NO notice; the event log shows message.received BEFORE the turn events
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_message_acks_immediately_and_drives_a_turn() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"all done"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;

    let id = create_session(&server.base, &server.ws).await;
    let handle = server
        .state
        .handle(id.parse().expect("ulid"))
        .expect("live");
    let sub = handle
        .sequencer
        .subscribe_since(None)
        .await
        .expect("subscribe");

    let resp = post_message(&server.base, &id, text_message("please finish the turn")).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("ack json");
    let command_id = body["command_id"].as_str().expect("command_id").to_string();
    assert_eq!(body["accepted"], true);
    assert_eq!(body["actor_state"], "idle");
    assert!(body.get("notice").is_none(), "next-turn has no notice");

    let events = collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let received_pos = kinds
        .iter()
        .position(|k| *k == "message.received")
        .expect("message.received sequenced");
    let start_pos = kinds
        .iter()
        .position(|k| *k == "turn.start")
        .expect("turn.start");
    assert!(
        received_pos < start_pos,
        "event-before-action: message.received ({received_pos}) before turn.start ({start_pos})"
    );
    let received = &events[received_pos];
    assert_eq!(received.payload["command_id"], command_id.as_str());
    assert_eq!(received.payload["source"], "user:http");
    assert_eq!(received.payload["content"], "please finish the turn");
}

// ---- (c) full inbox -> 503 {accepted:false, reason:"inbox_full"} ----

// Given a live session whose inbox is flooded to capacity
// When one more message is POSTed
// Then 503 with the frozen {accepted:false, reason:"inbox_full"} body
//
// How the inbox is forced full WITHOUT touching bus code: the runtime only
// drains the inbox at turn boundaries, and the scripted provider holds the
// connection open so the single in-flight turn never ends. While the actor
// is mid-turn, INBOX_CAPACITY posts fill the bounded transport; the next
// one must be rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_inbox_is_a_structured_503_inbox_full() {
    // A stream that never completes: the turn parks mid-flight.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    let (stream_started_tx, stream_started_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0u8; 16384];
        let _ = sock.read(&mut buf).await;
        let head =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        let _ = sock.write_all(head).await;
        let first = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let _ = sock.write_all(first).await;
        let _ = sock.flush().await;
        let _ = stream_started_tx.send(());
        // Never [DONE]; hold until the client disconnects.
        let mut sink = [0u8; 1];
        let _ = tokio::time::timeout(Duration::from_secs(15), sock.read(&mut sink)).await;
    });

    let server = spawn_test_server_with_provider_url(format!("http://{addr}/v1")).await;

    let id = create_session(&server.base, &server.ws).await;

    // First message starts the turn that will never finish.
    let resp = post_message(&server.base, &id, text_message("start the endless turn")).await;
    assert_eq!(resp.status(), 200);
    tokio::time::timeout(Duration::from_secs(5), stream_started_rx)
        .await
        .expect("mock stream started in time")
        .expect("stream started");

    // Flood the bounded transport while the actor is mid-turn.
    for i in 0..rust_agent::bus::inbox::INBOX_CAPACITY {
        let resp = post_message(&server.base, &id, text_message(&format!("filler {i}"))).await;
        assert_eq!(resp.status(), 200, "filler {i} must be accepted");
    }

    // One more: rejected, never dropped-or-blocked.
    let resp = post_message(&server.base, &id, text_message("one too many")).await;
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.expect("503 json");
    assert_eq!(body["accepted"], false);
    assert_eq!(body["reason"], "inbox_full");

    // The rejected command was never sequenced (reject happens BEFORE the
    // event): a FRESH subscription replays the ring and shows exactly
    // session.start + 1 + INBOX_CAPACITY message.received envelopes.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let handle = server
        .state
        .handle(id.parse().expect("ulid"))
        .expect("live after 503");
    let fresh = handle
        .sequencer
        .subscribe_since(None)
        .await
        .expect("fresh subscribe");
    let received = match fresh.replay {
        rust_agent::bus::ResumeOutcome::Replay(replay) => replay
            .iter()
            .filter(|e| e.kind == "message.received")
            .count(),
        other => panic!("expected replay, got {other:?}"),
    };
    assert_eq!(
        received,
        rust_agent::bus::inbox::INBOX_CAPACITY + 1,
        "rejected command produced NO message.received"
    );
}

/// Variant of [`spawn_test_server`] with a caller-supplied provider URL (the
/// never-ending-stream test owns its mock connection directly).
async fn spawn_test_server_with_provider_url(provider_url: String) -> TestServer {
    let mock = MockProvider { url: provider_url };
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let config = test_config(&root, &mock.url);
    let state = AppState::new(config);
    let (addr, task) = spawn_server(state.clone()).await;
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("workspace dir");
    TestServer {
        base: format!("http://{addr}"),
        state,
        ws,
        _mock: mock,
        task,
        _root: tmp,
    }
}

// ---- (d) non-Text content -> 400 structured ----

// Given a message whose content includes a non-Text block
// When it is POSTed
// Then 400 with the shared {"error": ...} shape and NOTHING is sequenced
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_text_content_is_a_structured_400_and_not_sequenced() {
    let server = spawn_test_server(vec![]).await;
    let id = create_session(&server.base, &server.ws).await;
    let handle = server
        .state
        .handle(id.parse().expect("ulid"))
        .expect("live");
    let sub = handle
        .sequencer
        .subscribe_since(None)
        .await
        .expect("subscribe");

    let resp = post_message(
        &server.base,
        &id,
        json!({
            "target_actor_id": "agent:main",
            "mode": "next-turn",
            "content": [
                { "type": "text", "text": "lead" },
                { "type": "tool_use", "id": "t1", "name": "bash", "args": {} }
            ]
        }),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.expect("400 json");
    assert!(body.get("error").is_some(), "structured 400 body: {body}");

    // Nothing beyond session.start was sequenced (reject before the event).
    tokio::time::sleep(Duration::from_millis(100)).await;
    let replayed = match sub.replay {
        rust_agent::bus::ResumeOutcome::Replay(replay) => replay,
        other => panic!("expected replay, got {other:?}"),
    };
    assert_eq!(replayed.len(), 1, "only session.start");
    assert_eq!(replayed[0].kind, "session.start");
}

// ---- (e) mode:"steer" -> 200 with downgrade notice ----

// Given a message with mode "steer"
// When it is POSTed
// Then 200 (mode NEVER rejected) with a notice naming the recorded mode and
//      next-turn timing; interrupt behaves the same
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_and_interrupt_modes_are_downgraded_with_a_notice() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;

    for (mode, name) in [("steer", "steer"), ("interrupt", "interrupt")] {
        let resp = post_message(
            &server.base,
            &id,
            json!({
                "target_actor_id": "agent:main",
                "mode": mode,
                "content": [{ "type": "text", "text": format!("message for {name}") }]
            }),
        )
        .await;
        assert_eq!(resp.status(), 200, "{name} is accepted, never rejected");
        let body: Value = resp.json().await.expect("ack json");
        let notice = body["notice"].as_str().expect("downgrade notice");
        assert!(notice.contains(name), "notice names the mode: {notice}");
        assert!(
            notice.contains("next turn"),
            "notice names the timing: {notice}"
        );
    }
}

// ---- (f) POST /stop -> ordered teardown, then 404/409, no event after session.end ----

// Given a session mid-turn against a stream that never completes
// When POST /stop fires
// Then the subscribed stream shows turn.end{interrupted} -> agent.state{idle}
//      -> session.end{final_seq} IN ORDER, session.end is LAST, and a
//      follow-up POST /messages is rejected (404/409)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_tears_down_in_order_and_later_messages_are_rejected() {
    // A stream that never finishes (same technique as the 503 test).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    let (stream_started_tx, stream_started_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0u8; 16384];
        let _ = sock.read(&mut buf).await;
        let head =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        let _ = sock.write_all(head).await;
        let first = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let _ = sock.write_all(first).await;
        let _ = sock.flush().await;
        let _ = stream_started_tx.send(());
        let mut sink = [0u8; 1];
        let _ = tokio::time::timeout(Duration::from_secs(20), sock.read(&mut sink)).await;
    });

    let server = spawn_test_server_with_provider_url(format!("http://{addr}/v1")).await;
    let id = create_session(&server.base, &server.ws).await;
    let handle = server
        .state
        .handle(id.parse().expect("ulid"))
        .expect("live");
    let sub = handle
        .sequencer
        .subscribe_since(None)
        .await
        .expect("subscribe before stop");

    // Start the turn that will be interrupted.
    let resp = post_message(&server.base, &id, text_message("will be interrupted")).await;
    assert_eq!(resp.status(), 200);
    tokio::time::timeout(Duration::from_secs(5), stream_started_rx)
        .await
        .expect("stream started")
        .expect("stream started");

    // Stop.
    let client = reqwest::Client::new();
    let stop = client
        .post(format!("{}/v1/sessions/{}/stop", server.base, id))
        .send()
        .await
        .expect("stop");
    assert_eq!(stop.status(), 200);
    let stop_body: Value = stop.json().await.expect("stop json");
    assert_eq!(
        stop_body,
        json!({ "accepted": true, "actor_state": "stopped" })
    );

    // Ordered terminal events, session.end LAST.
    let events = collect_until(sub, |e| e.kind == "session.end").await;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let turn_end_idx = kinds
        .iter()
        .position(|k| *k == "turn.end")
        .expect("turn.end present");
    let agent_state_idx = kinds[turn_end_idx + 1..]
        .iter()
        .position(|k| *k == "agent.state")
        .expect("agent.state after turn.end")
        + turn_end_idx
        + 1;
    let end_idx = kinds
        .iter()
        .rposition(|k| *k == "session.end")
        .expect("end");
    assert!(
        turn_end_idx < agent_state_idx && agent_state_idx < end_idx,
        "turn.end -> agent.state -> session.end IN ORDER: {kinds:?}"
    );
    assert_eq!(kinds.last(), Some(&"session.end"), "session.end is LAST");
    assert_eq!(events[turn_end_idx].payload["outcome"], "interrupted");
    assert_eq!(events[agent_state_idx].payload["to"], "idle");
    let end_env = &events[end_idx];
    assert_eq!(end_env.payload["final_seq"], end_env.seq);

    // Gapless seqs across the whole terminal stream.
    let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "gapless: {seqs:?}"
    );

    // Follow-up POST /messages -> 404/409; nothing ever sequenced after
    // session.end.
    let late = post_message(&server.base, &id, text_message("too late")).await;
    assert!(
        late.status() == 404 || late.status() == 409,
        "post-stop message must be 404/409, got {}",
        late.status()
    );
    assert_eq!(
        end_env.seq,
        events.iter().map(|e| e.seq).max().expect("max")
    );

    // DEFECT-2 PROOF: GET-after-stop answers from the tombstone with
    // current_seq == the seq session.end ACTUALLY carried on the wire.
    let summary: Value = client
        .get(format!("{}/v1/sessions/{}", server.base, id))
        .send()
        .await
        .expect("get after stop")
        .json()
        .await
        .expect("summary json");
    assert_eq!(summary["actor_state"], "stopped");
    assert_eq!(
        summary["current_seq"], end_env.seq,
        "tombstone final_seq == observed session.end payload seq"
    );
    assert_eq!(
        summary["current_seq"], end_env.payload["final_seq"],
        "tombstone final_seq == session.end.payload.final_seq"
    );
}

// DEFECT-3 DEFERRAL PIN: reconnect-after-final_seq (replaying a stopped
// session's ring through the tombstone) is TODO 8's SSE resume behavior.
// THIS slice's contract: GET answers from the tombstone, while the routing
// resources are gone — a message POST is 404/409 and no NEW session can be
// confused with it. Pinned so the deferral is visible and testable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_session_answers_get_but_routing_is_gone_deferral_pin() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;

    let client = reqwest::Client::new();
    let stop = client
        .post(format!("{}/v1/sessions/{}/stop", server.base, id))
        .send()
        .await
        .expect("stop");
    assert_eq!(stop.status(), 200);

    // Tombstone answers GET (stopped, with a final_seq for todo 8's resume
    // floor); routing is gone for commands.
    let summary: Value = client
        .get(format!("{}/v1/sessions/{}", server.base, id))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(summary["actor_state"], "stopped");
    assert!(summary["current_seq"].is_u64(), "final_seq retained");

    let late = post_message(&server.base, &id, text_message("late")).await;
    assert!(late.status() == 404 || late.status() == 409);
}

// DEFECT-1 PROOF (slow-runtime variant, REAL HTTP): a mid-turn stop against
// a provider that DELAYS its final SSE chunk. The runtime needs real time to
// unwind, so the manager must AWAIT it (not fence first): the stream still
// ends turn.end{interrupted} -> agent.state{idle} -> session.end, with
// session.end LAST and nothing after it — even though the stop raced a
// slow in-flight turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_during_a_slow_turn_still_orders_the_terminal_tail() {
    // Scripted provider: one chunk, a real 700ms pause, then [DONE]. The
    // turn is mid-flight when /stop arrives and takes >700ms to unwind.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    let (stream_started_tx, stream_started_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0u8; 16384];
        let _ = sock.read(&mut buf).await;
        let head =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        let _ = sock.write_all(head).await;
        let first =
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"slow partial\"}}]}\n\n";
        let _ = sock.write_all(first).await;
        let _ = sock.flush().await;
        let _ = stream_started_tx.send(());
        tokio::time::sleep(Duration::from_millis(700)).await;
        let done = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late end\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let _ = sock.write_all(done).await;
        let _ = sock.flush().await;
        let _ = sock.shutdown().await;
    });

    let server = spawn_test_server_with_provider_url(format!("http://{addr}/v1")).await;
    let id = create_session(&server.base, &server.ws).await;
    let handle = server
        .state
        .handle(id.parse().expect("ulid"))
        .expect("live");
    let sub = handle
        .sequencer
        .subscribe_since(None)
        .await
        .expect("subscribe before stop");

    // Start the slow turn.
    let resp = post_message(&server.base, &id, text_message("slow turn")).await;
    assert_eq!(resp.status(), 200);
    tokio::time::timeout(Duration::from_secs(5), stream_started_rx)
        .await
        .expect("stream started")
        .expect("stream started");

    // Stop ~immediately: the turn is still unwinding inside the provider
    // stream for another ~700ms, so the manager MUST wait for the runtime.
    let client = reqwest::Client::new();
    let stop = client
        .post(format!("{}/v1/sessions/{}/stop", server.base, id))
        .send()
        .await
        .expect("stop");
    assert_eq!(stop.status(), 200);
    let stop_body: Value = stop.json().await.expect("stop json");
    assert_eq!(
        stop_body,
        json!({ "accepted": true, "actor_state": "stopped" })
    );

    // Ordered terminal tail, session.end LAST even under the delay.
    let events = collect_until(sub, |e| e.kind == "session.end").await;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let turn_end_idx = kinds
        .iter()
        .rposition(|k| *k == "turn.end")
        .expect("turn.end present");
    let agent_state_idx = kinds[turn_end_idx + 1..]
        .iter()
        .position(|k| *k == "agent.state")
        .expect("agent.state after turn.end")
        + turn_end_idx
        + 1;
    let end_idx = kinds
        .iter()
        .rposition(|k| *k == "session.end")
        .expect("end");
    assert!(
        turn_end_idx < agent_state_idx && agent_state_idx < end_idx,
        "slow runtime still ordered turn.end -> agent.state -> session.end: {kinds:?}"
    );
    assert_eq!(events[turn_end_idx].payload["outcome"], "interrupted");
    assert_eq!(events[agent_state_idx].payload["to"], "idle");
    let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "gapless: {seqs:?}"
    );

    // DEFECT-2 PROOF (slow variant): tombstone seq == the session.end the
    // subscriber actually observed.
    let summary: Value = client
        .get(format!("{}/v1/sessions/{}", server.base, id))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(
        summary["current_seq"], events[end_idx].seq,
        "tombstone == observed session.end seq under slow teardown"
    );
}

// ---- (g) unknown id on messages/stop -> structured 404 ----

// Given an id that was never minted
// When POST /messages or POST /stop targets it
// Then a structured 404 {"error": ...}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_ids_are_structured_404_on_messages_and_stop() {
    let server = spawn_test_server(vec![]).await;
    let missing = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let client = reqwest::Client::new();

    let resp = post_message(&server.base, missing, text_message("anyone?")).await;
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.expect("404 json");
    assert!(body.get("error").is_some(), "structured 404 body: {body}");

    let stop = client
        .post(format!("{}/v1/sessions/{}/stop", server.base, missing))
        .send()
        .await
        .expect("stop");
    assert_eq!(stop.status(), 404);
    let body: Value = stop.json().await.expect("404 json");
    assert!(body.get("error").is_some(), "structured 404 body: {body}");
}

// ---- (b) event-before-action, observed via sequencer.subscribe_since ----

// Given a live session and a subscriber registered BEFORE the POST
// When a message is POSTed
// Then message.received appears with the session.start before it, a
//      well-ordered seq, and its payload carries the server-assigned
//      user:http source and the command_id echoed from the ack
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_received_is_sequenced_before_any_action_with_a_seq() {
    let done =
        r#"{"choices":[{"index":0,"delta":{"content":"done here"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;
    let handle = server
        .state
        .handle(id.parse().expect("ulid"))
        .expect("live");
    let sub = handle
        .sequencer
        .subscribe_since(None)
        .await
        .expect("subscribe BEFORE the post");

    let resp = post_message(&server.base, &id, text_message("sequence me")).await;
    assert_eq!(resp.status(), 200);
    let ack: Value = resp.json().await.expect("ack json");

    let events = collect_until(sub, |e| {
        e.kind == "session.end" || { e.kind == "agent.state" && e.payload["to"] == "idle" }
    })
    .await;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds.first(), Some(&"session.start"));
    let received = events
        .iter()
        .find(|e| e.kind == "message.received")
        .expect("message.received in the subscribed stream");
    assert!(received.seq >= 1, "message.received has a real seq");
    assert_eq!(received.payload["source"], "user:http");
    assert_eq!(received.payload["target"], "agent:main");
    assert_eq!(received.payload["command_id"], ack["command_id"]);
    assert_eq!(received.payload["content"], "sequence me");
}

// ---- manual QA: pasteable real-HTTP transcript for the evidence file ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_qa_real_http_transcript() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"all done"},"finish_reason":"stop"}]}"#;
    let server = spawn_test_server(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;

    eprintln!("QA POST /messages (text) ->");
    let resp = post_message(&server.base, &id, text_message("qa hello")).await;
    let status = resp.status();
    eprintln!("  {}: {}", status, resp.text().await.expect("body"));
    assert_eq!(status, 200);

    eprintln!("QA POST /messages (non-text) ->");
    let resp = post_message(
        &server.base,
        &id,
        json!({
            "target_actor_id": "agent:main",
            "mode": "next-turn",
            "content": [{ "type": "tool_result", "tool_call_id": "t", "content": "x" }]
        }),
    )
    .await;
    let status = resp.status();
    eprintln!("  {}: {}", status, resp.text().await.expect("body"));
    assert_eq!(status, 400);

    eprintln!("QA POST /stop ->");
    let stop = reqwest::Client::new()
        .post(format!("{}/v1/sessions/{}/stop", server.base, id))
        .send()
        .await
        .expect("stop");
    let status = stop.status();
    eprintln!("  {}: {}", status, stop.text().await.expect("body"));
    assert_eq!(status, 200);

    eprintln!("QA POST /messages (after stop) ->");
    let late = post_message(&server.base, &id, text_message("late")).await;
    let status = late.status();
    eprintln!("  {}: {}", status, late.text().await.expect("body"));
    assert!(status == 404 || status == 409);
}
