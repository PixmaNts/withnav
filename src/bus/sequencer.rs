//! Per-session sequencer task (todo 2): the ONE authoritative publisher.
//!
//! Exactly one task per session owns `next_seq` — it is the only code that
//! assigns `seq` (strictly monotonic, starting at 0 so `session.start` is seq
//! 0). Producers submit unsequenced [`Candidate`]s over an UNBOUNDED
//! non-blocking ingress (`tokio::sync::mpsc::unbounded_channel`): an accepted
//! durable event is never lost, and ingress send failure means the session is
//! closed ([`BusError::SequencerClosed`]), never silent loss. Per event the
//! task performs seq-assign → [`Emitter::emit`] (ring append + subscriber
//! fan-out are todo 3 and plug in at that seam).
//!
//! AcceptCommand ordering hook (Oracle O4, event-before-action): the
//! `message.received` envelope is sequenced + emitted BEFORE the reserved
//! inbox permit is committed, and the ack is sent only after the commit — so
//! the runtime (a separate task) can never consume a command whose
//! `message.received` is not already sequenced.

use tokio::sync::{mpsc, oneshot};
use ulid::Ulid;

use crate::bus::event::EventEnvelope;
use crate::bus::event::EventKind;
use crate::bus::SubscribeReply;
use crate::error::BusError;

/// A command still unsequenced by the runtime (todo 4 freezes the full shape).
#[derive(Debug, Clone)]
pub struct PendingCommand {
    pub command_id: Ulid,
    pub content: String,
}

/// Committer invoked by the sequencer AFTER `message.received` is seq-assigned
/// and emitted, and BEFORE the ack — this IS the event-before-action contract
/// (Oracle O4) as code. The REAL inbox permit commit (handing the command to
/// the runtime actor, todo 4's `ActorInbox`) is wired in todo 7 through this
/// seam, which is why `accept_command` TAKES the committer instead of owning
/// one. Must not block: it runs inside the single sequencer mailbox.
pub type CommandCommit = Box<dyn FnOnce() + Send>;

/// Ack returned AFTER the event is sequenced and the command committed.
#[derive(Debug, Clone, Copy)]
pub struct AcceptReceipt {
    pub seq: u64,
}

/// Control + candidate messages drained by the single sequencer task.
#[allow(clippy::type_complexity)]
pub enum Ingress {
    /// An unsequenced event candidate from any producer.
    Candidate(Candidate),
    /// Event-before-action acceptance (O4): sequence + emit `message.received`
    /// FIRST, then invoke `commit`, then ack with the assigned seq.
    AcceptCommand {
        commit: CommandCommit,
        command: PendingCommand,
        reply: oneshot::Sender<Result<AcceptReceipt, BusError>>,
    },
    /// Atomic subscribe-then-replay (todo 3): resolve the cursor against the
    /// ring and register a live queue in ONE synchronous mailbox step.
    SubscribeSince {
        cursor: Option<u64>,
        reply: oneshot::Sender<crate::bus::SubscribeReply>,
    },
    /// Session teardown (todo 5): sequence + emit `session.end`, pin
    /// `final_seq`, then the sequencer task EXITS — the fence that guarantees
    /// no event is ever sequenced after `session.end`.
    CloseSession { reason: String },
    /// Head query (todo 6): reply with the next unassigned seq.
    CurrentSeq {
        reply: oneshot::Sender<Result<u64, BusError>>,
    },
}

/// An unsequenced event candidate submitted by any producer.
#[derive(Debug)]
pub struct Candidate {
    pub kind: EventKind,
    pub payload: serde_json::Value,
    pub causation_id: Option<Ulid>,
    pub correlation_id: Option<Ulid>,
}

impl Candidate {
    pub fn new(kind: EventKind, payload: serde_json::Value) -> Self {
        Self {
            kind,
            payload,
            causation_id: None,
            correlation_id: None,
        }
    }

    pub fn with_correlation_id(mut self, id: Ulid) -> Self {
        self.correlation_id = Some(id);
        self
    }
}

/// Per-event downstream seam. Todo 2 tests collect into a channel; the
/// concrete [`crate::bus::SessionBus`] implements ring append + bounded
/// fan-out here.
pub trait Emitter: Send + 'static {
    fn emit(&mut self, envelope: EventEnvelope);

    /// Atomic subscribe-then-replay (todo 8 consumes this): called from the
    /// single mailbox handler with NO await around it, so the replay snapshot
    /// and the live registration share one synchronous step (exactly-once at
    /// the seam). `next_seq` is the session head + 1 for cursor sanity.
    fn subscribe_since(
        &mut self,
        cursor: Option<u64>,
        next_seq: u64,
        reply: oneshot::Sender<crate::bus::SubscribeReply>,
    );
}

