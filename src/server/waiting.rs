//! `GET /v1/waiting`: the permission requests every live session waits on, so a client raises
//! one notification per blocked turn and closes it when the request is answered anywhere.

use axum::{Json, extract::State, response::IntoResponse};
use serde_json::json;

use super::AppState;

pub(super) async fn get_waiting(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({ "waiting": state.waiting().await }))
}
