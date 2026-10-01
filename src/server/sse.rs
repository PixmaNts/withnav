//! SSE event streaming with `id=seq` and `Last-Event-ID` resume.
//!
//! `GET /v1/sessions/:id/events` streams sequenced `EventEnvelope`s. The SSE
//! `id:` field is the event `seq`, so a reconnecting client sends
//! `Last-Event-ID: <seq>` and resumes gaplessly. The atomic subscribe-then-
//! replay handshake lives in the sequencer (`subscribe_since`): it returns
//! the ring replay through the current head plus a live receiver yielding
//! only later seqs, so nothing is lost or duplicated at the seam. A rejected
//! cursor (`Resync`/`InvalidCursor`) or a stopped session yields a single
//! typed control frame then ends — a per-connection signal, never published
//! back through the bus.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::stream::{self, Stream, StreamExt};
use std::convert::Infallible;
use ulid::Ulid;

use crate::bus::event::EventEnvelope;
use crate::bus::{ResumeOutcome, SubscribeReply};
use crate::error::ServerError;
use crate::server::routes::error_response;
use crate::server::session::{AppState, SessionSlot, Tombstone};

/// `GET /v1/sessions/:id/events`: subscribe to the session's event stream.
pub async fn get_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let session_id: Ulid = match id.parse() {
        Ok(parsed) => parsed,
        Err(_) => return error_response(ServerError::SessionNotFound(id)),
    };
    let cursor = parse_last_event_id(&headers);

    match state.slot(session_id) {
        None => error_response(ServerError::SessionNotFound(session_id.to_string())),
        Some(SessionSlot::Tombstone(tombstone)) => {
            tombstone_resume(&tombstone, cursor).into_response()
        }
        Some(SessionSlot::Live(handle)) => match handle.sequencer.subscribe_since(cursor).await {
            Ok(reply) => sse_from_reply(reply).into_response(),
            Err(_) => sse_terminated().into_response(),
        },
    }
}

/// Parse `Last-Event-ID` into a resume cursor. Absent or non-numeric is
/// `None` (full retained history incl. seq 0), NEVER coerced to `0`.
fn parse_last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

type EventStream = std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

/// Turn a subscribe reply into an SSE response: replay events first, then the
/// live receiver; a rejected cursor becomes a single `resync` control frame.
fn sse_from_reply(reply: SubscribeReply) -> impl IntoResponse {
    match reply.replay {
        ResumeOutcome::Replay(events) => {
            let replayed = stream::iter(
                events
                    .into_iter()
                    .map(|e| Ok::<_, Infallible>(envelope_event(&e))),
            );
            let live = match reply.live {
                Some(rx) => stream::unfold(rx, |mut rx| async move {
                    rx.recv()
                        .await
                        .map(|e| (Ok::<_, Infallible>(envelope_event(&e)), rx))
                })
                .left_stream(),
                None => stream::empty().right_stream(),
            };
            let body: EventStream = replayed.chain(live).boxed();
            Sse::new(body).keep_alive(KeepAlive::default())
        }
        ResumeOutcome::Resync | ResumeOutcome::InvalidCursor => {
            let frame = Ok(Event::default()
                .event("resync")
                .data("cursor out of range; reload"));
            let body = stream::once(async move { frame }).boxed();
            Sse::new(body).keep_alive(KeepAlive::default())
        }
    }
}

/// A cleanly terminated stream: a closed sequencer replays nothing and
/// ends, so a reconnecting client never hangs and never gets a 404 mid-race.
fn sse_terminated() -> impl IntoResponse {
    let body: EventStream = stream::empty().boxed();
    Sse::new(body).keep_alive(KeepAlive::default())
}

/// Serialize an envelope as an SSE frame with `id:` == its `seq`.
fn envelope_event(envelope: &EventEnvelope) -> Event {
    let event = Event::default().id(envelope.seq.to_string());
    match event.json_data(envelope) {
        Ok(event) => event,
        Err(_) => Event::default()
            .id(envelope.seq.to_string())
            .event("error")
            .data("event serialization failed"),
    }
}

/// Reconnect after `final_seq`: a stopped session is a tombstone that
/// retains its frozen durable history, so GET /events answers 200 with the
/// snapshot replayed through `final_seq` (session.end LAST) and then a clean
/// EOF — never a 404 mid-race, never a hang.
///
/// Cursor handling against the frozen snapshot mirrors the live contract:
/// absent -> full retained history; `Some(k)` -> retained seqs `> k`;
/// `k < resume_floor` (a durable event after `k` was evicted before the
/// snapshot froze) -> Resync; `k >= final_seq` (future) -> InvalidCursor.
fn tombstone_resume(tombstone: &Tombstone, cursor: Option<u64>) -> impl IntoResponse {
    let replay = tombstone_resume_outcome(tombstone, cursor);
    let reply = SubscribeReply {
        replay,
        live: None,
        resume_floor: tombstone.resume_floor,
    };
    sse_from_reply(reply)
}

/// The tombstone's replay decision, mirroring [`crate::bus::SessionBus`]'s
/// resume contract against the frozen snapshot.
fn tombstone_resume_outcome(tombstone: &Tombstone, cursor: Option<u64>) -> ResumeOutcome {
    let Some(envelopes) = tombstone.retained.as_deref() else {
        return ResumeOutcome::Resync;
    };
    match cursor {
        None => ResumeOutcome::Replay(envelopes.to_vec()),
        Some(k) if k >= tombstone.final_seq => ResumeOutcome::InvalidCursor,
        Some(k) => match tombstone.resume_floor {
            None => {
                ResumeOutcome::Replay(envelopes.iter().filter(|e| e.seq > k).cloned().collect())
            }
            Some(floor) if k >= floor => {
                ResumeOutcome::Replay(envelopes.iter().filter(|e| e.seq > k).cloned().collect())
            }
            Some(_) => ResumeOutcome::Resync,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        match tombstone_resume_outcome(&t, None) {
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
        match tombstone_resume_outcome(&t, Some(1)) {
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
            tombstone_resume_outcome(&t, Some(0)),
            ResumeOutcome::Resync
        ));
        assert!(matches!(
            tombstone_resume_outcome(&t, Some(1)),
            ResumeOutcome::Replay(_)
        ));
        assert!(matches!(
            tombstone_resume_outcome(&t, Some(2)),
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
            tombstone_resume_outcome(&t, Some(3)),
            ResumeOutcome::InvalidCursor
        ));
        assert!(matches!(
            tombstone_resume_outcome(&t, Some(99)),
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
        assert!(matches!(
            tombstone_resume_outcome(&t, None),
            ResumeOutcome::Resync
        ));
        assert!(matches!(
            tombstone_resume_outcome(&t, Some(2)),
            ResumeOutcome::Resync
        ));
    }
}
