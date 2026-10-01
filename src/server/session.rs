//! HTTP state: [`AppState`] holds the session [`Service`] plus the HTTP
//! configuration, and derefs to the service, so routes call
//! `state.create_session(..)` and so on. The other items are compatibility
//! re-exports of the moved session types.

use std::ops::Deref;
use std::sync::Arc;

// Compatibility path: remove once callers import from `crate::session`.
use crate::session::Service;
pub use crate::session::{
    Lifecycle, SessionHandle, SessionId, SessionSlot, SessionSummary, Tombstone, COGNITION_PROMPT,
    MAX_TOMBSTONES, NAVIGATOR_PROFILE,
};

pub use super::config::{HttpConfig, ServerConfig};

/// Axum state: the session service and the HTTP configuration. Cheap to
/// clone.
#[derive(Clone, Debug)]
pub struct AppState {
    service: Service,
    pub http: Arc<HttpConfig>,
}

impl AppState {
    pub fn new(config: ServerConfig) -> Self {
        Self::with_conversation(config, None)
    }

    /// [`AppState::new`] sharing the navigator's conversation hub.
    pub fn with_conversation(
        config: ServerConfig,
        conversation: Option<crate::navigator::ConversationFeed>,
    ) -> Self {
        let (session, http) = config.split();
        Self {
            service: Service::with_conversation(session, conversation),
            http: Arc::new(http),
        }
    }
}

impl Deref for AppState {
    type Target = Service;

    fn deref(&self) -> &Service {
        &self.service
    }
}
