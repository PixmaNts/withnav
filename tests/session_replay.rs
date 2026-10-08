//! Live subscriptions replay the stored history, then follow live events:
//! the replay/live seam past the 4096-event ring, the three cursor cases,
//! hidden stale `session.end` envelopes, and the fallback to the ring when
//! the history read fails.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use ulid::Ulid;

use withnav::bus::event::{EventEnvelope, EventKind};
use withnav::bus::ResumeOutcome;
use withnav::server::AppState;
use withnav::session::store::HistoryStore;
use withnav::session::Subscription;

mod common;
use common::server::{create_session, post_message_ok, spawn_server, TestServerConfig};
use common::sse::{http_sse, spawn_mock_provider, sse_body};

/// How many durable events the seeded conversation holds: past the ring.
const SEEDED: u64 = 4200;

fn done_responses(count: usize) -> Vec<Vec<u8>> {
    let chunk = r#"{"choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":"stop"}]}"#;
    (0..count).map(|_| http_sse(&sse_body(&[chunk]))).collect()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
    ws: PathBuf,
    provider_url: String,
}

async fn fixture() -> Fixture {
    let provider = spawn_mock_provider(done_responses(8)).await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("workspace dir");
    Fixture {
        db: root.join("sessions.db"),
        root,
        ws,
        provider_url: provider.url,
        _tmp: tmp,
    }
}

fn state(fx: &Fixture) -> AppState {
    TestServerConfig::mock(&fx.root, &fx.provider_url)
        .history_db(fx.db.clone())
        .state()
}

fn seqs(replay: &ResumeOutcome) -> Vec<u64> {
    match replay {
        ResumeOutcome::Replay(events) => events.iter().map(|e| e.seq).collect(),
        other => panic!("expected a replay, got {other:?}"),
    }
}

/// Reads live events until `pred` matches (inclusive).
async fn live_until(
    live: &mut tokio::sync::mpsc::Receiver<EventEnvelope>,
    pred: impl Fn(&EventEnvelope) -> bool,
) -> Vec<EventEnvelope> {
    let mut out = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(20), live.recv()).await {
        let done = pred(&event);
        out.push(event);
        if done {
            return out;
        }
    }
    panic!("the live stream ended or stalled before the awaited event: {out:?}");
}

/// Stores a conversation of `SEEDED` durable events (seq 0 is
/// `session.start`) for a fresh id, through a store of its own.
async fn seed_conversation(fx: &Fixture) -> Ulid {
    let id = Ulid::new();
    let store = HistoryStore::open(&fx.db).expect("open store");
    store
        .record_session(id, &fx.ws.display().to_string(), "mock/mock-model", false)
        .await
        .expect("record session");
    for seq in 0..SEEDED {
        let kind = if seq == 0 {
            EventKind::SessionStart
        } else {
            EventKind::TurnStart
        };
        store.append_event(id, EventEnvelope::new(id, seq, kind, json!({ "n": seq })));
    }
    store.flush().await.expect("flush seed");
    id
}

/// A fresh session driven through one turn; returns the id and its durable
/// seqs, once the turn has ended and the store holds them.
async fn session_with_one_turn(state: &AppState, base: &str, ws: &Path) -> (Ulid, Vec<u64>) {
    let id = create_session(base, ws).await;
    let ulid: Ulid = id.parse().expect("ulid");
    let mut sub = state.subscribe(ulid, None).await.expect("subscribe");
    post_message_ok(base, &id, "hello").await;
    let mut live = sub.live.take().expect("live receiver");
    live_until(&mut live, |e| e.kind == "turn.end").await;
    state.history().flush().await.expect("flush");
    let all = state.subscribe(ulid, None).await.expect("subscribe");
    (ulid, seqs(&all.replay))
}

