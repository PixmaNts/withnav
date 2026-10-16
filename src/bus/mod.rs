//! Phase 2 event bus: per-session sequencer, event envelopes, per-actor
//! inboxes, and the agent runtime task.
//!
//! The [`SessionBus`] is the concrete [`Emitter`] the
//! sequencer task drives after seq assignment. Per event it performs, in one
//! synchronous step inside the single sequencer task: ring-append (durable
//! kinds only) → fan-out to per-subscriber bounded mpsc queues. Subscribers
//! are best-effort: a full queue disconnects that subscriber (never blocks the
//! sequencer, never silently drops a durable event — the client reconnects and
//! replays via [`Sequencer::subscribe_since`], or reads the store up to the
//! [`LiveSubscription`] high-water mark). The in-memory [`EventRing`]
//! retains every durable kind and tracks `last_evicted_durable_seq`, the
//! resume floor for the [`ResumeOutcome`] contract.
pub mod command;
pub mod event;
pub mod inbox;
pub mod runtime;
pub mod sequencer;

use tokio::sync::{mpsc, oneshot};

use crate::bus::event::EventEnvelope;
use crate::bus::event::EventKind;

/// Per-subscriber live-queue capacity. When a subscriber's queue is full, the
/// bus disconnects it (drops the sender, closing the stream cleanly) rather
/// than blocking the sequencer or silently dropping a durable event.
pub const SUBSCRIBER_QUEUE_CAP: usize = 256;

/// In-memory reconnect-ring capacity per session: how many DURABLE events are
/// retained for `Last-Event-ID` replay. `llm.delta` never occupies a slot.
pub const RING_CAP: usize = 4096;

/// Result of the resume contract for a reconnecting client cursor.
///
/// The resume floor is `last_evicted_durable_seq` (the evicted-durable
/// watermark), NOT the oldest retained seq: while a durable event with
/// `seq > cursor` has not been evicted, replay is gapless even when the
/// cursor sits one below the oldest retained seq.
#[derive(Debug, Clone, PartialEq)]
pub enum ResumeOutcome {
    /// Durable events with `seq > cursor`, in seq order (empty when the
    /// client is already at the head).
    Replay(Vec<EventEnvelope>),
    /// The cursor predates an evicted durable event: the client must reload.
    Resync,
    /// The cursor is ahead of the session head (future or stale session):
    /// never treated as "caught up".
    InvalidCursor,
}

/// The atomic subscribe-then-replay handshake result (the SSE handler
/// consumes this): the replay vector is the ring snapshot taken in the SAME
/// synchronous step that registers `live`, and `live` yields only events
/// with `seq >` that snapshot's head — replay ∪ live is exactly-once.
#[derive(Debug)]
pub struct SubscribeReply {
    pub replay: ResumeOutcome,
    pub live: Option<mpsc::Receiver<EventEnvelope>>,
}

/// The atomic live-only subscription: no replay. Every event with
/// `seq <= high_water` was emitted before the registration and every later
/// event goes to `receiver`, exactly once and in order.
///
/// `llm.delta` shares the seq space, so `high_water` may be a delta's seq;
/// deltas are never stored. A store read through `high_water` plus
/// `receiver` therefore covers every durable event. `high_water` is `None`
/// when no seq was assigned yet.
#[derive(Debug)]
pub struct LiveSubscription {
    pub high_water: Option<u64>,
    pub receiver: mpsc::Receiver<EventEnvelope>,
}

/// FIFO over the durable subsequence makes evicted durables exactly the
/// prefix `[0..=watermark]`, so a cursor `k >= watermark` has every durable
/// event after it still retained (gapless), and `k < watermark` is a real gap.
#[derive(Debug, Default)]
pub struct EventRing {
    events: std::collections::VecDeque<EventEnvelope>,
    last_evicted_durable_seq: Option<u64>,
}

