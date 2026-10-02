//! Axum HTTP surface health, session create/get, message and
//! stop routing. SSE streaming lives in `server::sse`.
//!
//! Response bodies are FROZEN:
//! - `GET  /v1/health`        -> `{status:"ok"}`
//! - `GET  /v1/models?workspace=` -> `{models:[{alias, model, context_window?, enabled?, disabled_reason?}], default?}`
//! - `POST /v1/sessions`      -> `{session_id, root_actor_id:"agent:main", actor_state:"idle"}`
//!   (422 `{"error", "reason":"no_context_window"}` for a chat alias with no known window)
//! - `GET  /v1/sessions/:id`  -> `{session_id, actor_state, workspace, model, current_seq}`
//! - `POST /v1/sessions/:id/messages` -> `{command_id, accepted:true, actor_state, notice?}`
//!   (frozen error bodies: 404 unknown id / 409 stopping / 503 `{"accepted":false,"reason":"inbox_full"}`
//!   / 400 non-Text). All errors share the `{"error": ...}` shape.
//!   A stopped conversation (stopped, or stored only) answers 409
//!   `{"error", "reason":"resume_required"}`: resume it first.
//! - `POST /v1/sessions/:id/stop` -> `{accepted:true, actor_state:"stopped"}`

use std::net::SocketAddr;
use std::time::SystemTime;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use ulid::Ulid;

use crate::bus::command::{Command, ContentBlock, DeliveryMode};
use crate::bus::event::ROOT_ACTOR_ID;
use crate::error::ServerError;
use crate::server::session::{AppState, ServerConfig};

/// Frozen health body.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct HealthBody {
    pub status: &'static str,
}

/// Frozen create-session request.
#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub workspace: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    /// Optional id of a past conversation to resume (same id returned; the
    /// conversation continues with its stored events and transcript).
    #[serde(default)]
    pub resume: Option<String>,
}

/// Frozen create-session response.
#[derive(Debug, Serialize, PartialEq)]
pub struct CreateSessionResponse {
    pub session_id: String,
    pub root_actor_id: &'static str,
    pub actor_state: &'static str,
}

/// `POST /v1/sessions/:id/messages` request. The server — never
/// the client — assigns `source_actor_id` (`user:http`), so the wire carries
/// no impersonatable sender field.
#[derive(Debug, Deserialize)]
pub struct PostMessageRequest {
    pub target_actor_id: String,
    #[serde(default)]
    pub mode: Option<String>,
    pub content: Vec<ContentBlock>,
}

/// The frozen 503 body.
#[derive(Debug, Serialize, PartialEq)]
pub struct InboxFullResponse {
    pub accepted: bool,
    pub reason: &'static str,
}

/// The frozen stop ack.
#[derive(Debug, Serialize, PartialEq)]
pub struct StopAccepted {
    pub accepted: bool,
    pub actor_state: &'static str,
}

/// The Axum router for this slice's routes. Tests mount it directly via
/// `tower::ServiceExt::oneshot`.
pub fn app(state: AppState) -> Router {
    let token = state.http.bearer_token.clone();
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/v1/sessions", get(list_sessions).post(create_session))
        .route("/v1/sessions/{id}", get(get_session).delete(delete_session))
        .route("/v1/sessions/{id}/messages", post(post_message))
        .route("/v1/sessions/{id}/stop", post(stop_session))
        .route(
            "/v1/sessions/{id}/events",
            get(crate::server::sse::get_events),
        )
        .with_state(state)
        .layer(axum::middleware::from_fn(
            move |request: axum::http::Request<axum::body::Body>, next: axum::middleware::Next| {
                let token = token.clone();
                async move {
                    if request.uri().path() != "/v1/health" {
                        if let Some(token) = token {
                            use axum::http::header::AUTHORIZATION;
                            let expected = format!("Bearer {token}");
                            if request
                                .headers()
                                .get(AUTHORIZATION)
                                .and_then(|v| v.to_str().ok())
                                != Some(&expected)
                            {
                                return StatusCode::UNAUTHORIZED.into_response();
                            }
                        }
                    }
                    next.run(request).await
                }
            },
        ))
}