// Given a stored conversation of more than 4096 durable events, resumed
// When a client subscribes with no cursor
// Then every stored event arrives in order (the ring holds only 4096), and
//      events published afterwards arrive live exactly once, after the replay
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_4096_events_replay_completely_across_live_seam() {
    let fx = fixture().await;
    let id = seed_conversation(&fx).await;
    let state = state(&fx);
    let (resumed_id, resumed) = state
        .create_or_resume_session(&fx.ws, None, false, Some(id))
        .await
        .expect("resume");
    assert_eq!((resumed_id, resumed), (id, true));
    let (addr, task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");

    let sub = state.subscribe(id, None).await.expect("subscribe");
    // The resume itself wrote one event: bypass starts off.
    assert_eq!(seqs(&sub.replay), (0..=SEEDED).collect::<Vec<_>>());

    let mut live = sub.live.expect("live receiver");
    post_message_ok(&base, &id.to_string(), "after the replay").await;
    let events = live_until(&mut live, |e| e.kind == "turn.end").await;
    let live_seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(
        live_seqs,
        (SEEDED + 1..=SEEDED + live_seqs.len() as u64).collect::<Vec<_>>(),
        "live events continue the replay: each seq once, none missing"
    );
    assert!(events.iter().any(|e| e.kind == "message.received"));
    task.abort();
}

// Given a live session that completed a turn (head H)
// When a client subscribes with the cursor at H, in the middle, and in the
//      future
// Then at H the replay is empty and the stream stays live; in the middle
//      exactly the stored events after it replay; in the future the cursor
//      is invalid and there is no live stream
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_at_head_and_in_the_middle_and_in_the_future() {
    let fx = fixture().await;
    let state = state(&fx);
    let (addr, task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");
    let (id, all) = session_with_one_turn(&state, &base, &fx.ws).await;
    let head = *all.last().expect("events");
    assert!(all.len() > 3, "a turn stores several events: {all:?}");

    let at_head = state.subscribe(id, Some(head)).await.expect("at head");
    assert!(seqs(&at_head.replay).is_empty());
    assert!(at_head.live.is_some());

    let middle = all[all.len() / 2];
    let mid = state.subscribe(id, Some(middle)).await.expect("middle");
    let expected: Vec<u64> = all.iter().copied().filter(|&s| s > middle).collect();
    assert_eq!(seqs(&mid.replay), expected);
    assert!(mid.live.is_some());

    let future = state.subscribe(id, Some(head + 100)).await.expect("future");
    assert!(matches!(future.replay, ResumeOutcome::InvalidCursor));
    assert!(future.live.is_none());
    task.abort();
}

// Given a conversation that was stopped (its store holds a session.end)
// When it is resumed, driven, and a client subscribes with no cursor
// Then the replay is in order, shows no session.end, and runs past the
//      stale end's seq
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_stream_hides_the_stale_session_end() {
    let fx = fixture().await;
    let state = state(&fx);
    let (addr, task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");
    let (id, _) = session_with_one_turn(&state, &base, &fx.ws).await;

    let mut closing = state.subscribe(id, None).await.expect("subscribe");
    state.close_session(id).await.expect("close");
    let mut live = closing.live.take().expect("live receiver");
    let end_seq = live_until(&mut live, |e| e.kind == "session.end")
        .await
        .last()
        .expect("session.end")
        .seq;

    state
        .create_or_resume_session(&fx.ws, None, false, Some(id))
        .await
        .expect("resume");
    // Subscribe before posting: a fast turn could otherwise end before the
    // subscription exists, its `turn.end` would land in the replay, and the
    // live stream would wait for an event that already happened.
    let mut fresh = state.subscribe(id, None).await.expect("subscribe");
    let mut live = fresh.live.take().expect("live receiver");
    post_message_ok(&base, &id.to_string(), "second turn").await;
    live_until(&mut live, |e| e.kind == "turn.end").await;
    state.history().flush().await.expect("flush");

    let sub = state.subscribe(id, None).await.expect("subscribe");
    let replayed = match &sub.replay {
        ResumeOutcome::Replay(events) => events,
        other => panic!("expected a replay, got {other:?}"),
    };
    assert!(replayed.iter().all(|e| e.kind != "session.end"));
    assert!(replayed.windows(2).all(|w| w[0].seq < w[1].seq), "in order");
    assert!(
        replayed.iter().any(|e| e.seq > end_seq),
        "the replay runs past the stale end ({end_seq})"
    );
    task.abort();
}

// Given a live session whose stored history cannot be read (a row in the
//       replay range is corrupt)
// When a client subscribes
// Then the subscription falls back to the ring (replay + live receiver),
//      and the conversation keeps working. The session is fresh, so the ring
//      (an empty cache after a resume) holds the whole turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_history_read_falls_back_to_the_ring() {
    let fx = fixture().await;
    let state = state(&fx);
    let (addr, task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");
    let (id, all) = session_with_one_turn(&state, &base, &fx.ws).await;

    let conn = rusqlite::Connection::open(&fx.db).expect("open db");
    let corrupted = conn
        .execute(
            "UPDATE events SET envelope = 'not json' WHERE session_id = ?1 AND seq = ?2",
            rusqlite::params![id.to_string(), all[1] as i64],
        )
        .expect("corrupt a row");
    assert_eq!(corrupted, 1);
    drop(conn);

    let Subscription { replay, live } = state.subscribe(id, None).await.expect("subscribe");
    assert_eq!(seqs(&replay), all, "the ring still holds the whole turn");
    let mut live = live.expect("the fallback follows live events");

    post_message_ok(&base, &id.to_string(), "still there?").await;
    let events = live_until(&mut live, |e| e.kind == "turn.end").await;
    assert!(events.iter().all(|e| e.seq > *all.last().unwrap()));
    assert!(events.iter().any(|e| e.kind == "message.received"));
    task.abort();
}