impl EventRing {
    /// Appends an envelope; durable kinds are retained (bounded, evict
    /// oldest), `llm.delta` is ignored.
    fn append(&mut self, envelope: &EventEnvelope) {
        let Some(kind) = EventKind::from_wire(&envelope.kind) else {
            return;
        };
        if !kind.is_durable() {
            return;
        }
        if self.events.len() == RING_CAP
            && let Some(evicted) = self.events.pop_front()
        {
            self.last_evicted_durable_seq =
                Some(self.last_evicted_durable_seq.unwrap_or(0).max(evicted.seq));
            debug_assert!(envelope.seq > evicted.seq);
        }
        self.events.push_back(envelope.clone());
    }

    /// Durable events after `cursor`, in seq order; `None` means no cursor:
    /// the full retained history INCLUDING seq 0.
    fn replay_from(&self, cursor: Option<u64>) -> Vec<EventEnvelope> {
        self.events
            .iter()
            .filter(|e| cursor.is_none_or(|k| e.seq > k))
            .cloned()
            .collect()
    }

    fn last_evicted_durable_seq(&self) -> Option<u64> {
        self.last_evicted_durable_seq
    }
}

/// The concrete [`sequencer::Emitter`] for a session: ring append + bounded
/// per-subscriber fan-out, executed synchronously inside the single
/// sequencer task. One instance per session; never shared across sessions.
#[derive(Debug, Default)]
pub struct SessionBus {
    ring: EventRing,
    subscribers: Vec<mpsc::Sender<EventEnvelope>>,
}

impl SessionBus {
    /// The evicted-durable watermark (the resume floor), `None` while every
    /// durable event is still retained.
    pub fn last_evicted_durable_seq(&self) -> Option<u64> {
        self.ring.last_evicted_durable_seq()
    }

    /// Resume contract: resolves `cursor` against the ring and the
    /// session head (`next_seq`), then registers a new bounded live queue in
    /// the SAME synchronous step — so replay ∪ live is exactly-once. A
    /// rejected cursor (`Resync`/`InvalidCursor`) gets no live stream.
    fn subscribe_since(&mut self, cursor: Option<u64>, next_seq: u64) -> SubscribeReply {
        let replay = match cursor {
            None => ResumeOutcome::Replay(self.ring.replay_from(None)),
            Some(k) if k >= next_seq => ResumeOutcome::InvalidCursor,
            Some(k) => {
                match self.ring.last_evicted_durable_seq {
                    // Every durable event after `k` is still retained.
                    None => ResumeOutcome::Replay(self.ring.replay_from(Some(k))),
                    Some(watermark) if k >= watermark => {
                        ResumeOutcome::Replay(self.ring.replay_from(Some(k)))
                    }
                    // A durable event after `k` was evicted: real gap.
                    Some(_) => ResumeOutcome::Resync,
                }
            }
        };

        let live = match replay {
            ResumeOutcome::Replay(_) => {
                let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE_CAP);
                self.subscribers.push(tx);
                Some(rx)
            }
            _ => None,
        };
        SubscribeReply { replay, live }
    }
}

impl sequencer::Emitter for SessionBus {
    /// seq-assign (by the caller) → ring-append (durable only) → fan-out,
    /// all synchronous. A full subscriber queue disconnects that subscriber:
    /// dropping the sender closes its stream cleanly, and the events it
    /// missed are retained in the ring for reconnect replay.
    fn emit(&mut self, envelope: EventEnvelope) {
        self.ring.append(&envelope);
        if self.subscribers.is_empty() {
            return;
        }
        self.subscribers
            .retain_mut(|tx| tx.try_send(envelope.clone()).is_ok());
    }

    /// The atomic handshake, executed inside the sequencer's mailbox handler.
    fn subscribe_since(
        &mut self,
        cursor: Option<u64>,
        next_seq: u64,
        reply: oneshot::Sender<SubscribeReply>,
    ) {
        let _ = reply.send(self.subscribe_since(cursor, next_seq));
    }

    fn subscribe_live(&mut self) -> mpsc::Receiver<EventEnvelope> {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE_CAP);
        self.subscribers.push(tx);
        rx
    }
}

#[cfg(test)]
mod tests {
    use ulid::Ulid;

    use super::event::{EventEnvelope, EventKind};
    use super::sequencer::{Candidate, Emitter, Sequencer};
    use super::*;

