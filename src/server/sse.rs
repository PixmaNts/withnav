//! SSE event streaming with `id=seq` and `Last-Event-ID` resume (todo 8).
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
use crate::server::session::{AppState, SessionSlot};

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
        Some(SessionSlot::Tombstone(_)) => sse_terminated().into_response(),
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

/// A cleanly terminated stream: a stopped session (tombstone) or a closed
/// sequencer replays nothing and ends, so a reconnecting client never hangs
/// and never gets a 404 mid-race.
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
