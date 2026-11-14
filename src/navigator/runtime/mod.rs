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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tower_lsp_server::Client;
use tower_lsp_server::ls_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams,
};

use crate::navigator::config::NavigatorConfig;
use crate::navigator::conversation::{
    ConversationFeed, ConversationTarget, EditorId, EditorRegistration, EditorReveal,
    WorkspaceFeedRegistry,
};
use crate::navigator::document::Documents;
use crate::navigator::log::{LogRecord, NavLog};
use crate::navigator::lsp::protocol::{
    ClearNotification, ClearParams, ConversationParams, DiagnosticItem, DiagnosticsParams,
    FeedbackParams, IntentParams, ReflexShownParams, ServerInfoNotification, ServerInfoParams,
    StatusNotification, StatusParams,
};
use crate::navigator::metacognition::Diag;
use crate::navigator::ports::ReflexContextModel;
use crate::navigator::reflex::{ReflexContext, ReflexService};
use crate::navigator::{ConversationEvent, FlowFactory, Metacognition, NavigatorDeps, TriggerKind};

struct WorkspaceRegistration {
    guard: EditorRegistration,
    cancel: CancellationToken,
}

#[derive(Default)]
struct IntentBinding {
    text: Option<String>,
    bound_root: Option<PathBuf>,
}

pub(crate) struct Runtime {
    pub(crate) editor_id: EditorId,
    pub(crate) connection_cancel: CancellationToken,
    detached: AtomicBool,
    registered_root_mode: AtomicBool,
    registrations: std::sync::Mutex<HashMap<PathBuf, WorkspaceRegistration>>,
    reveal: EditorReveal,
    routing_generation: AtomicU64,
    pub(crate) registry: Option<WorkspaceFeedRegistry>,
    pub(crate) flow_host: Option<Arc<crate::flows::host::Host>>,
    pub(crate) flows: Option<Arc<FlowFactory>>,
    pub(crate) client: Client,
    pub(crate) metacognition: Arc<dyn Metacognition>,
    pub(crate) log: Arc<NavLog>,
    /// One id per server process, grouping every trigger record it writes.
    pub(crate) run_id: String,
    pub(crate) config: Mutex<NavigatorConfig>,
    pub(crate) documents: Mutex<Documents>,
    intent: Mutex<IntentBinding>,
    /// The workspace root for intent and flow context; set at initialize.
    pub(crate) workspace: Mutex<Option<PathBuf>>,
    /// Every workspace folder the client reported (multi-root), the primary
    /// included. Tools anchor on the primary workspace, but a file in any of
    /// these folders is in the workspace.
    pub(crate) folders: Mutex<Vec<PathBuf>>,
    /// Model ids for the per-layer call records.
    pub(crate) metacognition_model: Option<String>,
    pub(crate) reflex_model: Option<String>,
    /// The reflex service; None keeps the layer disabled (empty answers).
    pub(crate) reflex: Option<ReflexService>,
    /// The cognition chat server's endpoint; sent as `withnav/serverInfo`
    /// after `initialized`. None keeps the chat unannounced.
    pub(crate) server_info: Option<ServerInfoParams>,
    /// The conversation hub shared with the session server: the seam a
    /// wake posts through. None keeps wakes logged as
    /// `cognition_unavailable`.
    pub(crate) conversation: Option<ConversationFeed>,
    /// Startup notes about disabled layers, surfaced once after initialize.
    pub(crate) disabled_layers: Vec<String>,
    pub(crate) startup_warnings: Mutex<Vec<String>>,
    deps_reflex_context: Option<Arc<dyn ReflexContextModel>>,
    reflex_contexts: Mutex<HashMap<PathBuf, Arc<ReflexContext>>>,
    /// The focused document: the last uri that received `didOpen`,
    /// `didChange` or `didSave`. A captain turn in the conversation is
    /// judged against it; None until the first of those arrives.
    pub(crate) focused: Mutex<Option<String>>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.detach();
    }
}

impl Runtime {
    /// Builds the runtime from the injected seams and starts the listener
    /// for conversation turns when a feed is wired.
    pub(crate) fn new(client: Client, deps: NavigatorDeps) -> Arc<Self> {
        let reflex = deps.fim.map(ReflexService::new);
        let log = Arc::new(NavLog::new(deps.log_path));
        let editor_id = EditorId::new();
        let connection_cancel = CancellationToken::new();
        let reveal: EditorReveal = {
            let client = client.clone();
            Arc::new(move |params| {
                let client = client.clone();
                Box::pin(async move {
                    client
                        .send_request::<crate::navigator::lsp::protocol::RevealRequest>(params)
                        .await
                        .map_err(|e| e.message.to_string())
                })
            })
        };

        let has_initial_workspace = deps.workspace.is_some();
        Arc::new(Self {
            editor_id,
            connection_cancel,
            detached: AtomicBool::new(false),
            registered_root_mode: AtomicBool::new(has_initial_workspace),
            registrations: std::sync::Mutex::new(HashMap::new()),
            reveal,
            routing_generation: AtomicU64::new(0),
            registry: deps.registry,
            flow_host: deps.flow_host,
            flows: deps.flows,
            client,
            metacognition: deps.commenter,
            deps_reflex_context: deps.reflex_context,
            reflex_contexts: Mutex::new(HashMap::new()),
            log,
            run_id: crate::navigator::new_ulid_string(),
            config: Mutex::new(NavigatorConfig::default()),
            documents: Mutex::new(Documents::default()),
            intent: Mutex::new(IntentBinding::default()),
            folders: Mutex::new(deps.workspace.iter().cloned().collect()),
            workspace: Mutex::new(deps.workspace),
            metacognition_model: deps.metacognition_model,
            reflex,
            reflex_model: deps.reflex_model,
            server_info: deps.server_info,
            conversation: deps.conversation,
            disabled_layers: deps.disabled_layers,
            startup_warnings: Mutex::new(deps.startup_warnings),
            focused: Mutex::new(None),
        })
    }

    #[allow(dead_code)]
    pub(crate) fn editor_id(&self) -> EditorId {
        self.editor_id
    }

    #[allow(dead_code)]
    pub(crate) fn connection_cancel(&self) -> CancellationToken {
        self.connection_cancel.clone()
    }

    pub(crate) fn routing_generation(&self) -> u64 {
        self.routing_generation.load(Ordering::SeqCst)
    }

    pub(crate) fn detach(&self) {
        if !self.detached.swap(true, Ordering::SeqCst) {
            self.connection_cancel.cancel();
            let mut regs = self.registrations.lock().unwrap();
            for (_, reg) in regs.drain() {
                reg.cancel.cancel();
                reg.guard.detach();
            }
            if let Some(feed) = &self.conversation {
                feed.unregister_editor(self.editor_id);
            }
        }
    }

