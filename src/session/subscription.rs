//! Subscriptions behind the session service: one call yields the replay
//! decision and the live receiver, whether the session is live or stopped.
//! The sequencer never leaves the session module.
//!
//! - A live session captures its head atomically with a live receiver,
//!   replays the stored history through that head, then follows live events
//!   (each one above the head, exactly once). Stale `session.end` envelopes
//!   from an earlier stop of a resumed conversation are hidden. A failing
//!   history read falls back to the bus ring.
//! - A tombstone replays its retained history through `final_seq`, then
//!   ends: no live receiver.
//! - An unknown id is [`ServerError::SessionNotFound`].

use tokio::sync::mpsc;

use super::lifecycle::Tombstone;
use super::service::{Service, SessionId, SessionSlot};
use crate::bus::event::EventEnvelope;
use crate::bus::{LiveSubscription, ResumeOutcome};
use crate::error::ServerError;

/// The outcome of [`Service::subscribe`]: what to replay, then the live
/// stream. `live` is `None` once nothing more can arrive (a stopped
/// session, a rejected cursor, or a sequencer that already closed).
#[derive(Debug)]
pub struct Subscription {
    pub replay: ResumeOutcome,
    pub live: Option<mpsc::Receiver<EventEnvelope>>,
}

impl Service {
    /// Subscribes to a session's events from `cursor` (`None` = the whole
    /// retained history, seq 0 included).
    pub async fn subscribe(
        &self,
        id: SessionId,
        cursor: Option<u64>,
    ) -> Result<Subscription, ServerError> {
        match self.slot(id) {
            None => Err(ServerError::SessionNotFound(id.to_string())),
            Some(SessionSlot::Tombstone(tombstone)) => Ok(Subscription {
                replay: tombstone_replay(&tombstone, cursor),
                live: None,
            }),
            Some(SessionSlot::Live(handle)) => {
                let live = match handle.sequencer.subscribe_live().await {
                    Ok(live) => live,
                    // A closed sequencer replays nothing and ends, so a
                    // reconnecting client never hangs and never sees a 404
                    // mid-race.
                    Err(_) => return Ok(Subscription::ended()),
                };
                match self.replay_live(id, cursor, live).await {
                    Ok(subscription) => Ok(subscription),
                    Err(err) => {
                        tracing::error!(
                            session_id = %id,
                            error = %err,
                            "history replay failed; falling back to the ring"
                        );
                        Ok(match handle.sequencer.subscribe_since(cursor).await {
                            Ok(reply) => Subscription {
                                replay: reply.replay,
                                live: reply.live,
                            },
                            Err(_) => Subscription::ended(),
                        })
                    }
                }
            }
        }
    }

    /// The live-session subscription over the stored history. The cursor is
    /// decided first, from the captured head `H` alone: `Some(k)` with
    /// `k > H`, or with no `H` yet, is `InvalidCursor` (no live receiver, no
    /// store read). Otherwise the store is read once, through `H`: not at
    /// all when no event was emitted yet or the cursor already sits at `H`.
    /// `llm.delta` is never stored, and stale `session.end` envelopes (from
    /// an earlier stop of a resumed conversation) are hidden. Every event
    /// above `H` reaches the live receiver, so history plus receiver cover
    /// each durable event exactly once. On a failed flush or read the live
    /// receiver is dropped before the error is returned, so the caller's
    /// fallback does not leave two subscribers for one client.
    async fn replay_live(
        &self,
        id: SessionId,
        cursor: Option<u64>,
        live: LiveSubscription,
    ) -> Result<Subscription, String> {
        let LiveSubscription {
            high_water,
            receiver,
        } = live;
        let through = match (cursor, high_water) {
            (Some(_), None) => None,
            (Some(k), Some(h)) if k > h => None,
            (_, head) => Some(head),
        };
        let Some(head) = through else {
            drop(receiver);
            return Ok(Subscription {
                replay: ResumeOutcome::InvalidCursor,
                live: None,
            });
        };
        let nothing_to_read = match (cursor, head) {
            (_, None) => true,
            (Some(k), Some(h)) => k == h,
            (None, Some(_)) => false,
        };
        if nothing_to_read {
            return Ok(Subscription {
                replay: ResumeOutcome::Replay(Vec::new()),
                live: Some(receiver),
            });
        }
        let stored = async {
            self.history().flush().await?;
            self.history()
                .replay(id, cursor, head.unwrap_or_default())
                .await
        }
        .await;
        match stored {
            Ok(mut events) => {
                events.retain(|event| event.kind != "session.end");
                Ok(Subscription {
                    replay: ResumeOutcome::Replay(events),
                    live: Some(receiver),
                })
            }
            Err(err) => {
                drop(receiver);
                Err(err)
            }
        }
    }
}

