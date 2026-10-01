//! Custom LSP protocol between the navigator server and the extension
//! (see the POC spec: server->client and client->server notifications).

use serde::{Deserialize, Serialize};
use tower_lsp_server::ls_types::notification::Notification;

/// Server -> client: the cognition chat server's endpoint, sent after
/// `initialized` (and again whenever the server is restarted). The loopback
/// port of the in-process session server plus the per-process bearer token
/// its routes require.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfoParams {
    pub port: u16,
    pub token: String,
}

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

/// Client -> server: request ghost-text suggestions at a cursor position.
/// LSP positions, UTF-16 code units; the extension's inline-completion
/// provider sends one after its debounce, the server converts against its
/// full-sync text and drops the answer if the document version moved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineCompletionParams {
    pub uri: String,
    pub version: i32,
    pub line: u32,
    pub character: u32,
}

/// One suggestion: the text to insert and the range it replaces (the empty
/// range at the cursor when the suggestion is a pure insertion).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineCompletionItem {
    pub text: String,
    /// 0-based, end-exclusive, same units as the request position.
    pub range: Range,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// Server -> client: the answer to `navigator/inlineCompletion`. Empty when
/// the reflex layer is absent or had nothing to say.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InlineCompletionResult {
    pub items: Vec<InlineCompletionItem>,
    /// The reflex call's id, echoed back in `navigator/reflexShown` so the
    /// log can join the model call with its display. Absent when no
    /// suggestion was produced.
    #[serde(default, rename = "requestId", skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// Client -> server: the extension reporting that a suggestion was
/// displayed; `shownAtMs` is the display timestamp so the log holds both
/// halves of the pause-to-display latency.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReflexShownParams {
    pub request_id: String,
    pub shown_at_ms: u64,
}

// --- Notification trait impls (server -> client) ---

pub enum RemarkNotification {}

impl Notification for RemarkNotification {
    type Params = RemarkParams;
    const METHOD: &'static str = "navigator/remark";
}

pub enum ServerInfoNotification {}

impl Notification for ServerInfoNotification {
    type Params = ServerInfoParams;
    const METHOD: &'static str = "navigator/serverInfo";
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
        assert_eq!(ServerInfoNotification::METHOD, "navigator/serverInfo");
    }

    #[test]
    fn server_info_params_serialize_the_wire_shape() {
        let p = ServerInfoParams {
            port: 41235,
            token: "secret".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(s, r#"{"port":41235,"token":"secret"}"#);
    }

    #[test]
    fn inline_completion_params_serialize_the_wire_shape() {
        let p = InlineCompletionParams {
            uri: "file:///a.rs".into(),
            version: 7,
            line: 3,
            character: 8,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"uri":"file:///a.rs","version":7,"line":3,"character":8}"#
        );
    }

    #[test]
    fn inline_completion_result_items_and_range_round_trip() {
        let r = InlineCompletionResult {
            items: vec![InlineCompletionItem {
                text: "world".into(),
                range: Range {
                    start: Position {
                        line: 0,
                        character: 6,
                    },
                    end: Position {
                        line: 0,
                        character: 6,
                    },
                },
            }],
            request_id: Some("t1".into()),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "items": [{
                    "text": "world",
                    "range": {
                        "start": {"line": 0, "character": 6},
                        "end": {"line": 0, "character": 6}
                    }
                }],
                "requestId": "t1"
            })
        );
        let back: InlineCompletionResult = serde_json::from_value(v).unwrap();
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.items[0].text, "world");
        assert_eq!(back.request_id.as_deref(), Some("t1"));
    }

    #[test]
    fn empty_inline_completion_result_is_an_empty_items_array() {
        let r = InlineCompletionResult {
            items: Vec::new(),
            request_id: None,
        };
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"items":[]}"#);
    }

    #[test]
    fn reflex_shown_params_use_camel_case() {
        let p = ReflexShownParams {
            request_id: "t9".into(),
            shown_at_ms: 1234,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(s, r#"{"requestId":"t9","shownAtMs":1234}"#);
    }
}