    /// Takes the workspace from initialize: the root URI wins as the primary
    /// workspace, else the first folder; every folder stays known so its files
    /// count as in-workspace. Falls back to the process cwd when none is given.
    pub(crate) async fn set_workspace(
        self: &Arc<Self>,
        from_root: Option<PathBuf>,
        from_folders: Vec<PathBuf>,
    ) {
        let has_supplied_roots = from_root.is_some() || !from_folders.is_empty();
        if has_supplied_roots {
            self.registered_root_mode.store(true, Ordering::SeqCst);
        }
        let mut workspace = self.workspace.lock().await;
        let mut folders = self.folders.lock().await;
        if from_root.is_some() {
            *workspace = from_root;
        } else if let Some(first) = from_folders.first() {
            *workspace = Some(first.clone());
        }
        for folder in from_folders {
            if !folders.contains(&folder) {
                folders.push(folder);
            }
        }
        if let Some(primary) = workspace.as_ref().filter(|p| !folders.contains(p)) {
            folders.push(primary.clone());
        }
        self.routing_generation.fetch_add(1, Ordering::SeqCst);
        self.reflex_contexts.lock().await.clear();
        let roots = folders.clone();
        drop(folders);
        drop(workspace);
        self.sync_registrations(&roots).await;
    }

    /// `workspace/didChangeWorkspaceFolders`: folders come and go while the
    /// server runs. When the primary workspace is removed, the first
    /// remaining folder takes over; with none left the last primary is kept,
    /// the flows and the intent record still need an anchor.
    pub(crate) async fn change_workspace_folders(
        self: &Arc<Self>,
        added: Vec<PathBuf>,
        removed: Vec<PathBuf>,
    ) {
        self.registered_root_mode.store(true, Ordering::SeqCst);
        let mut workspace = self.workspace.lock().await;
        let mut folders = self.folders.lock().await;
        apply_folder_change(&mut workspace, &mut folders, added, removed);
        self.routing_generation.fetch_add(1, Ordering::SeqCst);
        self.reflex_contexts.lock().await.clear();
        let roots = folders.clone();
        drop(folders);
        drop(workspace);
        self.sync_registrations(&roots).await;
    }

    pub(crate) async fn register_after_initialize(self: &Arc<Self>) {
        if self.detached.load(Ordering::SeqCst) || self.connection_cancel.is_cancelled() {
            return;
        }
        let folders = self.folders.lock().await.clone();
        if !folders.is_empty() {
            self.sync_registrations(&folders).await;
        } else if let Some(ws) = self.workspace.lock().await.clone() {
            self.sync_registrations(&[ws]).await;
        } else if let Some(feed) = &self.conversation {
            let canonical_feed_ws = WorkspaceFeedRegistry::canonical_key(feed.workspace());
            let mut regs = self.registrations.lock().unwrap();
            if self.detached.load(Ordering::SeqCst) || self.connection_cancel.is_cancelled() {
                for (_, reg) in regs.drain() {
                    reg.cancel.cancel();
                    reg.guard.detach();
                }
                return;
            }
            if !regs.contains_key(&canonical_feed_ws) {
                let child_cancel = self.connection_cancel.child_token();
                let guard = feed.register_editor_lifecycle(
                    self.editor_id,
                    self.reveal.clone(),
                    child_cancel.clone(),
                );
                regs.insert(
                    canonical_feed_ws.clone(),
                    WorkspaceRegistration {
                        guard,
                        cancel: child_cancel.clone(),
                    },
                );
                let turns = feed.subscribe();
                tokio::spawn(listen_for_captain_turns(
                    Arc::downgrade(self),
                    canonical_feed_ws,
                    child_cancel,
                    turns,
                ));
            }
        }
    }

    pub(crate) async fn sync_registrations(self: &Arc<Self>, active_roots: &[PathBuf]) {
        if self.detached.load(Ordering::SeqCst) || self.connection_cancel.is_cancelled() {
            return;
        }

        let active_canonical: HashSet<PathBuf> = active_roots
            .iter()
            .map(|r| WorkspaceFeedRegistry::canonical_key(r))
            .collect();

        // 1. Remove and unregister roots that are no longer active
        {
            let mut regs = self.registrations.lock().unwrap();
            if self.detached.load(Ordering::SeqCst) || self.connection_cancel.is_cancelled() {
                for (_, reg) in regs.drain() {
                    reg.cancel.cancel();
                    reg.guard.detach();
                }
                return;
            }
            let to_remove: Vec<PathBuf> = regs
                .keys()
                .filter(|k| !active_canonical.contains(*k))
                .cloned()
                .collect();

            for key in to_remove {
                if let Some(reg) = regs.remove(&key) {
                    reg.cancel.cancel();
                    reg.guard.detach();
                }
            }
        }

        // 2. Register newly active roots
        let Some(registry) = &self.registry else {
            if let Some(feed) = &self.conversation {
                let canonical_feed_ws = WorkspaceFeedRegistry::canonical_key(feed.workspace());
                let matches = canonical_feed_ws.as_os_str().is_empty()
                    || active_canonical.is_empty()
                    || active_canonical.contains(&canonical_feed_ws);
                if matches {
                    let mut regs = self.registrations.lock().unwrap();
                    if self.detached.load(Ordering::SeqCst) || self.connection_cancel.is_cancelled()
                    {
                        for (_, reg) in regs.drain() {
                            reg.cancel.cancel();
                            reg.guard.detach();
                        }
                        return;
                    }
                    if !regs.contains_key(&canonical_feed_ws) {
                        let child_cancel = self.connection_cancel.child_token();
                        let guard = feed.register_editor_lifecycle(
                            self.editor_id,
                            self.reveal.clone(),
                            child_cancel.clone(),
                        );
                        regs.insert(
                            canonical_feed_ws.clone(),
                            WorkspaceRegistration {
                                guard,
                                cancel: child_cancel.clone(),
                            },
                        );
                        let turns = feed.subscribe();
                        tokio::spawn(listen_for_captain_turns(
                            Arc::downgrade(self),
                            canonical_feed_ws,
                            child_cancel,
                            turns,
                        ));
                    }
                }
            }
            return;
        };

        for root in active_roots {
            let canonical_root = WorkspaceFeedRegistry::canonical_key(root);
            let mut regs = self.registrations.lock().unwrap();
            if self.detached.load(Ordering::SeqCst) || self.connection_cancel.is_cancelled() {
                for (_, reg) in regs.drain() {
                    reg.cancel.cancel();
                    reg.guard.detach();
                }
                return;
            }
            if !regs.contains_key(&canonical_root) {
                let feed = registry.get_or_create(&canonical_root);
                let child_cancel = self.connection_cancel.child_token();
                let guard = feed.register_editor_lifecycle(
                    self.editor_id,
                    self.reveal.clone(),
                    child_cancel.clone(),
                );
                regs.insert(
                    canonical_root.clone(),
                    WorkspaceRegistration {
                        guard,
                        cancel: child_cancel.clone(),
                    },
                );
                let turns = feed.subscribe();
                tokio::spawn(listen_for_captain_turns(
                    Arc::downgrade(self),
                    canonical_root,
                    child_cancel,
                    turns,
                ));
            }
        }
    }

    pub(crate) async fn resolve_document_root(&self, uri: &str) -> Option<PathBuf> {
        let ws = self.workspace.lock().await;
        let folders = self.folders.lock().await;
        if !folders.is_empty() {
            return resolve_root_for_uri(uri, &folders);
        }
        if let Some(ref w) = *ws {
            return resolve_root_for_uri(uri, std::slice::from_ref(w));
        }
        if !self.registered_root_mode.load(Ordering::SeqCst) {
            return Some(PathBuf::from("."));
        }
        None
    }

