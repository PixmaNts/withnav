//! The terminal client's transport against a real in-process session server:
//! catalog, create/subscribe/submit, reconnect, resync, stop/resume, auth
//! failures, delivery uncertainty and the tool duration metadata.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use navai::bus::command::ContentBlock;
use navai::bus::event::{EventEnvelope, EventKind};
use navai::tui::client::{Client, SendError};
use navai::tui::protocol::{ClientEvent, CreateSessionRequest, Generation, SubmitRequest};
use navai::tui::stream::{subscribe, EventStream};

mod common;
use common::server::{spawn_server, CuttableProxy, TestServer, TestServerConfig};
use common::sse::{http_sse, sse_body, text_chunk, tool_call_chunk};

const WAIT: Duration = Duration::from_secs(15);

fn generation() -> Generation {
    Generation(7)
}

fn answer(text: &str) -> Vec<u8> {
    http_sse(&sse_body(&[&text_chunk(text)]))
}

fn submit(text: &str) -> SubmitRequest {
    SubmitRequest {
        target_actor_id: "agent:main".into(),
        mode: "next-turn".into(),
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

fn workspace(server: &TestServer) -> String {
    server.ws.to_str().expect("utf8 workspace").to_string()
}

async fn create(client: &Client, server: &TestServer) -> String {
    client
        .create(CreateSessionRequest {
            workspace: workspace(server),
            model: Some("default".into()),
            resume: None,
        })
        .await
        .expect("create")
        .session_id
}

fn open(client: &Client, id: &str, cursor: Option<u64>) -> (EventStream, CancellationToken) {
    let cancel = CancellationToken::new();
    let stream = subscribe(
        client.clone(),
        id.to_string(),
        cursor,
        generation(),
        cancel.clone(),
    );
    (stream, cancel)
}

async fn next(stream: &mut EventStream) -> Option<ClientEvent> {
    tokio::time::timeout(WAIT, stream.next())
        .await
        .expect("timed out waiting for a stream event")
}

/// Pulls events until `pred` holds for one, returning all seen including it.
async fn until(stream: &mut EventStream, pred: impl Fn(&ClientEvent) -> bool) -> Vec<ClientEvent> {
    let mut seen = Vec::new();
    loop {
        let event = next(stream)
            .await
            .unwrap_or_else(|| panic!("stream ended early; seen: {seen:?}"));
        let done = pred(&event);
        seen.push(event);
        if done {
            return seen;
        }
    }
}

fn is_kind(event: &ClientEvent, kind: &str) -> bool {
    matches!(event, ClientEvent::Envelope { envelope, .. } if envelope.kind == kind)
}

fn envelopes(events: &[ClientEvent]) -> Vec<&EventEnvelope> {
    events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::Envelope { envelope, .. } => Some(envelope),
            _ => None,
        })
        .collect()
}

fn seqs(events: &[ClientEvent]) -> Vec<u64> {
    envelopes(events).iter().map(|e| e.seq).collect()
}

