//! The fill-in-the-middle engine of the reflex layer: `POST
//! {base}/fim/completions` in the Inception Labs shape — body `{model,
//! prompt, suffix, max_tokens, temperature}`, answer text in
//! `choices[0].text`, usage in `usage.{prompt_tokens, completion_tokens}`.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::navigator::ports::{FimAnswer, FimModel, FimRequest, ModelError};
use crate::provider::client::ModelClient;
use crate::provider::config::{ProviderConfig, ResolvedModel};
use crate::provider::openai::ClientCache;
use crate::provider::Usage;

/// Wall-clock deadline of one reflex call, covering send and body. Applied
/// by clamping the provider's configured `request_timeout_ms` when the
/// client is built, so a shared per-provider client can never hang a
/// suggestion behind a timeout meant for slower layers.
pub const REFLEX_DEADLINE_MS: u64 = 1500;

/// Sampling temperature for suggestions: low, but not exactly zero (some
/// endpoints reject a literal 0).
const TEMPERATURE: f64 = 0.1;

/// A fill-in-the-middle model over the Inception endpoint. One HTTP client
/// per provider, built once and shared through the [`ClientCache`].
pub struct InceptionFim {
    provider_name: String,
    provider: ProviderConfig,
    model: String,
    api_key: String,
    clients: Arc<ClientCache>,
    /// Built on the first call (the constructor cannot fail), then reused.
    client: OnceLock<ModelClient>,
}

impl InceptionFim {
    /// Builds the engine from a resolved `reflex`-style alias. The client
    /// timeout is clamped to [`REFLEX_DEADLINE_MS`] even when the provider
    /// configures a longer one for chat layers sharing the provider.
    pub fn new(resolved: ResolvedModel, api_key: String, clients: Arc<ClientCache>) -> Self {
        let mut provider = resolved.provider.clone();
        provider.request_timeout_ms = provider.request_timeout_ms.min(REFLEX_DEADLINE_MS);
        Self {
            provider_name: resolved.provider_name,
            provider,
            model: resolved.model.model,
            api_key,
            clients,
            client: OnceLock::new(),
        }
    }

    fn client(&self) -> Result<&ModelClient, ModelError> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        let built = ModelClient::new(
            &self.provider_name,
            &self.provider,
            self.api_key.clone(),
            &self.clients,
        )?;
        Ok(self.client.get_or_init(|| built))
    }

    /// The deadline actually applied to this engine's HTTP client.
    pub fn deadline(&self) -> Duration {
        Duration::from_millis(self.provider.request_timeout_ms)
    }
}

#[derive(Serialize)]
struct FimBody<'a> {
    model: &'a str,
    prompt: &'a str,
    suffix: &'a str,
    max_tokens: u64,
    temperature: f64,
}

#[derive(Deserialize)]
struct FimResponseBody {
    #[serde(default)]
    choices: Vec<FimChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct FimChoice {
    #[serde(default)]
    text: String,
}

impl FimModel for InceptionFim {
    fn complete<'a>(
        &'a self,
        request: FimRequest,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<FimAnswer, ModelError>> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let client = self.client()?;
            let body = FimBody {
                model: self.model.as_str(),
                prompt: request.prompt.as_str(),
                suffix: request.suffix.as_str(),
                max_tokens: request.max_tokens,
                temperature: TEMPERATURE,
            };
            let bytes = client
                .post_json("fim/completions", &body, self.deadline(), cancel)
                .await?;
            let parsed: FimResponseBody = serde_json::from_slice(&bytes)
                .map_err(|e| ModelError::Malformed(format!("fim response is not JSON: {e}")))?;
            let Some(choice) = parsed.choices.into_iter().next() else {
                return Err(ModelError::Malformed(
                    "fim response carries no choices".to_string(),
                ));
            };
            Ok(FimAnswer {
                text: choice.text,
                usage: parsed.usage.unwrap_or_default(),
                latency: started.elapsed(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::config::{ApiKind, ModelConfig, ReasoningHistory};

    fn resolved_with_timeout(base_url: &str, request_timeout_ms: u64) -> ResolvedModel {
        ResolvedModel {
            alias: "reflex".to_string(),
            provider_name: "inception".to_string(),
            provider: ProviderConfig {
                base_url: base_url.to_string(),
                api_key_env: None,
                headers: Default::default(),
                connect_timeout_ms: 1_000,
                request_timeout_ms,
            },
            model: ModelConfig {
                provider: "inception".to_string(),
                model: "mercury-edit-2".to_string(),
                api: ApiKind::Fim,
                context_window: None,
                max_output_tokens: None,
                supports_tools: false,
                supports_parallel_tool_calls: None,
                thinking: None,
                reasoning_history: ReasoningHistory::Preserve,
            },
        }
    }

    #[test]
    fn the_configured_timeout_is_clamped_to_the_reflex_deadline() {
        let long = InceptionFim::new(
            resolved_with_timeout("http://x/v1", 600_000),
            "k".to_string(),
            Arc::new(ClientCache::new()),
        );
        assert_eq!(long.deadline(), Duration::from_millis(REFLEX_DEADLINE_MS));
        let short = InceptionFim::new(
            resolved_with_timeout("http://x/v1", 500),
            "k".to_string(),
            Arc::new(ClientCache::new()),
        );
        assert_eq!(short.deadline(), Duration::from_millis(500));
    }
}
