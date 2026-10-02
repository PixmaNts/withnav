use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ProviderError;
use crate::provider::config::ProviderConfig;
use crate::provider::Usage;

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        reasoning_content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        tool_calls: Vec<RequestToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: RequestFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolDef {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub error: Option<ApiErrorBody>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub delta: ChunkDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChunkDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallDelta {
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorBody {
    pub message: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

pub fn build_client(provider: &ProviderConfig) -> Result<reqwest::Client, ProviderError> {
    build_client_with(provider, reqwest::redirect::Policy::default())
}

/// A client that follows only same-origin redirects (same scheme, host and
/// port, at most three), so credentials and custom headers never reach
/// another origin.
fn build_same_origin_client(provider: &ProviderConfig) -> Result<reqwest::Client, ProviderError> {
    build_client_with(
        provider,
        reqwest::redirect::Policy::custom(|attempt| {
            let same = attempt
                .previous()
                .last()
                .is_some_and(|p| p.origin() == attempt.url().origin());
            if same && attempt.previous().len() <= 3 {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }),
    )
}

fn build_client_with(
    provider: &ProviderConfig,
    redirect: reqwest::redirect::Policy,
) -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .redirect(redirect)
        .connect_timeout(Duration::from_millis(provider.connect_timeout_ms))
        // Total deadline covering send and body: the configured
        // request_timeout_ms must actually be applied to the client.
        .timeout(Duration::from_millis(provider.request_timeout_ms))
        .no_proxy()
        .build()
        .map_err(ProviderError::Http)
}

pub fn build_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    api_key: &str,
    body: &ChatRequest,
) -> reqwest::RequestBuilder {
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let mut req = client.post(url).bearer_auth(api_key).json(body);
    for (k, v) in &provider.headers {
        req = req.header(k, v);
    }
    req
}

/// A per-provider HTTP client cache: one `reqwest::Client` built once per
/// provider and reused, instead of rebuilding a client (and its connection
/// pool) on every call.
///
/// The key includes the timeouts, not only the provider name: layers sharing
/// one provider apply different deadlines (observed 2026-09-29: the instinct's
/// 2 s client was reused by the metacognition on the same `omniroute`
/// provider, cutting every remark at exactly 2000 ms).
#[derive(Default)]
pub struct ClientCache {
    clients: std::sync::Mutex<std::collections::HashMap<(String, u64, u64), reqwest::Client>>,
    same_origin: std::sync::Mutex<std::collections::HashMap<(String, u64, u64), reqwest::Client>>,
}

impl ClientCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the shared client for the named provider, building it on
    /// first use.
    pub fn get(
        &self,
        provider_name: &str,
        provider: &ProviderConfig,
    ) -> Result<reqwest::Client, ProviderError> {
        // The lock is only held for a map lookup after the first build, so
        // a Mutex stays cheap and the fast path needs no extra allocation.
        let key = (
            provider_name.to_string(),
            provider.connect_timeout_ms,
            provider.request_timeout_ms,
        );
        let mut clients = self.clients.lock().expect("client cache poisoned");
        if let Some(c) = clients.get(&key) {
            return Ok(c.clone());
        }
        let built = build_client(provider)?;
        clients.insert(key, built.clone());
        Ok(built)
    }

    /// [`Self::get`] for a client that never follows a redirect to another
    /// origin (see [`build_same_origin_client`]).
    pub fn get_same_origin(
        &self,
        provider_name: &str,
        provider: &ProviderConfig,
    ) -> Result<reqwest::Client, ProviderError> {
        let key = (
            provider_name.to_string(),
            provider.connect_timeout_ms,
            provider.request_timeout_ms,
        );
        let mut clients = self.same_origin.lock().expect("client cache poisoned");
        if let Some(c) = clients.get(&key) {
            return Ok(c.clone());
        }
        let built = build_same_origin_client(provider)?;
        clients.insert(key, built.clone());
        Ok(built)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_reasoning_chunk() {
        let raw = include_str!("../../tests/fixtures/chunks/reasoning.json");
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        assert_eq!(
            chunk.choices[0].delta.reasoning_content.as_deref(),
            Some("We")
        );
        assert!(chunk.choices[0].delta.content.is_none());
    }

    #[test]
    fn deserialize_toolcall_first_fragment() {
        let raw = include_str!("../../tests/fixtures/chunks/toolcall_first.json");
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        let tc = &chunk.choices[0].delta.tool_calls[0];
        assert_eq!(tc.index, 0);
        assert_eq!(tc.id.as_deref(), Some("call_00_199TsKpFMjN3EXHTQZFE3344"));
        assert_eq!(
            tc.function.as_ref().unwrap().name.as_deref(),
            Some("get_weather")
        );
    }

    #[test]
    fn deserialize_toolcall_args_fragment_has_no_id_or_name() {
        let raw = include_str!("../../tests/fixtures/chunks/toolcall_args.json");
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        let tc = &chunk.choices[0].delta.tool_calls[0];
        assert_eq!(tc.index, 0);
        assert!(tc.id.is_none());
        let f = tc.function.as_ref().unwrap();
        assert!(f.name.is_none());
        assert_eq!(f.arguments.as_deref(), Some("{\"city\":\"Paris\"}"));
    }

    #[test]
    fn deserialize_final_usage_chunk() {
        let raw = include_str!("../../tests/fixtures/chunks/final_usage.json");
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        assert_eq!(
            chunk.choices[0].finish_reason.as_deref(),
            Some("tool_calls")
        );
        let usage = chunk.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 365);
        assert_eq!(usage.completion_tokens, 63);
        assert_eq!(usage.total_tokens, 428);
    }

    #[test]
    fn deserialize_error_object_routes_to_error_branch() {
        let raw = r#"{"error":{"message":"model overloaded","type":"server_error","code":"503"}}"#;
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        assert!(chunk.error.is_some());
        assert_eq!(chunk.error.unwrap().message, "model overloaded");
    }

    #[test]
    fn client_cache_reuses_one_client_per_provider() {
        let provider = ProviderConfig {
            base_url: "http://cache-test.invalid/v1".to_string(),
            api_key_env: None,
            headers: Default::default(),
            connect_timeout_ms: 100,
            request_timeout_ms: 250,
        };
        let cache = ClientCache::new();
        let _ = cache.get("p", &provider).unwrap();
        let _ = cache.get("p", &provider).unwrap();
        let _ = cache.get("q", &provider).unwrap();
        // Two providers, two entries: repeated gets for the same provider
        // never grow the cache (the client is reused, not rebuilt).
        assert_eq!(cache.clients.lock().unwrap().len(), 2);
    }

    /// A local server that answers every request `200` after `delay`.
    async fn slow_server(delay: Duration) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    tokio::time::sleep(delay).await;
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });
        format!("http://{addr}/v1")
    }

    fn provider_at(base_url: String, request_timeout_ms: u64) -> ProviderConfig {
        ProviderConfig {
            base_url,
            api_key_env: None,
            headers: Default::default(),
            connect_timeout_ms: 1_000,
            request_timeout_ms,
        }
    }

    #[tokio::test]
    async fn client_cache_keeps_distinct_deadlines_apart_on_one_provider() {
        let url = slow_server(Duration::from_millis(300)).await;
        let fast = provider_at(url.clone(), 100);
        let slow = provider_at(url.clone(), 10_000);
        let cache = ClientCache::new();
        // The short-deadline client is built first; a layer with a long
        // deadline on the same provider must not inherit it.
        let fast_client = cache.get("omniroute", &fast).unwrap();
        let slow_client = cache.get("omniroute", &slow).unwrap();

        let err = fast_client.get(&url).send().await.unwrap_err();
        assert!(err.is_timeout(), "{err}");
        let ok = slow_client.get(&url).send().await.unwrap();
        assert_eq!(ok.status(), 200);
    }

    #[tokio::test]
    async fn build_client_applies_the_configured_request_timeout() {
        let url = slow_server(Duration::from_secs(30)).await;
        let client = build_client(&provider_at(url.clone(), 200)).unwrap();
        let started = std::time::Instant::now();
        let err = client.get(&url).send().await.unwrap_err();
        let elapsed = started.elapsed();
        assert!(err.is_timeout(), "{err}");
        assert!(
            elapsed >= Duration::from_millis(150) && elapsed < Duration::from_secs(5),
            "failed after {elapsed:?}, not near the 200 ms deadline"
        );
    }
}