fn assert_strictly_increasing(seqs: &[u64]) {
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "seqs must strictly increase: {seqs:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_lists_the_catalog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = TestServerConfig::mock(tmp.path(), "http://127.0.0.1:9")
        .chat_tools(128_000)
        .state();
    let (addr, task) = spawn_server(state).await;
    let client = Client::new(format!("http://{addr}"), None);
    let body = client
        .models(tmp.path().to_str().expect("utf8"))
        .await
        .expect("models");
    task.abort();
    assert_eq!(body.models.len(), 1);
    assert_eq!(body.models[0].alias, "default");
    assert_eq!(body.models[0].model, "mock-model");
    assert_eq!(body.models[0].context_window, Some(128_000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_lists_a_created_conversation_and_summary_reports_the_cursor() {
    let server = TestServer::start(vec![]).await;
    let client = Client::new(server.base.clone(), None);
    let id = create(&client, &server).await;
    let listed = client.sessions(&workspace(&server)).await.expect("list");
    assert!(listed.sessions.iter().any(|s| s.id == id && s.live));
    let summary = client.summary(&id).await.expect("summary");
    assert_eq!(summary.session_id, id);
    assert_eq!(summary.model, "default");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_subscribe_send_delivers_envelopes_in_seq_order() {
    let server = TestServer::start(vec![answer("hello there")]).await;
    let client = Client::new(server.base.clone(), None);
    let id = create(&client, &server).await;

    let (mut stream, cancel) = open(&client, &id, None);
    let first = next(&mut stream).await.expect("first event");
    assert_eq!(
        first,
        ClientEvent::StreamConnected {
            generation: generation()
        },
        "the connection is reported before any envelope"
    );

    let ack = client.send(&id, submit("say hello")).await.expect("send");
    assert!(ack.accepted);

    let events = until(&mut stream, |e| is_kind(e, "turn.end")).await;
    let seqs = seqs(&events);
    assert_eq!(seqs.first(), Some(&0), "replay starts at session.start");
    assert_strictly_increasing(&seqs);
    for kind in [
        "session.start",
        "message.received",
        "llm.response",
        "turn.end",
    ] {
        assert!(
            events.iter().any(|e| is_kind(e, kind)),
            "missing {kind}: {events:?}"
        );
    }
    assert!(events.iter().all(|e| match e {
        ClientEvent::Envelope { generation: g, .. } => *g == generation(),
        _ => true,
    }));
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribing_from_a_cursor_skips_what_was_already_applied() {
    let server = TestServer::start(vec![answer("one"), answer("two")]).await;
    let client = Client::new(server.base.clone(), None);
    let id = create(&client, &server).await;

    let (mut stream, cancel) = open(&client, &id, None);
    client.send(&id, submit("first")).await.expect("send");
    let first = until(&mut stream, |e| is_kind(e, "turn.end")).await;
    let cursor = *seqs(&first).last().expect("some envelope");
    cancel.cancel();

    let (mut again, cancel) = open(&client, &id, Some(cursor));
    client.send(&id, submit("second")).await.expect("send");
    let second = until(&mut again, |e| is_kind(e, "turn.end")).await;
    let seqs = seqs(&second);
    assert!(
        seqs.iter().all(|s| *s > cursor),
        "no replay at or below {cursor}: {seqs:?}"
    );
    assert_strictly_increasing(&seqs);
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_connection_reconnects_without_duplicates() {
    let server = TestServer::start(vec![answer("one"), answer("two")]).await;
    let proxy = CuttableProxy::start(&server.base).await;
    let direct = Client::new(server.base.clone(), None);
    let via_proxy = Client::new(proxy.base.clone(), None);
    let id = create(&direct, &server).await;

    let (mut stream, cancel) = open(&via_proxy, &id, None);
    direct.send(&id, submit("first")).await.expect("send");
    let mut all = until(&mut stream, |e| is_kind(e, "turn.end")).await;

    proxy.cut();
    let after_cut = until(&mut stream, |e| {
        matches!(e, ClientEvent::StreamConnected { .. })
    })
    .await;
    assert!(
        after_cut
            .iter()
            .any(|e| matches!(e, ClientEvent::Reconnecting { .. })),
        "the loss is reported: {after_cut:?}"
    );
    all.extend(after_cut);

    let first_turn_end = *seqs(&all).last().expect("envelopes before the cut");
    direct.send(&id, submit("second")).await.expect("send");
    let rest = until(&mut stream, |e| {
        is_kind(e, "turn.end")
            && matches!(e, ClientEvent::Envelope { envelope, .. } if envelope.seq > first_turn_end)
    })
    .await;
    all.extend(rest);

    let seqs = seqs(&all);
    assert_strictly_increasing(&seqs);
    let turn_ends = envelopes(&all)
        .iter()
        .filter(|e| e.kind == "turn.end")
        .count();
    assert_eq!(turn_ends, 2, "each turn.end arrives exactly once");
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cursor_beyond_the_head_yields_resync_then_a_full_replay() {
    let server = TestServer::start(vec![]).await;
    let client = Client::new(server.base.clone(), None);
    let id = create(&client, &server).await;

    let (mut stream, cancel) = open(&client, &id, Some(100_000));
    let events = until(&mut stream, |e| matches!(e, ClientEvent::Envelope { .. })).await;
    let g = generation();
    assert_eq!(
        events[..events.len() - 1],
        [
            ClientEvent::StreamConnected {
                generation: g.clone()
            },
            ClientEvent::Resync {
                generation: g.clone()
            },
            ClientEvent::Reconnecting {
                generation: g.clone()
            },
            ClientEvent::StreamConnected { generation: g },
        ]
    );
    assert_eq!(
        seqs(&events),
        vec![0],
        "the replay restarts at session.start"
    );
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_token_ends_the_stream_without_a_reconnect_loop() {
    let server = TestServer::start_with_token(vec![], "right").await;
    let wrong = Client::new(server.base.clone(), Some("wrong".into()));

    let error = wrong.models(&workspace(&server)).await.expect_err("401");
    assert!(error.contains("401"), "{error}");

    let right = Client::new(server.base.clone(), Some("right".into()));
    let id = create(&right, &server).await;
    let (mut stream, _cancel) = open(&wrong, &id, None);
    match next(&mut stream).await.expect("protocol error") {
        ClientEvent::ProtocolError { message, .. } => assert!(message.contains("401"), "{message}"),
        other => panic!("expected a protocol error, got {other:?}"),
    }
    assert_eq!(
        next(&mut stream).await,
        Some(ClientEvent::StreamDisconnected {
            generation: generation()
        })
    );
    assert_eq!(next(&mut stream).await, None, "no reconnect attempts");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_session_ends_the_stream_with_the_reason() {
    let server = TestServer::start(vec![]).await;
    let client = Client::new(server.base.clone(), None);
    let missing = ulid::Ulid::new().to_string();

    let (mut stream, _cancel) = open(&client, &missing, None);
    match next(&mut stream).await.expect("protocol error") {
        ClientEvent::ProtocolError { message, .. } => assert!(message.contains("404"), "{message}"),
        other => panic!("expected a protocol error, got {other:?}"),
    }
    assert!(matches!(
        next(&mut stream).await,
        Some(ClientEvent::StreamDisconnected { .. })
    ));
    assert_eq!(next(&mut stream).await, None);

    let error = client.summary(&missing).await.expect_err("404");
    assert!(error.contains("404"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_ends_the_stream_and_the_same_id_resumes() {
    let server = TestServer::start(vec![answer("one"), answer("two")]).await;
    let client = Client::new(server.base.clone(), None);
    let id = create(&client, &server).await;

    let (mut stream, _cancel) = open(&client, &id, None);
    client.send(&id, submit("first")).await.expect("send");
    until(&mut stream, |e| is_kind(e, "turn.end")).await;

    let stopped = client.stop(&id).await.expect("stop");
    assert!(stopped.accepted);
    let tail = until(&mut stream, |e| {
        matches!(e, ClientEvent::StreamDisconnected { .. })
    })
    .await;
    let end_seq = envelopes(&tail)
        .iter()
        .find(|e| e.kind == "session.end")
        .map(|e| e.seq)
        .unwrap_or_else(|| panic!("session.end delivered before the disconnect: {tail:?}"));
    assert_eq!(
        next(&mut stream).await,
        None,
        "a stopped session stays stopped"
    );

    let resumed = client
        .resume(&workspace(&server), &id)
        .await
        .expect("resume");
    assert_eq!(resumed.session_id, id, "the same conversation");

    let (mut stream, cancel) = open(&client, &id, None);
    client.send(&id, submit("second")).await.expect("send");
    let events = until(&mut stream, |e| {
        is_kind(e, "turn.end")
            && matches!(e, ClientEvent::Envelope { envelope, .. } if envelope.seq > end_seq)
    })
    .await;
    assert_strictly_increasing(&seqs(&events));
    assert!(
        events
            .iter()
            .filter(|e| is_kind(e, "message.received"))
            .count()
            >= 2,
        "the history replays with the new message: {events:?}"
    );
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_ends_an_idle_stream_promptly() {
    let server = TestServer::start(vec![]).await;
    let client = Client::new(server.base.clone(), None);
    let id = create(&client, &server).await;

    let (mut stream, cancel) = open(&client, &id, None);
    until(&mut stream, |e| is_kind(e, "session.start")).await;
    cancel.cancel();
    let ended = tokio::time::timeout(Duration::from_secs(2), async {
        while stream.next().await.is_some() {}
    })
    .await;
    assert!(ended.is_ok(), "the stream must end after cancel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_post_call_carries_duration_ms() {
    let call = tool_call_chunk("call_1", "bash", &json!({ "command": "echo hi" }));
    let server = TestServer::start(vec![http_sse(&sse_body(&[&call])), answer("done")]).await;
    let client = Client::new(server.base.clone(), None);
    let id = create(&client, &server).await;

    let (mut stream, cancel) = open(&client, &id, None);
    client.send(&id, submit("run it")).await.expect("send");
    let events = until(&mut stream, |e| is_kind(e, "turn.end")).await;
    let post = envelopes(&events)
        .into_iter()
        .find(|e| e.kind == EventKind::ToolPostCall.as_str())
        .expect("tool.post_call");
    assert!(
        post.payload["metadata"]["duration_ms"].is_u64(),
        "{}",
        post.payload
    );
    cancel.cancel();
}

// ---- scripted raw servers: failure shapes a real server will not produce ----

/// Accepts connections, records each request head, and runs `respond` on the
/// socket. Returns the base URL and the recorded heads.
async fn raw_server<F, Fut>(respond: F) -> (String, Arc<Mutex<Vec<String>>>)
where
    F: Fn(usize, tokio::net::TcpStream) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let heads: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = heads.clone();
    tokio::spawn(async move {
        let mut index = 0;
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 16384];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            seen.lock()
                .expect("heads")
                .push(String::from_utf8_lossy(&buf[..n]).into_owned());
            tokio::spawn(respond(index, socket));
            index += 1;
        }
    });
    (format!("http://{addr}"), heads)
}

fn sse_head() -> &'static str {
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
}

fn frame(seq: u64, kind: EventKind) -> String {
    let envelope = EventEnvelope::new(ulid::Ulid::new(), seq, kind, json!({}));
    format!(
        "id: {seq}\ndata: {}\n\n",
        serde_json::to_string(&envelope).expect("envelope json")
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undecodable_frames_are_reported_and_the_stream_keeps_going() {
    let body = format!(
        "{}data: {{not json\n\n{}",
        frame(0, EventKind::SessionStart),
        frame(1, EventKind::TurnStart)
    );
    let (base, heads) = raw_server(move |index, mut socket| {
        let body = body.clone();
        async move {
            if index == 0 {
                let _ = socket
                    .write_all(format!("{}{body}", sse_head()).as_bytes())
                    .await;
            } else {
                // Second connection: hold it open so the test can end it.
                let _ = socket.write_all(sse_head().as_bytes()).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    })
    .await;
    let client = Client::new(base, None);
    let (mut stream, cancel) = open(&client, "01ARZ3NDEKTSV4RRFFQ69G5FAV", None);

    let events = until(&mut stream, |e| {
        matches!(e, ClientEvent::Reconnecting { .. })
    })
    .await;
    assert!(matches!(events[0], ClientEvent::StreamConnected { .. }));
    assert_eq!(seqs(&events), vec![0, 1]);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ClientEvent::ProtocolError { message, .. } if message.contains("undecodable"))),
        "{events:?}"
    );

    until(&mut stream, |e| {
        matches!(e, ClientEvent::StreamConnected { .. })
    })
    .await;
    let heads = heads.lock().expect("heads").clone();
    assert!(
        !heads[0].to_lowercase().contains("last-event-id"),
        "first connect has no cursor: {}",
        heads[0]
    );
    assert!(
        heads[1].to_lowercase().contains("last-event-id: 1"),
        "reconnect resumes after the highest delivered seq: {}",
        heads[1]
    );
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_the_server_may_have_received_is_uncertain() {
    // The server reads the request and hangs up without answering.
    let (base, _) = raw_server(|_, mut socket| async move {
        let _ = socket.shutdown().await;
    })
    .await;
    let client = Client::new(base, None);
    let result = client
        .send("01ARZ3NDEKTSV4RRFFQ69G5FAV", submit("hi"))
        .await;
    assert!(matches!(result, Err(SendError::Uncertain(_))), "{result:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_without_an_answer_in_time_is_uncertain() {
    let (base, _) = raw_server(|_, _socket| async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
    })
    .await;
    let client = Client::new(base, None).with_request_timeout(Duration::from_millis(200));
    let result = client
        .send("01ARZ3NDEKTSV4RRFFQ69G5FAV", submit("hi"))
        .await;
    assert!(matches!(result, Err(SendError::Uncertain(_))), "{result:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_that_never_connected_or_was_refused_is_rejected() {
    // Nothing listens on port 1.
    let client = Client::new("http://127.0.0.1:1", None);
    let result = client
        .send("01ARZ3NDEKTSV4RRFFQ69G5FAV", submit("hi"))
        .await;
    assert!(matches!(result, Err(SendError::Rejected(_))), "{result:?}");

    let server = TestServer::start(vec![]).await;
    let client = Client::new(server.base.clone(), None);
    let missing = ulid::Ulid::new().to_string();
    match client.send(&missing, submit("hi")).await {
        Err(SendError::Rejected(message)) => assert!(message.contains("404"), "{message}"),
        other => panic!("expected a rejection, got {other:?}"),
    }
}