    pub(crate) async fn has_root(&self, root: &Path) -> bool {
        let key = WorkspaceFeedRegistry::canonical_key(root);
        let ws = self.workspace.lock().await;
        let folders = self.folders.lock().await;
        if !folders.is_empty() {
            return folders
                .iter()
                .any(|f| WorkspaceFeedRegistry::canonical_key(f) == key);
        }
        if let Some(ref w) = *ws {
            return WorkspaceFeedRegistry::canonical_key(w) == key;
        }
        !self.registered_root_mode.load(Ordering::SeqCst)
    }

    pub(crate) fn feed_for_workspace(&self, root: &Path) -> Option<ConversationFeed> {
        if let Some(registry) = &self.registry {
            Some(registry.get_or_create(root))
        } else {
            self.conversation.clone()
        }
    }

    pub(crate) async fn reflex_context_for_workspace(&self, root: &Path) -> Arc<ReflexContext> {
        let key = WorkspaceFeedRegistry::canonical_key(root);
        let mut map = self.reflex_contexts.lock().await;
        map.entry(key)
            .or_insert_with(|| {
                Arc::new(ReflexContext::new(
                    self.deps_reflex_context.clone(),
                    Arc::clone(&self.log),
                    self.metacognition_model.clone(),
                ))
            })
            .clone()
    }

    /// The workspace folders other than the primary one, for tools that must
    /// accept a path in any of them.
    #[allow(dead_code)]
    pub(crate) async fn extra_roots(&self) -> Vec<PathBuf> {
        let workspace = self.workspace.lock().await.clone();
        self.folders
            .lock()
            .await
            .iter()
            .filter(|f| Some(*f) != workspace.as_ref())
            .cloned()
            .collect()
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

    pub(crate) async fn report_startup_warnings(&self) {
        for message in std::mem::take(&mut *self.startup_warnings.lock().await) {
            self.client
                .show_message(tower_lsp_server::ls_types::MessageType::WARNING, message)
                .await;
        }
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
        let cancel = self.connection_cancel.clone();
        tokio::spawn(async move {
            let idle_ms = inner.config.lock().await.idle_ms;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_millis(idle_ms)) => {}
            }
            if cancel.is_cancelled() {
                return;
            }
            // Stale timer: a newer edit restarted the countdown.
            let current = inner.documents.lock().await.get(&uri).map(|d| d.version);
            if current == Some(version) {
                inner.evaluate(uri, TriggerKind::Idle).await;
            }
        });
    }

    pub(crate) async fn intent_for_workspace(&self, root: Option<&Path>) -> (String, bool) {
        let Some(root) = root else {
            return (String::new(), false);
        };
        if let Ok(Some(record)) = crate::navigator::read_intent(root) {
            let summary = record.summary().to_string();
            if !summary.trim().is_empty() {
                return (summary, true);
            }
        }
        let canonical_root = WorkspaceFeedRegistry::canonical_key(root);
        let intent = self.intent.lock().await;
        if let Some(text) = &intent.text {
            let matches_bound = match &intent.bound_root {
                Some(bound) => *bound == canonical_root,
                None => !self.registered_root_mode.load(Ordering::SeqCst),
            };
            if matches_bound {
                return (text.clone(), true);
            }
        }
        (String::new(), false)
    }

    /// The workspace intent record, falling back to the session intent set
    /// via `withnav/intent`. Returns the summary text and whether any
    /// intent exists.
    #[allow(dead_code)]
    pub(crate) async fn current_intent(&self) -> (String, bool) {
        let primary = self.workspace.lock().await.clone();
        self.intent_for_workspace(primary.as_deref()).await
    }

    /// Hands the document's in-flight slot from the instinct evaluation to a
    /// remark request: the instinct handle is released (a new trigger may
    /// evaluate while the remark is written), a remark still running for the
    /// document is superseded, and the new remark's handle is returned.
    /// `None` when typing already cancelled the instinct stage or the
    /// document is gone. `help_calls` are the calls for help the remark answers
    /// (empty for an ordinary remark); `conversation` marks a remark about
    /// the conversation, which is not tied to a version of the text.
    #[allow(clippy::too_many_arguments)]
    async fn begin_remark(
        &self,
        uri: &str,
        instinct_cancel: &CancellationToken,
        help_calls: &[String],
        conversation: bool,
        pinned_generation: u64,
        target: Option<&ConversationTarget>,
        feed: Option<&ConversationFeed>,
    ) -> Option<CancellationToken> {
        let mut state = self.documents.lock().await;
        if instinct_cancel.is_cancelled()
            || self.connection_cancel.is_cancelled()
            || self.routing_generation() != pinned_generation
            || target.is_some_and(|target| {
                !feed.is_some_and(|feed| {
                    feed.is_incarnation_available(target.session_id, target.incarnation)
                })
            })
        {
            return None;
        }
        let doc = state.get_mut(uri)?;
        doc.in_flight_cancel = None;
        doc.in_flight_version = None;
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
        if let Some(doc) = state.get_mut(uri)
            && !cancel.is_cancelled()
        {
            doc.remark_cancel = None;
            doc.remark_version = None;
            doc.help.end();
        }
    }

    /// Releases the document's in-flight instinct slot, unless the evaluation
    /// that took it was cancelled: an edit cancels the running trigger and
    /// clears the marker at once (`DocumentState::cancel_instinct`), so a
    /// cancelled trigger that unwinds afterwards must not erase the marker a
    /// successor registered in the vacant slot in the meantime. Without this
    /// guard the document loses the successor's handle: a third evaluation can
    /// start while the successor still runs, and the next edit has nothing to
    /// cancel.
    async fn clear_in_flight(&self, uri: &str, cancel: &CancellationToken) {
        let mut state = self.documents.lock().await;
        if let Some(doc) = state.get_mut(uri)
            && !cancel.is_cancelled()
        {
            doc.in_flight_cancel = None;
            doc.in_flight_version = None;
        }
    }

    /// `withnav/reflexShown`: the extension reporting display time of a
    /// suggestion, so the log holds both halves of the pause-to-display
    /// latency.
    pub(crate) async fn on_reflex_shown(&self, params: ReflexShownParams) {
        self.log.append(&LogRecord::ReflexShown {
            request_id: &params.request_id,
            shown_at_ms: params.shown_at_ms,
        });
    }

    /// `withnav/feedback` client->server notification.
    pub(crate) async fn on_feedback(&self, params: FeedbackParams) {
        self.log.append(&LogRecord::Feedback {
            id: &params.id,
            verdict: &params.verdict,
        });
    }

    /// `withnav/diagnostics`: keep the latest set per uri and run the
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

    /// `withnav/intent`: the user's current goal, global to the session.
    pub(crate) async fn on_intent(&self, params: IntentParams) {
        let text = params.text.trim().to_string();
        let primary = self.workspace.lock().await.clone();
        let bound_root = primary.map(|p| WorkspaceFeedRegistry::canonical_key(&p));
        let mut intent = self.intent.lock().await;
        intent.text = if text.is_empty() { None } else { Some(text) };
        intent.bound_root = bound_root;
    }

    /// `withnav/conversation`: explicitly binds or unbinds this editor connection to a session.
    pub(crate) async fn on_conversation(&self, params: ConversationParams) {
        let Ok(session_id) = params.session_id.parse::<ulid::Ulid>() else {
            tracing::warn!(
                "withnav/conversation: invalid sessionId '{}'",
                params.session_id
            );
            return;
        };
        let feed = if let Some(feed) = &self.conversation {
            feed.clone()
        } else if let Some(registry) = &self.registry {
            let ws = self
                .workspace
                .lock()
                .await
                .clone()
                .unwrap_or_else(|| PathBuf::from("."));
            registry.get_or_create(&ws)
        } else {
            tracing::warn!("withnav/conversation: no conversation feed available");
            return;
        };

        if params.focus {
            let res = if let Some(ref inc_str) = params.incarnation {
                if let Ok(incarnation) = inc_str.parse::<ulid::Ulid>() {
                    let target = ConversationTarget {
                        session_id,
                        incarnation,
                        workspace: feed.workspace().to_path_buf(),
                    };
                    feed.bind_editor_target(&target, self.editor_id)
                } else {
                    tracing::warn!("withnav/conversation: invalid incarnation '{}'", inc_str);
                    feed.bind_editor(session_id, self.editor_id)
                }
            } else {
                feed.bind_editor(session_id, self.editor_id)
            };
            if let Err(e) = res {
                tracing::warn!("failed to bind editor to session {session_id}: {e}");
            }
        } else {
            feed.unbind_editor_connection(session_id, self.editor_id);
        }
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
        if let Some(reflex) = &self.reflex {
            reflex.close_document(&uri).await;
        }
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
    pub(crate) async fn evaluate_conversation_turn(
        self: &Arc<Self>,
        uri: String,
        target: ConversationTarget,
        workspace_cancel: CancellationToken,
    ) {
        if self.connection_cancel.is_cancelled() || workspace_cancel.is_cancelled() {
            return;
        }
        let root = self.resolve_document_root(&uri).await;
        if self.connection_cancel.is_cancelled() || workspace_cancel.is_cancelled() {
            return;
        }
        let folders_empty = self.folders.lock().await.is_empty();
        if !folders_empty && root.is_none() {
            return;
        }
        if let Some(r) = &root
            && !r.as_os_str().is_empty()
            && !target.workspace.as_os_str().is_empty()
            && WorkspaceFeedRegistry::canonical_key(r)
                != WorkspaceFeedRegistry::canonical_key(&target.workspace)
        {
            return;
        }
        if self.config.lock().await.enabled {
            self.evaluate_flows_targeted(&uri, TriggerKind::Conversation, Some(target))
                .await;
        }
    }
}

