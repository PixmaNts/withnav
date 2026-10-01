//! Integration tests: POST /v1/sessions/:id/messages and
//! POST /v1/sessions/:id/stop over a REAL HTTP server on an ephemeral
//! loopback socket. Sessions are created against a MOCK PROVIDER (the same
//! scripted-TCP technique as the runtime tests) so a posted message drives a
//! real turn. Bodies are the frozen ones from `server::routes`.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod common;
use common::server::{collect_until, create_session, post_message, post_message_body, TestServer};
use common::sse::{http_sse, sse_body};

// ---- (a) happy path: 200 {accepted:true} immediately; message reaches the agent ----

// Given a live session backed by a scripted one-turn mock provider
// When a text message is POSTed
// Then 200 with the frozen {command_id, accepted:true, actor_state} body and
//      NO notice; the event log shows message.received BEFORE the turn events
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_message_acks_immediately_and_drives_a_turn() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"all done"},"finish_reason":"stop"}]}"#;
    let server = TestServer::start(vec![http_sse(&sse_body(&[done]))]).await;

    let id = create_session(&server.base, &server.ws).await;
    let sub = server
        .state
        .subscribe(id.parse().expect("ulid"), None)
        .await
        .expect("subscribe");

    let resp = post_message(&server.base, &id, "please finish the turn").await;
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

    let server = TestServer::start_with_provider_url(&format!("http://{addr}/v1")).await;

    let id = create_session(&server.base, &server.ws).await;

    // First message starts the turn that will never finish.
    let resp = post_message(&server.base, &id, "start the endless turn").await;
    assert_eq!(resp.status(), 200);
    tokio::time::timeout(Duration::from_secs(5), stream_started_rx)
        .await
        .expect("mock stream started in time")
        .expect("stream started");

    // Flood the bounded transport while the actor is mid-turn.
    for i in 0..rust_agent::bus::inbox::INBOX_CAPACITY {
        let resp = post_message(&server.base, &id, &format!("filler {i}")).await;
        assert_eq!(resp.status(), 200, "filler {i} must be accepted");
    }

    // One more: rejected, never dropped-or-blocked.
    let resp = post_message(&server.base, &id, "one too many").await;
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.expect("503 json");
    assert_eq!(body["accepted"], false);
    assert_eq!(body["reason"], "inbox_full");

    // The rejected command was never sequenced (reject happens BEFORE the
    // event): a FRESH subscription replays the ring and shows exactly
    // session.start + 1 + INBOX_CAPACITY message.received envelopes.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let fresh = server
        .state
        .subscribe(id.parse().expect("ulid"), None)
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

// ---- (d) non-Text content -> 400 structured ----

// Given a message whose content includes a non-Text block
// When it is POSTed
// Then 400 with the shared {"error": ...} shape and NOTHING is sequenced
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_text_content_is_a_structured_400_and_not_sequenced() {
    let server = TestServer::start(vec![]).await;
    let id = create_session(&server.base, &server.ws).await;
    let sub = server
        .state
        .subscribe(id.parse().expect("ulid"), None)
        .await
        .expect("subscribe");

    let resp = post_message_body(
        &server.base,
        &id,
        &json!({
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
    let server = TestServer::start(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;

    for (mode, name) in [("steer", "steer"), ("interrupt", "interrupt")] {
        let resp = post_message_body(
            &server.base,
            &id,
            &json!({
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

    let server = TestServer::start_with_provider_url(&format!("http://{addr}/v1")).await;
    let id = create_session(&server.base, &server.ws).await;
    let sub = server
        .state
        .subscribe(id.parse().expect("ulid"), None)
        .await
        .expect("subscribe before stop");

    // Start the turn that will be interrupted.
    let resp = post_message(&server.base, &id, "will be interrupted").await;
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
    let late = post_message(&server.base, &id, "too late").await;
    assert!(
        late.status() == 404 || late.status() == 409,
        "post-stop message must be 404/409, got {}",
        late.status()
    );
    assert_eq!(
        end_env.seq,
        events.iter().map(|e| e.seq).max().expect("max")
    );

    // GET-after-stop answers from the store with
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
        "stored last_seq == observed session.end payload seq"
    );
    assert_eq!(
        summary["current_seq"], end_env.payload["final_seq"],
        "stored last_seq == session.end.payload.final_seq"
    );
}

// A stopped session's runtime is gone: GET still answers from the store,
// but a message POST is a 409 asking to resume first (the store answers
// reads, never commands).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_session_answers_get_but_routing_is_gone() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
    let server = TestServer::start(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;

    let client = reqwest::Client::new();
    let stop = client
        .post(format!("{}/v1/sessions/{}/stop", server.base, id))
        .send()
        .await
        .expect("stop");
    assert_eq!(stop.status(), 200);

    // The store answers GET (stopped, with the stored last_seq);
    // routing is gone for commands.
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

    let late = post_message(&server.base, &id, "late").await;
    assert_eq!(late.status(), 409);
    let body: Value = late.json().await.expect("json");
    assert_eq!(body["reason"], "resume_required", "{body}");
}

// Slow-runtime variant, REAL HTTP: a mid-turn stop against
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

    let server = TestServer::start_with_provider_url(&format!("http://{addr}/v1")).await;
    let id = create_session(&server.base, &server.ws).await;
    let sub = server
        .state
        .subscribe(id.parse().expect("ulid"), None)
        .await
        .expect("subscribe before stop");

    // Start the slow turn.
    let resp = post_message(&server.base, &id, "slow turn").await;
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

    // Slow variant: stored seq == the session.end the
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
        "stored seq == observed session.end seq under slow teardown"
    );
}

// ---- (g) unknown id on messages/stop -> structured 404 ----

// Given an id that was never minted
// When POST /messages or POST /stop targets it
// Then a structured 404 {"error": ...}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_ids_are_structured_404_on_messages_and_stop() {
    let server = TestServer::start(vec![]).await;
    let missing = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let client = reqwest::Client::new();

    let resp = post_message(&server.base, missing, "anyone?").await;
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
    let server = TestServer::start(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;
    let sub = server
        .state
        .subscribe(id.parse().expect("ulid"), None)
        .await
        .expect("subscribe BEFORE the post");

    let resp = post_message(&server.base, &id, "sequence me").await;
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
    let server = TestServer::start(vec![http_sse(&sse_body(&[done]))]).await;
    let id = create_session(&server.base, &server.ws).await;

    eprintln!("QA POST /messages (text) ->");
    let resp = post_message(&server.base, &id, "qa hello").await;
    let status = resp.status();
    eprintln!("  {}: {}", status, resp.text().await.expect("body"));
    assert_eq!(status, 200);

    eprintln!("QA POST /messages (non-text) ->");
    let resp = post_message_body(
        &server.base,
        &id,
        &json!({
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
    let late = post_message(&server.base, &id, "late").await;
    let status = late.status();
    eprintln!("  {}: {}", status, late.text().await.expect("body"));
    assert!(status == 404 || status == 409);
}
