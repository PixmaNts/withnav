//! One authenticated HTTP client for one provider endpoint: the shared
//! send/cancel/deadline/error-body handling behind the streaming chat call
//! and the plain JSON POSTs (fill-in-the-middle, System One).

use std::future::Future;
use std::time::Duration;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::config::{ProviderConfig, Variant};
use crate::error::ProviderError;
use crate::openai::{ChatRequest, ClientCache};
use crate::{AssistantMessage, StreamDelta, extract_error_message, start_and_consume};

/// The largest model list accepted.
const MAX_CATALOG_BYTES: usize = 2_000_000;

/// A provider's base URL, auth and extra headers over a pooled
/// `reqwest::Client`. Build it once and reuse it across calls.
#[derive(Clone)]
pub struct ModelClient {
    base_url: String,
    headers: Vec<(String, String)>,
    api_key: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for ModelClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl ModelClient {
    /// The underlying `reqwest::Client` comes from `cache`, so its timeouts
    /// are the ones in `provider`.
    pub fn new(
        provider_name: &str,
        provider: &ProviderConfig,
        api_key: String,
        cache: &ClientCache,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            base_url: provider.base_url.trim_end_matches('/').to_string(),
            headers: provider
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            api_key,
            http: cache.get(provider_name, provider)?,
        })
    }

    /// A POST to `{base_url}/{endpoint}` carrying the bearer key.
    pub fn post(&self, endpoint: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}/{}", self.base_url, endpoint))
            .bearer_auth(&self.api_key)
    }

    /// Add the provider's configured extra headers to a request.
    pub fn prepare_request(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        req
    }

    /// [`Self::new`] over the cache's no-cross-origin-redirect client, for
    /// the model list: its credentials and headers never leave the endpoint.
    pub fn for_catalog(
        provider_name: &str,
        provider: &ProviderConfig,
        api_key: String,
        cache: &ClientCache,
    ) -> Result<Self, ProviderError> {
        let mut client = Self::new(provider_name, provider, api_key, cache)?;
        client.http = cache.get_same_origin(provider_name, provider)?;
        Ok(client)
    }

    /// Fetch model metadata (bounded in time and size). A redirect to
    /// another origin is not followed, so this client's credentials and
    /// headers never reach it.
    pub async fn get_json(
        &self,
        endpoint: &str,
        budget: Duration,
        cancel: &CancellationToken,
    ) -> Result<bytes::Bytes, ProviderError> {
        let url = format!("{}/{}", self.base_url, endpoint);
        let req = self.prepare_request(self.http.get(url).bearer_auth(&self.api_key));
        let call = async {
            let mut response = cancellable(cancel, req.send()).await??;
            let status = response.status();
            if !status.is_success() {
                return Err(ProviderError::Api {
                    status: status.as_u16(),
                    message: if status.is_redirection() {
                        "model catalog redirected to another origin (not followed)"
                    } else {
                        "model catalog request failed"
                    }
                    .into(),
                    partial: None,
                });
            }
            let too_big =
                || ProviderError::MalformedResponse("model catalog response exceeds 2 MB".into());
            if response
                .content_length()
                .is_some_and(|n| n > MAX_CATALOG_BYTES as u64)
            {
                return Err(too_big());
            }
            let mut body = Vec::new();
            while let Some(chunk) = cancellable(cancel, response.chunk()).await?? {
                if body.len() + chunk.len() > MAX_CATALOG_BYTES {
                    return Err(too_big());
                }
                body.extend_from_slice(&chunk);
            }
            Ok(bytes::Bytes::from(body))
        };
        tokio::time::timeout(budget, call)
            .await
            .map_err(|_| ProviderError::Api {
                status: 0,
                message: "model catalog deadline exceeded".into(),
                partial: None,
            })?
    }

    /// POSTs `body` as JSON to `{base_url}/{endpoint}` and returns the raw
    /// success body; the caller parses it. A non-2xx status becomes
    /// [`ProviderError::Api`] carrying the API's error message when the body
    /// has one. Cancellation covers the send and both bodies; `budget` is one
    /// deadline over send, body and error body.
    pub async fn post_json<B: Serialize>(
        &self,
        endpoint: &str,
        body: &B,
        budget: Duration,
        cancel: &CancellationToken,
    ) -> Result<bytes::Bytes, ProviderError> {
        let req = self.prepare_request(self.post(endpoint)).json(body);
        let call = async {
            let response = cancellable(cancel, req.send()).await??;
            let status = response.status();
            if !status.is_success() {
                let text = super::read_error_body(response, cancel).await?;
                let message = crate::scrub_data_urls(&extract_error_message(&text).unwrap_or(text));
                return Err(ProviderError::Api {
                    status: status.as_u16(),
                    message,
                    partial: None,
                });
            }
            Ok(cancellable(cancel, response.bytes()).await??)
        };
        match tokio::time::timeout(budget, call).await {
            Ok(result) => result,
            Err(_) => Err(ProviderError::Api {
                status: 0,
                message: format!("deadline exceeded ({} ms)", budget.as_millis()),
                partial: None,
            }),
        }
    }

    /// The streaming chat call: sends `request` with `variant` (the selected
    /// variant's overrides, if any) merged into the body under `cancel`, then
    /// streams the answer through `on_delta` until `[DONE]`.
    pub async fn chat(
        &self,
        mut request: ChatRequest,
        variant: Option<&Variant>,
        cancel: &CancellationToken,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> Result<AssistantMessage, ProviderError> {
        if let Some(variant) = variant {
            request.apply_variant(variant);
        }
        let response = cancellable(
            cancel,
            self.prepare_request(self.post("chat/completions"))
                .json(&request)
                .send(),
        )
        .await??;
        start_and_consume(response, cancel, on_delta).await
    }
}