/// Evaluates the `conversation` event on the focused document each time a
/// captain-caused turn ends. A turn caused by a navigator wake never gets
/// here as a trigger (no loop). Holds the server weakly: once the server is
/// gone the next event ends the task.
async fn listen_for_captain_turns(
    inner: std::sync::Weak<Runtime>,
    workspace: PathBuf,
    workspace_cancel: CancellationToken,
    mut turns: tokio::sync::broadcast::Receiver<ConversationEvent>,
) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        let event = tokio::select! {
            biased;
            _ = workspace_cancel.cancelled() => return,
            event = turns.recv() => match event {
                Ok(event) => event,
                // Missed events: the next turn end triggers anyway.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            }
        };
        let ConversationEvent::TurnEnded {
            session_id,
            incarnation,
            caused_by_captain: true,
            target_editor,
        } = event
        else {
            continue;
        };
        let Some(inner) = inner.upgrade() else {
            return;
        };
        if inner.connection_cancel.is_cancelled() || workspace_cancel.is_cancelled() {
            return;
        }
        if target_editor != Some(inner.editor_id) {
            continue;
        }
        let Some(uri) = inner.focused.lock().await.clone() else {
            continue;
        };
        let target = ConversationTarget {
            workspace: workspace.clone(),
            session_id,
            incarnation,
        };
        let cancel_for_turn = workspace_cancel.clone();
        tokio::spawn(async move {
            inner
                .evaluate_conversation_turn(uri, target, cancel_for_turn)
                .await;
        });
    }
}

/// Resolves a document path to its most-specific canonical registered workspace root.
/// Existing files are canonicalized directly; new files are resolved through their
/// nearest existing canonical parent directory. Returns None if the document lies outside
/// all known roots or cannot be canonicalized to an existing parent.
pub(crate) fn resolve_root_for_path(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    if roots.is_empty() {
        return None;
    }
    let canonical_doc = if path.exists() {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    } else {
        let mut current = path.parent();
        let mut resolved = None;
        while let Some(parent) = current {
            if parent.exists()
                && parent != Path::new("/")
                && let Ok(canonical_parent) = std::fs::canonicalize(parent)
                && let Ok(relative) = path.strip_prefix(parent)
            {
                resolved = Some(canonical_parent.join(relative));
                break;
            }
            current = parent.parent();
        }
        resolved?
    };

    let mut best_match: Option<(PathBuf, usize)> = None;
    for root in roots {
        let canonical_root = WorkspaceFeedRegistry::canonical_key(root);
        if canonical_doc.starts_with(&canonical_root) {
            let depth = canonical_root.components().count();
            match &best_match {
                Some((_, best_depth)) if depth > *best_depth => {
                    best_match = Some((root.clone(), depth));
                }
                None => {
                    best_match = Some((root.clone(), depth));
                }
                _ => {}
            }
        }
    }
    best_match.map(|(root, _)| root)
}

