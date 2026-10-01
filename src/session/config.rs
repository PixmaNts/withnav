//! What the session service needs to run, with no transport concern in it.

use std::path::PathBuf;

/// Immutable session-service configuration.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Explicit models.json path; `None` resolves per-workspace then the
    /// config dir (same order as the CLI `run` path).
    pub models_path: Option<PathBuf>,
    /// Auth store directory for API-key resolution.
    pub config_dir: PathBuf,
    /// Conversation-history database (`sessions.db`); `None` keeps the
    /// service free of persistence (tests, CLI `run`, `serve`).
    pub history_db: Option<PathBuf>,
}