    fn durable_envelope(seq: u64) -> EventEnvelope {
        EventEnvelope::new(
            Ulid::generate(),
            seq,
            EventKind::TurnStart,
            serde_json::json!({ "n": seq }),
        )
    }

    fn delta_envelope(seq: u64) -> EventEnvelope {
        EventEnvelope::new(
            Ulid::generate(),
            seq,
            EventKind::LlmDelta,
            serde_json::json!({ "n": seq }),
        )
    }

    fn durable_candidate(n: u64) -> Candidate {
        Candidate::new(EventKind::TurnStart, serde_json::json!({ "n": n }))
    }

    /// The bus replies synchronously inside `subscribe_since`; this helper
    /// collects the reply without awaiting.
    fn subscribe(bus: &mut SessionBus, cursor: Option<u64>, next_seq: u64) -> SubscribeReply {
        bus.subscribe_since(cursor, next_seq)
    }

    fn replay_seqs(outcome: &ResumeOutcome) -> Vec<u64> {
        match outcome {
            ResumeOutcome::Replay(events) => events.iter().map(|e| e.seq).collect(),
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    /// (f) The ring never exceeds its cap; the watermark advances only when a
    /// DURABLE event is evicted; `llm.delta` never enters (and never evicts).
    #[test]
    fn ring_caps_and_advances_watermark_only_on_durable_eviction() {
        let mut ring = EventRing::default();
        for seq in 0..(RING_CAP as u64 + 25) {
            ring.append(&durable_envelope(seq));
        }
        assert_eq!(ring.replay_from(None).len(), RING_CAP);
        assert_eq!(ring.last_evicted_durable_seq(), Some(24));
        let oldest = ring.replay_from(None).first().map(|e| e.seq);
        assert_eq!(oldest, Some(25));

        // llm.delta is live-only: never retained, never evicts a durable.
        let mut ring = EventRing::default();
        ring.append(&durable_envelope(0));
        ring.append(&delta_envelope(1));
        ring.append(&durable_envelope(2));
        assert_eq!(ring.replay_from(None).len(), 2);
        assert_eq!(ring.last_evicted_durable_seq(), None);

        // replay_from(Some(k)) is after-k, ordered, durable-only; None is
        // the full retained history including seq 0.
        let seqs: Vec<u64> = ring.replay_from(Some(0)).iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![2]);
        assert!(ring.replay_from(Some(2)).is_empty());
        let all: Vec<u64> = ring.replay_from(None).iter().map(|e| e.seq).collect();
        assert_eq!(all, vec![0, 2]);
    }

    /// (e) cursor == oldest_retained - 1 replays GAPLESSLY because the
    /// floor is the evicted-durable watermark, not the oldest retained seq.
    #[test]
    fn cursor_at_oldest_retained_minus_one_still_replays_gaplessly() {
        let mut bus = SessionBus::default();
        let total = RING_CAP as u64 + 10;
        for seq in 0..total {
            bus.emit(durable_envelope(seq));
        }
        let oldest_retained = total - RING_CAP as u64; // 10

        // The watermark sits exactly one below the oldest retained event.
        assert_eq!(bus.last_evicted_durable_seq(), Some(oldest_retained - 1));

        let reply = subscribe(&mut bus, Some(oldest_retained - 1), total);
        let seqs = replay_seqs(&reply.replay);
        assert_eq!(seqs.len(), RING_CAP);
        assert_eq!(seqs[0], oldest_retained);
        assert_eq!(*seqs.last().unwrap(), total - 1);
        assert!(
            seqs.windows(2).all(|w| w[1] == w[0] + 1),
            "gapless durable replay: {seqs:?}"
        );

        // One BELOW the watermark is a real gap: explicit Resync.
        let reply = subscribe(&mut bus, Some(oldest_retained - 2), total);
        assert!(matches!(reply.replay, ResumeOutcome::Resync));
        assert!(reply.live.is_none(), "rejected cursors get no live stream");
    }

    /// (c) The five pinned resume cases.
    #[tokio::test]
    async fn the_five_resume_cases_behave_as_pinned() {
        let mut bus = SessionBus::default();
        for seq in 0..5u64 {
            bus.emit(durable_envelope(seq));
        }
        let next: u64 = 5;

        // (1) None → FULL retained history including session.start (seq 0).
        let reply = subscribe(&mut bus, None, next);
        assert_eq!(replay_seqs(&reply.replay), vec![0, 1, 2, 3, 4]);
        assert!(reply.live.is_some(), "accepted cursor gets a live stream");

        // (2) Some(k), k >= watermark (None here), k < next → durable seq > k.
        let reply = subscribe(&mut bus, Some(2), next);
        assert_eq!(replay_seqs(&reply.replay), vec![3, 4]);

        // (4) Some(head) → empty replay + live.
        let reply = subscribe(&mut bus, Some(4), next);
        assert_eq!(replay_seqs(&reply.replay), Vec::<u64>::new());
        assert!(reply.live.is_some());

        // (5) Some(k > head) → InvalidCursor, NEVER "caught up".
        let reply = subscribe(&mut bus, Some(9), next);
        assert!(matches!(reply.replay, ResumeOutcome::InvalidCursor));
        assert!(reply.live.is_none());

        // (3) Some(k < watermark) → Resync. Flood past the cap to evict
        // seqs 0..=5 (watermark = 5).
        for seq in next..(RING_CAP as u64 + 6) {
            bus.emit(durable_envelope(seq));
        }
        assert_eq!(bus.last_evicted_durable_seq(), Some(5));
        let reply = subscribe(&mut bus, Some(2), RING_CAP as u64 + 6);
        assert!(matches!(reply.replay, ResumeOutcome::Resync));
        assert!(reply.live.is_none());
    }

    /// (d) A 10k llm.delta flood interleaved with one of EACH
    /// durable kind: deltas never enter the ring, and ALL durable kinds
    /// remain resumable in seq order (none lost, none invalidated). Deltas
    /// consume no ring slots, so every durable kind fits — the watermark
    /// stays None and every durable event survives the flood.
    #[test]
    fn ten_k_delta_flood_leaves_every_durable_kind_resumable() {
        let durable_kinds: Vec<EventKind> = EventKind::ALL
            .iter()
            .copied()
            .filter(|k| k.is_durable())
            .collect();
        // Only `llm.delta`, `context.updated` and `context.compacting` are live-only.
        assert_eq!(durable_kinds.len(), super::event::EVENT_KIND_COUNT - 3);

        let mut bus = SessionBus::default();
        let mut seq: u64 = 0;
        let mut expected_durable_kinds: Vec<EventKind> = Vec::new();
        for kind in &durable_kinds {
            bus.emit(EventEnvelope::new(
                Ulid::generate(),
                seq,
                *kind,
                serde_json::json!({ "n": seq }),
            ));
            expected_durable_kinds.push(*kind);
            seq += 1;
            for _ in 0..833 {
                bus.emit(delta_envelope(seq));
                seq += 1;
            }
        }
        // Each durable kind is followed by 833 llm.delta.
        assert_eq!(seq, durable_kinds.len() as u64 * 834);
        assert_eq!(bus.last_evicted_durable_seq(), None);

        let reply = subscribe(&mut bus, None, seq);
        let replayed = match reply.replay {
            ResumeOutcome::Replay(events) => events,
            other => panic!("expected Replay, got {other:?}"),
        };
        assert_eq!(
            replayed.len(),
            durable_kinds.len(),
            "exactly the durable events"
        );

        // Every durable kind present, in emission (== seq) order.
        let replayed_kinds: Vec<EventKind> = replayed
            .iter()
            .map(|e| EventKind::from_wire(&e.kind).expect("catalog kinds parse"))
            .collect();
        assert_eq!(replayed_kinds, durable_kinds);
        assert!(
            replayed.windows(2).all(|w| w[0].seq < w[1].seq),
            "strictly seq-ordered durable replay"
        );

        // Deltas are ABSENT: no retained event carries the llm.delta name.
        assert!(
            replayed
                .iter()
                .all(|e| EventKind::from_wire(&e.kind) != Some(EventKind::LlmDelta))
        );

        // A delta gap NEVER invalidates replay: a cursor sitting deep inside
        // the flood still replays the remaining durables (deltas are legal
        // gaps). Durables sit at seqs i*834.
        let reply = subscribe(&mut bus, Some(5_000), seq);
        let expected: Vec<u64> = (0..durable_kinds.len() as u64)
            .map(|i| i * 834)
            .filter(|s| *s > 5_000)
            .collect();
        assert_eq!(replay_seqs(&reply.replay), expected);
    }

    /// (a) + (b) DETERMINISTIC disconnect-not-drop, driven directly on the
    /// SessionBus fan-out (the same synchronous `emit` step the sequencer
    /// task performs). No spawned racer, no yields: per event the test emits
    /// and drains the healthy subscriber (`try_recv`) BEFORE the next emit;
    /// the slow subscriber is NEVER drained. Ground truth is structural:
    /// the slow queue deterministically fills to SUBSCRIBER_QUEUE_CAP and
    /// disconnects on the next durable event, while the healthy subscriber
    /// deterministically receives the COMPLETE durable sequence in order.
    #[test]
    fn slow_subscriber_disconnects_while_healthy_one_receives_everything() {
        let mut bus = SessionBus::default();
        let mut slow = subscribe(&mut bus, None, 0).live.take().unwrap();
        let mut healthy = subscribe(&mut bus, None, 0).live.take().unwrap();

        let total = (SUBSCRIBER_QUEUE_CAP + 5) as u64;
        let mut healthy_seqs = Vec::new();
        for seq in 0..total {
            // The publisher is never blocked: emit is synchronous and
            // bounded by the subscriber count, not by subscriber speed.
            bus.emit(durable_envelope(seq));
            // Drain-before-next-publish: the healthy subscriber provably
            // keeps up; its queue never accumulates more than 1 event.
            while let Ok(envelope) = healthy.try_recv() {
                healthy_seqs.push(envelope.seq);
            }

            // Structural disconnect checkpoints (no timing involved):
            // below the cap nothing was dropped, above it the slow stream
            // is CLOSED, not silently missing events.
            if seq < SUBSCRIBER_QUEUE_CAP as u64 {
                assert!(
                    !slow.is_closed(),
                    "slow must stay connected through seq {seq}"
                );
            } else {
                assert!(
                    slow.is_closed(),
                    "slow must be disconnected at seq {seq} (queue overflowed at CAP={SUBSCRIBER_QUEUE_CAP})"
                );
            }
        }
        while let Ok(envelope) = healthy.try_recv() {
            healthy_seqs.push(envelope.seq);
        }
        assert_eq!(
            healthy_seqs,
            (0..total).collect::<Vec<_>>(),
            "healthy subscriber received the complete durable sequence in order"
        );

        // The slow subscriber saw EXACTLY its queue worth, then its stream
        // ended (disconnect) — nothing silently missing: the overflow is
        // retained in the ring and replays gaplessly on reconnect.
        let mut slow_seqs = Vec::new();
        while let Ok(envelope) = slow.try_recv() {
            slow_seqs.push(envelope.seq);
        }
        assert_eq!(
            slow_seqs,
            (0..SUBSCRIBER_QUEUE_CAP as u64).collect::<Vec<_>>()
        );

        let resume = subscribe(&mut bus, Some(SUBSCRIBER_QUEUE_CAP as u64 - 1), total);
        assert_eq!(
            replay_seqs(&resume.replay),
            (SUBSCRIBER_QUEUE_CAP as u64..total).collect::<Vec<_>>()
        );
        assert!(resume.live.is_some(), "reconnect re-registers live");
    }

    /// (g) Through the real sequencer: replay-then-live chains without a gap
    /// or a duplicate across the subscribe boundary.
    #[tokio::test]
    async fn sequencer_subscribe_replay_then_live_is_gap_and_dup_free() {
        let sequencer = Sequencer::spawn(Ulid::generate(), SessionBus::default(), 0);
        sequencer.publish(durable_candidate(0)).unwrap();
        sequencer.publish(durable_candidate(1)).unwrap();

        let mut sub = sequencer.subscribe_since(Some(0)).await.unwrap();
        assert_eq!(replay_seqs(&sub.replay), vec![1]);

        sequencer.publish(durable_candidate(2)).unwrap();
        let mut live = sub.live.take().unwrap();
        assert_eq!(live.recv().await.unwrap().seq, 2);

        // Exactly once across replay ∪ live: [1, 2], no dup, no gap.
        drop(sequencer);
    }

    /// Wraps a bus and records how many events were emitted at the moment
    /// the live receiver was registered.
    struct Probe {
        bus: SessionBus,
        emitted: u64,
        at_registration: std::sync::Arc<std::sync::Mutex<Option<u64>>>,
    }

    impl Emitter for Probe {
        fn emit(&mut self, envelope: EventEnvelope) {
            self.emitted += 1;
            self.bus.emit(envelope);
        }

        fn subscribe_since(
            &mut self,
            cursor: Option<u64>,
            next_seq: u64,
            reply: oneshot::Sender<SubscribeReply>,
        ) {
            let _ = reply.send(self.bus.subscribe_since(cursor, next_seq));
        }

        fn subscribe_live(&mut self) -> mpsc::Receiver<EventEnvelope> {
            *self.at_registration.lock().unwrap() = Some(self.emitted);
            self.bus.subscribe_live()
        }
    }

    /// Publishing races `subscribe_live`: the receiver gets exactly the
    /// events above `high_water`, in order, none missing, none duplicated,
    /// and everything at or below it was emitted before the registration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn atomic_live_subscription_partitions_at_high_water() {
        // Below SUBSCRIBER_QUEUE_CAP: the receiver is only drained at the end.
        const EVENTS: u64 = 200;
        for iteration in 0..300u64 {
            let at_registration = std::sync::Arc::new(std::sync::Mutex::new(None));
            let probe = Probe {
                bus: SessionBus::default(),
                emitted: 0,
                at_registration: std::sync::Arc::clone(&at_registration),
            };
            let sequencer = Sequencer::spawn(Ulid::generate(), probe, 0);

            // An OS thread publishes while this task subscribes, so the two
            // genuinely race; the spin length sweeps the landing point.
            let publisher = {
                let sequencer = sequencer.clone();
                std::thread::spawn(move || {
                    for n in 0..EVENTS {
                        // Deltas share the seq space and must not break it.
                        let candidate = if n % 3 == 0 {
                            Candidate::new(EventKind::LlmDelta, serde_json::json!({ "n": n }))
                        } else {
                            durable_candidate(n)
                        };
                        sequencer.publish(candidate).unwrap();
                    }
                })
            };
            for _ in 0..(iteration * 37 % 3000) {
                std::hint::spin_loop();
            }
            let LiveSubscription {
                high_water,
                mut receiver,
            } = sequencer.subscribe_live().await.unwrap();
            publisher.join().unwrap();
            // session.end is the last event (seq EVENTS) and ends the stream.
            sequencer.close_session("done").unwrap();

            let mut seqs = Vec::new();
            while let Some(envelope) = receiver.recv().await {
                seqs.push(envelope.seq);
            }
            let first = high_water.map_or(0, |h| h + 1);
            let expected: Vec<u64> = (first..=EVENTS).collect();
            assert_eq!(
                seqs, expected,
                "iteration {iteration}, high_water {high_water:?}"
            );
            assert_eq!(
                *at_registration.lock().unwrap(),
                Some(first),
                "events at or below high_water were emitted before registration"
            );
        }
    }

    /// After the session closes (sequencer task gone), the live stream of an
    /// existing subscriber ends cleanly — and a late publish fails loudly
    /// instead of being silently lost.
    #[tokio::test]
    async fn session_close_ends_live_streams_and_fails_late_publish() {
        let sequencer = Sequencer::spawn(Ulid::generate(), SessionBus::default(), 0);
        let mut sub = sequencer.subscribe_since(None).await.unwrap();
        sequencer.publish(durable_candidate(0)).unwrap();

        drop(sequencer);
        let mut live = sub.live.take().unwrap();
        assert_eq!(live.recv().await.unwrap().seq, 0);
        // Draining to None proves the sequencer task (holding the only
        // subscriber senders) has exited.
        assert!(live.recv().await.is_none());
    }
}
