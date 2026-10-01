//! Phase 2 event bus: per-session sequencer, event envelopes, per-actor
//! inboxes, and the agent runtime task.
//!
//! Todo 3 (this module): the [`SessionBus`] is the concrete [`Emitter`] the
//! sequencer task drives after seq assignment. Per event it performs, in one
//! synchronous step inside the single sequencer task: ring-append (durable
//! kinds only) → fan-out to per-subscriber bounded mpsc queues. Subscribers
//! are best-effort: a full queue disconnects that subscriber (never blocks the
//! sequencer, never silently drops a durable event — the client reconnects and
//! replays via [`Sequencer::subscribe_since`]). The in-memory [`EventRing`]
//! retains the 11 durable kinds and tracks `last_evicted_durable_seq`, the
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
/// cursor sits one below the oldest retained seq (P1-6).
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

/// The atomic subscribe-then-replay handshake result (todo 8's SSE handler
/// consumes this): the replay vector is the ring snapshot taken in the SAME
/// synchronous step that registers `live`, and `live` yields only events
/// with `seq >` that snapshot's head — replay ∪ live is exactly-once.
#[derive(Debug)]
pub struct SubscribeReply {
    pub replay: ResumeOutcome,
    pub live: Option<mpsc::Receiver<EventEnvelope>>,
    /// The evicted-durable watermark at handshake time (the resume floor):
    /// durable seqs `<= resume_floor` are unreplayable. `None` while every
    /// durable event is retained. The SSE tombstone snapshot
    /// (reconnect after `final_seq`) consumes this to answer late cursors.
    pub resume_floor: Option<u64>,
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
        if self.events.len() == RING_CAP {
            if let Some(evicted) = self.events.pop_front() {
                self.last_evicted_durable_seq =
                    Some(self.last_evicted_durable_seq.unwrap_or(0).max(evicted.seq));
                debug_assert!(envelope.seq > evicted.seq);
            }
        }
        self.events.push_back(envelope.clone());
    }

    /// Durable events after `cursor`, in seq order; `None` means no cursor
    /// (O7): the full retained history INCLUDING seq 0.
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
    /// A bus whose ring is pre-filled with `retained` envelopes (a resumed
    /// session; empty for a fresh one): the durable kinds re-enter the ring
    /// in order, so a fresh (no-cursor) subscriber replays the whole past
    /// conversation. The envelopes are already seq-stamped; the ring's
    /// append (durable-only) is exactly the filter.
    pub fn new(retained: Vec<EventEnvelope>) -> Self {
        let mut bus = Self::default();
        for envelope in retained {
            bus.ring.append(&envelope);
        }
        bus
    }

    /// The evicted-durable watermark (the resume floor), `None` while every
    /// durable event is still retained.
    pub fn last_evicted_durable_seq(&self) -> Option<u64> {
        self.ring.last_evicted_durable_seq()
    }

    /// Resume contract (O7): resolves `cursor` against the ring and the
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
        SubscribeReply {
            replay,
            live,
            resume_floor: self.ring.last_evicted_durable_seq,
        }
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
}

#[cfg(test)]
mod tests {
    use ulid::Ulid;

    use super::event::{EventEnvelope, EventKind};
    use super::sequencer::{Candidate, Emitter, Sequencer};
    use super::*;

    fn durable_envelope(seq: u64) -> EventEnvelope {
        EventEnvelope::new(
            Ulid::new(),
            seq,
            EventKind::TurnStart,
            serde_json::json!({ "n": seq }),
        )
    }

