//! HTTP state: [`AppState`] holds the session [`Service`] plus the HTTP
//! configuration, and derefs to the service, so routes call
//! `state.create_session(..)` and so on.

use std::ops::Deref;
use std::sync::Arc;

use crate::error::ServerError;
use crate::provider::openai::ClientCache;
use crate::session::{Service, SessionHooks};

pub use super::config::{HttpConfig, ServerConfig};

/// Axum state: the session service and the HTTP configuration. Cheap to
/// clone.
#[derive(Clone, Debug)]
pub struct AppState {
    service: Service,
    pub http: Arc<HttpConfig>,
    pub(super) settings_write: Arc<tokio::sync::Mutex<()>>,
    pub(super) flows: Arc<crate::flows::host::Host>,
    pub(super) mcp_apps: Arc<super::mcp_app::McpAppStore>,
    /// Browser pairing codes and credentials.
    pub(crate) pairing: Arc<super::pairing::Store>,
    /// The navigator the editors' `/v1/lsp` connections run in; `None` in a server built
    /// without one (tests).
    pub(crate) navigator: Option<Arc<crate::navigator::bootstrap::Shared>>,
    /// Random owner instance string for hello proof verification.
    pub(crate) instance: Arc<String>,
    /// Shutdown trigger for graceful server termination via `/v1/shutdown`.
    pub(crate) shutdown_notify: Arc<tokio::sync::Notify>,
}

impl AppState {
    pub fn service(&self) -> Service {
        self.service.clone()
    }
    /// A state without any profile behaviour. Fails when the history
    /// database cannot be opened.
    pub fn new(config: ServerConfig, client_cache: Arc<ClientCache>) -> Result<Self, ServerError> {
        Self::with_hooks(config, Arc::new(crate::session::NoHooks), client_cache)
    }

    /// [`AppState::new`] with the host's session hooks.
    pub fn with_hooks(
        config: ServerConfig,
        hooks: Arc<dyn SessionHooks>,
        client_cache: Arc<ClientCache>,
    ) -> Result<Self, ServerError> {
        let (session, http) = config.split();
        let service = Service::with_hooks(session, hooks, client_cache)?;
        let flows = service.flows.clone();
        let instance = crate::server::owner::generate_instance()
            .unwrap_or_else(|_| ulid::Ulid::generate().to_string());
        Ok(Self {
            service,
            http: Arc::new(http),
            settings_write: Arc::default(),
            flows,
            mcp_apps: Arc::default(),
            pairing: Arc::default(),
            navigator: None,
            instance: Arc::new(instance),
            shutdown_notify: Arc::default(),
        })
    }

    /// Sets a specific instance identifier on the state.
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Arc::new(instance.into());
        self
    }

    /// Returns the owner's random instance identifier.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// Triggers graceful shutdown of the server.
    pub fn trigger_shutdown(&self) {
        self.shutdown_notify.notify_waiters();
    }

    /// Waits until graceful shutdown is triggered.
    pub async fn shutdown_notified(&self) {
        self.shutdown_notify.notified().await;
    }

    /// Returns true if any active session is currently running a turn.
    pub fn is_turn_running(&self) -> bool {
        self.service.is_turn_running()
    }

    /// Marks a session's turn as running for tests.
    #[doc(hidden)]
    pub async fn pin_session_turn(&self, session_id: ulid::Ulid) {
        self.service.pin_session_turn(session_id).await;
    }

    /// Serves editors' `/v1/lsp` connections with `navigator`.
    pub fn with_navigator(mut self, navigator: Arc<crate::navigator::bootstrap::Shared>) -> Self {
        self.navigator = Some(navigator);
        self
    }
}

impl Deref for AppState {
    type Target = Service;

    fn deref(&self) -> &Service {
        &self.service
    }
}
