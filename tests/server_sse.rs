//! Integration tests: GET /v1/sessions/:id/events over a REAL HTTP
//! server on an ephemeral loopback socket. The SSE `id:` field is the event
//! `seq`; `Last-Event-ID` resumes gaplessly; a rejected cursor or a stopped
//! session yields a control frame / clean termination, never a hang or a
//! 404 mid-race. Sessions are created against a scripted MOCK PROVIDER so a
//! posted message drives real events.

use std::time::Duration;

use serde_json::{json, Value};

mod common;
use common::server::{create_session, post_message, TestServer};
use common::sse::{has_kind, http_sse, read_sse, sse_body, SseEnd, SseFrame};

const DONE: &str = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;

/// A server with a one-turn scripted provider and one session.
async fn server_with_session() -> (TestServer, String) {
    let server = TestServer::start(vec![http_sse(&sse_body(&[DONE]))]).await;
    let id = create_session(&server.base, &server.ws).await;
    (server, id)
}

/// Connects first, then drives one turn, and returns the live frames up to
/// `turn.end`.
async fn live_turn(server: &TestServer, id: &str) -> Vec<SseFrame> {
    let (base, sid) = (server.base.clone(), id.to_string());
    let reader =
        tokio::spawn(async move { read_sse(&base, &sid, None, |f| has_kind(f, "turn.end")).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    post_message(&server.base, id, "go").await;
    let read = reader.await.expect("reader");
    assert_eq!(
        read.end,
        SseEnd::Until,
        "the live turn ends: {:?}",
        read.frames
    );
    read.frames
}

/// `(seq, kind)` of the replayable frames: live-only `llm.delta` is dropped.
fn durable(frames: &[SseFrame]) -> Vec<(u64, String)> {
    frames
        .iter()
        .filter_map(|f| Some((f.id?, f.kind()?)))
        .filter(|(_, k)| k != "llm.delta")
        .collect()
}

// ---- (a) each durable frame carries id == seq, monotonic ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_frames_carry_id_equal_to_seq_and_are_monotonic() {
    let (server, id) = server_with_session().await;
    let frames = live_turn(&server, &id).await;

    let durable: Vec<&SseFrame> = frames.iter().filter(|f| f.kind().is_some()).collect();
    assert!(!durable.is_empty(), "durable frames present: {frames:?}");
    // Each durable frame's id == its envelope seq, and ids strictly increase.
    let mut last: Option<u64> = None;
    for f in &durable {
        let seq = f.id.expect("durable frame has id");
        assert_eq!(Some(seq), f.seq(), "id == seq");
        if let Some(prev) = last {
            assert!(seq > prev, "monotonic ids: {seq} after {prev}");
        }
        last = Some(seq);
    }
    // session.start is seq 0 and is present (absent Last-Event-ID replays it).
    assert_eq!(durable[0].id, Some(0), "first durable is session.start@0");
    assert_eq!(durable[0].kind().as_deref(), Some("session.start"));
}

// ---- (b) reconnect with Last-Event-ID: K resumes gaplessly; a fresh
//         connection without the header replays from seq 0 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_with_last_event_id_resumes_without_gap_or_dup() {
    let (server, id) = server_with_session().await;
    let live = live_turn(&server, &id).await;
    // The ring retains only DURABLE kinds; `llm.delta` is live-only and
    // legitimately absent on replay, so the resume contract is over the
    // durable subsequence (a dropped-delta seq gap is expected + legal).
    let seqs = |fs: &[SseFrame]| -> Vec<u64> { durable(fs).into_iter().map(|(s, _)| s).collect() };
    let first_durable = seqs(&live);
    assert!(
        first_durable.len() >= 3,
        "several durable events: {first_durable:?}"
    );

    let k = first_durable[1];
    let resumed = read_sse(&server.base, &id, Some(k), |f| has_kind(f, "turn.end")).await;
    let resumed_durable = seqs(&resumed.frames);
    let expected: Vec<u64> = first_durable.iter().copied().filter(|s| *s > k).collect();
    assert_eq!(
        resumed_durable, expected,
        "gapless resume of durable seq > {k}"
    );
    assert!(
        !resumed_durable.contains(&k),
        "no duplicate of the cursor event"
    );

    // A fresh connection after the turn, with NO Last-Event-ID, must replay
    // session.start@0 (not skip it as if the cursor were 0).
    let fresh = read_sse(&server.base, &id, None, |f| has_kind(f, "turn.end")).await;
    let first = fresh
        .frames
        .iter()
        .find(|f| f.kind().is_some())
        .expect("a durable frame");
    assert_eq!(first.id, Some(0), "replayed from seq 0, not cursor-0-skip");
    assert_eq!(first.kind().as_deref(), Some("session.start"));
}

