//! The instinct layer's [`DecisionModel`] over the System One wire API:
//! `POST {base_url}/systemone` with body `{model, state, questions}` and the
//! answer in `answers` (shape captured in
//! `tests/fixtures/navigator/systemone_multi.json`).
//!
//! One HTTP client per provider via [`ClientCache`], one ~2 s deadline
//! covering send and body, cancellation honoured between send and body.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::ProviderError;
use crate::navigator::ports::{Answers, DecisionModel, ModelError, Questions};
use crate::provider::client::ModelClient;
use crate::provider::config::{ModelConfig, ProviderConfig};
use crate::provider::openai::ClientCache;

/// The instinct's wall-clock budget for one System One call, covering send
/// and body (spec: ~2 s).
pub const SYSTEMONE_DEADLINE: Duration = Duration::from_millis(2_000);

/// The outgoing System One request body.
#[derive(Debug, Serialize)]
struct SystemOneRequest<'a> {
    model: &'a str,
    state: &'a str,
    questions: &'a Questions,
}

/// The parts of the System One response this layer reads. `usage` follows the
/// captured fixture (`input_tokens`, `output_tokens`).
#[derive(Debug, Deserialize)]
struct SystemOneResponse {
    #[serde(default)]
    answers: Answers,
    #[serde(default)]
    usage: Option<SystemOneUsage>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct SystemOneUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

/// One instinct answer plus the cost fields the log wants.
#[derive(Debug, Clone)]
pub struct SystemOneAnswer {
    pub answers: Answers,
    pub latency: Duration,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// A [`DecisionModel`] answering typed questions over the System One
/// endpoint. Cheap to clone: the HTTP client is shared through the
/// [`ClientCache`].
#[derive(Clone)]
pub struct SystemOneClient {
    base_url: String,
    model: String,
    client: ModelClient,
}

impl std::fmt::Debug for SystemOneClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemOneClient")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl SystemOneClient {
    /// Builds the client for one resolved model. The API key is resolved once
    /// here (auth store or environment) and never logged.
    pub fn new(
        provider_name: &str,
        provider: &ProviderConfig,
        model: &ModelConfig,
        config_dir: &std::path::Path,
        cache: std::sync::Arc<ClientCache>,
    ) -> Result<Self, ProviderError> {
        let api_key = crate::provider::auth::resolve_key(
            config_dir,
            provider_name,
            provider.api_key_env.as_deref(),
        )?;
        let base_url = provider.base_url.trim_end_matches('/').to_string();
        // The per-layer deadline overrides the configured request timeout, so
        // a slow-configured provider still answers within ~2 s.
        let deadline_ms = SYSTEMONE_DEADLINE.as_millis() as u64;
        let layer_config = ProviderConfig {
            base_url: base_url.clone(),
            api_key_env: None,
            headers: provider.headers.clone(),
            connect_timeout_ms: deadline_ms,
            request_timeout_ms: deadline_ms,
        };
        Ok(Self {
            base_url,
            model: model.model.clone(),
            client: ModelClient::new(provider_name, &layer_config, api_key, &cache)?,
        })
    }

    /// The model identifier sent on the wire (`model` field of the body).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Resolves the `instinct` alias from a loaded models config and builds
    /// the client for it.
    pub fn from_alias(
        models: &crate::provider::config::ModelsConfig,
        alias: &str,
        config_dir: &std::path::Path,
        cache: std::sync::Arc<ClientCache>,
    ) -> Result<Self, ProviderError> {
        let resolved = models.resolve(alias)?;
        Self::new(
            &resolved.provider_name,
            &resolved.provider,
            &resolved.model,
            config_dir,
            cache,
        )
    }

    /// One System One call: POST {base_url}/systemone under the ~2 s
    /// deadline, shared client, cancellation between send and body.
    pub async fn ask(
        &self,
        state: &str,
        questions: &Questions,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<SystemOneAnswer, ModelError> {
        let started = std::time::Instant::now();
        let body = SystemOneRequest {
            model: &self.model,
            state,
            questions,
        };
        let bytes = self
            .client
            .post_json("systemone", &body, SYSTEMONE_DEADLINE, cancel)
            .await?;
        let latency = started.elapsed();

        let parsed: SystemOneResponse = serde_json::from_slice(&bytes).map_err(|e| {
            let excerpt = std::str::from_utf8(&bytes)
                .map(|s| crate::navigator::log::excerpt(s, 200))
                .unwrap_or_else(|_| format!("{} bytes", bytes.len()));
            ModelError::Malformed(format!(
                "systemone response did not parse: {e}; body: {excerpt}"
            ))
        })?;
        Ok(SystemOneAnswer {
            answers: parsed.answers,
            latency,
            input_tokens: parsed.usage.map(|u| u.input_tokens),
            output_tokens: parsed.usage.map(|u| u.output_tokens),
        })
    }
}

impl DecisionModel for SystemOneClient {
    fn decide<'a>(
        &'a self,
        state: &'a str,
        questions: &'a Questions,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Answers, ModelError>> + Send + 'a>> {
        Box::pin(async move { self.ask(state, questions, cancel).await.map(|a| a.answers) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_serializes_model_state_questions() {
        let mut questions = Questions::new();
        questions.insert(
            "go".to_string(),
            crate::navigator::ports::Question::Noul {
                instructions: "Worth a remark?".to_string(),
            },
        );
        let body = SystemOneRequest {
            model: "typesafe/jev-1.13",
            state: "Diff:\n+ x",
            questions: &questions,
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["model"], "typesafe/jev-1.13");
        assert_eq!(v["state"], "Diff:\n+ x");
        assert_eq!(v["questions"]["go"]["type"], "noul");
        assert_eq!(v["questions"]["go"]["instructions"], "Worth a remark?");
        // Only the three spec'd fields travel.
        assert_eq!(v.as_object().unwrap().len(), 3);
    }

    #[test]
    fn response_parses_the_captured_fixture_shape() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/navigator/systemone_multi.json"
        ))
        .unwrap();
        let parsed: SystemOneResponse =
            serde_json::from_value(fixture["response"].clone()).unwrap();
        assert_eq!(parsed.answers.len(), 5);
        assert_eq!(parsed.usage.unwrap().input_tokens, 454);
    }

    #[test]
    fn missing_answers_defaults_to_empty_not_error() {
        let parsed: SystemOneResponse =
            serde_json::from_value(serde_json::json!({"model": "m"})).unwrap();
        assert!(parsed.answers.is_empty());
        assert!(parsed.usage.is_none());
    }

    #[test]
    fn debug_never_prints_the_api_key() {
        let client = SystemOneClient {
            base_url: "http://localhost/v1".to_string(),
            model: "m".to_string(),
            client: ModelClient::new(
                "p",
                &ProviderConfig {
                    base_url: "http://localhost/v1".to_string(),
                    api_key_env: None,
                    headers: Default::default(),
                    connect_timeout_ms: 1_000,
                    request_timeout_ms: 1_000,
                },
                "sk-SUPERSECRET".to_string(),
                &ClientCache::new(),
            )
            .unwrap(),
        };
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("sk-SUPERSECRET"));
        assert!(rendered.contains("m"));
    }
}
