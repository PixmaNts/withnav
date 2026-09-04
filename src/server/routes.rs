//! Axum HTTP surface (todo 6): health, session create/get. This todo's
//! routes ONLY — messages/stop are todo 7, SSE todo 8, WS todo 9.
//!
//! Response bodies are FROZEN (ticket 04 Q3):
//! - `GET  /v1/health`        -> `{status:"ok"}`
//! - `POST /v1/sessions`      -> `{session_id, root_actor_id:"agent:main", actor_state:"idle"}`
//! - `GET  /v1/sessions/:id`  -> `{session_id, actor_state, workspace, model, current_seq}`
//!   (unknown id -> structured 404). All errors share the `{"error": ...}` shape.

use std::net::SocketAddr;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use ulid::Ulid;

use crate::bus::event::ROOT_ACTOR_ID;
use crate::error::ServerError;
use crate::server::session::{AppState, ServerConfig};

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

/// The Axum router for this slice's routes. Tests mount it directly via
/// `tower::ServiceExt::oneshot`.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/{id}", get(get_session))
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

/// The single structured-error funnel: every failure is a typed
/// [`ServerError`] mapped onto a status code + `{"error": ...}` JSON body.
fn error_response(err: ServerError) -> Response {
    let status = match &err {
        ServerError::SessionNotFound(_) => StatusCode::NOT_FOUND,
        ServerError::NonLoopbackBind(_) | ServerError::Bind(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ServerError::InboxFull => StatusCode::SERVICE_UNAVAILABLE,
        ServerError::NonTextContent | ServerError::MalformedBody(_) => StatusCode::BAD_REQUEST,
        ServerError::Config(_) | ServerError::Model(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
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
