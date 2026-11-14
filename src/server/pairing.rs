//! Browser pairing: a single-use code minted with the owner's token, exchanged once by the
//! browser for its own expiring credential. The owner's token never reaches a browser.
use axum::Router;

use super::AppState;

/// Pairing codes and browser credentials, in memory only.
#[derive(Debug, Default)]
pub struct Store {}

impl Store {
    /// True when `credential` is a live browser credential. The auth middleware accepts it in
    /// place of the owner's token on every route except the owner-only ones
    /// (`/v1/shutdown`, `/v1/pair`, `/v1/lsp`).
    pub fn accepts(&self, credential: &str) -> bool {
        let _ = credential;
        false
    }
}

/// `POST /v1/pair` (owner token) and `POST /v1/pair/exchange` (public).
pub(super) fn router() -> Router<AppState> {
    Router::new()
}
