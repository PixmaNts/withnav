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
    /// The endpoint answered successfully but the body is not the expected
    /// shape.
    #[error("malformed provider response: {0}")]
    MalformedResponse(String),
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
    #[error(transparent)]
    Bus(#[from] BusError),
    #[error(transparent)]
    Server(#[from] ServerError),
}

#[derive(Debug, Error)]
pub enum BusError {
    #[error("event sequencer is closed for this session")]
    SequencerClosed,
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("session event sequence space exhausted")]
    SeqSpaceExhausted,
    #[error("actor inbox is full; command rejected")]
    InboxFull,
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("server bind error: {0}")]
    Bind(#[from] std::io::Error),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("inbox is full; command rejected")]
    InboxFull,
    #[error("session is stopping; command rejected")]
    SessionStopping,
    #[error("message content must be text")]
    NonTextContent,
    #[error("non-loopback bind refused: {0} (pass --allow-non-loopback to override)")]
    NonLoopbackBind(std::net::SocketAddr),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("model resolution error: {0}")]
    Model(#[from] crate::error::ProviderError),
    #[error("malformed request body: {0}")]
    MalformedBody(String),
    #[error("history store error: {0}")]
    History(String),
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
    fn error_agent_wraps_bus_via_from() {
        let e: AgentError = BusError::SequencerClosed.into();
        assert!(matches!(e, AgentError::Bus(_)));
        assert!(!e.to_string().is_empty());
    }

    #[test]
    fn error_agent_wraps_server_via_from() {
        let e: AgentError = ServerError::InboxFull.into();
        assert!(matches!(e, AgentError::Server(_)));
        assert!(!e.to_string().is_empty());
    }

    #[test]
    fn error_bus_session_not_found_display_contains_id() {
        let e = BusError::SessionNotFound("sess_1".into());
        assert!(e.to_string().contains("sess_1"));
    }

    #[test]
    fn error_server_bind_wraps_io_error() {
        let e = ServerError::Bind(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "address in use",
        ));
        assert!(e.to_string().contains("address in use"));
        let a: AgentError = e.into();
        assert!(matches!(a, AgentError::Server(_)));
    }

    #[test]
    fn error_server_variants_display_non_empty() {
        assert!(!ServerError::SessionNotFound("s".into())
            .to_string()
            .is_empty());
        assert!(!ServerError::InboxFull.to_string().is_empty());
        assert!(!ServerError::NonTextContent.to_string().is_empty());
    }
}