/// Handle to a running sequencer task. Cloning clones the ingress sender;
/// dropping the LAST handle closes the ingress, ending the task once it
/// drains.
#[derive(Clone, Debug)]
pub struct Sequencer {
    session_id: Ulid,
    ingress_tx: mpsc::UnboundedSender<Ingress>,
}

impl Sequencer {
    /// Spawns the ONE authoritative sequencer task for a session.
    pub fn spawn(session_id: Ulid, emitter: impl Emitter) -> Self {
        let (ingress_tx, ingress_rx) = mpsc::unbounded_channel();
        tokio::spawn(run(session_id, ingress_rx, emitter));
        Self {
            session_id,
            ingress_tx,
        }
    }

    /// Submits an unsequenced candidate. Never blocks and never drops: the
    /// only failure is a closed session.
    pub fn publish(&self, candidate: Candidate) -> Result<(), BusError> {
        ingress_send(&self.ingress_tx, Ingress::Candidate(candidate))
    }

    /// Event-before-action acceptance (O4). The handler's order is: seq-assign
    /// `message.received` -> emit it -> invoke `commit` -> ack with the seq.
    /// The ack therefore PROVES the commit ran after the emit. Commit failure
    /// or a session ended before handling yields
    /// `BusError::SessionNotFound`; no event is emitted on the reject path.
    pub async fn accept_command(
        &self,
        command: PendingCommand,
        commit: CommandCommit,
    ) -> Result<AcceptReceipt, BusError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        ingress_send(
            &self.ingress_tx,
            Ingress::AcceptCommand {
                commit,
                command,
                reply: reply_tx,
            },
        )?;
        reply_rx
            .await
            .map_err(|_| BusError::SessionNotFound(self.session_id.to_string()))?
    }

    /// Atomic subscribe-then-replay: ONE control op on the sequencer task.
    /// The mailbox handler, without awaiting, resolves the cursor against the
    /// ring (per the todo-3 resume contract), registers a bounded live queue,
    /// and returns the replay-through-head snapshot + live receiver — an
    /// event published after this returns is live-only (exactly once).
    /// A closed session maps to `BusError::SessionNotFound`.
    pub async fn subscribe_since(&self, cursor: Option<u64>) -> Result<SubscribeReply, BusError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        ingress_send(
            &self.ingress_tx,
            Ingress::SubscribeSince {
                cursor,
                reply: reply_tx,
            },
        )?;
        reply_rx
            .await
            .map_err(|_| BusError::SessionNotFound(self.session_id.to_string()))
    }

    /// Head accessor (todo 6's `GET /sessions/:id` `current_seq`): replies
    /// with the next unassigned seq, so the last-EMITTED seq is
    /// `current - 1` (the pinned `current_seq` = last emitted seq, ticket
    /// 04). A closed session maps to `BusError::SessionNotFound`.
    pub async fn current_seq(&self) -> Result<u64, BusError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        ingress_send(&self.ingress_tx, Ingress::CurrentSeq { reply: reply_tx })?;
        reply_rx
            .await
            .map_err(|_| BusError::SessionNotFound(self.session_id.to_string()))?
    }

    /// Sequence + emit `session.end{reason, final_seq}` and END the sequencer
    /// task. Every later publish (even from a clone of this handle) fails
    /// with `BusError::SequencerClosed` — the fence that keeps `session.end`
    /// the last event the session ever sequences.
    pub fn close_session(&self, reason: impl Into<String>) -> Result<(), BusError> {
        ingress_send(
            &self.ingress_tx,
            Ingress::CloseSession {
                reason: reason.into(),
            },
        )
    }
}

/// Ingress send: the ONLY failure mode is a closed session, never loss.
fn ingress_send(tx: &mpsc::UnboundedSender<Ingress>, msg: Ingress) -> Result<(), BusError> {
    tx.send(msg).map_err(|_| BusError::SequencerClosed)
}

