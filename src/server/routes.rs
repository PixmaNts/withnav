//! Axum HTTP surface (todos 6-7): health, session create/get, message and
//! stop routing. SSE is todo 8, WS todo 9.
//!
//! Response bodies are FROZEN (ticket 04 Q3):
//! - `GET  /v1/health`        -> `{status:"ok"}`
//! - `POST /v1/sessions`      -> `{session_id, root_actor_id:"agent:main", actor_state:"idle"}`
//! - `GET  /v1/sessions/:id`  -> `{session_id, actor_state, workspace, model, current_seq}`
//! - `POST /v1/sessions/:id/messages` -> `{command_id, accepted:true, actor_state, notice?}`
//!   (frozen error bodies: 404 unknown id / 409 stopping / 503 `{"accepted":false,"reason":"inbox_full"}`
//!   / 400 non-Text). All errors share the `{"error": ...}` shape.
//! - `POST /v1/sessions/:id/stop` -> `{accepted:true, actor_state:"stopped"}`
//!
//! allow: SIZE_OK — ticket 05's approved module map fixes the file set
//! (mod/routes/sse/websocket/session); every HTTP route lives here.

use std::net::SocketAddr;
use std::time::SystemTime;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use ulid::Ulid;

use crate::bus::command::{Command, ContentBlock, DeliveryMode};
use crate::bus::event::ROOT_ACTOR_ID;
use crate::bus::sequencer::PendingCommand;
use crate::error::ServerError;
use crate::server::session::{AppState, ServerConfig, SessionHandle};

/// Frozen health body (SPEC §20.2).
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct HealthBody {
    pub status: &'static str,
}

/// Frozen create-session request (ticket 04 Q3).
#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub workspace: String,
    #[serde(default)]
    pub model: Option<String>,
}

/// Frozen create-session response (ticket 04 Q3).
#[derive(Debug, Serialize, PartialEq)]
pub struct CreateSessionResponse {
    pub session_id: String,
    pub root_actor_id: &'static str,
    pub actor_state: &'static str,
}

/// `POST /v1/sessions/:id/messages` request (SPEC §20.2). The server — never
/// the client — assigns `source_actor_id` (`user:http`), so the wire carries
/// no impersonatable sender field.
#[derive(Debug, Deserialize)]
pub struct PostMessageRequest {
    pub target_actor_id: String,
    #[serde(default)]
    pub mode: Option<String>,
    pub content: Vec<WireContentBlock>,
}

/// The wire form of a content block (PLAN §7.5): only Text is accepted this
/// phase; any other `type` is a 400.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
    },
}

impl From<&WireContentBlock> for ContentBlock {
    fn from(wire: &WireContentBlock) -> Self {
        match wire {
            WireContentBlock::Text { text } => ContentBlock::Text { text: text.clone() },
            WireContentBlock::Thinking { text } => ContentBlock::Thinking { text: text.clone() },
            WireContentBlock::ToolUse { id, name, args } => ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                args: args.clone(),
            },
            WireContentBlock::ToolResult {
                tool_call_id,
                content,
            } => ContentBlock::ToolResult {
                tool_call_id: tool_call_id.clone(),
                content: content.clone(),
            },
        }
    }
}

/// The frozen message ack (ticket 04 Q3): `notice` carries the visible
/// steer/interrupt downgrade and is absent for next-turn.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct MessageAccepted {
    pub command_id: String,
    pub accepted: bool,
    pub actor_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// The frozen 503 body (wayfinder ticket 02).
#[derive(Debug, Serialize, PartialEq)]
pub struct InboxFullResponse {
    pub accepted: bool,
    pub reason: &'static str,
}

/// The frozen stop ack (wayfinder ticket 02).
#[derive(Debug, Serialize, PartialEq)]
pub struct StopAccepted {
    pub accepted: bool,
    pub actor_state: &'static str,
}

/// The Axum router for this slice's routes. Tests mount it directly via
/// `tower::ServiceExt::oneshot`.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/{id}", get(get_session))
        .route("/v1/sessions/{id}/messages", post(post_message))
        .route("/v1/sessions/{id}/stop", post(stop_session))
        .route(
            "/v1/sessions/{id}/events",
            get(crate::server::sse::get_events),
        )
        .route(
            "/v1/sessions/{id}/ws",
            any(crate::server::websocket::get_ws),
        )
        .with_state(state)
}

/// Loopback bind + graceful shutdown (SIGINT/SIGTERM -> every active session
/// gets its sequencer fence fired, emitting `session.end`, then the server
/// drains and exits).
pub async fn serve(config: ServerConfig) -> Result<(), ServerError> {
    config.validate()?;
    let addr: SocketAddr = config.bind;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_state_on(AppState::new(config), listener).await
}

