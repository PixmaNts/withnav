//! HTTP state: [`AppState`] holds the session [`Service`] plus the HTTP
//! configuration, and derefs to the service, so routes call
//! `state.create_session(..)` and so on.

use std::ops::Deref;
use std::sync::Arc;

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
        Self::with_hooks(config, Arc::new(crate::session::NoHooks))
    }

    /// [`AppState::new`] with the host's session hooks.
    pub fn with_hooks(config: ServerConfig, hooks: Arc<dyn SessionHooks>) -> Self {
        let (session, http) = config.split();
        Self {
            service: Service::with_hooks(session, hooks),
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
