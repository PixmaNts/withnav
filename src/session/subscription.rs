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
//! - A conversation with no registry entry (stopped, or only stored) replays
//!   its stored history through `last_seq`, then ends, with the same cursor
//!   rules as a live session. Stale `session.end` envelopes are hidden there
//!   too; the replay ends with the conversation's final `session.end` so an
//!   offline client still receives its terminal signal.
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
                        // The ring starts empty on resume, so it cannot hold
                        // a stale `session.end` from an earlier stop: the
                        // only end it can hold is the one this stop just
                        // published, which is this subscriber's correct
                        // terminal signal. Replay it unchanged.
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
            replay: ResumeOutcome::Replay(replay_history(events, true)),
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
            Ok(events) => Ok(Subscription {
                replay: ResumeOutcome::Replay(replay_history(events, false)),
                live: Some(receiver),
            }),
            Err(err) => {
                drop(receiver);
                Err(err)
            }
        }
    }
}

/// Filters a stored replay so a client that treats `session.end` as terminal
/// sees each conversation once, from its first event to its last. A resume
/// continues the same sequence space, so every stop of a conversation left a
/// `session.end` mid-history in the store; those stale ends would otherwise
/// truncate the replay at the first one (`webui/src/api/sse.ts` and
/// `crates/withnav-client/src/stream.rs` both stop reading on `session.end`).
///
/// A live session keeps none: stopping it later emits a fresh end on the live
/// stream. A stopped conversation keeps one, only when the replay's last
/// event already is one, so an offline client still receives its terminal
/// signal after the whole history. The store keeps every envelope; this
/// filter applies at subscription time only.
fn replay_history(events: Vec<EventEnvelope>, offline: bool) -> Vec<EventEnvelope> {
    let terminal = if offline {
        events
            .last()
            .filter(|event| event.kind == "session.end")
            .cloned()
    } else {
        None
    };
    let mut events: Vec<EventEnvelope> = events
        .into_iter()
        .filter(|event| event.kind != "session.end")
        .collect();
    if let Some(end) = terminal {
        events.push(end);
    }
    events
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::event::EventKind;
    use serde_json::json;
    use ulid::Ulid;

    fn envelope(seq: u64, kind: EventKind) -> EventEnvelope {
        EventEnvelope::new(Ulid::generate(), seq, kind, json!({}))
    }

    fn kinds(events: &[EventEnvelope]) -> Vec<&str> {
        events.iter().map(|event| event.kind.as_str()).collect()
    }

    // A live replay drops every `session.end`: a resumed session's stale ends
    // never reach a client, and the fresh one arrives on the live stream.
    #[test]
    fn live_replay_drops_every_session_end() {
        let events = vec![
            envelope(1, EventKind::SessionStart),
            envelope(2, EventKind::MessageReceived),
            envelope(3, EventKind::SessionEnd),
            envelope(4, EventKind::MessageReceived),
            envelope(5, EventKind::TurnEnd),
        ];
        assert_eq!(
            kinds(&replay_history(events, false)),
            [
                "session.start",
                "message.received",
                "message.received",
                "turn.end"
            ]
        );
    }

    // A stopped conversation keeps exactly one end, in last position, after
    // the full history: clients end playback at the conversation's final
    // event, not at its first stop.
    #[test]
    fn stored_replay_keeps_only_the_trailing_session_end() {
        let events = vec![
            envelope(1, EventKind::SessionStart),
            envelope(2, EventKind::MessageReceived),
            envelope(3, EventKind::SessionEnd),
            envelope(4, EventKind::MessageReceived),
            envelope(5, EventKind::SessionEnd),
        ];
        assert_eq!(
            kinds(&replay_history(events, true)),
            [
                "session.start",
                "message.received",
                "message.received",
                "session.end"
            ]
        );
    }

    // A stored replay whose last event is not an end invents none: the
    // conversation ends on its last stored event (e.g. the process died
    // mid-turn after an earlier stop).
    #[test]
    fn stored_replay_without_a_trailing_end_invents_none() {
        let events = vec![
            envelope(1, EventKind::SessionStart),
            envelope(2, EventKind::SessionEnd),
            envelope(3, EventKind::MessageReceived),
        ];
        assert_eq!(
            kinds(&replay_history(events, true)),
            ["session.start", "message.received"]
        );
    }

    #[test]
    fn empty_replay_stays_empty() {
        assert!(replay_history(Vec::new(), true).is_empty());
        assert!(replay_history(Vec::new(), false).is_empty());
    }
}
