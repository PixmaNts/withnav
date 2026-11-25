//! Workspace resolution and the loopback health check every local client uses before it talks
//! to a server.
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;

const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
const HEALTH_DIAGNOSTIC_MAX_BYTES: usize = 16 * 1024;

/// The canonical form of a workspace argument, which must be an existing directory.
pub(super) fn resolve_workspace(path: &Path) -> anyhow::Result<PathBuf> {
    let resolved = std::fs::canonicalize(path)
        .with_context(|| format!("workspace {} is not accessible", path.display()))?;
    let resolved = crate::tools::canonical_path(&resolved)?;
    anyhow::ensure!(
        resolved.is_dir(),
        "workspace {} is not a directory",
        resolved.display()
    );
    Ok(resolved)
}

/// `GET /v1/health` (no auth), retried briefly so a server that is just
/// starting is not reported as down.
pub(super) async fn check_health(base: &str) -> anyhow::Result<()> {
    let mut builder = reqwest::Client::builder().timeout(HEALTH_TIMEOUT);
    if withnav_client::client::is_loopback(base) {
        builder = builder.no_proxy();
    }
    let client = builder.build().context("cannot build the HTTP client")?;
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
                if let Some(body) = health_diagnostic(response).await
                    && let Some(message) = body.get("error").and_then(serde_json::Value::as_str)
                {
                    let diagnostic: String = message
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(1024)
                        .collect();
                    last.push_str(": ");
                    last.push_str(&diagnostic);
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
