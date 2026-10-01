//! HTTP state: [`AppState`] holds the session [`Service`] plus the HTTP
//! configuration, and derefs to the service, so routes call
//! `state.create_session(..)` and so on. The other items are compatibility
//! re-exports of the moved session types.

use std::ops::Deref;
use std::sync::Arc;

// Compatibility path: remove once callers import from `crate::session`.
pub use crate::session::{
    Lifecycle, SessionHandle, SessionId, SessionSlot, SessionSummary, Tombstone, MAX_TOMBSTONES,
};
use crate::session::{Service, SessionHooks};

pub use super::config::{HttpConfig, ServerConfig};

/// Axum state: the session service and the HTTP configuration. Cheap to
/// clone.
#[derive(Clone, Debug)]
pub struct AppState {
    service: Service,
    pub http: Arc<HttpConfig>,
}

impl AppState {
    /// A state without any profile behaviour.
    pub fn new(config: ServerConfig) -> Self {
        Self::with_conversation(config, Arc::new(crate::session::NoHooks), None)
    }

    /// [`AppState::new`] with the host's actor-profile policy.
    pub fn with_hooks(config: ServerConfig, hooks: Arc<dyn SessionHooks>) -> Self {
        Self::with_conversation(config, hooks, None)
    }

    /// [`AppState::with_hooks`] sharing the navigator's conversation hub.
    pub fn with_conversation(
        config: ServerConfig,
        hooks: Arc<dyn SessionHooks>,
        conversation: Option<crate::navigator::ConversationFeed>,
    ) -> Self {
        let (session, http) = config.split();
        Self {
            service: Service::with_conversation(session, hooks, conversation),
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
