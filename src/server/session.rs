//! HTTP state: [`AppState`] holds the session [`Service`] and derefs to it,
//! so routes call `state.create_session(..)`, `state.config`, and so on.
//! The other items are compatibility re-exports of the moved session types.

use std::ops::Deref;

// Compatibility path: remove once callers import from `crate::session`.
use crate::session::Service;
pub use crate::session::{
    Lifecycle, ServerConfig, SessionHandle, SessionId, SessionSlot, SessionSummary, Tombstone,
    COGNITION_PROMPT, MAX_TOMBSTONES, NAVIGATOR_PROFILE,
};

/// Axum state: the session service. Cheap to clone.
#[derive(Clone, Debug)]
pub struct AppState {
    service: Service,
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
        Self {
            service: Service::with_conversation(config, conversation),
        }
    }
}

impl Deref for AppState {
    type Target = Service;

    fn deref(&self) -> &Service {
        &self.service
    }
}
