//! Where WithNav keeps its data and configuration.
//!
//! Data (the conversation database, the decision log and the terminal log)
//! lives in the XDG data dir, `~/.local/share/withnav` on Linux. Configuration
//! (models.json, auth.json, the user pipeline dir) lives in the config dir,
//! `~/.config/withnav`, see [`crate::provider::auth::default_config_dir`].

use std::path::PathBuf;

use directories::ProjectDirs;

/// The application name every dir is derived from.
pub(crate) const APP_NAME: &str = "withnav";

/// WithNav's data dir (`~/.local/share/withnav` on Linux), when the home
/// directory can be resolved.
pub fn data_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", APP_NAME).map(|dirs| dirs.data_dir().to_path_buf())
}

/// The conversation-history database, `<data_dir>/withnav.db`.
pub fn history_db() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("withnav.db"))
}

/// The decision log the replay reads, `<data_dir>/withnav.jsonl`.
pub fn decision_log() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("withnav.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given a data dir name and a file below it
    // When the paths resolve
    // Then the database and the log sit side by side in the withnav dir
    #[test]
    fn data_paths_share_the_withnav_dir() {
        let (Some(dir), Some(db), Some(log)) = (data_dir(), history_db(), decision_log()) else {
            return; // no home directory in this environment
        };
        assert!(dir.ends_with("withnav"));
        assert_eq!(db, dir.join("withnav.db"));
        assert_eq!(log, dir.join("withnav.jsonl"));
    }
}

/// Selects the workspace configuration directory, reading legacy names for 0.7.
pub fn workspace_dir(workspace: &std::path::Path) -> std::path::PathBuf {
    workspace_dir_with_hint(
        workspace,
        |legacy| tracing::warn!(workspace = %workspace.display(), "{legacy} is deprecated; move it to .withnav"),
    )
}

fn workspace_dir_with_hint(
    workspace: &std::path::Path,
    hint: impl FnOnce(&str),
) -> std::path::PathBuf {
    use std::sync::{Mutex, OnceLock};
    static WARNED: OnceLock<Mutex<std::collections::HashSet<std::path::PathBuf>>> = OnceLock::new();
    let new = workspace.join(".withnav");
    if new.exists() {
        return new;
    }
    for legacy in [".navai", ".navigator"] {
        let path = workspace.join(legacy);
        if path.exists() {
            if WARNED
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(workspace.to_path_buf())
            {
                hint(legacy);
            }
            return path;
        }
    }
    new
}

#[cfg(test)]
mod workspace_tests {
    #[test]
    fn old_workspace_directories_are_read_in_priority_order() {
        let root = tempfile::tempdir().unwrap();
        for name in [".navigator", ".navai", ".withnav"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
            assert_eq!(super::workspace_dir(root.path()), root.path().join(name));
        }
    }
}

/// Reads a renamed environment variable, warning once for each legacy name.
pub fn env_var_os(name: &str) -> Option<std::ffi::OsString> {
    env_with(name, |key| std::env::var_os(key))
}
pub fn env_var(name: &str) -> Result<String, std::env::VarError> {
    env_var_os(name)
        .ok_or(std::env::VarError::NotPresent)?
        .into_string()
        .map_err(std::env::VarError::NotUnicode)
}
fn env_with(
    name: &str,
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<std::ffi::OsString> {
    use std::sync::{Mutex, OnceLock};
    static WARNED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    if let Some(value) = lookup(name) {
        return Some(value);
    }
    let old = name
        .strip_prefix("WITHNAV_")
        .map(|suffix| format!("NAVAI_{suffix}"))?;
    let value = lookup(&old)?;
    if WARNED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(old.clone())
    {
        tracing::warn!("{old} is deprecated; use {name}");
    }
    Some(value)
}
#[cfg(test)]
mod env_tests {
    #[test]
    fn legacy_environment_is_read_only_when_new_name_is_absent() {
        for suffix in [
            "MAX_STEPS",
            "BASH_GRACE_MS",
            "HTML_CONVERTER",
            "SERVER_TOKEN",
        ] {
            let new = format!("WITHNAV_{suffix}");
            let old = format!("NAVAI_{suffix}");
            assert_eq!(
                super::env_with(&new, |key| (key == old).then(|| "old".into())),
                Some("old".into())
            );
            assert_eq!(
                super::env_with(&new, |_| Some("new".into())),
                Some("new".into())
            );
        }
    }
}
