use super::*;
use crate::agent::memory::{
    component::{Component, Part},
    EpisodeBody, Progress,
};
use crate::bus::sequencer::Emitter;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};
use tokio::sync::{mpsc, Notify};

struct Events(mpsc::UnboundedSender<EventEnvelope>);
impl Emitter for Events {
    fn emit(&mut self, event: EventEnvelope) {
        let _ = self.0.send(event);
    }
    fn subscribe_since(
        &mut self,
        _: Option<u64>,
        _: u64,
        _: tokio::sync::oneshot::Sender<crate::bus::SubscribeReply>,
    ) {
    }
    fn subscribe_live(&mut self) -> mpsc::Receiver<EventEnvelope> {
        mpsc::channel(1).1
    }
}
struct FakeObserver {
    calls: AtomicUsize,
    inputs: Mutex<Vec<String>>,
    entered: Notify,
    release: Notify,
    slow: bool,
}
impl Observer for FakeObserver {
    fn observe<'a>(
        &'a self,
        input: &'a str,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Verdict, String>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            self.inputs.lock().unwrap().push(input.into());
            if self.slow && call == 0 {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(Verdict {
                goal_progress: if call == 0 {
                    Progress::Advancing
                } else {
                    Progress::Done
                },
                new_goal: false,
            })
        })
    }
}
struct FakeWriter {
    inputs: Mutex<Vec<String>>,
    calls: AtomicUsize,
    fail_first: bool,
    slow: bool,
    entered: Notify,
    release: Notify,
    store: HistoryStore,
    session: Ulid,
}
impl Writer for FakeWriter {
    fn write<'a>(
        &'a self,
        input: &'a str,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Written, String>> {
        Box::pin(async move {
            self.inputs.lock().unwrap().push(input.into());
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let goals = self.store.memory_goals(&self.session.to_string()).await?;
            let goal = &goals[0];
            assert_eq!(goal.writer_status, "writing");
            assert_eq!(goal.attempts, (call + 1) as u32);
            if self.fail_first && call == 0 {
                return Err("fake writer unavailable".into());
            }
            if self.slow {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(Written {
                text: serde_json::to_string(&EpisodeBody {
                    version: 1,
                    goal: goal.text.clone(),
                    decisions: vec![],
                    changes: vec![],
                    verification: vec![],
                    open_items: vec![],
                })
                .unwrap(),
                model: "fake".into(),
                usage: "{}".into(),
                truncated: false,
            })
        })
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    watcher: Watcher,
    observer: Arc<FakeObserver>,
    events: mpsc::UnboundedReceiver<EventEnvelope>,
}
async fn fixture(slow: bool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let store = HistoryStore::open(temp.path().join("history.db")).unwrap();
    let session = Ulid::new();
    store
        .record_session(session, "/workspace", "fake", false)
        .await
        .unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let observer = Arc::new(FakeObserver {
        calls: AtomicUsize::new(0),
        inputs: Mutex::new(vec![]),
        entered: Notify::new(),
        release: Notify::new(),
        slow,
    });
    Fixture {
        _temp: temp,
        watcher: Watcher {
            store,
            session,
            workspace: "/workspace".into(),
            models: Models {
                observer: Some(observer.clone()),
                writer: None,
            },
            sequencer: Sequencer::spawn(session, Events(tx), 100),
            cancel: CancellationToken::new(),
        },
        observer,
        events: rx,
    }
}
async fn turn(w: &Watcher, seq: u64, text: &str) -> Ulid {
    let turn = Ulid::new();
    for (seq, kind, payload) in [
        (
            seq,
            EventKind::MessageReceived,
            serde_json::json!({"source":"captain","content":text}),
        ),
        (
            seq + 1,
            EventKind::TurnEnd,
            serde_json::json!({"turn_id":turn,"outcome":"done"}),
        ),
    ] {
        w.store
            .append_event_acked(w.session, EventEnvelope::new(w.session, seq, kind, payload))
            .await
            .unwrap();
    }
    turn
}
fn writer(w: &Watcher, fail_first: bool, slow: bool) -> Arc<FakeWriter> {
    Arc::new(FakeWriter {
        inputs: Mutex::new(vec![]),
        calls: AtomicUsize::new(0),
        fail_first,
        slow,
        entered: Notify::new(),
        release: Notify::new(),
        store: w.store.clone(),
        session: w.session,
    })
}

#[tokio::test]
async fn secrets_never_reach_memory_models_or_episodes() {
    let mut f = fixture(false).await;
    let writer = writer(&f.watcher, false, false);
    f.watcher.models.writer = Some(writer.clone());
    let secrets = [
        "sk-synthetic0123456789012345",
        "Bearer synthetic-bearer-token",
        "API_KEY=synthetic-env-credential",
    ];
    let first = turn(&f.watcher, 1, secrets[0]).await;
    f.watcher.completed(first).await.unwrap();
    for (seq, kind, payload) in [
        (
            3,
            EventKind::LlmResponse,
            serde_json::json!({"message":{"text":secrets[1]}}),
        ),
        (
            4,
            EventKind::ToolPostCall,
            serde_json::json!({"name":"bash","output":format!("cat .env\n{}", secrets[2]),"status":"ok"}),
        ),
    ] {
        f.watcher
            .store
            .append_event_acked(
                f.watcher.session,
                EventEnvelope::new(f.watcher.session, seq, kind, payload),
            )
            .await
            .unwrap();
    }
    let second = turn(&f.watcher, 5, "finish").await;
    f.watcher.completed(second).await.unwrap();
    let stored = f
        .watcher
        .store
        .memory_episodes("/workspace", None, 3)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    let outputs = format!(
        "{:?}{:?}{:?}",
        f.observer.inputs.lock().unwrap(),
        writer.inputs.lock().unwrap(),
        stored
    );
    for secret in secrets {
        assert!(!outputs.contains(secret));
    }
}

#[tokio::test]
async fn recovery_replays_only_tracked_successful_turns_in_order() {
    let f = fixture(false).await;
    let first = turn(&f.watcher, 1, "goal").await;
    f.watcher.recover().await.unwrap();
    assert_eq!(f.observer.calls.load(Ordering::SeqCst), 0);
    f.watcher.completed(first).await.unwrap();
    turn(&f.watcher, 3, "clarification").await;
    turn(&f.watcher, 5, "next task").await;
    f.watcher.recover().await.unwrap();
    assert_eq!(f.observer.calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        f.watcher
            .store
            .memory_state(&f.watcher.session.to_string())
            .await
            .unwrap()
            .unwrap()
            .last_turn,
        6
    );
    f.watcher.recover().await.unwrap();
    assert_eq!(f.observer.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn writer_gate_wait_does_not_consume_a_lease_or_attempt() {
    let mut f = fixture(false).await;
    let first = turn(&f.watcher, 1, "goal").await;
    f.watcher.completed(first).await.unwrap();
    let second = turn(&f.watcher, 3, "finish").await;
    f.watcher.completed(second).await.unwrap();
    f.watcher.models.writer = Some(writer(&f.watcher, false, false));
    let permit = WRITER_GATE.acquire().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), f.watcher.jobs())
            .await
            .is_err()
    );
    let goals = f
        .watcher
        .store
        .memory_goals(&f.watcher.session.to_string())
        .await
        .unwrap();
    assert_eq!(goals[0].attempts, 0);
    assert_eq!(goals[0].writer_status, "pending");
    assert!(goals[0].claim.is_none());
    drop(permit);
    f.watcher.jobs().await.unwrap();
}

