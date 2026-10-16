//! One trigger, end to end up to its decision: gathers the context from the
//! document state for editor flows; their remark action is handed to
//! [`delivery`](super::delivery).

use std::sync::Arc;

use tokio::time::Instant;

use super::Runtime;
use crate::navigator::TriggerKind;
use crate::navigator::document::text_around_line;
use crate::navigator::metacognition::RemarkContext;

/// Lines on each side of the last edited line shown to the conversation
/// trigger.
const CONVERSATION_CONTEXT_LINES: usize = 20;

/// One trigger's gathered context: everything the editor flow inputs, the
/// templates and the log record need.
#[derive(Clone)]
pub(super) struct TriggerContext {
    pub(super) uri: String,
    pub(super) event: &'static str,
    pub(super) version: i32,
    pub(super) changed_lines: i64,
    pub(super) since_last_remark_ms: i64,
    pub(super) since_last_wake_ms: i64,
    pub(super) called_for_help: bool,
    /// The unanswered calls for help in the added lines (normalized comment
    /// text): what a remark about this trigger answers.
    pub(super) help_calls: Vec<String>,
    /// A remark is already being written for every one of `help_calls`.
    pub(super) help_in_flight: bool,
    /// A remark request is already running on this version of the text.
    pub(super) remark_in_flight: bool,
    pub(super) has_errors: bool,
    pub(super) has_intent: bool,
    pub(super) has_conversation: bool,
    pub(super) intent_text: String,
    pub(super) language_id: String,
    pub(super) diff: String,
    /// The document text this trigger judged: a late remark's line is
    /// re-anchored from it onto the current text.
    pub(super) text: String,
    pub(super) context_text: String,
    pub(super) diagnostics_text: String,
    pub(super) diagnostics_payload: serde_json::Value,
    pub(super) conversation_text: String,
    pub(super) live_state_text: String,
}

impl TriggerContext {
    pub(super) fn template_inputs(&self) -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::from([
            ("file".to_string(), self.uri.clone()),
            ("language".to_string(), self.language_id.clone()),
            ("intent".to_string(), self.intent_text.clone()),
            ("diff".to_string(), self.diff.clone()),
            ("context".to_string(), self.context_text.clone()),
            ("diagnostics".to_string(), self.diagnostics_text.clone()),
            ("live_state".to_string(), self.live_state_text.clone()),
            ("conversation".to_string(), self.conversation_text.clone()),
        ])
    }

    pub(super) fn prefilter_inputs(&self) -> std::collections::BTreeMap<String, serde_json::Value> {
        std::collections::BTreeMap::from([
            (
                "changed_lines".to_string(),
                serde_json::json!(self.changed_lines),
            ),
            (
                "since_last_remark_ms".to_string(),
                serde_json::json!(self.since_last_remark_ms),
            ),
            (
                "since_last_wake_ms".to_string(),
                serde_json::json!(self.since_last_wake_ms),
            ),
            (
                "called_for_help".to_string(),
                serde_json::json!(self.called_for_help),
            ),
            ("has_errors".to_string(), serde_json::json!(self.has_errors)),
            ("has_intent".to_string(), serde_json::json!(self.has_intent)),
            (
                "has_conversation".to_string(),
                serde_json::json!(self.has_conversation),
            ),
        ])
    }
}

impl Runtime {
    /// Gathers everything one trigger needs from the document state.
    pub(super) async fn gather(
        &self,
        uri: &str,
        event: TriggerKind,
        now: Instant,
    ) -> Option<TriggerContext> {
        let snapshot = self.documents.lock().await.snapshot(uri, event, now)?;
        let has_errors = snapshot.diagnostics.iter().any(|d| d.severity == "error");
        let diagnostics_text = snapshot
            .diagnostics
            .iter()
            .map(|d| format!("line {}: [{}] {}", d.line + 1, d.severity, d.message))
            .collect::<Vec<_>>()
            .join("\n");
        let diagnostics_payload = serde_json::json!(snapshot.diagnostics.iter().map(|d| serde_json::json!({"message":d.message,"severity": match d.severity.as_str() { "error" => 1, "warning" => 2, "information" => 3, _ => 4 }, "range":{"start":{"line":d.line,"character":0},"end":{"line":d.line,"character":0}}})).collect::<Vec<_>>());
        let (intent_text, has_intent) = self.current_intent().await;
        // Reuse the prototype's diff/excerpt builders for the {{diff}} and
        // {{context}} template variables.
        let mut ctx = RemarkContext::build(
            uri,
            &snapshot.language_id,
            &snapshot.baseline,
            &snapshot.text,
            Vec::new(),
            None,
        );
        if event == TriggerKind::Conversation {
            // The change is in the conversation: no diff, and the context is
            // the code around the last edited line (the top of the file when
            // nothing was edited yet).
            ctx.diff = String::new();
            ctx.text_excerpt = text_around_line(
                &snapshot.text,
                snapshot.last_edit_line,
                CONVERSATION_CONTEXT_LINES,
            );
        }
        ctx.workspace = self.workspace.lock().await.clone();
        ctx.open_buffers = snapshot.open_buffers;
        // The conversation digest the fast layers judge against: the feed's
        // bounded view of what the Captain and the cognition last agreed.
        let conversation_text = self
            .conversation
            .as_ref()
            .map(|f| f.digest())
            .unwrap_or_default();
        let has_conversation = !conversation_text.trim().is_empty();
        // A conversation turn changed nothing in the file; a call already
        // answered (or being answered) is not a new one.
        let called_for_help = !snapshot.help_calls.is_empty();
        Some(TriggerContext {
            uri: uri.to_string(),
            event: "",
            version: snapshot.version,
            changed_lines: snapshot.changed_lines,
            since_last_remark_ms: snapshot.since_last_remark_ms,
            since_last_wake_ms: snapshot.since_last_wake_ms,
            called_for_help,
            help_calls: snapshot.help_calls,
            help_in_flight: snapshot.help_in_flight,
            remark_in_flight: snapshot.remark_in_flight,
            has_errors,
            has_intent,
            has_conversation,
            intent_text,
            language_id: snapshot.language_id,
            diff: ctx.diff,
            text: snapshot.text,
            context_text: ctx.text_excerpt,
            diagnostics_text,
            diagnostics_payload,
            conversation_text,
            live_state_text: snapshot.live_state,
        })
    }

    /// Editor triggers are exclusively owned by enabled flows.
    pub(super) async fn evaluate(self: &Arc<Self>, uri: String, event: TriggerKind) {
        if self.config.lock().await.enabled {
            self.evaluate_flows(&uri, event).await;
        }
    }
}
