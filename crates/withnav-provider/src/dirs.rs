//! The config and data directories the model access layer reads and writes.
//!
//! `WITHNAV_CONFIG_DIR` and `WITHNAV_DATA_DIR` move them (portable installs, tests); without the
//! override they sit under the platform's per-user directories.

use std::path::PathBuf;

use directories::ProjectDirs;

/// The application name every dir is derived from.
pub const APP_NAME: &str = "withnav";

/// Resolve portable/test roots against the process directory, never a tool's workspace.
pub fn root_override(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name).filter(|value| !value.is_empty())?;
    let path = PathBuf::from(value);
    Some(if path.is_absolute() {
        path
    } else {
        std::path::absolute(path)
            .expect("cannot resolve relative WithNav root override: process directory unavailable")
    })
}

/// WithNav's config dir (`~/.config/withnav` on Linux), when the home directory can be resolved.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(path) = root_override("WITHNAV_CONFIG_DIR") {
        return Some(path);
    }
    ProjectDirs::from("", "", APP_NAME).map(|dirs| dirs.config_dir().to_path_buf())
}

/// WithNav's data dir (`~/.local/share/withnav` on Linux), when the home
/// directory can be resolved.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(path) = root_override("WITHNAV_DATA_DIR") {
        return Some(path);
    }
    ProjectDirs::from("", "", APP_NAME).map(|dirs| dirs.data_dir().to_path_buf())
}