/// The sequencer task body: drains the single mailbox in order.
async fn run(
    session_id: Ulid,
    mut ingress_rx: mpsc::UnboundedReceiver<Ingress>,
    mut emitter: impl Emitter,
) {
    // `next_seq` lives ONLY inside this task (single assignment point).
    let mut next_seq: u64 = 0;

    while let Some(msg) = ingress_rx.recv().await {
        match msg {
            Ingress::Candidate(candidate) => {
                let Some(seq) = take_seq(&mut next_seq) else {
                    // Seq space exhausted (2^64 events): unreachable in
                    // practice, but panic-free by policy — end the session.
                    break;
                };
                emitter.emit(stamp(session_id, seq, candidate));
            }
            Ingress::AcceptCommand {
                commit,
                command,
                reply,
            } => {
                let Some(seq) = take_seq(&mut next_seq) else {
                    let _ = reply.send(Err(BusError::SeqSpaceExhausted));
                    break;
                };
                // Event-before-action (O4), in this exact order: emit the
                // sequenced envelope, THEN commit the command, THEN ack —
                // so the ack is observable proof that commit followed emit.
                let mut candidate = message_received_candidate(&command);
                candidate.causation_id = Some(command.command_id);
                emitter.emit(stamp(session_id, seq, candidate));
                commit();
                let _ = reply.send(Ok(AcceptReceipt { seq }));
            }
            Ingress::SubscribeSince { cursor, reply } => {
                // The atomic handshake: no await between the replay snapshot
                // and the live registration (both live inside `emitter`).
                emitter.subscribe_since(cursor, next_seq, reply);
            }
            Ingress::CurrentSeq { reply } => {
                let _ = reply.send(Ok(next_seq));
            }
            Ingress::CloseSession { reason } => {
                let Some(seq) = take_seq(&mut next_seq) else {
                    break;
                };
                emitter.emit(stamp(
                    session_id,
                    seq,
                    Candidate::new(
                        EventKind::SessionEnd,
                        serde_json::json!({
                            "reason": reason,
                            "final_seq": seq,
                        }),
                    ),
                ));
                // The fence: the task exits, so nothing can ever be
                // sequenced after session.end. Queued candidates behind
                // this CloseSession are dropped (the session is over).
                break;
            }
        }
    }
}

/// The single seq-assignment point. Takes `&mut u64` so no other code path
/// can mint a seq; ordering never derives from wall-clock time.
fn take_seq(next_seq: &mut u64) -> Option<u64> {
    let seq = *next_seq;
    *next_seq = next_seq.checked_add(1)?;
    Some(seq)
}

fn message_received_candidate(command: &PendingCommand) -> Candidate {
    Candidate::new(
        EventKind::MessageReceived,
        serde_json::json!({
            "command_id": command.command_id,
            "source": "user:http",
            "target": "agent:main",
            "mode": "next-turn",
            "content": command.content,
        }),
    )
}