impl Subscription {
    /// Nothing to replay and nothing more to come.
    fn ended() -> Self {
        Self {
            replay: ResumeOutcome::Replay(Vec::new()),
            live: None,
        }
    }
}

/// The tombstone's replay decision against its frozen snapshot, mirroring
/// the live resume contract: absent cursor -> full retained history;
/// `Some(k)` -> retained seqs `> k`; `k < resume_floor` (a durable event
/// after `k` was evicted before the snapshot froze) -> Resync;
/// `k >= final_seq` (future) -> InvalidCursor.
fn tombstone_replay(tombstone: &Tombstone, cursor: Option<u64>) -> ResumeOutcome {
    let Some(envelopes) = tombstone.retained.as_deref() else {
        return ResumeOutcome::Resync;
    };
    match cursor {
        None => ResumeOutcome::Replay(envelopes.to_vec()),
        Some(k) if k >= tombstone.final_seq => ResumeOutcome::InvalidCursor,
        Some(k) => match tombstone.resume_floor {
            Some(floor) if k < floor => ResumeOutcome::Resync,
            _ => ResumeOutcome::Replay(envelopes.iter().filter(|e| e.seq > k).cloned().collect()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ulid::Ulid;

    fn envelope(seq: u64) -> EventEnvelope {
        EventEnvelope::new(
            Ulid::new(),
            seq,
            crate::bus::event::EventKind::TurnStart,
            serde_json::json!({ "n": seq }),
        )
    }

    fn tombstone(seq_count: u64) -> Tombstone {
        let mut retained: Vec<EventEnvelope> = (0..seq_count).map(envelope).collect();
        let mut end = envelope(seq_count);
        end.kind = "session.end".to_string();
        end.payload = serde_json::json!({ "reason": "stop", "final_seq": seq_count });
        retained.push(end);
        Tombstone {
            final_seq: seq_count,
            workspace: std::path::PathBuf::from("/w"),
            model: "m".to_string(),
            retained: Some(retained),
            resume_floor: None,
        }
    }

    // Given a stopped session's frozen snapshot
    // When a client reconnects with NO Last-Event-ID
    // Then the full retained history replays (0..=final_seq)
    #[test]
    fn tombstone_full_replay_includes_seq_zero() {
        let t = tombstone(3);
        match tombstone_replay(&t, None) {
            ResumeOutcome::Replay(events) => {
                let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
                assert_eq!(seqs, vec![0, 1, 2, 3]);
            }
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    // Given a stopped session's frozen snapshot
    // When a client resumes from cursor k < final_seq
    // Then only retained seqs > k replay (same after-k contract as live)
    #[test]
    fn tombstone_cursor_replays_only_after_k() {
        let t = tombstone(4);
        match tombstone_replay(&t, Some(1)) {
            ResumeOutcome::Replay(events) => {
                let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
                assert_eq!(seqs, vec![2, 3, 4]);
            }
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    // Given a snapshot that froze AFTER ring eviction (resume_floor set)
    // When a cursor sits BELOW the floor
    // Then Resync — a durable event after `k` was evicted before the freeze.
    // k == floor still replays: every durable seq > floor is retained, the
    // same resume-floor watermark contract as the live bus.
    #[test]
    fn tombstone_cursor_below_floor_is_resync() {
        let mut t = tombstone(3);
        t.resume_floor = Some(1);
        assert!(matches!(
            tombstone_replay(&t, Some(0)),
            ResumeOutcome::Resync
        ));
        assert!(matches!(
            tombstone_replay(&t, Some(1)),
            ResumeOutcome::Replay(_)
        ));
        assert!(matches!(
            tombstone_replay(&t, Some(2)),
            ResumeOutcome::Replay(_)
        ));
    }

    // Given a stopped session
    // When the cursor is at or beyond final_seq (future/stale)
    // Then InvalidCursor — never a silent empty "caught up" replay
    #[test]
    fn tombstone_future_cursor_is_invalid_cursor() {
        let t = tombstone(3);
        assert!(matches!(
            tombstone_replay(&t, Some(3)),
            ResumeOutcome::InvalidCursor
        ));
        assert!(matches!(
            tombstone_replay(&t, Some(99)),
            ResumeOutcome::InvalidCursor
        ));
    }

    // Given the ring evicted before the close (snapshot refused)
    // When any client reconnects to the tombstone
    // Then Resync, never a partial history masquerading as complete
    #[test]
    fn tombstone_without_snapshot_resyncs() {
        let mut t = tombstone(3);
        t.retained = None;
        assert!(matches!(tombstone_replay(&t, None), ResumeOutcome::Resync));
        assert!(matches!(
            tombstone_replay(&t, Some(2)),
            ResumeOutcome::Resync
        ));
    }
}
