//! CLI adapter for the terminal client; owns process configuration only.
//!
//! Embedded mode (the default) starts a private server on a loopback port
//! with a per-process bearer token and stops it when the client quits.
//! `--url` mode only connects to a server somebody else runs; quitting just
//! disconnects. One process owns a conversation at a time: to share one
//! between clients, run a server and point each client at it with `--url`.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::embedded::{EmbeddedServer, check_health, resolve_workspace};
use super::{EXIT_DONE, TuiArgs};

/// Environment variable holding the bearer token of a server given by `--url`.
pub const TOKEN_ENV: &str = "WITHNAV_SERVER_TOKEN";
/// The former name of [`TOKEN_ENV`], still read when the new one is unset.
const LEGACY_TOKEN_ENV: &str = "RUST_AGENT_SERVER_TOKEN";

/// What the terminal client takes from this crate: the settings file, the git reader and the
/// home folder. The settings format and the git limits are the server's own.
#[derive(Debug, Clone, Copy, Default)]
pub struct Host;

// The panel is laid out for as many files as the reader keeps.
const _: () = assert!(withnav_tui::MAX_FILES == crate::workspace::git::MAX_FILES);

impl withnav_tui::TuiHost for Host {
    fn load_theme(&self, config_dir: &Path) -> Result<Option<String>, String> {
        crate::settings::load(config_dir)
            .map(|settings| settings.tui_theme().map(str::to_owned))
            .map_err(|err| err.to_string())
    }

    fn save_theme(&self, config_dir: &Path, id: &str) -> Result<(), String> {
        crate::settings::save_tui_theme(config_dir, id).map_err(|err| err.to_string())
    }

    fn load_favorite_models(&self, config_dir: &Path) -> Result<Vec<String>, String> {
        crate::settings::load(config_dir)
            .map(|settings| settings.tui_favorite_models())
            .map_err(|err| err.to_string())
    }

    fn save_favorite_models(&self, config_dir: &Path, names: &[String]) -> Result<(), String> {
        crate::settings::save_tui_favorite_models(config_dir, names).map_err(|err| err.to_string())
    }

    fn read_git(
        &self,
        workspace: &Path,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<withnav_tui::GitStatus>> + Send + '_>,
    > {
        let workspace = workspace.to_path_buf();
        Box::pin(async move {
            let status = crate::workspace::git::read(&workspace).await?;
            Some(withnav_tui::GitStatus {
                branch: status.branch,
                dirty: status.dirty,
                files: status
                    .files
                    .into_iter()
                    .map(|change| withnav_tui::Change {
                        mark: change.mark,
                        path: change.path,
                    })
                    .collect(),
            })
        })
    }

    fn home_dir(&self) -> Option<PathBuf> {
        crate::paths::home_dir()
    }
}

/// The log file of the terminal client, `<data dir>/tui.log`.
pub fn log_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|dir| dir.join("tui.log"))
}

/// Sends tracing output to the append-only log file and nowhere else: the
/// screen belongs to the UI. If the file cannot be opened, logging is off.
/// The token and request headers are never logged by this crate.
pub fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let Some(file) = log_path().and_then(|path| open_log(&path).ok()) else {
        return;
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(super::logging::mcp_safe_filter())
        .with(
            fmt::layer()
                .with_target(false)
                .with_ansi(false)
                .with_writer(Mutex::new(file)),
        )
        .try_init();
}

fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    writeln!(file, "--- terminal client started ---")?;
    Ok(file)
}

pub async fn run(args: TuiArgs) -> anyhow::Result<i32> {
    let workspace = resolve_workspace(&args.workspace)?;
    match args.url.clone() {
        Some(url) => run_remote(args, workspace, url).await?,
        None => run_embedded(args, workspace).await?,
    }
    Ok(EXIT_DONE)
}

async fn run_remote(args: TuiArgs, workspace: PathBuf, url: String) -> anyhow::Result<()> {
    let url = url.trim_end_matches('/').to_string();
    anyhow::ensure!(
        url.starts_with("http://") || url.starts_with("https://"),
        "--url must start with http:// or https://, got {url:?}"
    );
    let (token, deprecated) = server_token(|name| crate::paths::env_var(name).ok());
    if deprecated {
        eprintln!("warning: {LEGACY_TOKEN_ENV} is deprecated, use {TOKEN_ENV}");
    }
    check_health(&url).await?;
    withnav_tui::run(withnav_tui::TuiOptions {
        url,
        token,
        workspace,
        initial_model: args.model,
        initial_session: args.session,
        embedded: false,
        config_dir: crate::provider::auth::default_config_dir(),
        host: Arc::new(Host),
    })
    .await?;
    Ok(())
}

/// The bearer token from `lookup` (an environment reader): [`TOKEN_ENV`],
/// else the legacy name. The flag is true when the legacy name supplied it.
fn server_token(lookup: impl Fn(&str) -> Option<String>) -> (Option<String>, bool) {
    let non_empty = |name: &str| lookup(name).filter(|t| !t.is_empty());
    match non_empty(TOKEN_ENV) {
        Some(token) => (Some(token), false),
        None => match non_empty(LEGACY_TOKEN_ENV) {
            Some(token) => (Some(token), true),
            None => (None, false),
        },
    }
}

async fn run_embedded(args: TuiArgs, workspace: PathBuf) -> anyhow::Result<()> {
    let server = EmbeddedServer::start(
        args.models.as_deref(),
        std::slice::from_ref(&workspace),
        None,
        0,
    )
    .await?;
    let result = withnav_tui::run(withnav_tui::TuiOptions {
        url: server.url.clone(),
        token: Some(server.token.clone()),
        workspace,
        initial_model: args.model,
        initial_session: args.session,
        embedded: true,
        config_dir: crate::provider::auth::default_config_dir(),
        host: Arc::new(Host),
    })
    .await;
    server.stop().await;
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    // Given only the former variable name
    // When the token is read
    // Then its value is used and the deprecation flag is raised
    #[test]
    fn the_legacy_token_variable_still_works_and_is_flagged() {
        let (token, deprecated) = server_token(env(&[(LEGACY_TOKEN_ENV, "old")]));
        assert_eq!(token.as_deref(), Some("old"));
        assert!(deprecated);
    }

    // Given both names
    // When the token is read
    // Then the new one wins and nothing is flagged
    #[test]
    fn the_new_token_variable_wins_without_a_warning() {
        let (token, deprecated) =
            server_token(env(&[(TOKEN_ENV, "new"), (LEGACY_TOKEN_ENV, "old")]));
        assert_eq!(token.as_deref(), Some("new"));
        assert!(!deprecated);
    }

    // Given neither name, or empty values
    // When the token is read
    // Then there is no token
    #[test]
    fn no_token_variable_means_no_token() {
        assert_eq!(server_token(env(&[])), (None, false));
        assert_eq!(
            server_token(env(&[(TOKEN_ENV, ""), (LEGACY_TOKEN_ENV, "")])),
            (None, false)
        );
    }
}
