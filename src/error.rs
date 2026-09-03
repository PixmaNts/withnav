use std::path::PathBuf;

use thiserror::Error;

use crate::provider::AssistantMessage;

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
    #[error("unexpected content-type (expected text/event-stream): {0}")]
    WrongContentType(String),
    #[error("provider request canceled")]
    Canceled {
        partial: Option<Box<AssistantMessage>>,
    },
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("path traversal outside configured roots: {0}")]
    PathTraversal(PathBuf),
    #[error("path not found: {0}")]
    NotFound(PathBuf),
    #[error("no match found for the requested edit")]
    NoMatch,
    #[error("ambiguous edit: {count} matches found (use replace_all)")]
    AmbiguousMatch { count: usize },
    #[error("tool execution timed out")]
    Timeout,
    #[error("tool execution canceled")]
    Canceled,
    #[error("tool denied [{code}]: {message}")]
    Denied { code: String, message: String },
    #[error("tool I/O error: {0}")]
    Io(#[from] std::io::Error),
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
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Sse(#[from] SseError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Done,
    Yielded,
    Interrupted,
    Error,
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
    fn error_path_traversal_display_contains_path() {
        let p = PathBuf::from("/etc/passwd");
        let e = ToolError::PathTraversal(p.clone());
        assert!(e.to_string().contains("/etc/passwd"));
    }

    #[test]
    fn error_denied_display_contains_code_and_message() {
        let e = ToolError::Denied {
            code: "plugin_denied".to_string(),
            message: "blocked by policy".to_string(),
        };
        let s = e.to_string();
        assert!(s.contains("plugin_denied"));
        assert!(s.contains("blocked by policy"));
    }

    #[test]
    fn error_ambiguous_match_display_contains_count() {
        let e = ToolError::AmbiguousMatch { count: 3 };
        assert!(e.to_string().contains('3'));
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

    #[test]
    fn error_agent_wraps_provider_via_from() {
        let e: AgentError = ProviderError::MissingApiKey.into();
        assert!(matches!(e, AgentError::Provider(_)));
        assert!(!e.to_string().is_empty());
    }

    #[test]
    fn turn_outcome_variants_distinct() {
        assert_ne!(TurnOutcome::Done, TurnOutcome::Interrupted);
        assert_ne!(TurnOutcome::Yielded, TurnOutcome::Error);
    }
}
