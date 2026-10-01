//! The System One wire API client: `POST {base_url}/systemone` with body
//! `{model, state, questions}` and the answer in `answers` (shape captured in
//! `tests/fixtures/navigator/systemone_multi.json`).
//!
//! One HTTP client per provider via [`ClientCache`], one ~2 s deadline
//! covering send and body, cancellation honoured between send and body.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::ProviderError;
use crate::provider::client::ModelClient;
use crate::provider::config::{ModelConfig, ProviderConfig};
use crate::provider::openai::ClientCache;

/// The instinct's wall-clock budget for one System One call, covering send
/// and body (spec: ~2 s).
pub const SYSTEMONE_DEADLINE: Duration = Duration::from_millis(2_000);

/// One typed ask put to the model, answered as a yes-probability (`noul`),
/// a choice between named options, or a score over an ordered list of level
/// labels. This is the request half of the System One wire shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// A yes-probability question; the answer is a float in 0..1.
    Noul { instructions: String },
    /// A choice between named options; `criteria` maps each option to the
    /// instruction describing when it applies.
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    /// A score over an ordered list of 2 to 10 level labels (weakest first
    /// or strongest first, as the instructions define); the answer is an
    /// expected level index plus per-level probabilities.
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
}

/// The answer to one typed question: the response half of the System One
/// wire shape. `probabilities` keys are the choice options, or the level
/// indices as strings for scores; `confidence` is the model's own
/// self-assessed certainty in 0..1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        /// Expected level index (fractional, e.g. `1.56`).
        score: f64,
        /// Level index (as a string) to level label, as returned.
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

/// The questions of one call, keyed by question name.
pub type Questions = BTreeMap<String, Question>;

/// The answers of one call, keyed by the same question names.
pub type Answers = BTreeMap<String, Answer>;

/// What a caller supplies for one System One call; the client adds the
/// model identifier it was built for.
#[derive(Debug, Clone, Copy)]
pub struct DecisionRequest<'a> {
    pub state: &'a str,
    pub questions: &'a Questions,
}

/// The parts of the System One response this crate reads.
#[derive(Debug, Default, Deserialize)]
pub struct DecisionResponse {
    #[serde(default)]
    pub answers: Answers,
    #[serde(default)]
    pub usage: Option<DecisionUsage>,
}

/// Token usage as the System One fixture reports it.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct DecisionUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

/// The outgoing System One request body.
#[derive(Debug, Serialize)]
struct SystemOneRequest<'a> {
    model: &'a str,
    state: &'a str,
    questions: &'a Questions,
}

/// Truncates `text` to `max_chars` characters for an error message, marking
/// the cut with an ellipsis.
fn excerpt(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max_chars).collect();
    s.push('…');
    s
}

/// A client for the System One endpoint. Cheap to clone: the HTTP client is
/// shared through the [`ClientCache`].
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
    /// deadline, shared client, cancellation between send and body. A body
    /// that does not parse is [`ProviderError::MalformedResponse`].
    pub async fn decide(
        &self,
        request: &DecisionRequest<'_>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<DecisionResponse, ProviderError> {
        let body = SystemOneRequest {
            model: &self.model,
            state: request.state,
            questions: request.questions,
        };
        let bytes = self
            .client
            .post_json("systemone", &body, SYSTEMONE_DEADLINE, cancel)
            .await?;

        serde_json::from_slice(&bytes).map_err(|e| {
            let body = std::str::from_utf8(&bytes)
                .map(|s| excerpt(s, 200))
                .unwrap_or_else(|_| format!("{} bytes", bytes.len()));
            ProviderError::MalformedResponse(format!(
                "systemone response did not parse: {e}; body: {body}"
            ))
        })
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
            Question::Noul {
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
        let parsed: DecisionResponse = serde_json::from_value(fixture["response"].clone()).unwrap();
        assert_eq!(parsed.answers.len(), 5);
        assert_eq!(parsed.usage.unwrap().input_tokens, 454);
    }

    #[test]
    fn missing_answers_defaults_to_empty_not_error() {
        let parsed: DecisionResponse =
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
