//! Per-session sequencer task: the ONE authoritative publisher.
//!
//! Exactly one task per session owns `next_seq` — it is the only code that
//! assigns `seq` (strictly monotonic, starting at 0 so `session.start` is seq
//! 0). Producers submit unsequenced [`Candidate`]s over an UNBOUNDED
//! non-blocking ingress (`tokio::sync::mpsc::unbounded_channel`): an accepted
//! durable event is never lost, and ingress send failure means the session is
//! closed ([`BusError::SequencerClosed`]), never silent loss. Per event the
//! task performs seq-assign → [`Emitter::emit`] (ring append + subscriber
//! fan-out) plugs in at that seam).
//!
//! AcceptCommand ordering hook (event-before-action): the
//! `message.received` envelope is sequenced + emitted BEFORE the reserved
//! inbox permit is committed, and the ack is sent only after the commit — so
//! the runtime (a separate task) can never consume a command whose
//! `message.received` is not already sequenced.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch};
use ulid::Ulid;

use crate::bus::event::EventEnvelope;
use crate::bus::event::EventKind;
use crate::bus::{LiveSubscription, SubscribeReply};
use crate::error::BusError;

/// A command still unsequenced by the runtime (the full shape is frozen in `bus::command`).
/// `source`/`target`/`mode` carry the frozen `message.received` payload
/// fields; the server message route is the producer.
#[derive(Debug, Clone)]
pub struct PendingCommand {
    pub answer: Option<crate::tools::control::AskAnswer>,
    pub selected_skill: Option<String>,
    pub command_id: Ulid,
    pub content: String,
    pub source: String,
    pub target: String,
    pub mode: String,
}

/// Committer invoked by the sequencer AFTER `message.received` is seq-assigned
/// and emitted, and BEFORE the ack — this IS the event-before-action contract
/// as code. The REAL inbox permit commit (handing the command to
/// the runtime actor's `ActorInbox`) is wired through this
/// seam, which is why `accept_command` TAKES the committer instead of owning
/// one. Must not block: it runs inside the single sequencer mailbox.
pub type CommandCommit = Box<dyn FnOnce() + Send>;

