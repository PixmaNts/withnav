//! Secrets the Captain hands to the tools: the `request_secret` details an `agent.yield`
//! carries, the body that answers it, the metadata the server reports back and the note a
//! `message.received` carries once a request is settled.
//!
//! A secret's value only ever travels in [`SecretSubmission`], from the client to the
//! dedicated endpoint. Every other type here is metadata: safe to show, store and replay.

use serde::{Deserialize, Serialize};

/// The `kind` of an `agent.yield` raised by `request_secret`.
pub const SECRET_KIND: &str = "secret";

/// How long a secret stays available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SecretTtl {
    /// One hour.
    #[serde(rename = "1h")]
    Hour,
    /// Twenty-four hours: the default.
    #[default]
    #[serde(rename = "24h")]
    Day,
    /// Until the conversation that holds it stops.
    #[serde(rename = "session")]
    Session,
}

impl SecretTtl {
    /// The wire spelling (`1h`, `24h`, `session`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::Day => "24h",
            Self::Session => "session",
        }
    }

    /// The lifetime in seconds; `None` for [`SecretTtl::Session`].
    pub const fn seconds(self) -> Option<i64> {
        match self {
            Self::Hour => Some(3_600),
            Self::Day => Some(86_400),
            Self::Session => None,
        }
    }

    /// Parses the wire spelling; anything else is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "1h" => Some(Self::Hour),
            "24h" => Some(Self::Day),
            "session" => Some(Self::Session),
            _ => None,
        }
    }
}

/// Who may use a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretScope {
    /// This conversation and its subagents: the default.
    #[default]
    Session,
    /// Every conversation of this workspace, until it expires.
    Workspace,
}

impl SecretScope {
    /// The wire spelling (`session`, `workspace`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Workspace => "workspace",
        }
    }

    /// Parses the wire spelling; anything else is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "session" => Some(Self::Session),
            "workspace" => Some(Self::Workspace),
            _ => None,
        }
    }
}

/// The `details` of a secret `agent.yield`: what the agent asked for, never a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRequestDetails {
    /// The environment variable the value is exposed as (`^[A-Z][A-Z0-9_]{1,63}$`).
    pub name: String,
    /// A short human label ("OpenAI API key").
    pub label: String,
    /// What the agent needs it for.
    pub reason: String,
    #[serde(default)]
    pub default_ttl: SecretTtl,
    #[serde(default)]
    pub default_scope: SecretScope,
}

/// `POST /v1/sessions/{id}/secrets/{request_id}`: the Captain's answer to a secret request.
///
/// `decline: true` refuses it (no value). Otherwise `value` is stored as `name`, which a client
/// may omit while the request waits (the request names it) and must give to provide a secret
/// after the request was settled ("Provide anyway", "Replace").
#[derive(Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSubmission {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default)]
    pub ttl: SecretTtl,
    #[serde(default)]
    pub scope: SecretScope,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub decline: bool,
}

/// Never prints the value: a submission may end up in a panic message or a log line.
impl std::fmt::Debug for SecretSubmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretSubmission")
            .field("name", &self.name)
            .field("value", &self.value.as_ref().map(|_| "<redacted>"))
            .field("ttl", &self.ttl)
            .field("scope", &self.scope)
            .field("decline", &self.decline)
            .finish()
    }
}

/// One stored secret as clients see it: metadata and the last four characters at most.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretView {
    pub name: String,
    pub scope: SecretScope,
    pub ttl: SecretTtl,
    /// RFC 3339; absent for a secret that lasts as long as its conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub created_at: String,
    /// How many processes it was given to.
    pub use_count: u64,
    /// The last four characters of a value of eight or more; empty for a shorter value.
    pub last4: String,
}

/// `GET /v1/sessions/{id}/secrets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretsBody {
    pub secrets: Vec<SecretView>,
}

/// The answer of `POST /v1/sessions/{id}/secrets/{request_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretAccepted {
    pub accepted: bool,
    /// The stored secret; absent for a decline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<SecretView>,
}

/// What became of a secret request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretOutcome {
    Stored,
    Declined,
}

/// The `secret` of a `message.received`: the host-written answer to a secret request (or a
/// secret provided later), so a client settles its card instead of showing a Captain bubble.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretNote {
    /// The `question_call_id` of the request it answers; absent for a secret provided after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub name: String,
    pub outcome: SecretOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<SecretScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<SecretTtl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// Whether `name` can name a secret: an environment variable of 2 to 64 characters,
/// `^[A-Z][A-Z0-9_]{1,63}$`.
pub fn valid_secret_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (2..=64).contains(&bytes.len())
        && bytes[0].is_ascii_uppercase()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_environment_variable_rule() {
        for ok in [
            "AB",
            "OPENAI_API_KEY",
            "A1",
            &format!("A{}", "B".repeat(63)),
        ] {
            assert!(valid_secret_name(ok), "{ok}");
        }
        for bad in [
            "",
            "A",
            "a_KEY",
            "1KEY",
            "_KEY",
            "KEY-1",
            "KEY 1",
            "KÉY",
            &format!("A{}", "B".repeat(64)),
        ] {
            assert!(!valid_secret_name(bad), "{bad}");
        }
    }

    #[test]
    fn submission_debug_never_prints_the_value() {
        let submission = SecretSubmission {
            value: Some("sk-very-secret-value".into()),
            ..SecretSubmission::default()
        };
        assert!(!format!("{submission:?}").contains("very-secret"));
    }

    #[test]
    fn wire_spellings() {
        let submission: SecretSubmission =
            serde_json::from_value(serde_json::json!({"value":"v","ttl":"1h","scope":"workspace"}))
                .unwrap();
        assert_eq!(submission.ttl, SecretTtl::Hour);
        assert_eq!(submission.scope, SecretScope::Workspace);
        let defaults: SecretSubmission =
            serde_json::from_value(serde_json::json!({"decline":true})).unwrap();
        assert_eq!(defaults.ttl, SecretTtl::Day);
        assert_eq!(defaults.scope, SecretScope::Session);
        assert!(
            serde_json::from_value::<SecretSubmission>(serde_json::json!({"ttl":"2h"})).is_err()
        );
        assert_eq!(SecretTtl::parse("session"), Some(SecretTtl::Session));
        assert_eq!(
            SecretScope::parse("workspace"),
            Some(SecretScope::Workspace)
        );
    }
}