/// Awaits `fut` unless `cancel` fires first.
async fn cancellable<T>(
    cancel: &CancellationToken,
    fut: impl Future<Output = T>,
) -> Result<T, ProviderError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(ProviderError::Canceled { partial: None }),
        r = fut => Ok(r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn client_at(addr: std::net::SocketAddr) -> ModelClient {
        let provider = ProviderConfig {
            base_url: format!("http://{addr}/v1/"),
            api_key_env: None,
            headers: Default::default(),
            connect_timeout_ms: 1_000,
            // Far above every budget below: only the call's own deadline can fire.
            request_timeout_ms: 600_000,
            catalog_format: None,
        };
        ModelClient::new("p", &provider, "key".into(), &ClientCache::new()).unwrap()
    }

    /// Accepts one connection, reads the request, writes `head` and holds the
    /// socket open without sending more.
    async fn serve_then_stall(head: &'static [u8]) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            if !head.is_empty() {
                let _ = sock.write_all(head).await;
                let _ = sock.flush().await;
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        addr
    }

    #[tokio::test]
    async fn an_error_body_is_read_under_the_deadline() {
        // Promises 100 body bytes, sends none.
        let addr =
            serve_then_stall(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 100\r\n\r\n")
                .await;
        let started = Instant::now();
        let err = client_at(addr)
            .post_json(
                "x",
                &serde_json::json!({}),
                Duration::from_millis(200),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        match err {
            ProviderError::Api {
                status: 0, message, ..
            } => assert!(message.contains("deadline"), "{message}"),
            other => panic!("expected the deadline error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_during_a_slow_send_returns_canceled_promptly() {
        // Accepts and reads the request but never answers.
        let addr = serve_then_stall(b"").await;
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let started = Instant::now();
        let err = client_at(addr)
            .post_json(
                "x",
                &serde_json::json!({}),
                Duration::from_secs(30),
                &cancel,
            )
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(matches!(err, ProviderError::Canceled { partial: None }));
    }

    #[tokio::test]
    async fn cancel_during_a_slow_chat_send_returns_canceled_promptly() {
        let addr = serve_then_stall(b"").await;
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let request = ChatRequest {
            model: "m".into(),
            messages: vec![],
            tools: vec![],
            stream: true,
            stream_options: None,
            max_tokens: None,
            temperature: None,
            extra: Default::default(),
            images: Default::default(),
        };
        let started = Instant::now();
        let err = client_at(addr)
            .chat(request, None, &cancel, &mut |_| {})
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(matches!(err, ProviderError::Canceled { partial: None }));
    }

    #[tokio::test]
    async fn the_api_error_message_is_extracted() {
        let addr = serve_then_stall(
            b"HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 40\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"slow down please\"}}",
        )
        .await;
        let err = client_at(addr)
            .post_json(
                "x",
                &serde_json::json!({}),
                Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        match err {
            ProviderError::Api {
                status, message, ..
            } => {
                assert_eq!(status, 429);
                assert_eq!(message, "slow down please");
            }
            other => panic!("expected Api 429, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_success_returns_the_raw_body() {
        let addr = serve_then_stall(
            b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot json",
        )
        .await;
        let bytes = client_at(addr)
            .post_json(
                "x",
                &serde_json::json!({}),
                Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        // The body is handed back unparsed, whatever it holds.
        assert_eq!(&bytes[..], b"not json");
    }
}