/// The store write of an acknowledged commit: handed the sequenced envelope,
/// it returns once the write is durable. Supplied by the session service, so
/// the bus never imports the store.
pub type AckedCommit = Box<
    dyn FnOnce(
            Arc<EventEnvelope>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
        + Send,
>;
pub type AckedBatchCommit = Box<
    dyn FnOnce(
            Arc<Vec<EventEnvelope>>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
        + Send,
>;

/// Ack returned AFTER the event is sequenced and the command committed.
#[derive(Debug, Clone, Copy)]
pub struct AcceptReceipt {
    pub seq: u64,
}

/// Control + candidate messages drained by the single sequencer task.
#[allow(clippy::type_complexity)]
pub enum Ingress {
    CommitBatch {
        candidates: Vec<Candidate>,
        commit: AckedBatchCommit,
        reply: oneshot::Sender<Result<u64, String>>,
    },
    /// An unsequenced event candidate from any producer.
    Candidate(Candidate),
    /// Event-before-action acceptance: sequence + emit `message.received`
    /// FIRST, then invoke `commit`, then ack with the assigned seq.
    AcceptCommand {
        commit: CommandCommit,
        command: PendingCommand,
        reply: oneshot::Sender<Result<AcceptReceipt, BusError>>,
    },
    /// Sequence + emit `candidate`, then ack with its seq. A candidate queued
    /// behind a close fence is dropped with the mailbox: the reply channel
    /// closes and the caller sees `SessionNotFound`.
    PublishAcked {
        candidate: Candidate,
        reply: oneshot::Sender<u64>,
    },
    /// Sequence `candidate`, hand the envelope to `commit` and wait for the
    /// write to be durable, and only then emit it. A failed commit emits
    /// nothing and gives the seq back, so the stream has no gap. The sequencer
    /// serves nothing else while the write runs: later events queue behind it.
    CommitAcked {
        candidate: Candidate,
        commit: AckedCommit,
        reply: oneshot::Sender<Result<u64, String>>,
    },
    /// Atomic subscribe-then-replay: resolve the cursor against the
    /// ring and register a live queue in ONE synchronous mailbox step.
    SubscribeSince {
        cursor: Option<u64>,
        reply: oneshot::Sender<crate::bus::SubscribeReply>,
    },
    /// Atomic live registration: capture the last emitted seq and register
    /// a live queue in ONE synchronous mailbox step, with no replay.
    SubscribeLive {
        reply: oneshot::Sender<LiveSubscription>,
    },
    /// Session teardown: sequence + emit `session.end`, pin
    /// `final_seq`, then the sequencer task EXITS — the fence that guarantees
    /// no event is ever sequenced after `session.end`.
    CloseSession { reason: String },
    /// Runs `action` on the sequencer task once every earlier ingress message
    /// has been handled (so every event published before it has been
    /// emitted). The action must not block.
    AfterEvents { action: Box<dyn FnOnce() + Send> },
    /// Head query: reply with the next unassigned seq.
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

/// Per-event downstream seam. Sequencer tests collect into a channel; the
/// concrete [`crate::bus::SessionBus`] implements ring append + bounded
/// fan-out here.
pub trait Emitter: Send + 'static {
    fn emit(&mut self, envelope: EventEnvelope);

    /// Atomic subscribe-then-replay (the SSE handler consumes this): called from the
    /// single mailbox handler with NO await around it, so the replay snapshot
    /// and the live registration share one synchronous step (exactly-once at
    /// the seam). `next_seq` is the session head + 1 for cursor sanity.
    fn subscribe_since(
        &mut self,
        cursor: Option<u64>,
        next_seq: u64,
        reply: oneshot::Sender<crate::bus::SubscribeReply>,
    );

    /// Registers a new live queue that receives every event emitted from now
    /// on (no replay). Called from the single mailbox handler with NO await
    /// around it, in the same step that captures the high-water mark.
    fn subscribe_live(&mut self) -> mpsc::Receiver<EventEnvelope>;
}

/// Handle to a running sequencer task. Cloning clones the ingress sender;
/// dropping the LAST handle closes the ingress, ending the task once it
/// drains.
#[derive(Clone, Debug)]
pub struct Sequencer {
    session_id: Ulid,
    ingress_tx: mpsc::UnboundedSender<Ingress>,
    /// Never sent on: `run` drops the sender when the task is done, which
    /// is what [`Sequencer::closed`] waits for.
    done_rx: watch::Receiver<()>,
}

impl Sequencer {
    pub async fn commit_batch_acked(
        &self,
        candidates: Vec<Candidate>,
        commit: AckedBatchCommit,
    ) -> Result<u64, String> {
        if candidates.is_empty() {
            return Err("empty event commit".into());
        }
        let (reply, answer) = oneshot::channel();
        ingress_send(
            &self.ingress_tx,
            Ingress::CommitBatch {
                candidates,
                commit,
                reply,
            },
        )
        .map_err(|e| e.to_string())?;
        answer
            .await
            .map_err(|_| BusError::SequencerClosed.to_string())?
    }
    /// Spawns the ONE authoritative sequencer task for a session. It assigns
    /// seqs from `next_seq`: 0 for a fresh session, `last_seq + 1` for a
    /// resumed one so stored and live seqs form one gapless sequence.
    pub fn spawn(session_id: Ulid, emitter: impl Emitter, next_seq: u64) -> Self {
        let (ingress_tx, ingress_rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = watch::channel(());
        tokio::spawn(run(session_id, ingress_rx, emitter, next_seq, done_tx));
        Self {
            session_id,
            ingress_tx,
            done_rx,
        }
    }

    /// Submits an unsequenced candidate. Never blocks and never drops: the
    /// only failure is a closed session.
    pub fn publish(&self, candidate: Candidate) -> Result<(), BusError> {
        ingress_send(&self.ingress_tx, Ingress::Candidate(candidate))
    }

    /// [`Sequencer::publish`] that waits until the event is sequenced and
    /// emitted, and returns its seq. Fails when the session is closed, or
    /// closes before the event is reached: an `Ok` is proof the event is on
    /// the stream, never discarded behind the close fence.
    pub async fn publish_acked(&self, candidate: Candidate) -> Result<u64, BusError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        ingress_send(
            &self.ingress_tx,
            Ingress::PublishAcked {
                candidate,
                reply: reply_tx,
            },
        )?;
        reply_rx.await.map_err(|_| BusError::SequencerClosed)
    }

    /// Sequences `candidate`, runs `commit` with the sequenced envelope, and
    /// emits the event only once `commit` returned `Ok`. The returned seq is
    /// proof that the write is durable and the event is on the stream. On a
    /// commit error nothing is emitted and the seq is not used. Fails when the
    /// session closes first.
    pub async fn commit_acked(
        &self,
        candidate: Candidate,
        commit: AckedCommit,
    ) -> Result<u64, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        ingress_send(
            &self.ingress_tx,
            Ingress::CommitAcked {
                candidate,
                commit,
                reply: reply_tx,
            },
        )
        .map_err(|e| e.to_string())?;
        reply_rx
            .await
            .map_err(|_| BusError::SequencerClosed.to_string())?
    }

    /// Event-before-action acceptance. The handler's order is: seq-assign
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

    /// Hands `action` to the sequencer task, which runs it after every event
    /// published before this call has been emitted (the mailbox is FIFO).
    /// Fails, returning the action's ownership to nobody, only when the
    /// session is closed. The action must not block.
    pub fn after_events(&self, action: Box<dyn FnOnce() + Send>) -> Result<(), BusError> {
        ingress_send(&self.ingress_tx, Ingress::AfterEvents { action })
    }

    /// Atomic subscribe-then-replay: ONE control op on the sequencer task.
    /// The mailbox handler, without awaiting, resolves the cursor against the
    /// ring (per the resume contract on `SessionBus`), registers a bounded live queue,
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

    /// Atomic live subscription: ONE control op on the sequencer task. The
    /// mailbox handler, without awaiting, captures the last emitted seq and
    /// registers a bounded live queue, and returns no replay. Every event
    /// with `seq <= high_water` was emitted before the registration; every
    /// later event goes to the receiver, exactly once and in order.
    ///
    /// `llm.delta` shares the seq space, so `high_water` may be a delta's
    /// seq. Deltas are never stored: a store read through `high_water` plus
    /// the receiver covers every durable event. A resumed session reports the
    /// last stored seq even before its first new emit. A closed session maps
    /// to `BusError::SessionNotFound`.
    pub async fn subscribe_live(&self) -> Result<LiveSubscription, BusError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        ingress_send(&self.ingress_tx, Ingress::SubscribeLive { reply: reply_tx })?;
        reply_rx
            .await
            .map_err(|_| BusError::SessionNotFound(self.session_id.to_string()))
    }

    /// Resolves once the sequencer task has finished: after `session.end`
    /// (or the last handle dropping) every event is emitted and the emitter
    /// is dropped. Resolves immediately when it already finished.
    pub async fn closed(&self) {
        let mut done = self.done_rx.clone();
        // `changed` only errors once the sender is dropped; nothing sends.
        while done.changed().await.is_ok() {}
    }

    /// Head accessor (`GET /sessions/:id` `current_seq`): replies
    /// with the next unassigned seq, so the last-EMITTED seq is
    /// `current - 1` (the pinned `current_seq` = last emitted seq).
    /// A closed session maps to `BusError::SessionNotFound` (the session
    /// manager treats that as "the fence already assigned final_seq").
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
    /// the last event the session ever sequences. Idempotent: calling it on
    /// an already-closed session is a `SequencerClosed` error, never a second
    /// `session.end`.
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
    next_seq: u64,
    done: watch::Sender<()>,
) {
    // `next_seq` lives ONLY inside this task (single assignment point).
    let mut next_seq: u64 = next_seq;

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
            Ingress::PublishAcked { candidate, reply } => {
                let Some(seq) = take_seq(&mut next_seq) else {
                    break;
                };
                emitter.emit(stamp(session_id, seq, candidate));
                let _ = reply.send(seq);
            }
            Ingress::CommitBatch {
                candidates,
                commit,
                reply,
            } => {
                let first = next_seq;
                let count = candidates.len();
                let mut envelopes = Vec::new();
                for candidate in candidates {
                    let Some(seq) = take_seq(&mut next_seq) else {
                        break;
                    };
                    envelopes.push(stamp(session_id, seq, candidate));
                }
                if envelopes.len() != count {
                    next_seq = first;
                    let _ = reply.send(Err(BusError::SeqSpaceExhausted.to_string()));
                    continue;
                }
                let envelopes = Arc::new(envelopes);
                match commit(Arc::clone(&envelopes)).await {
                    Ok(()) => {
                        for envelope in Arc::unwrap_or_clone(envelopes) {
                            emitter.emit(envelope);
                        }
                        let _ = reply.send(Ok(first));
                    }
                    Err(why) => {
                        next_seq = first;
                        let _ = reply.send(Err(why));
                    }
                }
            }
            Ingress::CommitAcked {
                candidate,
                commit,
                reply,
            } => {
                let Some(seq) = take_seq(&mut next_seq) else {
                    let _ = reply.send(Err(BusError::SeqSpaceExhausted.to_string()));
                    break;
                };
                let envelope = Arc::new(stamp(session_id, seq, candidate));
                match commit(Arc::clone(&envelope)).await {
                    Ok(()) => {
                        emitter.emit(Arc::unwrap_or_clone(envelope));
                        let _ = reply.send(Ok(seq));
                    }
                    Err(why) => {
                        // Nobody saw this seq: give it back.
                        next_seq = seq;
                        let _ = reply.send(Err(why));
                    }
                }
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
                // Event-before-action, in this exact order: emit the
                // sequenced envelope, THEN commit the command, THEN ack —
                // so the ack is observable proof that commit followed emit.
                let command_id = command.command_id;
                let mut candidate = message_received_candidate(command);
                candidate.causation_id = Some(command_id);
                emitter.emit(stamp(session_id, seq, candidate));
                commit();
                let _ = reply.send(Ok(AcceptReceipt { seq }));
            }
            Ingress::SubscribeSince { cursor, reply } => {
                // The atomic handshake: no await between the replay snapshot
                // and the live registration (both live inside `emitter`).
                emitter.subscribe_since(cursor, next_seq, reply);
            }
            Ingress::SubscribeLive { reply } => {
                // `next_seq - 1` is the last assigned (== emitted) seq; a
                // resumed session starts from its stored head.
                let high_water = next_seq.checked_sub(1);
                let receiver = emitter.subscribe_live();
                let _ = reply.send(LiveSubscription {
                    high_water,
                    receiver,
                });
            }
            Ingress::AfterEvents { action } => action(),
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
    // Emitter first (flushes its hand-offs, closes live queues), then the
    // completion signal.
    drop(emitter);
    drop(done);
}

