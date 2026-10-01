//! Subscriptions behind the session service: one call yields the replay
//! decision and the live receiver, whether the session is live or stopped.
//! The sequencer never leaves the session module.
//!
//! - A live session captures its head atomically with a live receiver,
//!   replays the stored history through that head, then follows live events
//!   (each one above the head, exactly once). Stale `session.end` envelopes
//!   from an earlier stop of a resumed conversation are hidden. A failing
//!   history read falls back to the bus ring, which is a bounded,
//!   non-authoritative cache (after a resume it holds only the events since).
//! - A conversation with no registry entry (stopped, or only stored) replays its stored history through
//!   `last_seq`, then ends, with the same cursor rules as a live session.
//! - An unknown id is [`ServerError::SessionNotFound`].

use tokio::sync::mpsc;

use super::service::{Service, SessionId};
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
        match self.handle(id) {
            None => self.stored_subscription(id, cursor).await,
            Some(handle) => {
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

    /// A conversation that only the store holds: its stored history through
    /// `last_seq` with the live-session cursor rules (absent = everything,
    /// `k <= last_seq` = after `k`, `k > last_seq` = invalid), then the
    /// stream ends. Unknown ids are not found.
    async fn stored_subscription(
        &self,
        id: SessionId,
        cursor: Option<u64>,
    ) -> Result<Subscription, ServerError> {
        let history = self.history();
        let meta = history
            .meta(id)
            .await
            .map_err(ServerError::History)?
            .ok_or_else(|| ServerError::SessionNotFound(id.to_string()))?;
        if cursor.is_some_and(|k| k > meta.last_seq) {
            return Ok(Subscription {
                replay: ResumeOutcome::InvalidCursor,
                live: None,
            });
        }
        let events = history
            .replay(id, cursor, meta.last_seq)
            .await
            .map_err(ServerError::History)?;
        Ok(Subscription {
            replay: ResumeOutcome::Replay(events),
            live: None,
        })
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
