//! HTTP state: [`AppState`] holds the session [`Service`] plus the HTTP
//! configuration, and derefs to the service, so routes call
//! `state.create_session(..)` and so on.

use std::ops::Deref;
use std::sync::Arc;

use crate::error::ServerError;
use crate::session::{Service, SessionHooks};

pub use super::config::{HttpConfig, ServerConfig};

/// Axum state: the session service and the HTTP configuration. Cheap to
/// clone.
#[derive(Clone, Debug)]
pub struct AppState {
    service: Service,
    pub http: Arc<HttpConfig>,
    pub(super) settings_write: Arc<tokio::sync::Mutex<()>>,
    pub(super) flows: Arc<super::flows::Host>,
}

impl AppState {
    pub fn service(&self) -> Service {
        self.service.clone()
    }
    /// A state without any profile behaviour. Fails when the history
    /// database cannot be opened.
    pub fn new(config: ServerConfig) -> Result<Self, ServerError> {
        Self::with_hooks(config, Arc::new(crate::session::NoHooks))
    }

    /// [`AppState::new`] with the host's session hooks.
    pub fn with_hooks(
        config: ServerConfig,
        hooks: Arc<dyn SessionHooks>,
    ) -> Result<Self, ServerError> {
        let flows = Arc::new(super::flows::Host::new(
            config.config_dir.clone(),
            config.data_dir.clone(),
            config.models_path.clone(),
        ));
        let (session, http) = config.split();
        Ok(Self {
            service: Service::with_hooks(session, hooks)?,
            http: Arc::new(http),
            settings_write: Arc::default(),
            flows,
        })
    }
}

impl Deref for AppState {
    type Target = Service;

    fn deref(&self) -> &Service {
        &self.service
    }
}
