//! Where Navai keeps its data and configuration.
//!
//! Data (the conversation database, the decision log and the terminal log)
//! lives in the XDG data dir, `~/.local/share/navai` on Linux. Configuration
//! (models.json, auth.json, the user pipeline dir) lives in the config dir,
//! `~/.config/navai`, see [`crate::provider::auth::default_config_dir`]. The
//! move from the former locations is in [`migrate`].

use std::path::PathBuf;

use directories::ProjectDirs;

pub mod migrate;

pub use migrate::{migrate_legacy, MigrationReport};

/// The application name every dir is derived from.
pub(crate) const APP_NAME: &str = "navai";

/// Navai's data dir (`~/.local/share/navai` on Linux), when the home
/// directory can be resolved.
pub fn data_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", APP_NAME).map(|dirs| dirs.data_dir().to_path_buf())
}

/// The conversation-history database, `<data_dir>/navai.db`. When the move of
/// a former database was refused, the former path, so that this run keeps
/// using the existing history.
pub fn history_db() -> Option<PathBuf> {
    if let Some(kept) = migrate::kept_history_db() {
        return Some(kept);
    }
    data_dir().map(|dir| dir.join("navai.db"))
}

/// The decision log the replay reads, `<data_dir>/navai.jsonl`.
pub fn decision_log() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("navai.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given a data dir name and a file below it
    // When the paths resolve
    // Then the database and the log sit side by side in the navai dir
    #[test]
    fn data_paths_share_the_navai_dir() {
        let (Some(dir), Some(db), Some(log)) = (data_dir(), history_db(), decision_log()) else {
            return; // no home directory in this environment
        };
        assert!(dir.ends_with("navai"));
        assert_eq!(db, dir.join("navai.db"));
        assert_eq!(log, dir.join("navai.jsonl"));
    }
}
