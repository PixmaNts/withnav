//! The LSP backend: document sync, trigger policy wiring, idle timers,
//! cancellation, custom notifications, JSONL logging.
//!
//! All mutable state lives in [`Inner`] behind an `Arc`, so trait methods
//! (`&self`) can spawn idle timers and commenter tasks that capture the state
//! without needing an `Arc<NavigatorBackend>`.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::Instant;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};

use crate::navigator::commenter::{Commenter, Diag, RemarkContext, MALFORMED_PREFIX};
use crate::navigator::config::NavigatorConfig;
use crate::navigator::document::DocumentState;
use crate::navigator::log::{excerpt, LogRecord, NavLog, PrefilterRecord, TriggerRecord};
use crate::navigator::policy::{decide, TriggerEvent, TriggerInput, TriggerOutcome};
use crate::navigator::protocol::{
    ClearNotification, ClearParams, DiagnosticItem, DiagnosticsParams, FeedbackParams,
    InlineCompletionResult, IntentParams, RemarkNotification, RemarkParams, ReflexShownParams,
    StatusNotification, StatusParams,
};

pub struct NavigatorBackend {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    commenter: Arc<dyn Commenter>,
    log: NavLog,
    /// One id per server process, grouping every trigger record it writes.
    run_id: String,
    config: Mutex<NavigatorConfig>,
    state: Mutex<HashMap<String, DocumentState>>,
    language_ids: Mutex<HashMap<String, String>>,
    diagnostics: Mutex<HashMap<String, Vec<Diag>>>,
    /// Latest remark id per uri (feedback targets it).
    last_remark_ids: Mutex<HashMap<String, String>>,
    intent: Mutex<Option<String>>,
}

impl NavigatorBackend {
    pub fn new(client: Client, deps: crate::navigator::NavigatorDeps) -> Self {
        Self {
            inner: Arc::new(Inner {
                client,
                commenter: deps.commenter,
                log: NavLog::new(deps.log_path),
                run_id: crate::navigator::new_ulid_string(),
                config: Mutex::new(NavigatorConfig::default()),
                state: Mutex::new(HashMap::new()),
                language_ids: Mutex::new(HashMap::new()),
                diagnostics: Mutex::new(HashMap::new()),
                last_remark_ids: Mutex::new(HashMap::new()),
                intent: Mutex::new(None),
            }),
        }
    }
}

impl Inner {
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
    async fn apply_config(&self, value: serde_json::Value) {
        let mut cfg = self.config.lock().await;
        *cfg = NavigatorConfig::from_value(&value, &cfg);
    }

