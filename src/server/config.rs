//! The configuration callers build for the HTTP server, split by owner:
//! the HTTP concerns stay here ([`HttpConfig`]), the rest goes to the
//! session service ([`SessionConfig`]).

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::error::ServerError;
use crate::session::SessionConfig;

/// Immutable server configuration, validated at bind time.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Loopback-only by default; a non-loopback bind is refused unless
    /// `allow_non_loopback` is set (and even then: NO TLS this slice).
    pub bind: SocketAddr,
    /// Explicit models.json path; `None` resolves per-workspace then the
    /// config dir (same order as the CLI `run` path).
    pub models_path: Option<PathBuf>,
    /// Auth store directory for API-key resolution.
    pub config_dir: PathBuf,
    pub allow_non_loopback: bool,
    /// When set, every route except health requires
    /// `Authorization: Bearer <token>` (the navigator's in-process server).
    pub bearer_token: Option<String>,
    /// Conversation-history database; the binaries default it to
    /// [`crate::paths::history_db`], tests give each server its own.
    pub history_db: PathBuf,
}

/// The HTTP-only half of [`ServerConfig`].
#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub bind: SocketAddr,
    pub allow_non_loopback: bool,
    pub bearer_token: Option<String>,
}

impl ServerConfig {
    /// Bind-time validation: refuse a non-loopback address unless the
    /// explicit escape hatch was passed.
    pub fn validate(&self) -> Result<(), ServerError> {
        if !self.bind.ip().is_loopback() && !self.allow_non_loopback {
            return Err(ServerError::NonLoopbackBind(self.bind));
        }
        Ok(())
    }

    /// Splits into the session service's configuration and the HTTP one.
    pub fn split(self) -> (SessionConfig, HttpConfig) {
        (
            SessionConfig {
                models_path: self.models_path,
                config_dir: self.config_dir,
                history_db: self.history_db,
            },
            HttpConfig {
                bind: self.bind,
                allow_non_loopback: self.allow_non_loopback,
                bearer_token: self.bearer_token,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(bind: &str, allow_non_loopback: bool) -> ServerConfig {
        ServerConfig {
            bind: bind.parse().expect("test bind addr"),
            models_path: None,
            config_dir: PathBuf::from("/tmp/navai-test"),
            allow_non_loopback,
            bearer_token: None,
            history_db: PathBuf::from("/tmp/navai-test/history.db"),
        }
    }

    // Given the default loopback bind and a 0.0.0.0 bind without the flag
    // When ServerConfig::validate runs
    // Then loopback is accepted and 0.0.0.0 is refused with NonLoopbackBind
    #[test]
    fn loopback_bind_is_accepted_and_wildcard_refused_without_flag() {
        assert!(test_config("127.0.0.1:7310", false).validate().is_ok());
        assert!(matches!(
            test_config("0.0.0.0:7310", false).validate(),
            Err(ServerError::NonLoopbackBind(_))
        ));
    }

    // Given a non-loopback bind WITH --allow-non-loopback
    // When ServerConfig::validate runs
    // Then it is accepted (documented at-your-own-risk; still no TLS)
    #[test]
    fn non_loopback_bind_is_accepted_with_the_explicit_flag() {
        assert!(test_config("0.0.0.0:7310", true).validate().is_ok());
        assert!(test_config("192.168.1.10:7310", true).validate().is_ok());
    }

    // Given a full server config
    // When it is split
    // Then each owner gets exactly its own fields
    #[test]
    fn split_gives_each_owner_its_fields() {
        let mut config = test_config("127.0.0.1:7310", true);
        config.bearer_token = Some("t".to_string());
        config.history_db = PathBuf::from("/tmp/h.db");
        config.models_path = Some(PathBuf::from("/tmp/m.json"));
        let (session, http) = config.split();
        assert_eq!(session.history_db, PathBuf::from("/tmp/h.db"));
        assert_eq!(session.models_path, Some(PathBuf::from("/tmp/m.json")));
        assert_eq!(session.config_dir, PathBuf::from("/tmp/navai-test"));
        assert_eq!(http.bind, "127.0.0.1:7310".parse().unwrap());
        assert!(http.allow_non_loopback);
        assert_eq!(http.bearer_token.as_deref(), Some("t"));
    }
}