/// The single seq-assignment point. Takes `&mut u64` so no other code path
/// can mint a seq; ordering never derives from wall-clock time.
fn take_seq(next_seq: &mut u64) -> Option<u64> {
    let seq = *next_seq;
    *next_seq = next_seq.checked_add(1)?;
    Some(seq)
}

fn message_received_candidate(command: PendingCommand) -> Candidate {
    Candidate::new(
        EventKind::MessageReceived,
        serde_json::to_value(crate::bus::event::MessageReceived {
            answer: command.answer,
            command_id: command.command_id,
            source: command.source,
            target: command.target,
            mode: command.mode,
            content: command.content,
            selected_skill: command
                .selected_skill
                .map(|name| crate::bus::event::SelectedSkill {
                    name,
                    origin: "captain".into(),
                }),
        })
        .expect("message payload serializes"),
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
            // Subscribe is a fan-out concern; this collector emitter
            // has no subscriber registry, so the request fails like a closed
            // session would.
            drop(reply);
        }

        fn subscribe_live(&mut self) -> mpsc::Receiver<EventEnvelope> {
            mpsc::channel(1).1
        }
    }

    fn spawn_with_collector() -> (Arc<Sequencer>, mpsc::UnboundedReceiver<EventEnvelope>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Sequencer::spawn(Ulid::generate(), ForwardEmitter { tx }, 0)),
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
                "turn_id": Ulid::generate(),
                "actor_id": "agent:main",
                "task_id": Ulid::generate(),
            }),
        )
        .with_correlation_id(Ulid::generate())
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

    /// (e) Event-before-action, GENUINELY OBSERVED: the emitter and the
    /// committer both append to one shared timeline, and the ack appends its
    /// entry after `accept_command` returns. The handler structure is
    /// emit -> commit -> ack in ONE task, so a recorded order of
    /// Emit,Commit,Ack with strictly increasing counters proves the property;
    /// any reordering (commit-before-emit, ack-before-commit) fails the
    /// assertions below.
    #[tokio::test]
    async fn accept_command_sequences_event_before_committing_and_acking() {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicU64, Ordering};

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

            fn subscribe_live(&mut self) -> mpsc::Receiver<EventEnvelope> {
                mpsc::channel(1).1
            }
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TimelineEmitter {
            timeline: Arc::clone(&timeline),
            tx,
        };
        let seq = Sequencer::spawn(Ulid::generate(), emitter, 0);

        let command_id = Ulid::generate();
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
                    answer: Some(crate::tools::control::AskAnswer {
                        question_call_id: "question-call".into(),
                        selected: vec![1],
                        custom: Some("custom answer".into()),
                    }),
                    selected_skill: Some("rust".into()),
                    command_id,
                    content: "hello there".into(),
                    source: "user:cli".into(),
                    target: "agent:main".into(),
                    mode: "next-turn".into(),
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
        assert_eq!(envelope.payload["source"], "user:cli");
        assert_eq!(envelope.payload["target"], "agent:main");
        assert_eq!(
            envelope.payload["selected_skill"],
            serde_json::json!({"name": "rust", "origin": "captain"})
        );
        assert_eq!(
            envelope.payload["answer"],
            serde_json::json!({"question_call_id": "question-call", "selected": [1], "custom": "custom answer"})
        );

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
    async fn after_events_runs_behind_every_event_published_before_it() {
        let (seq, mut rx) = spawn_with_collector();
        seq.publish(start_candidate()).unwrap();
        let (tx, done) = oneshot::channel();
        seq.after_events(Box::new(move || {
            let _ = tx.send(());
        }))
        .unwrap();
        done.await.unwrap();
        assert!(rx.try_recv().is_ok(), "the event was emitted first");
    }

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

    /// `closed` stays pending while the session lives and resolves once the
    /// sequencer task finished, i.e. after `session.end` was emitted.
    #[tokio::test]
    async fn closed_resolves_after_close_session_emitted_session_end() {
        let (seq, mut rx) = spawn_with_collector();
        let clone = seq.as_ref().clone();
        seq.publish(start_candidate()).unwrap();

        let pending = tokio::time::timeout(std::time::Duration::from_millis(50), seq.closed());
        assert!(pending.await.is_err(), "open session: closed is pending");

        let waiter = tokio::spawn(async move { clone.closed().await });
        seq.close_session("stop").unwrap();
        waiter.await.unwrap();
        seq.closed().await;

        let start = rx.try_recv().unwrap();
        assert_eq!(start.kind.as_str(), "session.start");
        let end = rx.try_recv().expect("session.end emitted before closed");
        assert_eq!((end.kind.as_str(), end.seq), ("session.end", 1));
        assert!(matches!(
            seq.publish(turn_candidate("late")),
            Err(BusError::SequencerClosed)
        ));
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

    // Given a commit that succeeds
    // Then the event is emitted only after it ran, and the ack carries its seq
    #[tokio::test]
    async fn a_commit_acked_event_is_emitted_after_its_commit() {
        let (seq, mut rx) = spawn_with_collector();
        let (ran_tx, ran_rx) = oneshot::channel();
        let assigned = seq
            .commit_acked(
                turn_candidate("switch"),
                Box::new(move |envelope| {
                    Box::pin(async move {
                        assert_eq!(Arc::strong_count(&envelope), 2);
                        let tag = envelope.payload["tag"].as_str().unwrap();
                        let _ =
                            ran_tx.send((envelope.seq, envelope.event_id, tag.as_ptr() as usize));
                        Ok(())
                    })
                }),
            )
            .await
            .unwrap();
        let (committed_seq, event_id, payload_pointer) = ran_rx.await.unwrap();
        assert_eq!(committed_seq, assigned);
        let emitted = rx.recv().await.unwrap();
        assert_eq!((emitted.seq, emitted.event_id), (assigned, event_id));
        assert_eq!(
            emitted.payload["tag"].as_str().unwrap().as_ptr() as usize,
            payload_pointer
        );
    }

    #[tokio::test]
    async fn batch_commit_shares_payload_then_emits_in_order_without_gaps_on_failure() {
        let (seq, mut rx) = spawn_with_collector();
        let (committed, seen) = oneshot::channel();
        let first = seq
            .commit_batch_acked(
                vec![turn_candidate("first"), turn_candidate("second")],
                Box::new(move |envelopes| {
                    Box::pin(async move {
                        assert_eq!(Arc::strong_count(&envelopes), 2);
                        let tag = envelopes[0].payload["tag"].as_str().unwrap();
                        let _ = committed.send((envelopes[0].event_id, tag.as_ptr() as usize));
                        Ok(())
                    })
                }),
            )
            .await
            .unwrap();
        let (event_id, payload_pointer) = seen.await.unwrap();
        let emitted = rx.recv().await.unwrap();
        assert_eq!((emitted.seq, emitted.event_id), (first, event_id));
        assert_eq!(
            emitted.payload["tag"].as_str().unwrap().as_ptr() as usize,
            payload_pointer
        );
        assert_eq!(rx.recv().await.unwrap().seq, first + 1);
        assert!(
            seq.commit_batch_acked(
                vec![turn_candidate("lost"), turn_candidate("also lost")],
                Box::new(|_| Box::pin(async { Err("disk full".into()) })),
            )
            .await
            .is_err()
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            seq.publish_acked(turn_candidate("next")).await.unwrap(),
            first + 2
        );
    }

    #[tokio::test]
    async fn cancelled_queued_callback_emits_nothing_and_reuses_reserved_sequence() {
        let (seq, mut rx) = spawn_with_collector();
        let (entered, wait) = oneshot::channel();
        let (release, resume) = oneshot::channel();
        let first = tokio::spawn({
            let seq = seq.clone();
            async move {
                seq.commit_acked(
                    turn_candidate("first"),
                    Box::new(move |_| {
                        Box::pin(async move {
                            let _ = entered.send(());
                            let _ = resume.await;
                            Ok(())
                        })
                    }),
                )
                .await
            }
        });
        wait.await.unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        let cancelled = tokio::spawn({
            let seq = seq.clone();
            let token = token.clone();
            async move {
                seq.commit_acked(
                    turn_candidate("cancelled"),
                    Box::new(move |_| {
                        Box::pin(async move {
                            if token.is_cancelled() {
                                Err("cancelled callback".into())
                            } else {
                                Ok(())
                            }
                        })
                    }),
                )
                .await
            }
        });
        token.cancel();
        release.send(()).unwrap();
        first.await.unwrap().unwrap();
        assert!(cancelled.await.unwrap().is_err());
        assert_eq!(rx.recv().await.unwrap().seq, 0);
        assert!(rx.try_recv().is_err());
        assert_eq!(seq.publish_acked(turn_candidate("next")).await.unwrap(), 1);
    }

    // Given a commit that fails
    // Then nothing is emitted, the error comes back, and the next event takes
    //      the same seq: no gap
    #[tokio::test]
    async fn a_failed_commit_emits_nothing_and_leaves_no_gap() {
        let (seq, mut rx) = spawn_with_collector();
        seq.publish(start_candidate()).unwrap();
        let failed = seq
            .commit_acked(
                turn_candidate("lost"),
                Box::new(|_| Box::pin(async { Err("disk full".to_string()) })),
            )
            .await;
        assert_eq!(failed.unwrap_err(), "disk full");
        let next = seq.publish_acked(turn_candidate("after")).await.unwrap();
        assert_eq!(next, 1);
        assert_eq!(rx.recv().await.unwrap().seq, 0);
        let after = rx.recv().await.unwrap();
        assert_eq!(
            (after.seq, after.payload["tag"].as_str()),
            (1, Some("after"))
        );
        assert!(rx.try_recv().is_err());
    }
}
