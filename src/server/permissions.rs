//! Permission rules, validations and decision history over HTTP, for `nav permissions`.
use std::path::PathBuf;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::permission::decisions::DecisionFilter;
use crate::permission::rules::{Effect, RuleFilter, RuleOrigin, RuleScope};
use crate::permission::store::PermissionStore;
use crate::permission::validation::ValidationKind;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/permissions/rules", get(list_rules).post(add_rule))
        .route("/v1/permissions/rules/{id}", delete(revoke_rule))
        .route("/v1/permissions/history", get(list_history))
        .route("/v1/permissions/validations", get(list_validations))
        .route("/v1/permissions/validation/check", get(check_validation))
        .route(
            "/v1/permissions/validations/{id}",
            delete(revoke_validation),
        )
        .route("/v1/permissions/validate", post(validate_flow))
}

#[derive(Debug, Deserialize)]
pub struct CheckValidationQuery {
    pub kind: ValidationKind,
    pub location: String,
    pub hash: String,
}

async fn check_validation(
    State(state): State<AppState>,
    Query(query): Query<CheckValidationQuery>,
) -> Response {
    let location = query.location;
    let hash = query.hash;
    let kind = query.kind;
    let validation_db = state.config.data_dir.join("withnav.db");
    let result = tokio::task::spawn_blocking(move || {
        if !validation_db.exists() {
            return Ok(false);
        }
        let connection = rusqlite::Connection::open_with_flags(
            validation_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let store = PermissionStore::with_connection(connection);
        store
            .is_valid(kind, &location, &hash)
            .map_err(anyhow::Error::from)
    })
    .await;
    match result {
        Ok(Ok(valid)) => (StatusCode::OK, Json(json!({"valid": valid}))).into_response(),
        Ok(Err(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error.to_string()})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct ListRulesQuery {
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub global: bool,
    pub project: Option<PathBuf>,
    pub agent: Option<String>,
}

async fn list_rules(
    State(state): State<AppState>,
    Query(query): Query<ListRulesQuery>,
) -> Response {
    if let Some(ref p) = query.project {
        if !p.is_absolute() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "project path must be absolute" })),
            )
                .into_response();
        }
    }
    let data_dir = state.config.data_dir.clone();
    let filter = RuleFilter {
        include_revoked: query.all,
        scope: if query.global {
            Some(RuleScope::Global)
        } else if let Some(p) = query.project {
            Some(RuleScope::Project(p))
        } else {
            query.agent.map(RuleScope::Agent)
        },
    };
    let result = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let store = PermissionStore::open(&data_dir).map_err(|e| e.to_string())?;
        store.list(&filter).map_err(|e| e.to_string())
    })
    .await;

    match result {
        Ok(Ok(rules)) => (StatusCode::OK, Json(rules)).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct AddRuleBody {
    pub action: String,
    pub resource: String,
    pub effect: Effect,
    pub scope: RuleScope,
}

async fn add_rule(State(state): State<AppState>, Json(body): Json<AddRuleBody>) -> Response {
    if let RuleScope::Project(ref p) = body.scope {
        if !p.is_absolute() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "project path must be absolute" })),
            )
                .into_response();
        }
    }
    let data_dir = state.config.data_dir.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let store = PermissionStore::open(&data_dir).map_err(|e| e.to_string())?;
        store
            .add(
                &body.action,
                &body.resource,
                body.effect,
                body.scope,
                RuleOrigin::Manual,
            )
            .map_err(|e| e.to_string())
    })
    .await;

    match result {
        Ok(Ok(rule)) => (StatusCode::CREATED, Json(rule)).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn revoke_rule(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let data_dir = state.config.data_dir.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let store = PermissionStore::open(&data_dir).map_err(|e| e.to_string())?;
        store.revoke(id).map_err(|e| e.to_string())
    })
    .await;

    match result {
        Ok(Ok(true)) => (StatusCode::OK, Json(json!({ "revoked": true }))).into_response(),
        Ok(Ok(false)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("permission rule {id} not found or already revoked") })),
        )
            .into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<usize>,
}