#[tokio::test]
async fn quick_turns_are_ordered_and_slow_instinct_does_not_delay_turn_submission() {
    let mut f = fixture(true).await;
    let one = turn(&f.watcher, 1, "first goal").await;
    let two = turn(&f.watcher, 3, "clarification").await;
    let cancel = f.watcher.cancel.clone();
    let tx = f.watcher.start();
    tx.send(one).unwrap();
    tokio::time::timeout(Duration::from_secs(1), f.observer.entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_millis(100), async {
        tx.send(two).unwrap();
    })
    .await
    .unwrap();
    assert_eq!(f.observer.calls.load(Ordering::SeqCst), 1);
    f.observer.release.notify_one();
    for _ in 0..2 {
        let e = tokio::time::timeout(Duration::from_secs(2), f.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(e.kind, "memory.goal_observed");
    }
    let inputs = f.observer.inputs.lock().unwrap();
    assert!(!inputs[0].contains("clarification"));
    assert!(inputs[1].contains("clarification"));
    cancel.cancel();
}

#[tokio::test]
async fn resumed_active_goal_keeps_identity_and_episode_is_not_duplicated() {
    let mut f = fixture(false).await;
    let one = turn(&f.watcher, 1, "unit test keyword").await;
    f.watcher.completed(one).await.unwrap();
    let id = f
        .watcher
        .store
        .memory_state(&f.watcher.session.to_string())
        .await
        .unwrap()
        .unwrap()
        .active_goal
        .unwrap();
    let fake = writer(&f.watcher, false, false);
    // A fresh watcher is what a resume constructs; only the fake provider retains its scripted call index.
    let resumed = Watcher {
        store: f.watcher.store.clone(),
        session: f.watcher.session,
        workspace: f.watcher.workspace.clone(),
        models: Models {
            observer: Some(f.observer),
            writer: Some(fake.clone()),
        },
        sequencer: f.watcher.sequencer.clone(),
        cancel: CancellationToken::new(),
    };
    let two = turn(&resumed, 3, "finish it").await;
    resumed.completed(two).await.unwrap();
    resumed.completed(two).await.unwrap();
    resumed.jobs().await.unwrap();
    assert_eq!(
        resumed
            .store
            .memory_goals(&resumed.session.to_string())
            .await
            .unwrap()[0]
            .id,
        id
    );
    assert_eq!(
        resumed
            .store
            .memory_episodes("/workspace", Some("keyword"), 5)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    // Drain to ensure no unexpected watcher errors were hidden by a spawned task.
    while f.events.try_recv().is_ok() {}
}

#[tokio::test]
async fn writer_failure_returns_pending_then_single_retry_stores_episode() {
    let mut f = fixture(false).await;
    let one = turn(&f.watcher, 1, "retry keyword").await;
    f.watcher.completed(one).await.unwrap();
    let fake = writer(&f.watcher, true, false);
    f.watcher.models.writer = Some(fake.clone());
    let two = turn(&f.watcher, 3, "finished").await;
    f.watcher.completed(two).await.unwrap();
    let goals = f
        .watcher
        .store
        .memory_goals(&f.watcher.session.to_string())
        .await
        .unwrap();
    assert_eq!(goals[0].attempts, 2);
    assert_eq!(goals[0].writer_status, "done");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        f.watcher
            .store
            .memory_episodes("/workspace", None, 5)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn conversation_deletion_cleans_memory_and_drops_late_writer() {
    let mut f = fixture(false).await;
    let one = turn(&f.watcher, 1, "deletion keyword").await;
    f.watcher.completed(one).await.unwrap();
    let fake = writer(&f.watcher, false, true);
    f.watcher.models.writer = Some(fake.clone());
    let two = turn(&f.watcher, 3, "finished").await;
    let store = f.watcher.store.clone();
    let session = f.watcher.session;
    let mut events = f.events;
    let job = tokio::spawn(async move { f.watcher.completed(two).await });
    tokio::time::timeout(Duration::from_secs(3), fake.entered.notified())
        .await
        .unwrap();
    store.delete(session).await.unwrap();
    fake.release.notify_one();
    job.await.unwrap().unwrap();
    while let Ok(event) = events.try_recv() {
        assert_ne!(event.kind, "memory.episode_written");
    }
    assert!(store
        .memory_goals(&session.to_string())
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .memory_state(&session.to_string())
        .await
        .unwrap()
        .is_none());
    assert!(store
        .memory_episodes("/workspace", None, 5)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .memory_episodes("/workspace", Some("keyword"), 5)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn startup_snapshot_is_identical_across_requests_and_resume() {
    let f = fixture(false).await;
    let part = Part {
        source: "startup".into(),
        ids: vec![7],
        text: "exact untrusted startup note".into(),
    };
    let store = f.watcher.store.clone();
    let session = f.watcher.session.to_string();
    store
        .memory_save_startup(&session, Some(part.clone()))
        .await
        .unwrap();
    store
        .memory_save_startup(
            &session,
            Some(Part {
                text: "later history".into(),
                ..part.clone()
            }),
        )
        .await
        .unwrap();
    let make = |startup| Component {
        session: session.clone(),
        turn: "turn".into(),
        startup,
        recalled: Arc::default(),
        record: Arc::new(|_| Box::pin(async { Ok(()) })),
    };
    let initial = make(Some(part));
    let first = serde_json::to_value(initial.admitted("fake").await).unwrap();
    assert_eq!(
        first,
        serde_json::to_value(initial.admitted("fake").await).unwrap()
    );
    let resumed = make(store.memory_startup(&session).await.unwrap());
    assert_eq!(
        first,
        serde_json::to_value(resumed.admitted("fake").await).unwrap()
    );
    store.delete(f.watcher.session).await.unwrap();
    assert!(store.memory_startup(&session).await.unwrap().is_none());
}

#[tokio::test]
async fn failed_memory_commit_publishes_nothing_and_rolls_back_goal() {
    let mut f = fixture(false).await;
    let one = turn(&f.watcher, 1, "atomic goal").await;
    // Reject the explanatory event insertion after the goal mutation in the same transaction.
    let conn = rusqlite::Connection::open(f._temp.path().join("history.db")).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_memory_event BEFORE INSERT ON events WHEN new.kind='memory.goal_observed' BEGIN SELECT RAISE(ABORT,'fake commit failure'); END;").unwrap();
    assert!(f.watcher.completed(one).await.is_err());
    assert!(f.events.try_recv().is_err());
    assert!(f
        .watcher
        .store
        .memory_goals(&f.watcher.session.to_string())
        .await
        .unwrap()
        .is_empty());
    assert!(f
        .watcher
        .store
        .memory_state(&f.watcher.session.to_string())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn memory_events_are_durable_before_publication() {
    let mut f = fixture(false).await;
    let one = turn(&f.watcher, 1, "atomic goal").await;
    f.watcher.completed(one).await.unwrap();
    let fake = writer(&f.watcher, false, false);
    f.watcher.models.writer = Some(fake);
    let two = turn(&f.watcher, 3, "finished").await;
    f.watcher.completed(two).await.unwrap();
    let loaded = f
        .watcher
        .store
        .load(f.watcher.session)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..3 {
        let event = f.events.recv().await.unwrap();
        assert!(loaded
            .envelopes
            .iter()
            .any(|e| e.seq == event.seq && e.kind == event.kind));
    }
}

#[tokio::test]
async fn missing_layers_produce_one_combined_captain_notice() {
    let mut f = fixture(false).await;
    let fake = writer(&f.watcher, false, false);
    f.watcher.models.writer = Some(fake);
    assert!(unavailable_notice(&f.watcher.models).is_none());
    f.watcher.models.writer = None;
    assert!(unavailable_notice(&f.watcher.models)
        .unwrap()
        .contains("metacognition"));
    f.watcher.models.observer = None;
    assert!(unavailable_notice(&f.watcher.models)
        .unwrap()
        .contains("Instinct and metacognition"));
}

#[tokio::test]
async fn oversized_observation_is_skipped_with_one_durable_notice() {
    let mut f = fixture(false).await;
    let turn = turn(&f.watcher, 1, &"x".repeat(memory::INSTINCT_INPUT_BYTES)).await;
    f.watcher.completed(turn).await.unwrap();
    f.watcher.completed(turn).await.unwrap();
    let event = f.events.try_recv().unwrap();
    assert_eq!(event.kind, "command.result");
    assert!(event.payload["error"]
        .as_str()
        .unwrap()
        .contains("oversized"));
    assert!(f.events.try_recv().is_err());
    assert_eq!(f.observer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.watcher
            .store
            .memory_goals(&f.watcher.session.to_string())
            .await
            .unwrap()[0]
            .writer_status,
        "skipped"
    );
    assert!(f
        .watcher
        .store
        .load(f.watcher.session)
        .await
        .unwrap()
        .unwrap()
        .envelopes
        .iter()
        .any(|e| e.seq == event.seq));
}

#[tokio::test]
async fn oversized_writer_input_is_skipped_with_one_notice_and_no_provider_call() {
    let mut f = fixture(false).await;
    let one = turn(&f.watcher, 1, "oversized episode").await;
    f.watcher.completed(one).await.unwrap();
    let two = turn(&f.watcher, 3, "finished").await;
    f.watcher.completed(two).await.unwrap();
    // Replace the earlier turn-end evidence inside the closed goal's range.
    let conn = rusqlite::Connection::open(f._temp.path().join("history.db")).unwrap();
    let envelope = EventEnvelope::new(
        f.watcher.session,
        2,
        EventKind::ToolPostCall,
        serde_json::json!({"output":"x".repeat(memory::WRITER_INPUT_BYTES)}),
    );
    conn.execute(
        "UPDATE events SET kind=?1,envelope=?2 WHERE session_id=?3 AND seq=2",
        rusqlite::params![
            envelope.kind,
            serde_json::to_string(&envelope).unwrap(),
            f.watcher.session.to_string()
        ],
    )
    .unwrap();
    let fake = writer(&f.watcher, false, false);
    f.watcher.models.writer = Some(fake.clone());
    while f.events.try_recv().is_ok() {}
    f.watcher.jobs().await.unwrap();
    f.watcher.jobs().await.unwrap();
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.events.try_recv().unwrap().kind, "command.result");
    assert!(f.events.try_recv().is_err());
    assert_eq!(
        f.watcher
            .store
            .memory_goals(&f.watcher.session.to_string())
            .await
            .unwrap()[0]
            .writer_status,
        "skipped"
    );
}