    /// Cancels any in-flight commenter call for a document.
    fn cancel_in_flight(doc: &mut DocumentState) {
        if let Some(cancel) = doc.in_flight_cancel.take() {
            cancel.cancel();
        }
        doc.in_flight_version = None;
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
            let current = inner.state.lock().await.get(&uri).map(|d| d.version);
            if current == Some(version) {
                inner.evaluate(uri, TriggerEvent::IdleExpired).await;
            }
        });
    }

    /// Runs the trigger policy and, if it fires, the commenter call. Logs
    /// every trigger decision (fire and skip alike).
    async fn evaluate(self: &Arc<Self>, uri: String, event: TriggerEvent) {
        let config = self.config.lock().await.clone();
        if !config.enabled {
            return;
        }

        let now = Instant::now();
        let (input, doc_version) = {
            let state = self.state.lock().await;
            let Some(doc) = state.get(&uri) else {
                return;
            };
            let input = TriggerInput {
                event,
                changed_lines: doc.changed_lines_since_baseline(),
                since_last_edit: Some(now.duration_since(doc.last_edit)),
                since_last_remark: doc.last_remark.map(|t| now.duration_since(t)),
            };
            (input, doc.version)
        };

        let event_name = match event {
            TriggerEvent::Save => "save",
            TriggerEvent::IdleExpired => "idle",
        };
        let outcome = decide(&input, &config);
        let (prefilter_outcome, skip_reason) = match &outcome {
            TriggerOutcome::Fire => ("pass", None),
            TriggerOutcome::Skip(r) => ("skip", Some(*r)),
        };
        let trigger_id = crate::navigator::new_ulid_string();
        let mut prefilter_inputs = std::collections::BTreeMap::new();
        prefilter_inputs.insert("changed_lines".to_string(), serde_json::json!(input.changed_lines));
        prefilter_inputs.insert(
            "since_last_remark_ms".to_string(),
            serde_json::json!(input
                .since_last_remark
                .map(|d| d.as_millis() as u64)
                .unwrap_or(u64::MAX)),
        );
        // One trigger record per trigger, written once the delivered outcome
        // is known. The prototype commenter path fills the pre-filter part
        // and the delivered outcome; the pipeline-era fields stay absent
        // until a decision pipeline runs.
        let log_trigger = |requested: Option<&'static str>,
                           delivered: &'static str,
                           remark_id: Option<&str>,
                           reason: Option<&str>| {
            self.log.append(&LogRecord::Trigger(TriggerRecord {
                trigger_id: &trigger_id,
                run_id: &self.run_id,
                workspace: None,
                uri: &uri,
                version: doc_version,
                event: event_name,
                prefilter: PrefilterRecord {
                    inputs: &prefilter_inputs,
                    outcome: prefilter_outcome,
                    reason: skip_reason,
                },
                pipeline_hash: None,
                template_inputs: None,
                rendered_state: None,
                questions: None,
                model: None,
                answers: None,
                branch: None,
                requested_action: requested,
                delivered,
                remark_id,
                reason,
            }));
        };
        if matches!(outcome, TriggerOutcome::Skip(_)) {
            // Skipped by a pre-filter: nothing was ever requested.
            log_trigger(None, "none", None, skip_reason);
            return;
        }
        // One in-flight call per uri at most; the cancel handle is stored in
        // the SAME critical section so a concurrent didChange always sees it.
        let cancel = CancellationToken::new();
        {
            let mut state = self.state.lock().await;
            if let Some(doc) = state.get_mut(&uri) {
                if doc.in_flight_version.is_some() {
                    return;
                }
                doc.in_flight_version = Some(doc.version);
                doc.in_flight_cancel = Some(cancel.clone());
            }
        }

        let ctx = {
            let state = self.state.lock().await;
            let Some(doc) = state.get(&uri) else {
                self.clear_in_flight(&uri).await;
                return;
            };
            let diags = self
                .diagnostics
                .lock()
                .await
                .get(&uri)
                .cloned()
                .unwrap_or_default();
            let language_id = self
                .language_ids
                .lock()
                .await
                .get(&uri)
                .cloned()
                .unwrap_or_else(|| "plaintext".to_string());
            RemarkContext::build(
                &uri,
                &language_id,
                &doc.baseline,
                &doc.text,
                diags,
                self.intent.lock().await.clone(),
            )
        };

        self.set_status("thinking", None).await;
        let started = Instant::now();
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => None, // superseded by a newer edit
            r = self.commenter.remark(&ctx, &cancel) => Some(r),
        };
        self.clear_in_flight(&uri).await;

        let Some(result) = result else {
            // Cancelled: a newer edit took over. Stay quiet.
            log_trigger(Some("remark"), "cancelled", None, None);
            return;
        };

        let model = config.model_alias.clone();
        match result {
            Ok(Some(remark)) => {
                // A remark about a stale document version is dropped; a
                // current one stamps the cooldown clock and rebases.
                {
                    let mut state = self.state.lock().await;
                    match state.get_mut(&uri) {
                        Some(doc) if doc.version == doc_version => {
                            doc.last_remark = Some(Instant::now());
                            doc.rebase();
                        }
                        _ => {
                            tracing::info!(%uri, "dropping remark for stale version");
                            log_trigger(Some("remark"), "cancelled", None, Some("stale_version"));
                            return;
                        }
                    }
                }
                let id = new_remark_id();
                self.log.append(&LogRecord::Remark {
                    id: &id,
                    uri: &uri,
                    line: remark.line,
                    text: &remark.text,
                    model: &model,
                    latency_ms: started.elapsed().as_millis(),
                });
                log_trigger(Some("remark"), "remark", Some(&id), None);
                self.last_remark_ids
                    .lock()
                    .await
                    .insert(uri.clone(), id.clone());
                self.client
                    .send_notification::<RemarkNotification>(RemarkParams {
                        id,
                        uri: uri.clone(),
                        line: remark.line,
                        text: remark.text,
                    })
                    .await;
            }
            Ok(None) => {
                self.log.append(&LogRecord::Silent {
                    uri: &uri,
                    model: &model,
                    latency_ms: started.elapsed().as_millis(),
                });
                log_trigger(Some("remark"), "silent", None, None);
                // Silence rebase too: this diff was considered and judged fine.
                let mut state = self.state.lock().await;
                if let Some(doc) = state.get_mut(&uri) {
                    doc.rebase();
                }
            }
            Err(message) => {
                // A malformed answer is a model failure, not a provider
                // failure: different delivered outcome in the log.
                if let Some(raw) = message.strip_prefix(MALFORMED_PREFIX) {
                    self.log.append(&LogRecord::Malformed {
                        uri: &uri,
                        model: &model,
                        latency_ms: started.elapsed().as_millis(),
                        raw_excerpt: &excerpt(raw, 400),
                    });
                    log_trigger(Some("remark"), "malformed", None, None);
                    self.set_status(
                        "error",
                        Some("model answer violated the prompt contract".to_string()),
                    )
                    .await;
                } else {
                    self.log.append(&LogRecord::Error {
                        uri: &uri,
                        message: &message,
                    });
                    log_trigger(Some("remark"), "provider_error", None, None);
                    self.set_status("error", Some(message)).await;
                }
                return;
            }
        }
        self.set_status("idle", None).await;
    }

    async fn clear_in_flight(&self, uri: &str) {
        let mut state = self.state.lock().await;
        if let Some(doc) = state.get_mut(uri) {
            doc.in_flight_cancel = None;
            doc.in_flight_version = None;
        }
    }

    /// `navigator/reflexShown`: the extension reporting display time of a
    /// suggestion, so the log holds both halves of the pause-to-display
    /// latency.
    async fn on_reflex_shown(&self, params: ReflexShownParams) {
        self.log.append(&LogRecord::ReflexShown {
            request_id: &params.request_id,
            shown_at_ms: params.shown_at_ms,
        });
    }

    /// `navigator/feedback` client->server notification.
    async fn on_feedback(&self, params: FeedbackParams) {
        self.log.append(&LogRecord::Feedback {
            id: &params.id,
            verdict: &params.verdict,
        });
    }

    /// `navigator/diagnostics`: keep the latest set per uri.
    async fn on_diagnostics(&self, params: DiagnosticsParams) {
        let items: Vec<Diag> = params
            .items
            .into_iter()
            .map(|d: DiagnosticItem| Diag {
                line: d.line,
                severity: d.severity,
                message: d.message,
            })
            .collect();
        self.diagnostics.lock().await.insert(params.uri, items);
    }

    /// `navigator/intent`: the user's current goal, global to the session.
    async fn on_intent(&self, params: IntentParams) {
        let text = params.text.trim().to_string();
        *self.intent.lock().await = if text.is_empty() { None } else { Some(text) };
    }

    async fn on_did_open(self: &Arc<Self>, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        let uri = crate::navigator::uri_key(&doc.uri);
        self.language_ids
            .lock()
            .await
            .insert(uri.clone(), doc.language_id.clone());
        let now = Instant::now();
        let version;
        {
            let mut state = self.state.lock().await;
            let entry = state
                .entry(uri.clone())
                .or_insert_with(|| DocumentState::new(String::new(), 0, now));
            entry.apply_change(doc.text, doc.version, now);
            entry.rebase();
            version = entry.version;
        }
        self.spawn_idle_timer(uri, version);
    }

    async fn on_did_change(self: &Arc<Self>, params: DidChangeTextDocumentParams) {
        let uri = crate::navigator::uri_key(&params.text_document.uri);
        let version = params.text_document.version;
        // Full sync: each change event carries the whole text.
        let text = params
            .content_changes
            .into_iter()
            .next()
            .map(|c| c.text)
            .unwrap_or_default();
        let now = Instant::now();
        {
            let mut state = self.state.lock().await;
            match state.get_mut(&uri) {
                Some(doc) => {
                    Self::cancel_in_flight(doc);
                    doc.apply_change(text, version, now);
                }
                None => {
                    let mut doc = DocumentState::new(text, version, now);
                    doc.rebase();
                    state.insert(uri.clone(), doc);
                }
            }
        }
        self.spawn_idle_timer(uri, version);
    }

    async fn on_did_save(self: &Arc<Self>, params: DidSaveTextDocumentParams) {
        let uri = crate::navigator::uri_key(&params.text_document.uri);
        self.evaluate(uri, TriggerEvent::Save).await;
    }

    async fn on_did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = crate::navigator::uri_key(&params.text_document.uri);
        if let Some(mut doc) = self.state.lock().await.remove(&uri) {
            Self::cancel_in_flight(&mut doc);
        }
        self.language_ids.lock().await.remove(&uri);
        self.diagnostics.lock().await.remove(&uri);
        self.last_remark_ids.lock().await.remove(&uri);
        self.client
            .send_notification::<ClearNotification>(ClearParams { uri })
            .await;
    }
}