    fn delta_envelope(seq: u64) -> EventEnvelope {
        EventEnvelope::new(
            Ulid::new(),
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

    /// (e) P1-6: cursor == oldest_retained - 1 replays GAPLESSLY because the
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

    /// (c) The five pinned resume cases (O7 cursor semantics).
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

    /// (d) A 10k llm.delta flood interleaved with one of EACH of the 11
    /// durable kinds: deltas never enter the ring, and ALL 11 durable kinds
    /// remain resumable in seq order (none lost, none invalidated). Deltas
    /// consume no ring slots, so `RING_CAP >= 11` suffices — the watermark
    /// stays None and every durable event survives the flood.
    #[test]
    fn ten_k_delta_flood_leaves_all_11_durable_kinds_resumable() {
        let durable_kinds: Vec<EventKind> = EventKind::ALL
            .iter()
            .copied()
            .filter(|k| k.is_durable())
            .collect();
        assert_eq!(durable_kinds.len(), 11);

        let mut bus = SessionBus::default();
        let mut seq: u64 = 0;
        let mut expected_durable_kinds: Vec<EventKind> = Vec::new();
        for kind in &durable_kinds {
            bus.emit(EventEnvelope::new(
                Ulid::new(),
                seq,
                *kind,
                serde_json::json!({ "n": seq }),
            ));
            expected_durable_kinds.push(*kind);
            seq += 1;
            for _ in 0..909 {
                bus.emit(delta_envelope(seq));
                seq += 1;
            }
        }
        // 11 * (1 + 909) = 10_010 events emitted; 9_990 are llm.delta.
        assert_eq!(seq, 10_010);
        assert_eq!(bus.last_evicted_durable_seq(), None);

        let reply = subscribe(&mut bus, None, seq);
        let replayed = match reply.replay {
            ResumeOutcome::Replay(events) => events,
            other => panic!("expected Replay, got {other:?}"),
        };
        assert_eq!(replayed.len(), 11, "exactly the 11 durable events");

        // All 11 kinds present, in emission (== seq) order.
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
        assert!(replayed
            .iter()
            .all(|e| EventKind::from_wire(&e.kind) != Some(EventKind::LlmDelta)));

        // A delta gap NEVER invalidates replay: a cursor sitting deep inside
        // the flood still replays the remaining durables (deltas are legal
        // gaps). Durables sit at seqs i*910: 0, 910, .., 9100.
        let reply = subscribe(&mut bus, Some(5_000), seq);
        assert_eq!(
            replay_seqs(&reply.replay),
            vec![5460, 6370, 7280, 8190, 9100]
        );
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

    /// (g) Atomic subscribe-then-replay, driven deterministically on the bus
    /// (the same synchronous mailbox step the sequencer task performs): the
    /// replay snapshot is taken and the live receiver registered with NO await
    /// between, so an event published after the handshake lands in live ONLY —
    /// exactly once, never replayed, never lost.
    #[tokio::test]
    async fn atomic_subscribe_then_replay_delivers_the_seam_event_exactly_once() {
        let mut bus = SessionBus::default();
        for seq in 0..3u64 {
            bus.emit(durable_envelope(seq));
        }

        // The handshake (one synchronous step inside the sequencer task).
        let reply = subscribe(&mut bus, Some(0), 3);
        assert_eq!(replay_seqs(&reply.replay), vec![1, 2]);

        // An event published EXACTLY after the handshake: live only.
        bus.emit(durable_envelope(3));
        let mut live = reply.live.expect("accepted cursor has a live stream");
        assert_eq!(live.recv().await.unwrap().seq, 3);
        // The replay vector above provably does not contain seq 3, and the
        // live stream provably does — delivered exactly once.
    }

    /// (g) Through the real sequencer: replay-then-live chains without a gap
    /// or a duplicate across the subscribe boundary.
    #[tokio::test]
    async fn sequencer_subscribe_replay_then_live_is_gap_and_dup_free() {
        let sequencer = Sequencer::spawn(Ulid::new(), SessionBus::default(), 0);
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

    /// After the session closes (sequencer task gone), the live stream of an
    /// existing subscriber ends cleanly — and a late publish fails loudly
    /// instead of being silently lost.
    #[tokio::test]
    async fn session_close_ends_live_streams_and_fails_late_publish() {
        let sequencer = Sequencer::spawn(Ulid::new(), SessionBus::default(), 0);
        let mut sub = sequencer.subscribe_since(None).await.unwrap();
        sequencer.publish(durable_candidate(0)).unwrap();

        drop(sequencer);
        let mut live = sub.live.take().unwrap();
        assert_eq!(live.recv().await.unwrap().seq, 0);
        // Draining to None proves the sequencer task (holding the only
        // subscriber senders) has exited.
        assert!(live.recv().await.is_none());
    }

    /// Manual QA observable (deterministic, same structure as the disconnect
    /// test): two subscribers on one session; the slow one fills to exactly
    /// QUEUE_CAP and is disconnected, the healthy one receives everything,
    /// and the reconnect replays gaplessly via `since(Some(k))`. Prints the
    /// per-subscriber seq sequences and the reconnect ResumeOutcome.
    #[test]
    fn manual_qa_two_subscribers_disconnect_then_resume() {
        let mut bus = SessionBus::default();
        let mut slow = subscribe(&mut bus, None, 0).live.take().unwrap();
        let mut healthy = subscribe(&mut bus, None, 0).live.take().unwrap();

        let total = (SUBSCRIBER_QUEUE_CAP + 3) as u64;
        let mut healthy_seqs = Vec::new();
        for seq in 0..total {
            bus.emit(durable_envelope(seq));
            while let Ok(envelope) = healthy.try_recv() {
                healthy_seqs.push(envelope.seq);
            }
        }
        while let Ok(envelope) = healthy.try_recv() {
            healthy_seqs.push(envelope.seq);
        }

        let mut slow_seqs = Vec::new();
        while let Ok(envelope) = slow.try_recv() {
            slow_seqs.push(envelope.seq);
        }
        assert!(slow.is_closed(), "slow disconnected on overflow");

        // Reconnect the slow client from its last observed seq on the SAME
        // live session: the overflow replays gaplessly.
        let resume_cursor = *slow_seqs.last().unwrap();
        let resume = subscribe(&mut bus, Some(resume_cursor), total);
        let resume_seqs = replay_seqs(&resume.replay);

        eprintln!(
            "manual QA: healthy subscriber seqs (count={}): {:?}",
            healthy_seqs.len(),
            edge(&healthy_seqs)
        );
        eprintln!(
            "manual QA: slow subscriber  seqs before disconnect (count={}): {:?}",
            slow_seqs.len(),
            edge(&slow_seqs)
        );
        eprintln!(
            "manual QA: slow DISCONNECTED after seq {resume_cursor} (queue full) — durable overflow retained, not dropped"
        );
        eprintln!(
            "manual QA: reconnect since(Some({resume_cursor})) → ResumeOutcome::Replay seqs {resume_seqs:?}"
        );
        eprintln!(
            "manual QA: union(replay, live) for the slow client is gapless {}..={}",
            resume_cursor + 1,
            total - 1
        );

        assert_eq!(healthy_seqs, (0..total).collect::<Vec<_>>());
        assert_eq!(
            slow_seqs,
            (0..SUBSCRIBER_QUEUE_CAP as u64).collect::<Vec<_>>()
        );
        assert_eq!(
            resume_seqs,
            (SUBSCRIBER_QUEUE_CAP as u64..total).collect::<Vec<_>>()
        );
    }

    /// First/last few elements of a long sequence for readable QA output.
    fn edge(seqs: &[u64]) -> Vec<u64> {
        if seqs.len() <= 8 {
            seqs.to_vec()
        } else {
            let mut shown = seqs[..4].to_vec();
            shown.push(u64::MAX); // "... " separator marker
            shown.extend_from_slice(&seqs[seqs.len() - 3..]);
            shown
        }
    }
}
