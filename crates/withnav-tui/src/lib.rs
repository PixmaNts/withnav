//! Terminal client of the WithNav session server: input, editor, views, themes and the settings
//! screen. It reaches the server over HTTP and SSE through `withnav-client`, and everything else
//! it needs from the machine through [`TuiHost`].
pub mod catalog;
pub mod commands;
pub mod config;
pub mod git;
pub mod input;
pub mod protocol;
pub mod reducer;
pub mod runtime;
pub mod state;
pub mod terminal;
#[cfg(test)]
mod test_host;
pub mod theme;
pub mod view;

use std::{future::Future, path::Path, path::PathBuf, pin::Pin, sync::Arc};

pub use git::{Change, GitStatus, MAX_FILES};

/// What the terminal client needs from the program that embeds it: the user settings, the
/// workspace's git state and the home folder. Errors are the message shown to the Captain.
pub trait TuiHost: Send + Sync + std::fmt::Debug {
    /// The theme id saved in the settings of `config_dir`; `None` when none is saved.
    fn load_theme(&self, config_dir: &Path) -> Result<Option<String>, String>;
    fn save_theme(&self, config_dir: &Path, id: &str) -> Result<(), String>;
    /// The favourite models saved in the settings of `config_dir`.
    fn load_favorite_models(&self, config_dir: &Path) -> Result<Vec<String>, String>;
    fn save_favorite_models(&self, config_dir: &Path, names: &[String]) -> Result<(), String>;
    /// The branch and changes of `workspace`; `None` when it is not a git repository.
    fn read_git(
        &self,
        workspace: &Path,
    ) -> Pin<Box<dyn Future<Output = Option<GitStatus>> + Send + '_>>;
    /// The home folder, shown as `~` in paths.
    fn home_dir(&self) -> Option<PathBuf>;
}

/// A host with no settings, no git state and no home folder.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHost;

impl TuiHost for NoHost {
    fn load_theme(&self, _: &Path) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn save_theme(&self, _: &Path, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn load_favorite_models(&self, _: &Path) -> Result<Vec<String>, String> {
        Ok(vec![])
    }
    fn save_favorite_models(&self, _: &Path, _: &[String]) -> Result<(), String> {
        Ok(())
    }
    fn read_git(&self, _: &Path) -> Pin<Box<dyn Future<Output = Option<GitStatus>> + Send + '_>> {
        Box::pin(async { None })
    }
    fn home_dir(&self) -> Option<PathBuf> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct TuiOptions {
    pub url: String,
    pub token: Option<String>,
    pub workspace: PathBuf,
    pub initial_model: Option<String>,
    pub initial_session: Option<String>,
    pub embedded: bool,
    /// Where `config.json` lives; `None` reads and saves no settings.
    pub config_dir: Option<PathBuf>,
    pub host: Arc<dyn TuiHost>,
}

#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("terminal client is not implemented yet")]
    NotImplemented,
    #[error("{0}")]
    Other(String),
}

/// Runs the terminal client until the user quits.
pub async fn run(options: TuiOptions) -> Result<(), TuiError> {
    runtime::run(options).await
}
