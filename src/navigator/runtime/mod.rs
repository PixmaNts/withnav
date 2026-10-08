//! The navigator's runtime: the shared state behind the Language Server and
//! the scheduling around it (document events, idle timers, the listener for
//! conversation turns, config and intent). The trigger
//! evaluation, the delivery of its outcome and the reflex-facing calls live
//! in the submodules.
//!
//! All mutable state lives in [`Runtime`] behind an `Arc`, so the server's
//! `&self` methods can spawn idle timers and metacognition tasks that
//! capture it without needing an `Arc<NavigatorBackend>`.

mod delivery;
mod flows;
mod inline;
mod trigger;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tower_lsp_server::ls_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams,
};
use tower_lsp_server::Client;

use crate::navigator::config::NavigatorConfig;
use crate::navigator::document::Documents;
use crate::navigator::log::{LogRecord, NavLog};
use crate::navigator::lsp::protocol::{
    ClearNotification, ClearParams, DiagnosticItem, DiagnosticsParams, FeedbackParams,
    IntentParams, ReflexShownParams, ServerInfoNotification, ServerInfoParams, StatusNotification,
    StatusParams,
};
use crate::navigator::metacognition::Diag;
use crate::navigator::reflex::{ReflexContext, ReflexService};
use crate::navigator::TriggerKind;
use crate::navigator::{ConversationEvent, Metacognition, NavigatorDeps};

pub(crate) struct Runtime {
    flows: Option<Arc<crate::navigator::FlowFactory>>,
    client: Client,
    metacognition: Arc<dyn Metacognition>,
    log: Arc<NavLog>,
    /// One id per server process, grouping every trigger record it writes.
    run_id: String,
    config: Mutex<NavigatorConfig>,
    documents: Mutex<Documents>,
    intent: Mutex<Option<String>>,
    /// The workspace root for intent and flow context; set at initialize.
    workspace: Mutex<Option<PathBuf>>,
    /// Model ids for the per-layer call records.
    metacognition_model: Option<String>,
    reflex_model: Option<String>,
    /// The reflex service; None keeps the layer disabled (empty answers).
    reflex: Option<ReflexService>,
    /// The cognition chat server's endpoint; sent as `navigator/serverInfo`
    /// after `initialized`. None keeps the chat unannounced.
    server_info: Option<ServerInfoParams>,
    /// The conversation hub shared with the session server: the seam a
    /// wake posts through. None keeps wakes logged as
    /// `cognition_unavailable`.
    conversation: Option<crate::navigator::ConversationFeed>,
    /// Startup notes about disabled layers, surfaced once after initialize.
    disabled_layers: Vec<String>,
    /// The refreshed reflex context; disabled without the metacognition's
    /// reflex-context seam (the intent summary is served instead).
    reflex_context: Arc<ReflexContext>,
    /// The focused document: the last uri that received `didOpen`,
    /// `didChange` or `didSave`. A driver turn in the conversation is
    /// judged against it; None until the first of those arrives.
    focused: Mutex<Option<String>>,
}

impl Runtime {
    /// Builds the runtime from the injected seams and starts the listener
    /// for conversation turns when a feed is wired.
    pub(crate) fn new(client: Client, deps: NavigatorDeps) -> Arc<Self> {
        let reflex = deps.fim.map(ReflexService::new);
        let log = Arc::new(NavLog::new(deps.log_path));
        let turns = deps.conversation.as_ref().map(|feed| feed.subscribe());
        let runtime = Arc::new(Self {
            flows: deps.flows,
            client,
            metacognition: deps.commenter,
            reflex_context: Arc::new(ReflexContext::new(
                deps.reflex_context,
                Arc::clone(&log),
                deps.metacognition_model.clone(),
            )),
            log,
            run_id: crate::navigator::new_ulid_string(),
            config: Mutex::new(NavigatorConfig::default()),
            documents: Mutex::new(Documents::default()),
            intent: Mutex::new(None),
            workspace: Mutex::new(deps.workspace),
            metacognition_model: deps.metacognition_model,
            reflex,
            reflex_model: deps.reflex_model,
            server_info: deps.server_info,
            conversation: deps.conversation,
            disabled_layers: deps.disabled_layers,
            focused: Mutex::new(None),
        });
        if let Some(turns) = turns {
            tokio::spawn(listen_for_driver_turns(Arc::downgrade(&runtime), turns));
        }
        runtime
    }

