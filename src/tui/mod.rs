//! Terminal client module tree. Owns only the HTTP/SSE client and UI types.
pub mod client;
pub mod commands;
pub mod git;
pub mod input;
pub mod protocol;
pub mod reducer;
pub mod runtime;
pub mod state;
pub mod stream;
pub mod terminal;
pub mod theme;
pub mod view;
use std::path::PathBuf;
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
