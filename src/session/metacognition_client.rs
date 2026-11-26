//! The metacognition layer's model, as the service's own bounded checks call it: the goal
//! completion check and the parallel permission review. One construction for both, so a change
//! to how the layer's key, client or variant is resolved reaches every caller.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::service::Service;
use crate::provider::assignments::{AssignedModel, Layer};
use crate::provider::client::ModelClient;
use crate::provider::openai::{
    ChatMessage, ChatRequest, ClientCache, StreamOptions, UserProvenance,
};

/// The metacognition model, ready for one tool-free chat call at a time.
pub(crate) struct MetacognitionClient {
    client: ModelClient,
    pub(crate) assigned: AssignedModel,
    key: String,
    cache: Arc<ClientCache>,
    catalog: crate::provider::catalog::ModelCatalog,
}

/// Why one call gave no answer, as the shadow logs class it. Each cause is also logged with
/// `tracing::warn`, so a layer that keeps failing is visible without reading the logs' lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AskError {
    /// The layer's model could not be resolved against the provider's catalogue or variants.
    Resolution,
    /// The provider call failed (transport, status, key).
    Provider,
    /// The caller's deadline passed first.
    Timeout,
    /// The provider answered, but not with usable text.
    Malformed,
}

/// The answer of one call: its text and, when the provider reported them, its token counts.
#[derive(Debug)]
pub(crate) struct Answer {
    pub(crate) text: String,
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
}

impl MetacognitionClient {
    /// The layer's model name, as the shadow logs record it.
    pub(crate) fn model(&self) -> String {
        self.assigned.name.to_string()
    }

    /// One tool-free call: `system` instructions, then `input` as navigator data, at most
    /// `max_tokens` of answer. `None` on any provider failure; never retried.
    pub(crate) async fn ask(
        &self,
        system: &str,
        input: String,
        max_tokens: u64,
        cancel: &CancellationToken,
    ) -> Result<Answer, AskError> {
        let model = self.model();
        let chat = crate::provider::catalog::resolve_chat(
            &self.catalog,
            &self.assigned.resolved,
            &self.key,
            &self.cache,
        )
        .await
        .map_err(|error| {
            tracing::warn!(%model, ?error, "metacognition model could not be resolved");
            AskError::Resolution
        })?;
        let variant =
            crate::provider::layers::chat_variant(&self.assigned, &chat).map_err(|error| {
                tracing::warn!(%model, %error, "metacognition variant could not be resolved");
                AskError::Resolution
            })?;
        let request = ChatRequest {
            model: self.assigned.resolved.model.model.clone(),
            messages: vec![
                ChatMessage::System {
                    content: system.to_owned(),
                },
                ChatMessage::User {
                    provenance: UserProvenance::Navigator,
                    content: input,
                    images: Vec::new(),
                },
            ],
            tools: Vec::new(),
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            max_tokens: Some(max_tokens),
            temperature: None,
            extra: Default::default(),
            images: Default::default(),
        };
        let answer = self
            .client
            .chat(request, variant.as_ref(), cancel, &mut |_| {})
            .await
            .map_err(|error| {
                tracing::warn!(%model, %error, "metacognition call failed");
                match error {
                    crate::provider::error::ProviderError::MalformedResponse(_) => {
                        AskError::Malformed
                    }
                    _ => AskError::Provider,
                }
            })?;
        if answer.text.trim().is_empty() {
            tracing::warn!(%model, "metacognition answered with no text");
            return Err(AskError::Malformed);
        }
        Ok(Answer {
            text: answer.text,
            input_tokens: answer.usage.map(|u| u.prompt_tokens),
            output_tokens: answer.usage.map(|u| u.completion_tokens),
        })
    }
}

