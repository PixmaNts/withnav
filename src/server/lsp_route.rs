//! `GET /v1/lsp`: an editor's language-server connection, upgraded from HTTP and served by the
//! navigator inside the owner.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use hyper_util::rt::TokioIo;
use serde_json::json;

use super::AppState;

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/v1/lsp", get(lsp_handler))
}

async fn lsp_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut req: Request,
) -> Response {
    let Some(navigator) = state.navigator.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "navigator service unavailable"})),
        )
            .into_response();
    };
    let _ = &state.pairing;

    let raw_ws = headers
        .get("x-withnav-workspace")
        .or_else(|| headers.get("x-withnav-cwd"))
        .and_then(|v| v.to_str().ok());

    let Some(raw_ws) = raw_ws else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "missing x-withnav-workspace header"})),
        )
            .into_response();
    };

    let decoded_ws = match crate::navigator::lsp::uri::percent_decode(raw_ws) {
        Some(s) => s,
        None => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": "invalid percent encoding in workspace header"})),
            )
                .into_response();
        }
    };

    let ws_path = std::path::PathBuf::from(decoded_ws);
    if !ws_path.is_absolute() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": format!("workspace {} is not absolute", ws_path.display())})),
        )
            .into_response();
    }

    let canonical = match std::fs::canonicalize(&ws_path) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": format!("workspace {} is not accessible: {e}", ws_path.display())})),
            )
                .into_response();
        }
    };

    let canonical = match crate::tools::canonical_path(&canonical) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": format!("workspace {} canonical path error: {e}", canonical.display())})),
            )
                .into_response();
        }
    };

    if !canonical.is_dir() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": format!("workspace {} is not a directory", canonical.display())})),
        )
            .into_response();
    }

    let config_dir = &state.config.config_dir;
    let models_path = state.config.models_path.as_deref();
    let rules_roots = &state.config.rules_roots;
    let clients = navigator.clients.clone();

    let preflight = match crate::session::preflight::Preflight::capture(
        config_dir,
        models_path,
        &canonical,
        rules_roots,
        clients,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    if let Err(e) = preflight.check(&canonical).await {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    let on_upgrade = hyper::upgrade::on(&mut req);

    let app_state = state.clone();
    let nav_shared = navigator.clone();
    let workspace_root = canonical.clone();

    tokio::spawn(async move {
        let upgraded = match on_upgrade.await {
            Ok(u) => u,
            Err(e) => {
                tracing::error!("LSP upgrade failed: {e}");
                return;
            }
        };
        let io = TokioIo::new(upgraded);
        nav_shared
            .spawn_connection(crate::navigator::bootstrap::serve_connection(
                io,
                workspace_root,
                preflight,
                app_state,
                nav_shared.clone(),
            ))
            .await;
    });

    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "withnav-lsp")
        .body(axum::body::Body::empty())
        .unwrap()
}