fn stamp(session_id: Ulid, seq: u64, candidate: Candidate) -> EventEnvelope {
    let mut envelope = EventEnvelope::new(session_id, seq, candidate.kind, candidate.payload);
    envelope.causation_id = candidate.causation_id;
    envelope.correlation_id = candidate.correlation_id;
    envelope
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::event::SessionStart;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    /// Forwards emitted envelopes to a channel: the test receives exactly the
    /// emitted sequence, in order — no polling, no timing.
    struct ForwardEmitter {
        tx: mpsc::UnboundedSender<EventEnvelope>,
    }

    impl Emitter for ForwardEmitter {
        fn emit(&mut self, envelope: EventEnvelope) {
            // The receiver outlives the task in these tests; a closed
            // receiver means the test is already gone — dropping is correct.
            let _ = self.tx.send(envelope);
        }

        fn subscribe_since(
            &mut self,
            _cursor: Option<u64>,
            _next_seq: u64,
            reply: oneshot::Sender<SubscribeReply>,
        ) {
            // Subscribe is a fan-out concern (todo 3); this collector emitter
            // has no subscriber registry, so the request fails like a closed
            // session would.
            drop(reply);
        }
    }

    fn spawn_with_collector() -> (Arc<Sequencer>, mpsc::UnboundedReceiver<EventEnvelope>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Sequencer::spawn(Ulid::new(), ForwardEmitter { tx })),
            rx,
        )
    }

    fn start_candidate() -> Candidate {
        let payload = match serde_json::to_value(SessionStart {
            workspace: "/w".into(),
            root_actor_id: "agent:main".into(),
        }) {
            Ok(v) => v,
            Err(_) => serde_json::Value::Null,
        };
        Candidate::new(EventKind::SessionStart, payload)
    }

    fn turn_candidate(tag: &str) -> Candidate {
        Candidate::new(
            EventKind::TurnStart,
            serde_json::json!({
                "tag": tag,
                "turn_id": Ulid::new(),
                "actor_id": "agent:main",
                "task_id": Ulid::new(),
            }),
        )
        .with_correlation_id(Ulid::new())
    }

    /// (c) 100 concurrent publishes: seqs form a gapless monotonic set with
    /// NO duplicates, and session.start is seq 0.
    #[tokio::test]
    async fn concurrent_publishes_yield_gapless_monotonic_seqs_from_zero() {
        let (seq, mut rx) = spawn_with_collector();

        seq.publish(start_candidate()).unwrap();

        let handles: Vec<_> = (0..100)
            .map(|i| {
                let seq = Arc::clone(&seq);
                let tag = format!("producer-{i}");
                tokio::spawn(async move { seq.publish(turn_candidate(&tag)) })
            })
            .collect();
        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        // Drain exactly 101 envelopes; recv order == emit order == seq order.
        let first = rx.recv().await.unwrap();
        assert_eq!((first.seq, first.kind.as_str()), (0, "session.start"));
        let mut seqs = vec![first.seq];
        for _ in 0..100 {
            seqs.push(rx.recv().await.unwrap().seq);
        }
        let unique: BTreeSet<u64> = seqs.iter().copied().collect();
        assert_eq!(unique.len(), 101, "no duplicate seq under contention");
        assert_eq!(*unique.last().unwrap(), 100);
        for (expected, s) in seqs.iter().enumerate() {
            assert_eq!(*s, expected as u64, "strictly monotonic, gapless");
        }

        // Closing the handle ends the task after the drain: nothing extra.
        drop(seq);
        assert!(rx.recv().await.is_none());
    }

    /// (d) Deterministic (channel-ordered, not timing-based): whatever order
    /// producers raced onto the ingress, the emitted order equals the seq
    /// order, which equals the single task's processing order.
    #[tokio::test]
    async fn emitted_order_equals_seq_order_across_mixed_producers() {
        let (seq, mut rx) = spawn_with_collector();

        let producers: Vec<Vec<Candidate>> = vec![
            vec![turn_candidate("a1"), turn_candidate("a2")],
            vec![turn_candidate("b1")],
            vec![
                turn_candidate("c1"),
                turn_candidate("c2"),
                turn_candidate("c3"),
            ],
        ];
        let handles: Vec<_> = producers
            .into_iter()
            .map(|batch| {
                let seq = Arc::clone(&seq);
                tokio::spawn(async move {
                    for candidate in batch {
                        seq.publish(candidate).unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.await.unwrap();
        }

        let mut seqs = Vec::new();
        for _ in 0..6 {
            seqs.push(rx.recv().await.unwrap().seq);
        }
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "emitted order == seq order == single-task processing order: {seqs:?}"
        );

        drop(seq);
        assert!(rx.recv().await.is_none());
    }

    /// (e) Event-before-action (O4), GENUINELY OBSERVED: the emitter and the
    /// committer both append to one shared timeline, and the ack appends its
    /// entry after `accept_command` returns. The handler structure is
    /// emit -> commit -> ack in ONE task, so a recorded order of
    /// Emit,Commit,Ack with strictly increasing counters proves the property;
    /// any reordering (commit-before-emit, ack-before-commit) fails the
    /// assertions below.
    #[tokio::test]
    async fn accept_command_sequences_event_before_committing_and_acking() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Mutex;

        #[derive(Debug, Clone, PartialEq, Eq)]
        enum Step {
            Emit(u64),
            Commit(u64),
            Ack(u64),
        }

        let timeline: Arc<Mutex<Vec<Step>>> = Arc::new(Mutex::new(Vec::new()));
        let counter = Arc::new(AtomicU64::new(0));

        // Emitter side of the seam: record every emitted envelope AND
        // forward it for payload assertions.
        struct TimelineEmitter {
            timeline: Arc<Mutex<Vec<Step>>>,
            tx: mpsc::UnboundedSender<EventEnvelope>,
        }
        impl Emitter for TimelineEmitter {
            fn emit(&mut self, envelope: EventEnvelope) {
                self.timeline.lock().unwrap().push(Step::Emit(envelope.seq));
                let _ = self.tx.send(envelope);
            }

            fn subscribe_since(
                &mut self,
                _cursor: Option<u64>,
                _next_seq: u64,
                reply: oneshot::Sender<SubscribeReply>,
            ) {
                drop(reply);
            }
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TimelineEmitter {
            timeline: Arc::clone(&timeline),
            tx,
        };
        let seq = Sequencer::spawn(Ulid::new(), emitter);

        let command_id = Ulid::new();
        let commit_timeline = Arc::clone(&timeline);
        let commit_counter = Arc::clone(&counter);
        let commit = Box::new(move || {
            commit_timeline
                .lock()
                .unwrap()
                .push(Step::Commit(commit_counter.fetch_add(1, Ordering::SeqCst)));
        });

        let receipt = seq
            .accept_command(
                PendingCommand {
                    command_id,
                    content: "hello there".into(),
                },
                commit,
            )
            .await
            .unwrap();

        // The ack has already been processed by the time this line runs.
        timeline
            .lock()
            .unwrap()
            .push(Step::Ack(counter.fetch_add(1, Ordering::SeqCst)));

        // The emitted envelope must be observable now (emit happens-before
        // ack happens-before this recv).
        let envelope = rx.recv().await.unwrap();
        assert_eq!(envelope.kind, "message.received");
        assert_eq!(envelope.seq, receipt.seq);
        assert_eq!(envelope.causation_id, Some(command_id));
        assert_eq!(envelope.payload["command_id"], command_id.to_string());
        assert_eq!(envelope.payload["content"], "hello there");
        assert_eq!(envelope.payload["mode"], "next-turn");

        // THE assertion: real recorded order is Emit < Commit < Ack.
        let recorded = timeline.lock().unwrap().clone();
        eprintln!("recorded timeline: {recorded:?}");
        assert!(
            matches!(
                recorded.as_slice(),
                [Step::Emit(_), Step::Commit(_), Step::Ack(_)]
            ),
            "emit -> commit -> ack order violated: {recorded:?}"
        );

        drop(seq);
        assert!(rx.recv().await.is_none());
    }

    /// Ingress send failure = session closed (never silent loss).
    #[tokio::test]
    async fn closed_ingress_maps_to_sequencer_closed() {
        let (tx, rx) = mpsc::unbounded_channel::<Ingress>();
        drop(rx);
        let err = ingress_send(&tx, Ingress::Candidate(turn_candidate("late")));
        assert!(matches!(err, Err(BusError::SequencerClosed)));
    }

    /// close_session emits session.end{final_seq} as the LAST event and the
    /// fence holds: any later publish (from the same handle or a clone)
    /// fails with SequencerClosed, and nothing further is emitted.
    #[tokio::test]
    async fn close_session_emits_session_end_then_fences_all_publishes() {
        let (seq, mut rx) = spawn_with_collector();

        seq.publish(start_candidate()).unwrap();
        seq.close_session("stop requested").unwrap();

        let first = rx.recv().await.unwrap();
        assert_eq!((first.seq, first.kind.as_str()), (0, "session.start"));
        let end = rx.recv().await.unwrap();
        assert_eq!(end.seq, 1);
        assert_eq!(end.kind, "session.end");
        assert_eq!(end.payload["reason"], "stop requested");
        assert_eq!(end.payload["final_seq"], 1);

        // The fence: a candidate racing the task exit may still land in the
        // (dropped-on-exit) ingress void or fail Closed; deterministically,
        // every publish fails SequencerClosed once the task is gone, and
        // NOTHING is ever emitted after session.end.
        let mut saw_closed = false;
        for _ in 0..200 {
            match seq.publish(turn_candidate("late")) {
                Err(BusError::SequencerClosed) => {
                    saw_closed = true;
                    break;
                }
                _ => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
            }
        }
        assert!(saw_closed, "publish must fail closed once the fence is up");
        assert!(rx.recv().await.is_none(), "nothing after session.end");
    }

    /// Seq assignment is strictly monotonic and panic-free: the seq space is
    /// [0, u64::MAX) — the final value is reserved so the counter can always
    /// advance its stored next, and exhaustion returns None instead of
    /// wrapping or panicking.
    #[test]
    fn take_seq_is_monotonic_and_exhaustion_is_explicit() {
        let mut next = 41;
        assert_eq!(take_seq(&mut next), Some(41));
        assert_eq!(take_seq(&mut next), Some(42));
        assert_eq!(next, 43);
        next = u64::MAX - 1;
        assert_eq!(take_seq(&mut next), Some(u64::MAX - 1));
        assert_eq!(take_seq(&mut next), None);
        assert_eq!(take_seq(&mut next), None);
    }
}
