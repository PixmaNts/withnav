//! Failures of the MCP App bridge (`read_ui_resource`, `call_ui_tool`): one variant per failure
//! class, so the HTTP layer can give each its own status. The public text is static, except a
//! server's own JSON-RPC error message, which is redacted, stripped of control characters and
//! bounded before it reaches the app. The longer redacted diagnostic goes to the log.

/// Characters of an MCP server's own error message an app may see. Servers control this text,
/// so an unbounded echo would let one server flood every client that renders its app.
pub const UI_ERROR_DETAIL_CHARS: usize = 256;
/// Characters of a server error kept in one log line: enough to diagnose, not enough for a
/// misbehaving server to flood the log.
const LOG_DETAIL_CHARS: usize = 4096;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum McpUiError {
    /// The request itself is invalid (for example a resource that is not `ui://`).
    #[error("{0}")]
    InvalidRequest(&'static str),
    /// The server refused the arguments (JSON-RPC invalid params).
    #[error("MCP server refused the {method} arguments: {detail}")]
    InvalidParams {
        method: &'static str,
        detail: String,
    },
    #[error("unknown MCP server")]
    UnknownServer,
    #[error("unknown MCP tool")]
    UnknownTool,
    /// Configured but not ready: disabled, invalid, starting, failed or disconnected.
    #[error("MCP server is unavailable")]
    Unavailable,
    /// The server answered with a JSON-RPC error other than invalid params.
    #[error("MCP server rejected {method}: {detail}")]
    Rejected {
        method: &'static str,
        detail: String,
    },
    #[error("invalid MCP {method} response")]
    InvalidResponse { method: &'static str },
    #[error("MCP {method} timed out")]
    Timeout { method: &'static str },
}

/// The class of an rmcp request failure, with the server's message made safe for the app and
/// a redacted, bounded diagnostic logged. `secrets` are the server's configured values.
pub(super) fn classify(
    server: &str,
    method: &'static str,
    error: rmcp::ServiceError,
    secrets: &[String],
) -> McpUiError {
    let redact = |text: &str| super::redaction::redact(text, secrets);
    match error {
        rmcp::ServiceError::McpError(data) => {
            let message = redact(&data.message);
            let extra = data
                .data
                .as_ref()
                .map(|value| redact(&value.to_string()))
                .unwrap_or_default();
            tracing::warn!(
                server,
                method,
                code = data.code.0,
                message = ?super::bounded_chars(&message, LOG_DETAIL_CHARS),
                data = ?super::bounded_chars(&extra, LOG_DETAIL_CHARS),
                "MCP server returned an error to an app request"
            );
            let detail = public_detail(&message);
            if data.code == rmcp::model::ErrorCode::INVALID_PARAMS {
                McpUiError::InvalidParams { method, detail }
            } else {
                McpUiError::Rejected { method, detail }
            }
        }
        rmcp::ServiceError::Timeout { timeout } => timed_out(server, method, timeout),
        rmcp::ServiceError::UnexpectedResponse => {
            tracing::warn!(server, method, "MCP server sent an unexpected response");
            McpUiError::InvalidResponse { method }
        }
        error @ (rmcp::ServiceError::TransportClosed
        | rmcp::ServiceError::TransportSend(_)
        | rmcp::ServiceError::Cancelled { .. }) => {
            let text = redact(&error.to_string());
            tracing::warn!(
                server,
                method,
                error = ?super::bounded_chars(&text, LOG_DETAIL_CHARS),
                "MCP transport failed during an app request"
            );
            McpUiError::Unavailable
        }
        error => {
            let text = redact(&error.to_string());
            tracing::warn!(
                server,
                method,
                error = ?super::bounded_chars(&text, LOG_DETAIL_CHARS),
                "MCP app request failed"
            );
            McpUiError::InvalidResponse { method }
        }
    }
}

/// The app request outlived its budget. Logged here because rmcp's own diagnostics are filtered
/// out of the log, and without the server and budget an operator cannot tell which one hung.
pub(super) fn timed_out(
    server: &str,
    method: &'static str,
    budget: std::time::Duration,
) -> McpUiError {
    tracing::warn!(
        server,
        method,
        budget_ms = budget.as_millis() as u64,
        "MCP app request timed out"
    );
    McpUiError::Timeout { method }
}

/// A redacted server message as an app may see it: one line, at most
/// [`UI_ERROR_DETAIL_CHARS`] characters.
fn public_detail(redacted: &str) -> String {
    let line: String = redacted
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let line = line.trim();
    if line.is_empty() {
        return "no reason given".into();
    }
    super::bounded_chars(line, UI_ERROR_DETAIL_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(message: String, secrets: &[&str]) -> McpUiError {
        let secrets: Vec<String> = secrets.iter().map(|s| (*s).to_owned()).collect();
        classify(
            "fake",
            "tools/call",
            rmcp::ServiceError::McpError(rmcp::model::ErrorData::internal_error(message, None)),
            &secrets,
        )
    }

    // Given a server error message with a secret straddling the cut, control characters and
    // multibyte text
    // When it is classified for the app
    // Then the secret is redacted before the cut, controls become spaces and the detail holds
    // exactly the bound in characters
    #[test]
    fn server_message_is_redacted_single_line_and_bounded() {
        let secret = "tok-sentinel-0123456789";
        let message = format!("é\n\x1b[31m{}{secret}{}", "a".repeat(200), "z".repeat(1000));
        let McpUiError::Rejected { detail, .. } = rejected(message, &[secret]) else {
            panic!("expected a rejected error");
        };
        assert!(
            detail.contains("[redacted]") && !detail.contains("tok-"),
            "{detail}"
        );
        assert!(!detail.chars().any(char::is_control), "{detail}");
        assert_eq!(detail.chars().count(), UI_ERROR_DETAIL_CHARS);
        assert!(detail.starts_with('é'));
        assert!(detail.ends_with('…'));
    }

    #[test]
    fn invalid_params_and_transport_failures_have_their_own_class() {
        let params = classify(
            "fake",
            "tools/call",
            rmcp::ServiceError::McpError(rmcp::model::ErrorData::invalid_params("", None)),
            &[],
        );
        assert_eq!(
            params,
            McpUiError::InvalidParams {
                method: "tools/call",
                detail: "no reason given".into()
            }
        );
        assert_eq!(
            classify(
                "fake",
                "tools/call",
                rmcp::ServiceError::TransportClosed,
                &[]
            ),
            McpUiError::Unavailable
        );
        assert_eq!(
            classify(
                "fake",
                "resources/read",
                rmcp::ServiceError::UnexpectedResponse,
                &[]
            ),
            McpUiError::InvalidResponse {
                method: "resources/read"
            }
        );
    }
}
