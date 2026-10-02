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
use std::time::Duration;

use anyhow::Context as _;

use super::{TuiArgs, EXIT_DONE};

/// Environment variable holding the bearer token of a server given by `--url`.
pub const TOKEN_ENV: &str = "RUST_AGENT_SERVER_TOKEN";
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// The log file of the terminal client, `<data dir>/tui.log`.
pub fn log_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|dir| dir.join("tui.log"))
}

/// Sends tracing output to the append-only log file and nowhere else: the
/// screen belongs to the UI. If the file cannot be opened, logging is off.
/// The token and request headers are never logged by this crate.
pub fn init_logging() {
    use tracing_subscriber::{fmt, EnvFilter};
    let Some(file) = log_path().and_then(|path| open_log(&path).ok()) else {
        return;
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false)
        .with_writer(Mutex::new(file))
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

fn resolve_workspace(path: &Path) -> anyhow::Result<PathBuf> {
    let resolved = std::fs::canonicalize(path)
        .with_context(|| format!("workspace {} is not accessible", path.display()))?;
    anyhow::ensure!(
        resolved.is_dir(),
        "workspace {} is not a directory",
        resolved.display()
    );
    Ok(resolved)
}

async fn run_remote(args: TuiArgs, workspace: PathBuf, url: String) -> anyhow::Result<()> {
    let url = url.trim_end_matches('/').to_string();
    anyhow::ensure!(
        url.starts_with("http://") || url.starts_with("https://"),
        "--url must start with http:// or https://, got {url:?}"
    );
    check_health(&url).await?;
    let token = std::env::var(TOKEN_ENV).ok().filter(|t| !t.is_empty());
    crate::tui::run(crate::tui::TuiOptions {
        url,
        token,
        workspace,
        initial_model: args.model,
        initial_session: args.session,
        embedded: false,
    })
    .await?;
    Ok(())
}

async fn run_embedded(args: TuiArgs, workspace: PathBuf) -> anyhow::Result<()> {
    let server = EmbeddedServer::start(args.models.as_deref(), &workspace).await?;
    let result = crate::tui::run(crate::tui::TuiOptions {
        url: server.url.clone(),
        token: Some(server.token.clone()),
        workspace,
        initial_model: args.model,
        initial_session: args.session,
        embedded: true,
    })
    .await;
    server.stop().await;
    result?;
    Ok(())
}

/// A private session server running in this process.
struct EmbeddedServer {
    url: String,
    token: String,
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl EmbeddedServer {
    /// Builds the server like `serve` does, binds a loopback port and waits
    /// until it answers; any failure is returned before the terminal is
    /// touched.
    async fn start(models: Option<&Path>, workspace: &Path) -> anyhow::Result<Self> {
        use crate::server::session::{AppState, ServerConfig};

        let token = bearer_token();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().expect("loopback bind"),
            models_path: crate::navigator::resolve_models(models, workspace),
            config_dir: super::config_dir()?,
            allow_non_loopback: false,
            bearer_token: Some(token.clone()),
            history_db: super::history_db_path()?,
        };
        let state = AppState::with_hooks(config, Arc::new(crate::navigator::NavigatorHooks::new()))
            .context("cannot start the embedded server")?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("cannot bind a loopback port for the embedded server")?;
        let url = format!("http://{}", listener.local_addr()?);
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let served = crate::server::routes::serve_until(state, listener, async move {
                let _ = shutdown_rx.await;
            })
            .await;
            if let Err(err) = served {
                tracing::error!("embedded server stopped: {err}");
            }
        });
        let server = Self {
            url,
            token,
            shutdown,
            task,
        };
        if let Err(err) = check_health(&server.url).await {
            server.stop().await;
            return Err(err.context("the embedded server did not come up"));
        }
        Ok(server)
    }

    /// Closes every session gracefully and waits for the server to drain.
    async fn stop(self) {
        let _ = self.shutdown.send(());
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, self.task)
            .await
            .is_err()
        {
            tracing::warn!("the embedded server did not stop within {SHUTDOWN_TIMEOUT:?}");
        }
    }
}

/// A random 32-byte bearer token, hex-encoded: two ULIDs give 128 random
/// bits each, plenty for a per-process secret.
fn bearer_token() -> String {
    format!("{}{}", ulid::Ulid::new(), ulid::Ulid::new()).to_lowercase()
}

/// `GET /v1/health` (no auth), retried briefly so a server that is just
/// starting is not reported as down.
async fn check_health(base: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .build()
        .context("cannot build the HTTP client")?;
    let url = format!("{base}/v1/health");
    let mut last = String::new();
    for attempt in 0..5 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                last = format!("answered HTTP {}", response.status());
                break;
            }
            Err(err) => last = err.without_url().to_string(),
        }
    }
    anyhow::bail!("cannot reach a server at {base}: {last}")
}
