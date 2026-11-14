//! Permission rules, validations and decision history over HTTP, for `nav permissions`.
use axum::Router;

use super::AppState;

pub(super) fn router() -> Router<AppState> {
    Router::new()
}
