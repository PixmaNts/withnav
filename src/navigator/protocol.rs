//! Custom LSP protocol between the navigator server and the extension
//! (see the POC spec: server->client and client->server notifications).

use serde::{Deserialize, Serialize};
use tower_lsp_server::ls_types::notification::Notification;

/// Server -> client: a new remark for a uri. Replaces any previous one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemarkParams {
    pub id: String,
    pub uri: String,
    /// 0-based line index.
    pub line: u32,
    pub text: String,
}

/// Server -> client: remove any remark shown for a uri.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClearParams {
    pub uri: String,
}

/// Server -> client: navigator state for the status bar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusParams {
    /// "idle" | "thinking" | "error"
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Client -> server: verdict on the last remark.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackParams {
    pub id: String,
    /// "useful" | "noise" | "dismissed"
    pub verdict: String,
}

/// Client -> server: forwarded editor diagnostics for a uri.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsParams {
    pub uri: String,
    pub items: Vec<DiagnosticItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticItem {
    /// 0-based line index.
    pub line: u32,
    pub severity: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Client -> server: the user's current goal, global to the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentParams {
    pub text: String,
}

// --- Notification trait impls (server -> client) ---

pub enum RemarkNotification {}

impl Notification for RemarkNotification {
    type Params = RemarkParams;
    const METHOD: &'static str = "navigator/remark";
}

pub enum ClearNotification {}

impl Notification for ClearNotification {
    type Params = ClearParams;
    const METHOD: &'static str = "navigator/clear";
}

pub enum StatusNotification {}

impl Notification for StatusNotification {
    type Params = StatusParams;
    const METHOD: &'static str = "navigator/status";
}

/// A remark ready to show: 1-based line from the model, normalized to 0-based
/// for the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct Remark {
    /// 0-based line index.
    pub line: u32,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remark_params_serialize_with_snake_case_fields() {
        let p = RemarkParams {
            id: "r1".into(),
            uri: "file:///a.rs".into(),
            line: 4,
            text: "careful".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"id":"r1","uri":"file:///a.rs","line":4,"text":"careful"}"#
        );
    }

    #[test]
    fn status_params_omit_missing_message() {
        let p = StatusParams {
            state: "idle".into(),
            message: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(s, r#"{"state":"idle"}"#);
    }

    #[test]
    fn notification_methods_match_contract() {
        assert_eq!(RemarkNotification::METHOD, "navigator/remark");
        assert_eq!(ClearNotification::METHOD, "navigator/clear");
        assert_eq!(StatusNotification::METHOD, "navigator/status");
    }
}
