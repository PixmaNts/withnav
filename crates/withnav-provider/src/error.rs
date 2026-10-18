//! The error types of the model access layer.

use thiserror::Error;

use withnav_client_api::provider::AssistantMessage;

#[derive(Debug, Error)]
pub enum SseError {
    #[error("invalid UTF-8 in SSE stream")]
    Utf8,
    #[error("unexpected EOF in SSE stream")]
    UnexpectedEof,
    #[error("malformed SSE frame: {0}")]
    MalformedFrame(String),
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("SSE decode error: {0}")]
    Sse(#[from] SseError),
    #[error("provider API error (status {status}): {message}")]
    Api {
        status: u16,
        message: String,
        partial: Option<Box<AssistantMessage>>,
    },
    #[error("no API key found for provider")]
    MissingApiKey,
    #[error("auth store error: {0}")]
    Auth(String),
    #[error("unknown model alias: {0}")]
    UnknownModelAlias(String),
    #[error("unknown catalog model: {0}")]
    UnknownModel(String),
    #[error("ambiguous catalog model '{model}': {}", keys.join(", "))]
    AmbiguousModel { model: String, keys: Vec<String> },
    #[error("unexpected content-type (expected text/event-stream): {0}")]
    WrongContentType(String),
    #[error("provider request canceled")]
    Canceled {
        partial: Option<Box<AssistantMessage>>,
    },
    /// The endpoint answered successfully but the body is not the expected
    /// shape.
    #[error("malformed provider response: {0}")]
    MalformedResponse(String),
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config read error: {0}")]
    Read(#[from] std::io::Error),
    #[error("config JSON parse error: {0}")]
    ParseJson(#[from] serde_json::Error),
    #[error("config TOML parse error: {0}")]
    ParseToml(#[from] toml::de::Error),
    #[error("missing provider in config: {0}")]
    MissingProvider(String),
    #[error("invalid models.json: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_unknown_model_alias_display_contains_name() {
        let e = ProviderError::UnknownModelAlias("gpt-nope".to_string());
        assert!(e.to_string().contains("gpt-nope"));
    }

    #[test]
    fn error_canceled_is_distinct_variant_carrying_partial() {
        let none: ProviderError = ProviderError::Canceled { partial: None };
        assert!(matches!(none, ProviderError::Canceled { .. }));

        let msg = AssistantMessage {
            text: "partial".into(),
            ..Default::default()
        };
        let some = ProviderError::Canceled {
            partial: Some(Box::new(msg)),
        };
        if let ProviderError::Canceled { partial: Some(m) } = &some {
            assert_eq!(m.text, "partial");
        } else {
            panic!("expected Canceled with partial");
        }
        assert!(!some.to_string().is_empty());
    }

    #[test]
    fn error_api_carries_partial_and_status() {
        let e = ProviderError::Api {
            status: 500,
            message: "boom".into(),
            partial: None,
        };
        let s = e.to_string();
        assert!(s.contains("500"));
        assert!(s.contains("boom"));
    }

    #[test]
    fn error_missing_api_key_display_non_empty() {
        assert!(!ProviderError::MissingApiKey.to_string().is_empty());
    }

    #[test]
    fn error_config_missing_provider_display_contains_name() {
        let e = ConfigError::MissingProvider("omniroute".into());
        assert!(e.to_string().contains("omniroute"));
    }
}