    /// Takes the workspace root from initialize, falling back to the process cwd.
    pub(crate) async fn set_workspace(
        self: &Arc<Self>,
        from_root: Option<PathBuf>,
        from_folder: Option<PathBuf>,
    ) {
        let mut workspace = self.workspace.lock().await;
        if from_root.is_some() {
            *workspace = from_root;
        } else if from_folder.is_some() {
            *workspace = from_folder;
        }
    }

    /// Surfaces the startup notes about disabled layers as one status error.
    pub(crate) async fn report_disabled_layers(&self) {
        if self.disabled_layers.is_empty() {
            return;
        }
        self.set_status(
            "error",
            Some(format!(
                "layer{} disabled: {}",
                if self.disabled_layers.len() == 1 {
                    ""
                } else {
                    "s"
                },
                self.disabled_layers.join("; ")
            )),
        )
        .await;
    }

    /// Announces the cognition chat server's endpoint when the binary started
    /// one, so the extension can bridge the chat panel.
    pub(crate) async fn announce_server_info(&self) {
        if let Some(info) = self.server_info.clone() {
            self.client
                .send_notification::<ServerInfoNotification>(info)
                .await;
        }
    }

    async fn set_status(&self, state: &str, message: Option<String>) {
        self.client
            .send_notification::<StatusNotification>(StatusParams {
                state: state.to_string(),
                message,
            })
            .await;
    }

    /// Applies configuration from initializationOptions or
    /// didChangeConfiguration. Unknown fields keep previous values.
    pub(crate) async fn apply_config(&self, value: serde_json::Value) {
        let mut cfg = self.config.lock().await;
        *cfg = NavigatorConfig::from_value(&value, &cfg);
    }

