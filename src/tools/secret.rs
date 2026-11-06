//! `request_secret`: the agent asks the Captain for a secret (an API key, a token). Like `ask`,
//! the call pauses the turn; the Captain answers on a card of its own, and the value goes from
//! the client to the server's secrets endpoint, never through the chat. The model reads only
//! `stored as $NAME (…)` or `declined by the Captain`.

use std::future::Future;
use std::pin::Pin;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::secrets::{SecretRequestDetails, SecretScope, SecretTtl, check_name};
use crate::tools::{Tool, ToolContext, ToolResult};

pub use crate::secrets::REQUEST_SECRET_TOOL;

const MAX_LABEL_CHARS: usize = 80;
const MAX_REASON_CHARS: usize = 500;

pub struct RequestSecretTool;

/// The request a valid call makes; a reason the model can act on otherwise.
pub fn validated_request(args: &Value) -> Result<SecretRequestDetails, String> {
    let text = |key: &str, max: usize| -> Result<String, String> {
        let value = args
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("missing string argument: {key}"))?;
        if value.chars().count() > max || value.chars().any(char::is_control) {
            return Err(format!(
                "{key} must be at most {max} characters, without control characters"
            ));
        }
        Ok(value.to_owned())
    };
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or("missing string argument: name")?;
    check_name(name).map_err(|e| e.to_string())?;
    let default_ttl = match args.get("default_ttl") {
        None | Some(Value::Null) => SecretTtl::default(),
        Some(value) => value
            .as_str()
            .and_then(SecretTtl::parse)
            .ok_or("default_ttl must be \"1h\", \"24h\" or \"session\"")?,
    };
    let default_scope = match args.get("default_scope") {
        None | Some(Value::Null) => SecretScope::default(),
        Some(value) => value
            .as_str()
            .and_then(SecretScope::parse)
            .ok_or("default_scope must be \"session\" or \"workspace\"")?,
    };
    Ok(SecretRequestDetails {
        name: name.to_owned(),
        label: text("label", MAX_LABEL_CHARS)?,
        reason: text("reason", MAX_REASON_CHARS)?,
        default_ttl,
        default_scope,
    })
}

/// The question an older client shows for a secret request: it names the card, and that a
/// written answer goes to the agent as plain text.
pub fn question_text(details: &SecretRequestDetails) -> String {
    format!(
        "Nav asks for a secret: {} (${}), for {}. Enter it in the secret card: it is kept out of the chat.",
        details.label, details.name, details.reason
    )
}

impl Tool for RequestSecretTool {
    fn name(&self) -> &str {
        REQUEST_SECRET_TOOL
    }
    fn schema(&self) -> Value {
        json!({
            "description": "Ask the Captain for a secret (an API key, a token, a password) on a dedicated input, kept out of the chat. The turn pauses until the Captain answers. You never see the value: once stored, it is set as the environment variable `name` in every command you run with bash afterwards, so refer to it as $NAME and never print, echo, write or transform it. The result says `stored as $NAME (expires …, scope …)` or `declined by the Captain`; if declined, continue without it. Never ask for a secret with `ask` or in plain text.",
            "parameters": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "pattern": "^[A-Z][A-Z0-9_]{1,63}$", "description": "Environment variable the value is exposed as, e.g. OPENAI_API_KEY"},
                    "label": {"type": "string", "maxLength": MAX_LABEL_CHARS, "description": "Short human label, e.g. \"OpenAI API key\""},
                    "reason": {"type": "string", "maxLength": MAX_REASON_CHARS, "description": "What it is needed for"},
                    "default_ttl": {"type": "string", "enum": ["1h", "24h", "session"], "default": "24h"},
                    "default_scope": {"type": "string", "enum": ["session", "workspace"], "default": "session"}
                },
                "required": ["name", "label", "reason"]
            }
        })
    }
    fn call<'a>(
        &'a self,
        args: Value,
        _cancel: CancellationToken,
        _cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        // The loop pauses on a valid call instead of dispatching it; a dispatched one is invalid.
        Box::pin(async move {
            let reason = validated_request(&args)
                .err()
                .unwrap_or_else(|| "request_secret is answered by the Captain".into());
            Ok(ToolResult::error("invalid_args", reason))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_request_takes_defaults() {
        let details = validated_request(
            &json!({"name":"OPENAI_API_KEY","label":"OpenAI API key","reason":"run the eval"}),
        )
        .unwrap();
        assert_eq!(details.default_ttl, SecretTtl::Day);
        assert_eq!(details.default_scope, SecretScope::Session);
        assert!(question_text(&details).contains("$OPENAI_API_KEY"));
    }

    #[test]
    fn invalid_requests_say_why() {
        let base = json!({"name":"API_KEY","label":"Key","reason":"tests"});
        for (key, value) in [
            ("name", json!("api_key")),
            ("name", json!("PATH")),
            ("name", json!(1)),
            ("label", json!("")),
            ("label", json!("a\nb")),
            ("label", json!("x".repeat(81))),
            ("reason", json!(null)),
            ("default_ttl", json!("2h")),
            ("default_scope", json!("global")),
        ] {
            let mut args = base.clone();
            args[key] = value;
            assert!(validated_request(&args).is_err(), "{args}");
        }
    }
}
