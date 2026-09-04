//! Minimal bidirectional WebSocket frames (todo 9).
//!
//! `GET /v1/sessions/:id/ws` upgrades to a WebSocket that wraps the SAME
//! event + command mechanics as SSE and POST /messages — one core, many
//! faces. Server -> client frames: `{type:"event", envelope}` |
//! `{type:"ack", request_id, command_id, accepted}` | `{type:"error",
//! request_id?, message}`. Client -> server: `{type:"message.send",
//! request_id, payload:{target_actor_id, mode, content}}`, mapped exactly
//! like POST /messages (same Command, same permit reserve, same
//! 503/400/downgrade). The event stream reuses the sequencer's atomic
//! subscribe-then-replay, so a WS client and an SSE client observe the same
//! durable history in seq order.

use std::collections::VecDeque;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::bus::event::{EventEnvelope, ROOT_ACTOR_ID};
use crate::bus::{ResumeOutcome, SubscribeReply};
use crate::error::ServerError;
use crate::server::routes::{
    build_command, error_response, parse_mode, route_message, MessageAccepted, PostMessageRequest,
};
use crate::server::session::{AppState, SessionHandle, SessionSlot};

/// Optional `?last_seq=<n>` resume cursor (mirrors SSE's `Last-Event-ID`).
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    #[serde(default)]
    last_seq: Option<u64>,
}

/// A client -> server frame.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame {
    #[serde(rename = "message.send")]
    MessageSend {
        request_id: String,
        payload: PostMessageRequest,
    },
}

/// A server -> client frame.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerFrame<'a> {
    Event {
        envelope: &'a EventEnvelope,
    },
    Ack {
        request_id: String,
        command_id: String,
        accepted: bool,
    },
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        message: String,
    },
}

/// `GET /v1/sessions/:id/ws`: upgrade to a bidirectional event + command
/// socket. An unknown id or a stopped/tombstoned session is refused BEFORE
/// the upgrade (404), so the socket only opens on a live session.
pub async fn get_ws(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let session_id: Ulid = match id.parse() {
        Ok(parsed) => parsed,
        Err(_) => return error_response(ServerError::SessionNotFound(id)),
    };
    let handle = match state.slot(session_id) {
        Some(SessionSlot::Live(handle)) => handle,
        Some(SessionSlot::Tombstone(_)) | None => {
            return error_response(ServerError::SessionNotFound(session_id.to_string()));
        }
    };
    let cursor = query.last_seq;
    upgrade.on_upgrade(move |socket| run_socket(socket, handle, session_id, cursor))
}

/// Drive one WebSocket: fan out sequenced events while accepting
/// `message.send` frames. Ends when either side closes or the event stream
/// terminates (session.end or a slow-subscriber disconnect).
async fn run_socket(
    mut socket: WebSocket,
    handle: SessionHandle,
    session_id: Ulid,
    cursor: Option<u64>,
) {
    let reply = match handle.sequencer.subscribe_since(cursor).await {
        Ok(reply) => reply,
        Err(_) => {
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    // A rejected cursor mirrors SSE's `resync` signal: send a client-visible
    // error frame telling it to reload, then close (never a silent close).
    if matches!(
        reply.replay,
        ResumeOutcome::Resync | ResumeOutcome::InvalidCursor
    ) {
        let _ = send_frame(
            &mut socket,
            &ServerFrame::Error {
                request_id: None,
                message: "resync: cursor out of range; reconnect without last_seq".to_string(),
            },
        )
        .await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    }
    let mut events = flatten_reply(reply);

    loop {
        tokio::select! {
            next = events.recv() => match next {
                Some(envelope) => {
                    if send_frame(&mut socket, &ServerFrame::Event { envelope: &envelope })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                None => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if handle_client_text(&mut socket, &handle, session_id, &text)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

/// One event source yielding the replay snapshot first, then the live
/// receiver — the same replay-then-live order the SSE handler uses.
struct EventStream {
    replay: VecDeque<EventEnvelope>,
    live: Option<tokio::sync::mpsc::Receiver<EventEnvelope>>,
}

impl EventStream {
    async fn recv(&mut self) -> Option<EventEnvelope> {
        if let Some(env) = self.replay.pop_front() {
            return Some(env);
        }
        match self.live.as_mut() {
            Some(rx) => rx.recv().await,
            None => None,
        }
    }
}

fn flatten_reply(reply: SubscribeReply) -> EventStream {
    let replay = match reply.replay {
        ResumeOutcome::Replay(events) => events.into(),
        ResumeOutcome::Resync | ResumeOutcome::InvalidCursor => VecDeque::new(),
    };
    EventStream {
        replay,
        live: reply.live,
    }
}

async fn send_frame(socket: &mut WebSocket, frame: &ServerFrame<'_>) -> Result<(), ()> {
    let text = match serde_json::to_string(frame) {
        Ok(text) => text,
        Err(_) => return Err(()),
    };
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

/// Parse + route ONE client text frame, replying with a single `ack` (success)
/// or `error` (malformed / 400 / 503 / stopping). A routing failure is a
/// frame, not a socket close, so the client can retry.
async fn handle_client_text(
    socket: &mut WebSocket,
    handle: &SessionHandle,
    session_id: Ulid,
    text: &str,
) -> Result<(), ()> {
    let frame: ClientFrame = match serde_json::from_str(text) {
        Ok(frame) => frame,
        Err(err) => {
            return send_frame(
                socket,
                &ServerFrame::Error {
                    request_id: None,
                    message: format!("malformed frame: {err}"),
                },
            )
            .await;
        }
    };
    let ClientFrame::MessageSend {
        request_id,
        payload,
    } = frame;

    match validate_and_route(handle, session_id, &payload).await {
        Ok(accepted) => {
            send_frame(
                socket,
                &ServerFrame::Ack {
                    request_id,
                    command_id: accepted.command_id,
                    accepted: accepted.accepted,
                },
            )
            .await
        }
        Err(message) => {
            send_frame(
                socket,
                &ServerFrame::Error {
                    request_id: Some(request_id),
                    message,
                },
            )
            .await
        }
    }
}

/// The SAME validation + routing POST /messages performs (one core, many
/// faces), adapted to a stringly error for the WS error frame.
async fn validate_and_route(
    handle: &SessionHandle,
    session_id: Ulid,
    payload: &PostMessageRequest,
) -> Result<MessageAccepted, String> {
    let mode = parse_mode(payload.mode.as_deref()).map_err(|e| e.to_string())?;
    if payload.target_actor_id != ROOT_ACTOR_ID {
        return Err(format!(
            "unknown target_actor_id: {} (this server routes to {ROOT_ACTOR_ID})",
            payload.target_actor_id
        ));
    }
    let command = build_command(session_id, payload, mode);
    command.validate_content().map_err(|e| e.to_string())?;
    route_message(handle, command)
        .await
        .map_err(|e| e.to_string())
}