/// The serve body over an already-bound listener with a fresh shared state —
/// the testable seam for the real-socket acceptance test (ephemeral
/// `127.0.0.1:0` bind).
pub async fn serve_on(
    config: ServerConfig,
    listener: tokio::net::TcpListener,
) -> Result<(), ServerError> {
    serve_state_on(AppState::new(config), listener).await
}

/// The serve body over an already-bound listener and a CALLER-SUPPLIED
/// state: the real graceful-shutdown path (`with_graceful_shutdown` +
/// `shutdown_signal`) runs against the same registry the caller holds, so a
/// test can create sessions and observe their events through it.
pub async fn serve_state_on(
    state: AppState,
    listener: tokio::net::TcpListener,
) -> Result<(), ServerError> {
    let app = app(state.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(state))
        .await?;
    Ok(())
}

/// SIGINT/SIGTERM: close all sessions (each emits `session.end{final_seq}`,
/// tombstoning itself) and only then release the server to finish draining.
async fn shutdown_signal(state: AppState) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    state.close_all().await;
}

async fn health() -> Json<HealthBody> {
    Json(HealthBody { status: "ok" })
}

async fn create_session(
    State(state): State<AppState>,
    body: Result<Json<CreateSessionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(parsed) => parsed,
        Err(rejection) => {
            return error_response(ServerError::MalformedBody(rejection.body_text()));
        }
    };
    let session_id = state.create_session(
        std::path::Path::new(&request.workspace),
        request.model.as_deref(),
    );
    match session_id {
        Ok(id) => (
            StatusCode::OK,
            Json(CreateSessionResponse {
                session_id: id.to_string(),
                root_actor_id: ROOT_ACTOR_ID,
                actor_state: "idle",
            }),
        )
            .into_response(),
        Err(err) => error_response(err),
    }
}

async fn get_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let parsed: Ulid = match id.parse() {
        Ok(parsed) => parsed,
        Err(_) => return error_response(ServerError::SessionNotFound(id)),
    };
    match state.session_summary(parsed).await {
        Ok(summary) => Json(summary).into_response(),
        Err(err) => error_response(err),
    }
}

/// The server-assigned sender for HTTP-submitted messages: clients can never
/// impersonate another actor by choosing it themselves (SPEC §11.7).
const HTTP_USER_ACTOR_ID: &str = "user:http";

/// Builds the Command for a posted message with a server-assigned sender.
pub(crate) fn build_command(
    session_id: Ulid,
    request: &PostMessageRequest,
    mode: DeliveryMode,
) -> Command {
    Command {
        command_id: Ulid::new(),
        session_id,
        source_actor_id: HTTP_USER_ACTOR_ID.to_string(),
        target_actor_id: ROOT_ACTOR_ID.to_string(),
        mode,
        content: request.content.iter().map(ContentBlock::from).collect(),
        created_at: SystemTime::now(),
    }
}

pub(crate) fn parse_mode(mode: Option<&str>) -> Result<DeliveryMode, ServerError> {
    match mode.unwrap_or("next-turn") {
        "next-turn" => Ok(DeliveryMode::NextTurn),
        "steer" => Ok(DeliveryMode::Steer),
        "interrupt" => Ok(DeliveryMode::Interrupt),
        other => Err(ServerError::MalformedBody(format!("unknown mode: {other}"))),
    }
}

/// `POST /v1/sessions/:id/messages` (todo 7): validate the session and the
/// content, sequence `message.received` BEFORE the inbox commit
/// (event-before-action), and ack with the frozen body — never blocking on
/// agent progress. A full inbox reserves the permit back and answers
/// `503 {accepted:false, reason:"inbox_full"}`.
async fn post_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<PostMessageRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(parsed) => parsed,
        Err(rejection) => {
            return error_response(ServerError::MalformedBody(rejection.body_text()));
        }
    };
    let session_id: Ulid = match id.parse() {
        Ok(parsed) => parsed,
        Err(_) => return error_response(ServerError::SessionNotFound(id)),
    };
    // Clone the handle and release the registry guard BEFORE any await.
    let handle = match state.routable_handle(session_id) {
        Ok(handle) => handle,
        Err(err) => return error_response(err),
    };
    let mode = match parse_mode(request.mode.as_deref()) {
        Ok(mode) => mode,
        Err(err) => return error_response(err),
    };
    // One root actor this slice: the request names the target, the server
    // pins it to the session's agent (a foreign target is a malformed ask).
    if request.target_actor_id != ROOT_ACTOR_ID {
        return error_response(ServerError::MalformedBody(format!(
            "unknown target_actor_id: {} (this server routes to {ROOT_ACTOR_ID})",
            request.target_actor_id
        )));
    }
    let command = build_command(session_id, &request, mode);
    if let Err(err) = command.validate_content() {
        return error_response(err);
    }
    match route_message(&handle, command).await {
        Ok(accepted) => Json(accepted).into_response(),
        Err(err) => error_response(err),
    }
}