    /// Spawns the idle timer for a uri: fires after `idle_ms` without edits.
    /// Each spawn replaces the previous timer for that uri (the old task sees
    /// a newer version on wake and stands down).
    fn spawn_idle_timer(self: &Arc<Self>, uri: String, version: i32) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let idle_ms = inner.config.lock().await.idle_ms;
            tokio::time::sleep(std::time::Duration::from_millis(idle_ms)).await;
            // Stale timer: a newer edit restarted the countdown.
            let current = inner.documents.lock().await.get(&uri).map(|d| d.version);
            if current == Some(version) {
                inner.evaluate(uri, TriggerKind::Idle).await;
            }
        });
    }

    /// The workspace intent record, falling back to the session intent set
    /// via `navigator/intent`. Returns the summary text and whether any
    /// intent exists.
    async fn current_intent(&self) -> (String, bool) {
        let workspace = self.workspace.lock().await.clone();
        if let Some(ws) = workspace {
            match crate::navigator::read_intent(&ws) {
                Ok(Some(record)) => {
                    let summary = record.summary().to_string();
                    let has = !summary.trim().is_empty();
                    return (summary, has);
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "intent record unreadable"),
            }
        }
        match self.intent.lock().await.clone() {
            Some(text) => (text, true),
            None => (String::new(), false),
        }
    }

    /// Hands the document's in-flight slot from the instinct evaluation to a
    /// remark request: the instinct handle is released (a new trigger may
    /// evaluate while the remark is written), a remark still running for the
    /// document is superseded, and the new remark's handle is returned.
    /// `None` when typing already cancelled the instinct stage or the
    /// document is gone. `help_calls` are the calls for help the remark answers
    /// (empty for an ordinary remark); `conversation` marks a remark about
    /// the conversation, which is not tied to a version of the text.
    async fn begin_remark(
        &self,
        uri: &str,
        instinct_cancel: &CancellationToken,
        help_calls: &[String],
        conversation: bool,
    ) -> Option<CancellationToken> {
        let mut state = self.documents.lock().await;
        let doc = state.get_mut(uri)?;
        doc.in_flight_cancel = None;
        doc.in_flight_version = None;
        if instinct_cancel.is_cancelled() {
            return None;
        }
        if let Some(previous) = doc.remark_cancel.take() {
            previous.cancel();
        }
        let cancel = CancellationToken::new();
        doc.remark_cancel = Some(cancel.clone());
        doc.remark_version = (!conversation).then_some(doc.version);
        doc.help.begin(help_calls);
        Some(cancel)
    }

    /// Releases the remark handle, unless a newer remark already replaced it.
    async fn end_remark(&self, uri: &str, cancel: &CancellationToken) {
        let mut state = self.documents.lock().await;
        if let Some(doc) = state.get_mut(uri) {
            if !cancel.is_cancelled() {
                doc.remark_cancel = None;
                doc.remark_version = None;
                doc.help.end();
            }
        }
    }

    async fn clear_in_flight(&self, uri: &str) {
        let mut state = self.documents.lock().await;
        if let Some(doc) = state.get_mut(uri) {
            doc.in_flight_cancel = None;
            doc.in_flight_version = None;
        }
    }

    /// `navigator/reflexShown`: the extension reporting display time of a
    /// suggestion, so the log holds both halves of the pause-to-display
    /// latency.
    pub(crate) async fn on_reflex_shown(&self, params: ReflexShownParams) {
        self.log.append(&LogRecord::ReflexShown {
            request_id: &params.request_id,
            shown_at_ms: params.shown_at_ms,
        });
    }

    /// `navigator/feedback` client->server notification.
    pub(crate) async fn on_feedback(&self, params: FeedbackParams) {
        self.log.append(&LogRecord::Feedback {
            id: &params.id,
            verdict: &params.verdict,
        });
    }

    /// `navigator/diagnostics`: keep the latest set per uri and run the
    /// diagnostics trigger.
    pub(crate) async fn on_diagnostics(self: &Arc<Self>, params: DiagnosticsParams) {
        let items: Vec<Diag> = params
            .items
            .into_iter()
            .map(|d: DiagnosticItem| Diag {
                line: d.line,
                severity: d.severity,
                message: d.message,
            })
            .collect();
        self.documents
            .lock()
            .await
            .set_diagnostics(&params.uri, items);
        self.evaluate(params.uri, TriggerKind::Diagnostics).await;
    }

    /// `navigator/intent`: the user's current goal, global to the session.
    pub(crate) async fn on_intent(&self, params: IntentParams) {
        let text = params.text.trim().to_string();
        *self.intent.lock().await = if text.is_empty() { None } else { Some(text) };
    }

    pub(crate) async fn on_did_open(self: &Arc<Self>, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        let uri = crate::navigator::uri_key(&doc.uri);
        let version =
            self.documents
                .lock()
                .await
                .open(&uri, doc.language_id, doc.text, doc.version);
        *self.focused.lock().await = Some(uri.clone());
        self.spawn_idle_timer(uri, version);
    }

    pub(crate) async fn on_did_change(self: &Arc<Self>, params: DidChangeTextDocumentParams) {
        let uri = crate::navigator::uri_key(&params.text_document.uri);
        let version = params.text_document.version;
        // Full sync: each change event carries the whole text.
        let text = params
            .content_changes
            .into_iter()
            .next()
            .map(|c| c.text)
            .unwrap_or_default();
        self.documents.lock().await.change(&uri, text, version);
        *self.focused.lock().await = Some(uri.clone());
        self.spawn_idle_timer(uri, version);
    }

    pub(crate) async fn on_did_save(self: &Arc<Self>, params: DidSaveTextDocumentParams) {
        let uri_key = crate::navigator::uri_key(&params.text_document.uri);
        *self.focused.lock().await = Some(uri_key.clone());
        self.refresh_reflex_context(&uri_key, None).await;
        self.evaluate(uri_key, TriggerKind::Save).await;
    }

    pub(crate) async fn on_did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = crate::navigator::uri_key(&params.text_document.uri);
        self.documents.lock().await.close(&uri);
        {
            let mut focused = self.focused.lock().await;
            if focused.as_deref() == Some(uri.as_str()) {
                *focused = None;
            }
        }
        self.client
            .send_notification::<ClearNotification>(ClearParams { uri })
            .await;
    }
}

/// Evaluates the `conversation` event on the focused document each time a
/// driver-caused turn ends. A turn caused by a navigator wake never gets
/// here as a trigger (no loop). Holds the server weakly: once the server is
/// gone the next event ends the task.
async fn listen_for_driver_turns(
    inner: std::sync::Weak<Runtime>,
    mut turns: tokio::sync::broadcast::Receiver<ConversationEvent>,
) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        let event = match turns.recv().await {
            Ok(event) => event,
            // Missed events: the next turn end triggers anyway.
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        let ConversationEvent::TurnEnded {
            caused_by_driver: true,
            ..
        } = event
        else {
            continue;
        };
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let Some(uri) = inner.focused.lock().await.clone() else {
            continue;
        };
        tokio::spawn(async move {
            inner.evaluate(uri, TriggerKind::Conversation).await;
        });
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn navigator_ids_are_unique() {
        assert_ne!(
            crate::navigator::new_ulid_string(),
            crate::navigator::new_ulid_string()
        );
    }
}