// ---- (d) a future cursor is a resync control frame, never silent caught-up ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn future_cursor_yields_resync_control_frame() {
    let (server, id) = server_with_session().await;

    // Cursor far beyond head -> InvalidCursor -> EXACTLY one "resync" control
    // frame, no durable events, then a clean server EOF (never a silent empty
    // "caught up" stream, never a hang).
    let read = read_sse(&server.base, &id, Some(9_999_999), |_| false).await;
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
        read.frames[0].kind().is_none(),
        "resync frame carries no durable envelope"
    );
}

// ---- (e) reconnect-after-stop replays through final_seq then terminates
//         (no 404 mid-race, no hang) ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_after_stop_replays_through_final_seq_then_terminates() {
    let (server, id) = server_with_session().await;

    // Observe the live history first so the expected replay set is grounded
    // in what actually happened (not a guess about teardown timing).
    let expected = durable(&live_turn(&server, &id).await);
    assert!(
        !expected.is_empty(),
        "observed a durable history: {expected:?}"
    );
    let stop = reqwest::Client::new()
        .post(format!("{}/v1/sessions/{id}/stop", server.base))
        .send()
        .await
        .expect("stop");
    assert_eq!(stop.status(), 200);

    // GET /events on the stopped session: 200 (not 404), replays the durable
    // history through final_seq (session.end LAST), then ends with a clean
    // server EOF — not a client-budget timeout, not a hang.
    let read = read_sse(&server.base, &id, None, |_| false).await;
    assert_eq!(read.end, SseEnd::Eof, "clean server EOF, not timeout/error");
    let replayed = durable(&read.frames);
    // Everything observed live is contained, in order.
    let mut it = replayed.iter();
    for pair in &expected {
        assert!(
            it.any(|r| r == pair),
            "stopped replay missing/misordered {pair:?}: got {replayed:?}"
        );
    }
    let seqs: Vec<u64> = replayed.iter().map(|(s, _)| *s).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] > w[0]),
        "stopped replay is seq-ordered: {seqs:?}"
    );
    let last = replayed.last().expect("stopped replayed something");
    assert_eq!(
        last.1, "session.end",
        "session.end (final_seq) is the LAST replayed event: {replayed:?}"
    );
    let payload: Value = serde_json::from_str(
        &read
            .frames
            .iter()
            .rev()
            .find(|f| f.kind().as_deref() == Some("session.end"))
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
    use navai::bus::event::EventKind;
    use navai::bus::sequencer::{Candidate, Sequencer};
    use navai::bus::{ResumeOutcome, SessionBus};

    let seq = Sequencer::spawn(ulid::Ulid::new(), SessionBus::default(), 0);

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
    let (server, id) = server_with_session().await;

    let reader = |base: String, id: String| {
        tokio::spawn(async move {
            read_sse(&base, &id, None, |f| has_kind(f, "turn.end"))
                .await
                .frames
        })
    };
    let r1 = reader(server.base.clone(), id.clone());
    let r2 = reader(server.base.clone(), id.clone());
    tokio::time::sleep(Duration::from_millis(150)).await;
    post_message(&server.base, &id, "go").await;

    let f1 = durable(&r1.await.expect("r1"));
    let f2 = durable(&r2.await.expect("r2"));
    assert_eq!(f1, f2, "identical durable histories across clients");
    assert!(!f1.is_empty(), "non-empty durable history");
}