/// Loopback bind + graceful shutdown (SIGINT/SIGTERM -> every active session
/// gets its sequencer fence fired, emitting `session.end`, then the server
/// drains and exits).
pub async fn serve(
    config: ServerConfig,
    hooks: std::sync::Arc<dyn crate::session::SessionHooks>,
) -> Result<(), ServerError> {
    config.validate()?;
    let addr: SocketAddr = config.bind;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_state_on(AppState::with_hooks(config, hooks)?, listener).await
}

/// The serve body over an already-bound listener with a fresh shared state —
/// the testable seam for the real-socket acceptance test (ephemeral
/// `127.0.0.1:0` bind).
pub async fn serve_on(
    config: ServerConfig,
    listener: tokio::net::TcpListener,
) -> Result<(), ServerError> {
    serve_state_on(AppState::new(config)?, listener).await
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

/// Serve until an EXTERNAL shutdown future resolves (the navigator drives it
/// with its LSP exit), then close every session the same way a signal
/// would: each one's fence emits `session.end`, then the server drains.
/// Unlike [`serve_state_on`] there is no signal handler — the caller owns
/// the shutdown condition.
pub async fn serve_until(
    state: AppState,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    let shutdown_state = state.clone();
    let shutdown = async move {
        shutdown.await;
        shutdown_state.close_all().await;
    };
    let app = app(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
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
    let resume: Option<Ulid> = match request.resume.as_deref().map(str::parse) {
        None => None,
        Some(Ok(parsed)) => Some(parsed),
        Some(Err(_)) => {
            return error_response(ServerError::SessionNotFound(request.resume.unwrap()));
        }
    };
    let session_id = state
        .create_or_resume_session(
            std::path::Path::new(&request.workspace),
            request.model.as_deref(),
            request.profile.as_deref(),
            resume,
        )
        .await;
    match session_id {
        Ok((id, _)) => (
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

/// One row of `GET /v1/sessions?workspace=`: the stored summary plus the
/// registry's liveness.
#[derive(Debug, Serialize)]
pub struct SessionListEntry {
    pub id: String,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Number of driver messages (`message.received` events).
    pub message_count: u64,
    /// True when a live, active session with this id exists in this process.
    pub live: bool,
}

#[derive(Debug, Serialize)]
pub struct SessionListBody {
    pub sessions: Vec<SessionListEntry>,
}

/// `GET /v1/sessions?workspace=<abs path>`: the workspace's conversations,
/// newest `updated_at` first.
async fn list_sessions(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(workspace) = params.get("workspace") else {
        return error_response(ServerError::MalformedBody(
            "missing query parameter: workspace".to_string(),
        ));
    };
    match state.list(workspace).await {
        Ok(rows) => {
            let sessions = rows
                .into_iter()
                .map(|row| SessionListEntry {
                    id: row.id.to_string(),
                    title: row.title,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                    message_count: row.message_count,
                    live: row.live,
                })
                .collect();
            (StatusCode::OK, Json(SessionListBody { sessions })).into_response()
        }
        Err(err) => error_response(err),
    }
}

/// `DELETE /v1/sessions/{id}`: 204 on success; 404 only when the id is
/// neither in memory nor on disk.
async fn delete_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let parsed: Ulid = match id.parse() {
        Ok(parsed) => parsed,
        Err(_) => return error_response(ServerError::SessionNotFound(id)),
    };
    match state.delete(parsed).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => error_response(err),
    }
}

#[derive(Debug, Serialize)]
pub struct ModelEntry {
    pub alias: String,
    pub model: String,
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ModelsBody {
    pub models: Vec<ModelEntry>,
    pub default: Option<String>,
}

/// `GET /v1/models?workspace=<abs path>`: chat models with tools.
async fn list_models(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(workspace) = params.get("workspace") else {
        return error_response(ServerError::MalformedBody(
            "missing query parameter: workspace".to_string(),
        ));
    };
    match state.list_models(std::path::Path::new(workspace)) {
        Ok(entries) => {
            let mut models = entries
                .into_iter()
                .map(|(alias, m)| ModelEntry {
                    alias,
                    model: m.model,
                    context_window: m.context_window,
                    enabled: None,
                    disabled_reason: None,
                })
                .collect::<Vec<_>>();
            for entry in &mut models {
                match state
                    .resolve_model_metadata(std::path::Path::new(workspace), &entry.alias)
                    .await
                {
                    Ok(resolution) => {
                        entry.context_window = Some(resolution.window);
                        entry.enabled = Some(true);
                    }
                    Err(error) => {
                        entry.enabled = Some(false);
                        entry.disabled_reason = Some(error.to_string());
                    }
                }
            }
            let default = state.default_model(Some("navigator")).map(str::to_string);
            (StatusCode::OK, Json(ModelsBody { models, default })).into_response()
        }
        Err(err) => error_response(err),
    }
}

/// The server-assigned sender for HTTP-submitted messages: clients can never
/// impersonate another actor by choosing it themselves.
const HTTP_USER_ACTOR_ID: &str = "user:http";

/// Builds the Command for a posted message with a server-assigned sender.
/// Public so the WS route and integration tests share the exact POST core.
pub fn build_command(
    session_id: Ulid,
    request: &PostMessageRequest,
    mode: DeliveryMode,
) -> Command {
    Command {
        command_id: Ulid::new(),
        session_id,
        source_actor_id: HTTP_USER_ACTOR_ID.to_string(),
        target_actor_id: request.target_actor_id.clone(),
        mode,
        content: request.content.clone(),
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

/// `POST /v1/sessions/:id/messages`: validate the session and the
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
    // Unknown or stopping sessions answer 404/409 before the body is judged.
    if let Err(err) = state.routable_handle(session_id).await {
        return error_response(err);
    }
    let mode = match parse_mode(request.mode.as_deref()) {
        Ok(mode) => mode,
        Err(err) => return error_response(err),
    };
    let command = build_command(session_id, &request, mode);
    match state.send(command).await {
        Ok(accepted) => Json(accepted).into_response(),
        Err(err) => error_response(err),
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
/// body; every other failure shares `{"error": ...}`.
pub(crate) fn error_response(err: ServerError) -> Response {
    let status = match &err {
        ServerError::SessionNotFound(_) => StatusCode::NOT_FOUND,
        ServerError::SessionStopping | ServerError::ResumeRequired => StatusCode::CONFLICT,
        ServerError::NonLoopbackBind(_) | ServerError::Bind(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ServerError::InboxFull => StatusCode::SERVICE_UNAVAILABLE,
        ServerError::NonTextContent | ServerError::MalformedBody(_) => StatusCode::BAD_REQUEST,
        ServerError::NoContextWindow { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        ServerError::ModelIdentityChanged { .. } => StatusCode::CONFLICT,
        ServerError::SavedResolutionInvalid { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        ServerError::History(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ServerError::Config(_) | ServerError::Model(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if matches!(err, ServerError::InboxFull) {
        return (
            status,
            Json(json!({ "accepted": false, "reason": "inbox_full" })),
        )
            .into_response();
    }
    if matches!(err, ServerError::ResumeRequired) {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "resume_required" })),
        )
            .into_response();
    }
    if matches!(err, ServerError::NoContextWindow { .. }) {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "no_context_window" })),
        )
            .into_response();
    }
    if matches!(err, ServerError::ModelIdentityChanged { .. }) {
        return (
            status,
            Json(json!({ "error": err.to_string(), "reason": "model_identity_changed" })),
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

    fn test_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = AppState::new(ServerConfig {
            bind: "127.0.0.1:0".parse().expect("loopback bind"),
            models_path: None,
            config_dir: dir.path().join("cfg"),
            allow_non_loopback: false,
            bearer_token: None,
            history_db: dir.path().join("history.db"),
            data_dir: dir.path().join("data"),
            rules_roots: Default::default(),
        })
        .expect("state over a temp database");
        (dir, state)
    }

    // Given a session POST with a malformed body
    // When the handler runs
    // Then 400 with the shared {"error": ...} shape (not a panic/plain text)
    #[tokio::test]
    async fn malformed_create_body_is_a_structured_400() {
        let (_dir, state) = test_state();
        let response = app(state)
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
