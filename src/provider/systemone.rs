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
    api_key: String,
    extra_headers: Vec<(String, String)>,
    cache: std::sync::Arc<ClientCache>,
    provider_name: String,
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
        Ok(Self {
            base_url: provider.base_url.trim_end_matches('/').to_string(),
            model: model.model.clone(),
            api_key,
            extra_headers: provider.headers.clone().into_iter().collect(),
            cache,
            provider_name: provider_name.to_string(),
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
        let client = self
            .cache
            .get(&self.provider_name, &self.request_config())?;
        let url = format!("{}/systemone", self.base_url);
        let body = SystemOneRequest {
            model: &self.model,
            state,
            questions,
        };
        let mut req = client.post(&url).bearer_auth(&self.api_key).json(&body);
        for (k, v) in &self.extra_headers {
            req = req.header(k, v);
        }

        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(ModelError::Provider(ProviderError::Canceled {
                    partial: None,
                }));
            }
            r = req.send() => r.map_err(ProviderError::from)?,
        };
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .map(String::from)
                })
                .unwrap_or(body);
            return Err(ModelError::Provider(ProviderError::Api {
                status: status.as_u16(),
                message,
                partial: None,
            }));
        }
        let bytes = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(ModelError::Provider(ProviderError::Canceled {
                    partial: None,
                }));
            }
            b = tokio::time::timeout(SYSTEMONE_DEADLINE, response.bytes()) => {
                match b {
                    Ok(Ok(bytes)) => bytes,
                    // The deadline covering send and body elapsed.
                    Err(_) => {
                        return Err(ModelError::Provider(ProviderError::Api {
                            status: 0,
                            message: "systemone deadline exceeded (~2 s)".to_string(),
                            partial: None,
                        }));
                    }
                    Ok(Err(e)) => return Err(ProviderError::from(e).into()),
                }
            }
        };
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

    /// The provider config this client's deadline is expressed through. The
    /// per-layer deadline overrides the configured request timeout: it is
    /// applied here so a slow-configured provider still answers within ~2 s.
    fn request_config(&self) -> ProviderConfig {
        ProviderConfig {
            base_url: self.base_url.clone(),
            api_key_env: None,
            headers: self.extra_headers.iter().cloned().collect(),
            connect_timeout_ms: SYSTEMONE_DEADLINE.as_millis() as u64,
            request_timeout_ms: SYSTEMONE_DEADLINE.as_millis() as u64,
        }
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
        assert_eq!(parsed.answers.len(), 4);
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
            api_key: "sk-SUPERSECRET".to_string(),
            extra_headers: vec![],
            cache: std::sync::Arc::new(ClientCache::new()),
            provider_name: "p".to_string(),
        };
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("sk-SUPERSECRET"));
        assert!(rendered.contains("m"));
    }
}