fn new_remark_id() -> String {
    crate::navigator::new_ulid_string()
}

impl LanguageServer for NavigatorBackend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        if let Some(options) = &params.initialization_options {
            self.inner.apply_config(options.clone()).await;
        }
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "rust-agent-navigator".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            ..InitializeResult::default()
        })
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.inner.on_did_open(params).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.inner.on_did_change(params).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.inner.on_did_save(params).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.inner.on_did_close(params).await;
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        self.inner.apply_config(params.settings).await;
    }
}

// Custom client->server notifications, wired as custom JSON-RPC methods in
// `navigator::build_service`. They must be inherent async methods on the
// backend to satisfy the custom_method signature.
impl NavigatorBackend {
    pub async fn feedback(&self, params: FeedbackParams) {
        self.inner.on_feedback(params).await;
    }

    pub async fn diagnostics(&self, params: DiagnosticsParams) {
        self.inner.on_diagnostics(params).await;
    }

    pub async fn intent(&self, params: IntentParams) {
        self.inner.on_intent(params).await;
    }

    /// `navigator/inlineCompletion`: ghost-text request from the extension's
    /// inline-completion provider. Returns an empty item list until the
    /// reflex layer is wired in; the shape is the frozen contract.
    pub async fn inline_completion(
        &self,
        _params: crate::navigator::protocol::InlineCompletionParams,
    ) -> tower_lsp_server::jsonrpc::Result<
        crate::navigator::protocol::InlineCompletionResult,
    > {
        Ok(InlineCompletionResult { items: Vec::new() })
    }

    pub async fn reflex_shown(&self, params: ReflexShownParams) {
        self.inner.on_reflex_shown(params).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remark_ids_are_unique() {
        assert_ne!(new_remark_id(), new_remark_id());
    }
}
