//! What the session service needs to run, with no transport concern in it.

use std::path::PathBuf;

use crate::agent::instructions::RulesRoots;

/// Immutable session-service configuration.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Explicit models.json path; `None` resolves per-workspace then the
    /// config dir (same order as the CLI `run` path).
    pub models_path: Option<PathBuf>,
    /// Auth store directory for API-key resolution.
    pub config_dir: PathBuf,
    /// Conversation-history database; every conversation is written there.
    pub history_db: PathBuf,
    /// Where the local instructions of a session are looked for beyond its
    /// workspace.
    pub rules_roots: RulesRoots,
}