/// The event-before-action route for ONE validated command: reserve an inbox
/// permit up front (a full inbox is a 503 BEFORE the event is sequenced, so
/// there is nothing to un-publish — the rejected permit releases its slot),
/// sequence `message.received` via `accept_command`, commit the command into
/// the reserved permit, ack.
pub(crate) async fn route_message(
    handle: &SessionHandle,
    command: Command,
) -> Result<MessageAccepted, ServerError> {
    let permit = match handle.inbox_tx.clone().try_reserve_owned() {
        Ok(permit) => permit,
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            return Err(ServerError::InboxFull);
        }
        Err(_) => {
            return Err(ServerError::SessionStopping);
        }
    };
    let command_id = command.command_id;
    let actor_state = handle.actor_state.clone();
    let notice = command.downgrade_notice().map(|n| n.to_string());
    let pending = PendingCommand {
        command_id,
        content: command.text_content(),
        source: command.source_actor_id.clone(),
        target: command.target_actor_id.clone(),
        mode: command.mode.as_str().to_string(),
    };
    let committed = command.clone();
    handle
        .sequencer
        .accept_command(
            pending,
            Box::new(move || {
                permit.send(committed);
            }),
        )
        .await
        .map_err(map_bus_error)?;
    Ok(MessageAccepted {
        command_id: command_id.to_string(),
        accepted: true,
        actor_state,
        notice,
    })
}

/// Map a routing failure onto the typed server error: full inbox stays 503;
/// a sequencer that closed under us means the session is stopping.
fn map_bus_error(err: crate::error::BusError) -> ServerError {
    match err {
        crate::error::BusError::InboxFull => ServerError::InboxFull,
        crate::error::BusError::SequencerClosed
        | crate::error::BusError::SessionNotFound(_)
        | crate::error::BusError::SeqSpaceExhausted => ServerError::SessionStopping,
    }
}

async fn stop_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let parsed: Ulid = match id.parse() {
        Ok(parsed) => parsed,
        Err(_) => return error_response(ServerError::SessionNotFound(id)),
    };
    match state.close_session(parsed).await {
        Ok(()) => Json(StopAccepted {
            accepted: true,
            actor_state: "stopped",
        })
        .into_response(),
        Err(err) => error_response(err),
    }
}

/// The single structured-error funnel: every failure is a typed
/// [`ServerError`] mapped onto a status code + JSON body. The full-inbox
/// rejection carries the FROZEN `{accepted:false, reason:"inbox_full"}`
/// body (wayfinder ticket 02); every other failure shares `{"error": ...}`.
pub(crate) fn error_response(err: ServerError) -> Response {
    let status = match &err {
        ServerError::SessionNotFound(_) => StatusCode::NOT_FOUND,
        ServerError::SessionStopping => StatusCode::CONFLICT,
        ServerError::NonLoopbackBind(_) | ServerError::Bind(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ServerError::InboxFull => StatusCode::SERVICE_UNAVAILABLE,
        ServerError::NonTextContent | ServerError::MalformedBody(_) => StatusCode::BAD_REQUEST,
        ServerError::Config(_) | ServerError::Model(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if matches!(err, ServerError::InboxFull) {
        return (
            status,
            Json(json!({ "accepted": false, "reason": "inbox_full" })),
        )
            .into_response();
    }
    (status, Json(json!({ "error": err.to_string() }))).into_response()
}

/// SIGTERM delivery for the graceful-shutdown acceptance test: the test
/// process IS the server process, so raising SIGTERM on itself exercises the
/// production `SignalKind::terminate` branch of `shutdown_signal`.
#[doc(hidden)]
pub fn send_test_sigterm() {
    let pid = std::process::id();
    let _ = std::process::Command::new("kill")
        .args(["-s", "TERM", &pid.to_string()])
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::session::ServerConfig;
    use axum::body::Body;
    use axum::http::StatusCode;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        AppState::new(ServerConfig {
            bind: "127.0.0.1:0".parse().expect("loopback bind"),
            models_path: None,
            config_dir: std::env::temp_dir(),
            allow_non_loopback: false,
        })
    }

    // Given the mounted router
    // When GET /v1/health
    // Then 200 with the frozen {status:"ok"} body
    #[tokio::test]
    async fn health_returns_frozen_ok_body() {
        let response = app(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/health")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value, serde_json::json!({ "status": "ok" }));
    }

    // Given a session POST with a malformed body
    // When the handler runs
    // Then 400 with the shared {"error": ...} shape (not a panic/plain text)
    #[tokio::test]
    async fn malformed_create_body_is_a_structured_400() {
        let response = app(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from("{not json"))
                    .expect("request"),
            )
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert!(
            value.get("error").is_some(),
            "structured 404/400 shape: {value}"
        );
    }
}
