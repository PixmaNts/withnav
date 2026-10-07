//! The private session server both embedded clients run in-process: the terminal client (`navai
//! tui`) and the browser client (`navai web`). It binds loopback only, protects every `/v1` route
//! with a per-process bearer token, and stops gracefully when its owner is done.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;

use crate::server::WebConfig;

const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
const HEALTH_DIAGNOSTIC_MAX_BYTES: usize = 16 * 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// The canonical form of a workspace argument, which must be an existing directory.
pub(super) fn resolve_workspace(path: &Path) -> anyhow::Result<PathBuf> {
    let resolved = std::fs::canonicalize(path)
        .with_context(|| format!("workspace {} is not accessible", path.display()))?;
    anyhow::ensure!(
        resolved.is_dir(),
        "workspace {} is not a directory",
        resolved.display()
    );
    Ok(resolved)
}

/// A private session server running in this process.
pub(super) struct EmbeddedServer {
    pub(super) url: String,
    pub(super) token: String,
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl EmbeddedServer {
    /// Builds the server like `serve` does, binds a loopback port and waits
    /// until it answers; any failure is returned before the terminal is
    /// touched.
    ///
    /// `workspaces` are all preflight-checked; the first one decides which
    /// `models.json` is found. `web` makes the server also serve the browser
    /// client; `port` 0 takes any free loopback port.
    pub(super) async fn start(
        models: Option<&Path>,
        workspaces: &[PathBuf],
        web: Option<WebConfig>,
        port: u16,
    ) -> anyhow::Result<Self> {
        use crate::server::session::{AppState, ServerConfig};

        let config_dir = super::config_dir()?;
        let rules_roots = crate::agent::instructions::RulesRoots::from_user_home();
        let primary = workspaces
            .first()
            .context("at least one workspace is required")?;
        for workspace in workspaces {
            crate::session::preflight::check(&config_dir, models, workspace, &rules_roots).await?;
        }
        let token = bearer_token();
        let config = ServerConfig {
            bind: std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            models_path: crate::navigator::resolve_models(models, primary),
            config_dir,
            allow_non_loopback: false,
            bearer_token: Some(token.clone()),
            history_db: super::history_db_path()?,
            data_dir: crate::paths::data_dir()
                .ok_or_else(|| anyhow::anyhow!("cannot resolve data dir"))?,
            rules_roots,
            web,
        };
        let config_bind = config.bind;
        let state = AppState::with_hooks(config, Arc::new(crate::navigator::NavigatorHooks::new()))
            .context("cannot start the embedded server")?;
        let listener = tokio::net::TcpListener::bind(config_bind)
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
    pub(super) async fn stop(self) {
        let _ = self.shutdown.send(());
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, self.task)
            .await
            .is_err()
        {
            tracing::warn!("the embedded server did not stop within {SHUTDOWN_TIMEOUT:?}");
        }
    }
}

/// A per-process bearer token: two ULIDs in lowercase Crockford base32
/// (52 characters, not hex), whose random parts give plenty of entropy for a
/// secret that only lives as long as the process.
fn bearer_token() -> String {
    format!("{}{}", ulid::Ulid::new(), ulid::Ulid::new()).to_lowercase()
}

/// `GET /v1/health` (no auth), retried briefly so a server that is just
/// starting is not reported as down.
pub(super) async fn check_health(base: &str) -> anyhow::Result<()> {
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
                if let Some(body) = health_diagnostic(response).await {
                    if let Some(message) = body.get("error").and_then(serde_json::Value::as_str) {
                        let diagnostic: String = message
                            .chars()
                            .filter(|c| !c.is_control())
                            .take(1024)
                            .collect();
                        last.push_str(": ");
                        last.push_str(&diagnostic);
                    }
                }
                break;
            }
            Err(err) => last = err.without_url().to_string(),
        }
    }
    anyhow::bail!("cannot reach a server at {base}: {last}")
}

/// Never drain an untrusted error response merely to display its diagnostic.
async fn health_diagnostic(mut response: reqwest::Response) -> Option<serde_json::Value> {
    if response
        .content_length()
        .is_some_and(|length| length > HEALTH_DIAGNOSTIC_MAX_BYTES as u64)
    {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > HEALTH_DIAGNOSTIC_MAX_BYTES - body.len() {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_chunked_oversize_stops_reading_before_eof_and_hides_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n").await.unwrap();
            let mut body = br#"{"error":"OVERSIZED_SENTINEL","padding":""#.to_vec();
            body.resize(HEALTH_DIAGNOSTIC_MAX_BYTES + 1, b'x');
            socket
                .write_all(format!("{:x}\r\n", body.len()).as_bytes())
                .await
                .unwrap();
            socket.write_all(&body).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
            // No terminating chunk: returning before EOF proves the read cap.
            std::future::pending::<()>().await;
        });
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            check_health(&format!("http://{address}")),
        )
        .await
        .expect("oversized diagnostics must not wait for EOF")
        .unwrap_err()
        .to_string();
        assert!(
            error.ends_with("answered HTTP 503 Service Unavailable"),
            "{error}"
        );
        assert!(!error.contains("OVERSIZED_SENTINEL"));
        server.abort();
    }

    #[tokio::test]
    async fn health_malformed_body_is_status_only_and_valid_diagnostic_is_filtered() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        for (body, expected) in [
            ("MALFORMED_SENTINEL".to_string(), None),
            (
                serde_json::json!({"error":format!("bad\n\u{1b}{}", "x".repeat(1100))}).to_string(),
                Some(format!("bad{}", "x".repeat(1021))),
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/health"))
                .respond_with(ResponseTemplate::new(503).set_body_string(body))
                .mount(&server)
                .await;
            let error = check_health(&server.uri()).await.unwrap_err().to_string();
            match expected {
                Some(message) => assert!(error.ends_with(&format!(": {message}")), "{error}"),
                None => assert!(
                    error.ends_with("answered HTTP 503 Service Unavailable"),
                    "{error}"
                ),
            }
            assert!(
                !error.contains("MALFORMED_SENTINEL")
                    && !error.contains('\n')
                    && !error.contains('\u{1b}')
            );
        }
    }
}