pub(crate) fn resolve_root_for_uri(uri: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    let path_str = crate::navigator::lsp::uri::uri_to_path_str(uri)?;
    let path = PathBuf::from(path_str);
    resolve_root_for_path(&path, roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigator::RemarkContext;
    use crate::navigator::conversation::SessionPost;
    use crate::navigator::lsp::protocol::Remark;
    use crate::navigator::ports::{
        Metacognition, ModelError, ReflexContextModel, ReflexContextRequest,
    };
    use std::time::Duration;
    use ulid::Ulid;

    struct SilentMetacognition;
    impl Metacognition for SilentMetacognition {
        fn remark<'a>(
            &'a self,
            _ctx: &'a RemarkContext,
            _cancel: &'a CancellationToken,
        ) -> futures::future::BoxFuture<'a, Result<Option<Remark>, ModelError>> {
            Box::pin(async { Ok(None) })
        }
    }

    struct DelayedReflexContextModel {
        delay: Duration,
        text: String,
    }
    impl ReflexContextModel for DelayedReflexContextModel {
        fn refresh<'a>(
            &'a self,
            _request: &'a ReflexContextRequest,
        ) -> futures::future::BoxFuture<'a, Result<String, ModelError>> {
            let delay = self.delay;
            let text = self.text.clone();
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                Ok(text)
            })
        }
    }

    #[test]
    fn navigator_ids_are_unique() {
        assert_ne!(
            crate::navigator::new_ulid_string(),
            crate::navigator::new_ulid_string()
        );
    }

    #[tokio::test]
    async fn empty_folder_routing_and_root_addition_do_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let deps = NavigatorDeps::new(Arc::new(SilentMetacognition), root.join("log"));
        let (service, _socket) = crate::navigator::build_service(deps);
        let runtime = service.inner().runtime().clone();
        // Force the reader to queue first on workspace. The updater queues behind
        // it and both must acquire folders in the same order after release.
        let workspace = runtime.workspace.lock().await;
        let reader = {
            let runtime = runtime.clone();
            let uri = format!("file://{}/new.rs", root.display());
            tokio::spawn(async move { runtime.resolve_document_root(&uri).await })
        };
        tokio::task::yield_now().await;
        let update = {
            let runtime = runtime.clone();
            let root = root.clone();
            tokio::spawn(async move { runtime.change_workspace_folders(vec![root], vec![]).await })
        };
        tokio::task::yield_now().await;
        drop(workspace);
        tokio::time::timeout(Duration::from_secs(1), async {
            reader.await.unwrap();
            update.await.unwrap();
        })
        .await
        .expect("routing and update must not deadlock");
        assert!(runtime.has_root(&root).await);
    }

    #[tokio::test]
    async fn stale_remark_admission_does_not_touch_existing_document_handles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let feed = ConversationFeed::with_workspace(root.clone());
        let session = Ulid::generate();
        let incarnation = feed.register_session(session, Arc::new(|_| Box::pin(async { Ok(()) })));
        let target = ConversationTarget {
            workspace: root.clone(),
            session_id: session,
            incarnation,
        };
        let deps = NavigatorDeps::new(Arc::new(SilentMetacognition), root.join("log"))
            .with_dirs(Some(root.clone()), None);
        let (service, _socket) = crate::navigator::build_service(deps);
        let runtime = service.inner().runtime().clone();
        let uri = format!("file://{}/file.rs", root.display());
        let mut docs = runtime.documents.lock().await;
        docs.open(&uri, "rust".into(), "fn main() {}".into(), 1);
        let previous = CancellationToken::new();
        let old_instinct = CancellationToken::new();
        let doc = docs.get_mut(&uri).unwrap();
        doc.in_flight_version = Some(7);
        doc.in_flight_cancel = Some(old_instinct.clone());
        doc.remark_cancel = Some(previous.clone());
        doc.remark_version = Some(5);
        doc.help.begin(&["existing help".into()]);
        let generation = runtime.routing_generation();
        let call = {
            let runtime = runtime.clone();
            let uri = uri.clone();
            let target = target.clone();
            let feed = feed.clone();
            tokio::spawn(async move {
                runtime
                    .begin_remark(
                        &uri,
                        &CancellationToken::new(),
                        &["new help".into()],
                        false,
                        generation,
                        Some(&target),
                        Some(&feed),
                    )
                    .await
            })
        };
        tokio::task::yield_now().await;
        feed.invalidate_session_incarnation(session, incarnation);
        drop(docs);
        assert!(call.await.unwrap().is_none());
        let docs = runtime.documents.lock().await;
        let doc = docs.get(&uri).unwrap();
        assert!(!previous.is_cancelled());
        assert!(!old_instinct.is_cancelled());
        assert_eq!(doc.in_flight_version, Some(7));
        assert_eq!(doc.remark_version, Some(5));
        assert!(doc.help.covers(&["existing help".into()]));
        assert!(!doc.help.covers(&["new help".into()]));
    }

    #[tokio::test]
    async fn one_workspace_two_editors_selection_sole_editor_explicit_binding_and_ambiguity() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = WorkspaceFeedRegistry::new();
        let feed = registry.get_or_create(tmp.path());

        let session_id = Ulid::generate();
        let post: SessionPost = Arc::new(|_| Box::pin(async { Ok(()) }));
        feed.register_session(session_id, post);

        let deps_a = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("a.jsonl"))
            .with_dirs(Some(tmp.path().to_path_buf()), None)
            .with_registry(registry.clone())
            .with_conversation(feed.clone());

        let deps_b = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("b.jsonl"))
            .with_dirs(Some(tmp.path().to_path_buf()), None)
            .with_registry(registry.clone())
            .with_conversation(feed.clone());

        let (service_a, _socket_a) = crate::navigator::build_service(deps_a);
        let (service_b, _socket_b) = crate::navigator::build_service(deps_b);

        let editor_id_a = service_a.inner().runtime().editor_id();
        let editor_id_b = service_b.inner().runtime().editor_id();
        assert_ne!(editor_id_a, editor_id_b);

        // Before initialize, editors are not registered in feed
        assert_eq!(
            feed.select_editor(Some(session_id)).unwrap_err(),
            crate::navigator::conversation::EditorSelectionError::Unavailable
        );

        // Registration after initialize
        service_a
            .inner()
            .runtime()
            .register_after_initialize()
            .await;
        service_b
            .inner()
            .runtime()
            .register_after_initialize()
            .await;

        // Both editors registered, neither explicitly bound -> Ambiguous
        assert_eq!(
            feed.select_editor(Some(session_id)).unwrap_err(),
            crate::navigator::conversation::EditorSelectionError::Ambiguous
        );

        // Explicit binding to Editor B
        assert_eq!(feed.bind_editor(session_id, editor_id_b), Ok(()));
        assert_eq!(
            feed.select_editor(Some(session_id)).unwrap().id,
            editor_id_b
        );

        // Detach Editor B: registration dropped, binding removed, sole editor fallback selects Editor A
        service_b.inner().runtime().detach();
        assert_eq!(
            feed.select_editor(Some(session_id)).unwrap().id,
            editor_id_a
        );

        // Resuming session under a new incarnation requires re-validation
        let new_inc = Ulid::generate();
        feed.register_session_incarnation(
            session_id,
            new_inc,
            Arc::new(|_| Box::pin(async { Ok(()) })),
            false,
        );
        // While rebuilding (available=false), binding fails
        assert_eq!(
            feed.bind_editor(session_id, editor_id_a),
            Err(crate::navigator::conversation::BindingError::SessionUnavailable)
        );

        // Once marked ready, binding succeeds
        feed.mark_incarnation_ready(session_id, new_inc);
        assert_eq!(feed.bind_editor(session_id, editor_id_a), Ok(()));
        assert_eq!(
            feed.select_editor(Some(session_id)).unwrap().id,
            editor_id_a
        );
    }

    #[test]
    fn multi_root_most_specific_canonical_root_resolution_and_new_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root_parent = tmp.path().join("workspace");
        let root_nested = root_parent.join("subproject");
        let external_dir = tmp.path().join("external");

        std::fs::create_dir_all(root_nested.join("src")).unwrap();
        std::fs::create_dir_all(root_parent.join("src")).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();

        let roots = vec![root_parent.clone(), root_nested.clone()];

        // Existing file in parent workspace
        let file_parent = root_parent.join("src/lib.rs");
        std::fs::write(&file_parent, "pub fn p() {}").unwrap();
        assert_eq!(
            resolve_root_for_path(&file_parent, &roots),
            Some(root_parent.clone())
        );

        // Existing file in nested workspace: most-specific canonical root wins
        let file_nested = root_nested.join("src/lib.rs");
        std::fs::write(&file_nested, "pub fn n() {}").unwrap();
        assert_eq!(
            resolve_root_for_path(&file_nested, &roots),
            Some(root_nested.clone())
        );

        // New file (not yet created on disk) in nested workspace resolves via existing canonical parent
        let new_nested_file = root_nested.join("src/brand_new.rs");
        assert!(!new_nested_file.exists());
        assert_eq!(
            resolve_root_for_path(&new_nested_file, &roots),
            Some(root_nested.clone())
        );

        // Existing file outside known roots resolves to None (no borrowed root)
        let outside_file = external_dir.join("other.rs");
        std::fs::write(&outside_file, "pub fn o() {}").unwrap();
        assert_eq!(resolve_root_for_path(&outside_file, &roots), None);

        // Non-existent file whose parents do not exist resolves to None
        let fake_path = Path::new("/nonexistent/external/path/file.rs");
        assert_eq!(resolve_root_for_path(fake_path, &roots), None);

        // Missing / external files in single-root mode NEVER fall back to the single root
        let single_root = vec![root_parent.clone()];
        assert_eq!(resolve_root_for_path(&outside_file, &single_root), None);
        assert_eq!(resolve_root_for_path(fake_path, &single_root), None);
    }

    #[tokio::test]
    async fn multi_root_intent_and_reflex_cache_isolation() {
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("root_a");
        let root_b = tmp.path().join("root_b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();

        // Root A has a persistent intent record
        let intent_file = root_a.join(".withnav/intent.md");
        std::fs::create_dir_all(intent_file.parent().unwrap()).unwrap();
        std::fs::write(&intent_file, "## Intent\nTarget A Intent\n").unwrap();

        let deps = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("log.jsonl"))
            .with_dirs(Some(root_a.clone()), None);

        let (service, _socket) = crate::navigator::build_service(deps);
        let runtime = service.inner().runtime();
        runtime
            .set_workspace(Some(root_a.clone()), vec![root_a.clone(), root_b.clone()])
            .await;

        // Root A gets its file-based intent
        let (intent_a, has_a) = runtime.intent_for_workspace(Some(&root_a)).await;
        assert!(has_a);
        assert!(intent_a.contains("Target A Intent"));

        // Root B has no intent record and does NOT borrow Root A's intent
        let (intent_b, has_b) = runtime.intent_for_workspace(Some(&root_b)).await;
        assert!(!has_b);
        assert!(intent_b.is_empty());

        // Reflex contexts are isolated per root
        let reflex_ctx_a = runtime.reflex_context_for_workspace(&root_a).await;
        let reflex_ctx_b = runtime.reflex_context_for_workspace(&root_b).await;
        assert!(!Arc::ptr_eq(&reflex_ctx_a, &reflex_ctx_b));
    }

    #[tokio::test]
    async fn nested_root_addition_and_removal_updates_routing() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let nested = parent.join("nested");
        std::fs::create_dir_all(nested.join("src")).unwrap();
        std::fs::create_dir_all(parent.join("src")).unwrap();

        let target_file = nested.join("src/lib.rs");
        std::fs::write(&target_file, "pub fn n() {}").unwrap();
        let target_uri = format!("file://{}", target_file.display());

        let deps = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("log.jsonl"));
        let (service, _socket) = crate::navigator::build_service(deps);
        let runtime = service.inner().runtime();

        // 1. Initially only parent folder is registered
        runtime
            .set_workspace(Some(parent.clone()), vec![parent.clone()])
            .await;
        assert_eq!(
            runtime.resolve_document_root(&target_uri).await,
            Some(parent.clone())
        );
        let gen_1 = runtime.routing_generation();

        // 2. Nested folder is added dynamically -> routing switches to the most-specific nested root
        runtime
            .change_workspace_folders(vec![nested.clone()], vec![])
            .await;
        let gen_2 = runtime.routing_generation();
        assert!(gen_2 > gen_1);
        assert_eq!(
            runtime.resolve_document_root(&target_uri).await,
            Some(nested.clone())
        );

        // 3. Nested folder is removed -> routing reverts to parent
        runtime
            .change_workspace_folders(vec![], vec![nested.clone()])
            .await;
        let gen_3 = runtime.routing_generation();
        assert!(gen_3 > gen_2);
        assert_eq!(
            runtime.resolve_document_root(&target_uri).await,
            Some(parent)
        );
    }

    #[tokio::test]
    async fn workspace_folder_removal_cancels_only_child_token_and_unregisters() {
        let tmp = tempfile::tempdir().unwrap();
        let root_1 = tmp.path().join("root_1");
        let root_2 = tmp.path().join("root_2");
        std::fs::create_dir_all(root_1.join("src")).unwrap();
        std::fs::create_dir_all(root_2.join("src")).unwrap();

        let registry = WorkspaceFeedRegistry::new();
        let feed_1 = registry.get_or_create(&root_1);
        let feed_2 = registry.get_or_create(&root_2);

        let session_1 = Ulid::generate();
        let session_2 = Ulid::generate();
        feed_1.register_session(session_1, Arc::new(|_| Box::pin(async { Ok(()) })));
        feed_2.register_session(session_2, Arc::new(|_| Box::pin(async { Ok(()) })));

        let deps = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("log.jsonl"))
            .with_registry(registry.clone());

        let (service, _socket) = crate::navigator::build_service(deps);
        let runtime = service.inner().runtime();

        // Set workspace with both roots
        runtime
            .set_workspace(Some(root_1.clone()), vec![root_1.clone(), root_2.clone()])
            .await;

        // Both feeds have the editor registered
        let editor_id = runtime.editor_id();
        assert_eq!(feed_1.select_editor(Some(session_1)).unwrap().id, editor_id);
        assert_eq!(feed_2.select_editor(Some(session_2)).unwrap().id, editor_id);

        // Remove root_2
        runtime
            .change_workspace_folders(vec![], vec![root_2.clone()])
            .await;

        // Root 2 is unregistered from feed 2 and selection returns Unavailable
        assert_eq!(
            feed_2.select_editor(Some(session_2)).unwrap_err(),
            crate::navigator::conversation::EditorSelectionError::Unavailable
        );

        // Root 1 remains registered and active in feed 1
        assert_eq!(feed_1.select_editor(Some(session_1)).unwrap().id, editor_id);

        // The connection token itself was NOT cancelled
        assert!(!runtime.connection_cancel().is_cancelled());

        // Document in removed root 2 now fails root resolution
        let file_2 = root_2.join("src/lib.rs");
        std::fs::write(&file_2, "fn r2() {}").unwrap();
        let uri_2 = format!("file://{}", file_2.display());
        assert_eq!(runtime.resolve_document_root(&uri_2).await, None);

        // Now remove root 1 as well: all roots removed
        runtime
            .change_workspace_folders(vec![], vec![root_1.clone()])
            .await;
        assert_eq!(
            feed_1.select_editor(Some(session_1)).unwrap_err(),
            crate::navigator::conversation::EditorSelectionError::Unavailable
        );

        // In registered-root mode, with all roots removed, root resolution returns None (no fallback)
        let file_1 = root_1.join("src/lib.rs");
        std::fs::write(&file_1, "fn r1() {}").unwrap();
        let uri_1 = format!("file://{}", file_1.display());
        assert_eq!(runtime.resolve_document_root(&uri_1).await, None);
    }

    #[tokio::test]
    async fn connection_cancellation_suppresses_late_reflex_cache_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let model = Arc::new(DelayedReflexContextModel {
            delay: Duration::from_millis(50),
            text: "REFRESHED TEXT".into(),
        });
        let log = Arc::new(NavLog::new(tmp.path().join("log.jsonl")));
        let reflex_ctx = Arc::new(ReflexContext::new(Some(model), log, Some("model".into())));

        let cancel = CancellationToken::new();
        let claim = reflex_ctx
            .claim("file:///doc.rs", Some(1), "intent", "conv", None)
            .await
            .expect("claim granted");

        // Spawn refresh with connection cancellation token
        reflex_ctx.spawn_refresh(
            claim,
            "file:///doc.rs",
            "intent".into(),
            "conv".into(),
            "rust".into(),
            "fn main() {}",
            Some(cancel.clone()),
        );

        // Cancel while model is in flight
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();

        // Wait for the model delay to elapse
        tokio::time::sleep(Duration::from_millis(70)).await;

        // Cache was NOT updated after cancellation
        assert_eq!(reflex_ctx.text().await, None);
    }

    #[tokio::test]
    async fn live_state_snapshot_does_not_leak_edits_from_foreign_workspace_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("root_a");
        let root_b = tmp.path().join("root_b");
        std::fs::create_dir_all(root_a.join("src")).unwrap();
        std::fs::create_dir_all(root_b.join("src")).unwrap();

        let file_a = root_a.join("src/main.rs");
        let file_b = root_b.join("src/foreign.rs");
        std::fs::write(&file_a, "fn main() {}\n").unwrap();
        std::fs::write(&file_b, "fn foreign() {}\n").unwrap();
        let uri_a = format!("file://{}", file_a.display());
        let uri_b = format!("file://{}", file_b.display());

        let deps = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("log.jsonl"));
        let (service, _socket) = crate::navigator::build_service(deps);
        let runtime = service.inner().runtime();
        runtime
            .set_workspace(Some(root_a.clone()), vec![root_a.clone(), root_b.clone()])
            .await;

        // Open file in root A and file in root B
        runtime
            .documents
            .lock()
            .await
            .open(&uri_a, "rust".into(), "fn main() {}\n".into(), 1);
        runtime
            .documents
            .lock()
            .await
            .open(&uri_b, "rust".into(), "fn foreign() {}\n".into(), 1);

        // Make an edit in Root B document
        runtime.documents.lock().await.change(
            &uri_b,
            "fn foreign() {\n    println!(\"foreign edit\");\n}\n".into(),
            2,
        );

        // Gather context for trigger on Root A document
        let ctx_a = runtime
            .gather(&uri_a, TriggerKind::Save, tokio::time::Instant::now())
            .await
            .expect("context gathered");

        // The live_state for Root A must NOT contain any edit from Root B's foreign.rs
        assert!(
            !ctx_a.live_state_text.contains("foreign.rs"),
            "live state for root A must not leak foreign.rs edits, got: {}",
            ctx_a.live_state_text
        );
    }

    #[tokio::test]
    async fn root_changes_invalidate_in_flight_effects_and_caches() {
        let tmp = tempfile::tempdir().unwrap();
        let root_1 = tmp.path().join("root_1");
        let root_2 = tmp.path().join("root_2");
        std::fs::create_dir_all(&root_1).unwrap();
        std::fs::create_dir_all(&root_2).unwrap();

        let deps = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("log.jsonl"));
        let (service, _socket) = crate::navigator::build_service(deps);
        let runtime = service.inner().runtime();

        runtime
            .set_workspace(Some(root_1.clone()), vec![root_1.clone()])
            .await;

        // Initialize reflex cache for root 1
        let ctx_1 = runtime.reflex_context_for_workspace(&root_1).await;
        let gen_before = runtime.routing_generation();

        // Adding root 2 updates routing generation and clears reflex contexts
        runtime
            .change_workspace_folders(vec![root_2.clone()], vec![])
            .await;

        let gen_after = runtime.routing_generation();
        assert!(gen_after > gen_before);

        // Reflex context for root 1 was cleared and recreated fresh
        let ctx_1_after = runtime.reflex_context_for_workspace(&root_1).await;
        assert!(!Arc::ptr_eq(&ctx_1, &ctx_1_after));
    }

    #[tokio::test]
    async fn legacy_feed_with_nonempty_workspace_registers_editor() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let feed = ConversationFeed::new();
        let session_id = Ulid::generate();
        feed.register_session(session_id, Arc::new(|_| Box::pin(async { Ok(()) })));

        let deps = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("log.jsonl"))
            .with_dirs(Some(workspace.clone()), None)
            .with_conversation(feed.clone());

        let (service, _socket) = crate::navigator::build_service(deps);
        let runtime = service.inner().runtime();

        runtime.register_after_initialize().await;

        assert_eq!(
            feed.select_editor(Some(session_id)).unwrap().id,
            runtime.editor_id()
        );
    }

    #[tokio::test]
    async fn unqualified_intent_remains_bound_to_original_primary_after_root_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("root_a");
        let root_b = tmp.path().join("root_b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();

        let deps = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("log.jsonl"))
            .with_dirs(Some(root_a.clone()), None);

        let (service, _socket) = crate::navigator::build_service(deps);
        let runtime = service.inner().runtime();

        runtime
            .set_workspace(Some(root_a.clone()), vec![root_a.clone(), root_b.clone()])
            .await;

        runtime
            .on_intent(crate::navigator::IntentParams {
                text: "Refactor core module".into(),
            })
            .await;

        let (intent_a, has_a) = runtime.intent_for_workspace(Some(&root_a)).await;
        assert!(has_a);
        assert_eq!(intent_a, "Refactor core module");

        let (_, has_b) = runtime.intent_for_workspace(Some(&root_b)).await;
        assert!(!has_b);

        runtime
            .change_workspace_folders(vec![], vec![root_a.clone()])
            .await;

        let (intent_b_after, has_b_after) = runtime.intent_for_workspace(Some(&root_b)).await;
        assert!(!has_b_after);
        assert!(intent_b_after.is_empty());
    }

    #[tokio::test]
    async fn bootstrap_root_is_not_borrowed_for_external_file() {
        let tmp = tempfile::tempdir().unwrap();
        let bootstrap_root = tmp.path().join("workspace");
        let external_dir = tmp.path().join("external");
        std::fs::create_dir_all(bootstrap_root.join("src")).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();

        let deps = NavigatorDeps::new(Arc::new(SilentMetacognition), tmp.path().join("log.jsonl"))
            .with_dirs(Some(bootstrap_root.clone()), None);

        let (service, _socket) = crate::navigator::build_service(deps);
        let runtime = service.inner().runtime();

        runtime.set_workspace(None, vec![]).await;

        let internal_file = bootstrap_root.join("src/lib.rs");
        std::fs::write(&internal_file, "fn i() {}").unwrap();
        let internal_uri = format!("file://{}", internal_file.display());
        assert_eq!(
            runtime.resolve_document_root(&internal_uri).await,
            Some(bootstrap_root)
        );

        let external_file = external_dir.join("other.rs");
        std::fs::write(&external_file, "fn e() {}").unwrap();
        let external_uri = format!("file://{}", external_file.display());
        assert_eq!(runtime.resolve_document_root(&external_uri).await, None);
    }

    #[tokio::test]
    async fn late_remark_after_root_change_is_cancelled_at_publication() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("src")).unwrap();

        let file = root.join("src/lib.rs");
        std::fs::write(&file, "fn old() {}\n").unwrap();
        let uri = format!("file://{}", file.display());

        struct DelayedCommenter {
            gate: Arc<tokio::sync::Notify>,
            entered: Arc<tokio::sync::Notify>,
        }
        impl Metacognition for DelayedCommenter {
            fn remark<'a>(
                &'a self,
                _ctx: &'a RemarkContext,
                _cancel: &'a CancellationToken,
            ) -> futures::future::BoxFuture<'a, Result<Option<Remark>, ModelError>> {
                let gate = self.gate.clone();
                let entered = self.entered.clone();
                Box::pin(async move {
                    entered.notify_one();
                    gate.notified().await;
                    Ok(Some(Remark {
                        line: 0,
                        text: "late remark".into(),
                    }))
                })
            }
        }

        let gate = Arc::new(tokio::sync::Notify::new());
        let entered = Arc::new(tokio::sync::Notify::new());
        let commenter = Arc::new(DelayedCommenter {
            gate: gate.clone(),
            entered: entered.clone(),
        });

        let deps = NavigatorDeps::new(commenter, tmp.path().join("log.jsonl"))
            .with_dirs(Some(root.clone()), None);

        let (service, _socket) = crate::navigator::build_service(deps);
        let runtime = service.inner().runtime();

        runtime
            .set_workspace(Some(root.clone()), vec![root.clone()])
            .await;

        runtime
            .documents
            .lock()
            .await
            .open(&uri, "rust".into(), "fn old() {}\n".into(), 1);

        let ctx = runtime
            .gather(&uri, TriggerKind::Save, tokio::time::Instant::now())
            .await
            .unwrap();

        let pending = crate::navigator::log::PendingTrigger::new(
            "run",
            Some(root.display().to_string()),
            &uri,
            1,
            "save",
            ctx.prefilter_inputs(),
        );

        let gen_before = runtime.routing_generation();

        let deliver_task = {
            let rt = runtime.clone();
            let uri = uri.clone();
            let root = root.clone();
            tokio::spawn(async move {
                rt.deliver_remark(
                    uri,
                    ctx,
                    pending,
                    "prompt".into(),
                    CancellationToken::new(),
                    root,
                    gen_before,
                    None,
                    None,
                    std::collections::BTreeMap::new(),
                )
                .await;
            })
        };

        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        let docs = runtime.documents.lock().await;
        gate.notify_one();
        // Keep the document lock held while the completed model unwinds.
        tokio::task::yield_now().await;
        runtime
            .change_workspace_folders(vec![], vec![root.clone()])
            .await;
        drop(docs);
        deliver_task.await.unwrap();

        let log_text = std::fs::read_to_string(tmp.path().join("log.jsonl")).unwrap();
        assert!(log_text.contains("\"delivered\":\"cancelled\""));
        assert!(!log_text.contains("\"delivered\":\"remark\""));

        // Document state was not mutated
        let docs = runtime.documents.lock().await;
        assert!(docs.get(&uri).unwrap().last_remark.is_none());
    }

    #[tokio::test]
    async fn no_factory_produces_no_model_call_after_generation_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("src")).unwrap();

        let file = root.join("src/lib.rs");
        std::fs::write(&file, "fn r() {}\n").unwrap();
        let uri = format!("file://{}", file.display());

        let model_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let model_called_clone = model_called.clone();

        struct CountingCommenter {
            called: Arc<std::sync::atomic::AtomicBool>,
        }
        impl Metacognition for CountingCommenter {
            fn remark<'a>(
                &'a self,
                _ctx: &'a RemarkContext,
                _cancel: &'a CancellationToken,
            ) -> futures::future::BoxFuture<'a, Result<Option<Remark>, ModelError>> {
                self.called.store(true, Ordering::SeqCst);
                Box::pin(async move { Ok(None) })
            }
        }

        let deps = NavigatorDeps::new(
            Arc::new(CountingCommenter {
                called: model_called_clone,
            }),
            tmp.path().join("log.jsonl"),
        )
        .with_dirs(Some(root.clone()), None);

        let (service, _socket) = crate::navigator::build_service(deps);
        let runtime = service.inner().runtime();

        runtime
            .set_workspace(Some(root.clone()), vec![root.clone()])
            .await;

        runtime
            .documents
            .lock()
            .await
            .open(&uri, "rust".into(), "fn r() {}\n".into(), 1);

        // Generation changed before evaluation begins.
        runtime.routing_generation.fetch_add(1, Ordering::SeqCst);

        // No flow factory is configured, so no evaluation is started.
        let evaluated = runtime.evaluate_flows(&uri, TriggerKind::Save).await;
        assert!(!evaluated);
        assert!(!model_called.load(Ordering::SeqCst));
    }
}

