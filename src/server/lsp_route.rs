//! `GET /v1/lsp`: an editor's language-server connection, upgraded from HTTP and served by the
//! navigator inside the owner.
use axum::Router;

use super::AppState;

pub(super) fn router() -> Router<AppState> {
    Router::new()
}