impl Service {
    /// The metacognition client, or `None` (silently: the layer is optional) when the layer is
    /// unassigned or disabled, or its key or client is unavailable.
    ///
    /// The same construction as the root watchdog reviewer's: the layer assignment, the
    /// provider key, one `ModelClient` over the service's client cache. The variant is resolved
    /// per call, against the runtime offer, so a model that offers `low` gets it.
    pub(crate) fn metacognition_client(
        &self,
        workspace: &std::path::Path,
    ) -> Option<MetacognitionClient> {
        let (models, _) = crate::provider::config::load_models(
            self.config.models_path.as_deref(),
            workspace,
            &self.config.config_dir,
        )
        .ok()?;
        let assigned = crate::provider::layers::assigned(
            &models,
            &self.config.config_dir,
            Layer::Metacognition,
        )
        .ok()?;
        let resolved = &assigned.resolved;
        let key = crate::provider::auth::resolve_key(
            &self.config.config_dir,
            &resolved.provider_name,
            resolved.provider.api_key_env.as_deref(),
        )
        .ok()?;
        let client = ModelClient::new(
            &resolved.provider_name,
            &resolved.provider,
            key.clone(),
            &self.client_cache,
        )
        .ok()?;
        Some(MetacognitionClient {
            client,
            assigned,
            key,
            cache: self.client_cache.clone(),
            catalog: self.model_catalog().clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A scripted chat provider over a real local socket: no remote service, but the full
    /// provider stack (headers, SSE decode, usage) reads its bytes.
    struct ScriptedServer {
        url: String,
        _keep: tokio::task::JoinHandle<()>,
    }

    impl Drop for ScriptedServer {
        fn drop(&mut self) {
            self._keep.abort();
        }
    }

    /// Serves `response` to every connection, ignoring the request once its body arrived.
    async fn scripted(response: &'static [u8]) -> ScriptedServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let keep = tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let s = String::from_utf8_lossy(&buf);
                    if let Some(idx) = s.find("\r\n\r\n") {
                        let content_len = header_content_length(&s[..idx]);
                        if buf.len() >= idx + 4 + content_len {
                            break;
                        }
                    }
                }
                let _ = sock.write_all(response).await;
                let _ = sock.shutdown().await;
            }
        });
        ScriptedServer {
            url: format!("http://{addr}/v1"),
            _keep: keep,
        }
    }

    fn header_content_length(header: &str) -> usize {
        header
            .lines()
            .find_map(|l| {
                l.to_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap_or(0))
            })
            .unwrap_or(0)
    }

    /// The assigned metacognition model the client is built around: a chat model with an
    /// explicit context window and no catalog format, so the per-call resolution reads the
    /// catalog's cache and never touches the network.
    fn client(base_url: &str) -> MetacognitionClient {
        let catalog = serde_json::json!({
            "providers": {"up": {"base_url": base_url}},
            "models": {
                "up/think": {
                    "provider": "up",
                    "model": "think",
                    "api": "chat",
                    "context_window": 128000,
                }
            }
        });
        let models = crate::provider::config::ModelsConfig::parse(&catalog.to_string()).unwrap();
        let name = crate::provider::model_name::ModelName::parse("up/think", &models).unwrap();
        let resolved = models.resolve_name(&name).unwrap();
        MetacognitionClient {
            client: ModelClient::new(
                "up",
                &resolved.provider,
                "test-key".into(),
                &ClientCache::new(),
            )
            .unwrap(),
            assigned: AssignedModel {
                name,
                resolved,
                variant: None,
                variant_choice: crate::settings::AssignmentVariant::Default,
                prefer_low: false,
            },
            key: "test-key".into(),
            cache: Arc::new(ClientCache::new()),
            catalog: crate::provider::catalog::ModelCatalog::default(),
        }
    }

    // Given a provider that answers with an HTTP error, and one that cannot be reached
    // When the metacognition layer asks
    // Then the failure is classed `Provider`: transport and status errors never masquerade
    //      as a malformed answer
    #[tokio::test]
    async fn transport_and_api_failures_are_provider_errors() {
        use tokio_util::sync::CancellationToken;

        let e500 = "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 29\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"boom\"}}";
        let server = scripted(e500.as_bytes()).await;
        let error = client(&server.url)
            .ask("system", "input".into(), 100, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error, AskError::Provider);

        // A port nothing listens on: the connection is refused before any HTTP.
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let error = client(&format!("http://127.0.0.1:{port}/v1"))
            .ask("system", "input".into(), 100, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error, AskError::Provider);
    }

    // Given a stream that completes but carries no text, and one that answers with text and
    //       its token usage
    // When the metacognition layer reads it
    // Then the empty answer is `Malformed` (the provider answered, but with nothing usable),
    //      and the usable one carries its text and the provider's token counts
    #[tokio::test]
    async fn a_blank_answer_is_malformed_and_a_usable_one_carries_its_tokens() {
        use tokio_util::sync::CancellationToken;

        let blank =
            crate::provider::test_support::http_sse(&crate::provider::test_support::sse_body(&[
                "{\"choices\":[]}",
            ]));
        let server = scripted(Box::leak(blank.into_boxed_slice())).await;
        let error = client(&server.url)
            .ask("system", "input".into(), 100, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error, AskError::Malformed);

        let chunk = serde_json::json!({
            "choices": [{"delta": {"content": "the answer"}}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 3},
        })
        .to_string();
        let sse =
            crate::provider::test_support::http_sse(&crate::provider::test_support::sse_body(&[
                &chunk,
            ]));
        let server = scripted(Box::leak(sse.into_boxed_slice())).await;
        let answer = client(&server.url)
            .ask("system", "input".into(), 100, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(answer.text, "the answer");
        assert_eq!(answer.input_tokens, Some(12));
        assert_eq!(answer.output_tokens, Some(3));
    }
}