async fn list_history(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    let data_dir = state.config.data_dir.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let store = PermissionStore::open(&data_dir).map_err(|e| e.to_string())?;
        store
            .list_decisions(&DecisionFilter::default(), query.limit.unwrap_or(50))
            .map_err(|e| e.to_string())
    })
    .await;

    match result {
        Ok(Ok(decisions)) => (StatusCode::OK, Json(decisions)).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn list_validations(State(state): State<AppState>) -> Response {
    let data_dir = state.config.data_dir.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let store = PermissionStore::open(&data_dir).map_err(|e| e.to_string())?;
        store.list_validations().map_err(|e| e.to_string())
    })
    .await;

    match result {
        Ok(Ok(validations)) => (StatusCode::OK, Json(validations)).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct RevokeValidationQuery {
    pub workspace: Option<PathBuf>,
}

async fn revoke_validation(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<RevokeValidationQuery>,
) -> Response {
    if let Some(ref w) = query.workspace {
        if !w.is_absolute() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "workspace path must be absolute" })),
            )
                .into_response();
        }
    }
    let data_dir = state.config.data_dir.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let store = PermissionStore::open(&data_dir).map_err(|e| e.to_string())?;
        store.revoke_validation(id).map_err(|e| e.to_string())
    })
    .await;

    match result {
        Ok(Ok(true)) => {
            // Make sure the caller's workspace is loaded, then reload every loaded workspace:
            // the revoked flow may be global or belong to a workspace other than the caller's.
            if let Some(workspace) = query.workspace {
                let _ = state.flows.store(workspace);
            }
            let flows = state.flows.clone();
            let failed = tokio::task::spawn_blocking(move || flows.reload_all())
                .await
                .unwrap_or_default();
            let failed: Vec<_> = failed
                .into_iter()
                .map(|(workspace, reason)| json!({ "workspace": workspace, "error": reason }))
                .collect();
            (
                StatusCode::OK,
                Json(json!({ "revoked": true, "reload_failed": failed })),
            )
                .into_response()
        }
        Ok(Ok(false)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("validation {id} not found or already revoked") })),
        )
            .into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct ValidateFlowBody {
    pub kind: ValidationKind,
    pub name: String,
    #[serde(default)]
    pub location: Option<crate::flows::store::Location>,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
}

async fn validate_flow(
    State(state): State<AppState>,
    Json(body): Json<ValidateFlowBody>,
) -> Response {
    if body.kind != ValidationKind::Flow {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "skill admission is not available yet" })),
        )
            .into_response();
    }

    if let Some(ref w) = body.workspace {
        if !w.is_absolute() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "workspace path must be absolute" })),
            )
                .into_response();
        }
    }

    let workspace = body
        .workspace
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let store = state.flows.store(workspace);

    let location = match body.location {
        Some(location) => location,
        None => {
            let list = match store.list() {
                Ok(l) => l,
                Err(e) => {
                    return (StatusCode::BAD_REQUEST, Json(json!({ "error": e.reason })))
                        .into_response();
                }
            };
            let matches: Vec<_> = list
                .into_iter()
                .filter(|flow| {
                    flow.name == body.name
                        && flow.location != crate::flows::store::Location::Builtin
                })
                .collect();
            if matches.len() != 1 {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "select --location global or workspace for an ambiguous or missing flow"
                    })),
                )
                    .into_response();
            }
            matches[0].location
        }
    };

    let document = match store.get(&body.name, location) {
        Ok(doc) => doc,
        Err(e) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": e.reason }))).into_response();
        }
    };

    let approval = match store.validate_version(&body.name, location, &document.revision) {
        Ok(appr) => appr,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e.reason }))).into_response();
        }
    };

    let _ = store.reload();

    (StatusCode::OK, Json(approval)).into_response()
}