/// The folder bookkeeping of `workspace/didChangeWorkspaceFolders`, apart from
/// the locks so it can be tested on plain values.
fn apply_folder_change(
    primary: &mut Option<PathBuf>,
    folders: &mut Vec<PathBuf>,
    added: Vec<PathBuf>,
    removed: Vec<PathBuf>,
) {
    folders.retain(|f| !removed.contains(f));
    for folder in added {
        if !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    if primary.as_ref().is_some_and(|p| removed.contains(p)) {
        *primary = folders.first().cloned();
    }
}

#[cfg(test)]
mod folder_tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn a_folder_added_later_is_known_and_the_primary_stays() {
        let mut primary = Some(p("/a"));
        let mut folders = vec![p("/a")];
        apply_folder_change(&mut primary, &mut folders, vec![p("/b")], vec![]);
        assert_eq!(primary, Some(p("/a")));
        assert_eq!(folders, vec![p("/a"), p("/b")]);
    }

    #[test]
    fn removing_the_primary_promotes_the_first_remaining_folder() {
        let mut primary = Some(p("/a"));
        let mut folders = vec![p("/a"), p("/b")];
        apply_folder_change(&mut primary, &mut folders, vec![], vec![p("/a")]);
        assert_eq!(primary, Some(p("/b")));
        assert_eq!(folders, vec![p("/b")]);
    }

    #[test]
    fn removing_the_only_folder_clears_primary_and_folders() {
        let mut primary = Some(p("/a"));
        let mut folders = vec![p("/a")];
        apply_folder_change(&mut primary, &mut folders, vec![], vec![p("/a")]);
        assert_eq!(primary, None);
        assert_eq!(folders, Vec::<PathBuf>::new());
    }
}
