//! The LSP backend: document sync, the decision pipeline wiring, idle
//! timers, cancellation, custom notifications, JSONL logging.
//!
//! All mutable state lives in [`Inner`] behind an `Arc`, so trait methods
//! (`&self`) can spawn idle timers and metacognition tasks that capture the
//! state without needing an `Arc<NavigatorBackend>`.
//!
//! Every trigger runs through the pipeline: save/idle/diagnostics events,
//! pre-filters, one instinct call, first matching branch. A `remark` branch
//! calls the metacognition with the branch prompt rendered through the
//! pipeline's templates; `wake_cognition` is logged (nothing shown) while
//! the cognition layer does not exist.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::Instant;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};

use crate::navigator::commenter::{Diag, RemarkContext};
use crate::navigator::config::NavigatorConfig;
use crate::navigator::document::{text_around_line, DocumentState};
use crate::navigator::log::{
    excerpt, now_iso8601, DeliveryOutcome, LogRecord, NavLog, PendingTrigger,
};
use crate::navigator::pipeline::{
    Action, Pipeline, PipelineError, PipelineSnapshot, TriggerInputs, TriggerKind, NO_REMARK_MS,
};
use crate::navigator::ports::{
    DecisionModel, ModelError, ReflexContextModel, ReflexContextRequest,
};
use crate::navigator::protocol::{
    ClearNotification, ClearParams, DiagnosticItem, DiagnosticsParams, FeedbackParams,
    InlineCompletionResult, IntentParams, Position, ReflexShownParams, RemarkNotification,
    RemarkParams, ServerInfoNotification, ServerInfoParams, StatusNotification, StatusParams,
};
use crate::navigator::reflex::{to_result, ReflexService};
use crate::navigator::{ConversationEvent, Metacognition, NavigatorDeps, WakeOutcome};

pub struct NavigatorBackend {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    metacognition: Arc<dyn Metacognition>,
    log: NavLog,
    /// One id per server process, grouping every trigger record it writes.
    run_id: String,
    config: Mutex<NavigatorConfig>,
    state: Mutex<HashMap<String, DocumentState>>,
    language_ids: Mutex<HashMap<String, String>>,
    diagnostics: Mutex<HashMap<String, Vec<Diag>>>,
    intent: Mutex<Option<String>>,
    /// The recent live state per uri: the last few edit summaries each
    /// `gather` saw, newest last, rendered into `{{live_state}}`.
    live_state: Mutex<HashMap<String, VecDeque<EditSummary>>>,
    /// The workspace root (pipeline + intent resolution); set at initialize.
    workspace: Mutex<Option<PathBuf>>,
    /// The user-level navigator dir, fixed at construction.
    user_dir: Option<PathBuf>,
    /// The active pipeline; swapped only by a successful reload.
    pipeline: Mutex<Pipeline>,
    /// The instinct seam; None keeps the layer disabled.
    decision: Option<Arc<dyn DecisionModel>>,
    /// Model ids for the per-layer call records.
    instinct_model: Option<String>,
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
    /// The metacognition's reflex-context seam; None keeps the intent
    /// summary as the reflex context.
    reflex_context_model: Option<Arc<dyn ReflexContextModel>>,
    /// The refreshed reflex context and what it was computed from.
    reflex_context: Mutex<ReflexContextCache>,
    /// The focused document: the last uri that received `didOpen`,
    /// `didChange` or `didSave`. A driver turn in the conversation is
    /// judged against it; None until the first of those arrives.
    focused: Mutex<Option<String>>,
}

/// The reflex context the metacognition keeps current: the text served in
/// front of the code and what it was computed from. Until the first
/// refresh succeeds `text` is None and the intent summary is served.
#[derive(Default)]
struct ReflexContextCache {
    text: Option<String>,
    /// Hash of (intent summary, conversation digest, active file uri) the
    /// current text was computed from.
    inputs: Option<u64>,
    /// Start of the latest refresh attempt, successful or not.
    last_attempt: Option<Instant>,
    refreshing: bool,
    /// The latest inline-completion cursor: the excerpt anchor for
    /// refreshes triggered from `didSave`.
    cursor: Option<(String, u32)>,
}

/// A refresh runs at most this often (and only when its inputs changed).
const REFLEX_CONTEXT_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// The refreshed text is kept to about 300 tokens.
const REFLEX_CONTEXT_MAX_CHARS: usize = 1200;
/// Lines of document on each side of the cursor in the refresh excerpt.
const REFLEX_EXCERPT_RADIUS: usize = 20;

impl NavigatorBackend {
    pub fn new(client: Client, deps: NavigatorDeps) -> Self {
        // Pipeline floor: whatever resolves now (project > user > built-in).
        // A failure falls back to the built-in so the editor keeps working;
        // reload retries and surfaces errors.
        let snapshot = load_snapshot(deps.workspace.as_deref(), deps.user_dir.as_deref())
            .unwrap_or_else(|e| {
                tracing::error!("initial pipeline load failed, using built-in: {e}");
                PipelineSnapshot::builtin().expect("the built-in pipeline must validate")
            });
        let reflex = deps.fim.map(ReflexService::new);
        let turns = deps.conversation.as_ref().map(|feed| feed.subscribe());
        let backend = Self {
            inner: Arc::new(Inner {
                client,
                metacognition: deps.commenter,
                log: NavLog::new(deps.log_path),
                run_id: crate::navigator::new_ulid_string(),
                config: Mutex::new(NavigatorConfig::default()),
                state: Mutex::new(HashMap::new()),
                language_ids: Mutex::new(HashMap::new()),
                diagnostics: Mutex::new(HashMap::new()),
                intent: Mutex::new(None),
                live_state: Mutex::new(HashMap::new()),
                workspace: Mutex::new(deps.workspace),
                user_dir: deps.user_dir,
                pipeline: Mutex::new(Pipeline::new(snapshot)),
                decision: deps.decision,
                instinct_model: deps.instinct_model,
                metacognition_model: deps.metacognition_model,
                reflex,
                reflex_model: deps.reflex_model,
                server_info: deps.server_info,
                conversation: deps.conversation,
                disabled_layers: deps.disabled_layers,
                reflex_context_model: deps.reflex_context,
                reflex_context: Mutex::new(ReflexContextCache::default()),
                focused: Mutex::new(None),
            }),
        };
        if let Some(turns) = turns {
            tokio::spawn(listen_for_driver_turns(
                Arc::downgrade(&backend.inner),
                turns,
            ));
        }
        backend
    }
}

/// Evaluates the `conversation` event on the focused document each time a
/// driver-caused turn ends. A turn caused by a navigator wake never gets
/// here as a trigger (no loop). Holds the server weakly: once the server is
/// gone the next event ends the task.
async fn listen_for_driver_turns(
    inner: std::sync::Weak<Inner>,
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

/// Pipeline resolution used at construction and reload: the workspace's
/// `.navigator/`, else the user dir, else the built-in.
fn load_snapshot(
    workspace: Option<&std::path::Path>,
    user_dir: Option<&std::path::Path>,
) -> std::result::Result<PipelineSnapshot, PipelineError> {
    match workspace {
        Some(ws) => {
            PipelineSnapshot::load(ws, user_dir.unwrap_or_else(|| std::path::Path::new("")))
        }
        None => PipelineSnapshot::builtin(),
    }
}

/// The wake message the metacognition sends the cognition: what changed,
/// where, why it looks like drift, and the intent it drifts from — plus the
/// recent conversation so the cognition can place the remark. Short by
/// construction: it starts a deliberate discussion, it is not the
/// discussion.
fn wake_message(ctx: &TriggerContext, digest: &str) -> String {
    let mut out = format!(
        "Navigator wake: this edit looks at odds with the intent on record or with what was \
         just agreed in this conversation.\n\n\
         File: {file}\nWhat changed:\n{diff}\n\nIntent on record:\n{intent}",
        file = ctx.uri,
        diff = if ctx.diff.trim().is_empty() {
            "(no diff captured)".to_string()
        } else {
            ctx.diff.trim().to_string()
        },
        intent = if ctx.intent_text.trim().is_empty() {
            "(none recorded)".to_string()
        } else {
            ctx.intent_text.trim().to_string()
        },
    );
    if !digest.trim().is_empty() {
        out.push_str("\n\nRecent conversation:\n");
        out.push_str(digest);
    }
    out
}

/// The first changed line (1-based) and its text in the new document:
/// where this edit starts and what it says. `(0, "")` when nothing was
/// inserted (a pure deletion has no new line to quote).
fn first_changed_line(old: &str, new: &str) -> (usize, String) {
    let diff = similar::TextDiff::from_lines(old, new);
    for change in diff.iter_all_changes() {
        if matches!(change.tag(), similar::ChangeTag::Insert) {
            if let Some(idx) = change.new_index() {
                return (idx + 1, change.value().trim_end().to_string());
            }
        }
    }
    (0, String::new())
}

/// Renders the live state as plain text: the newest edits first, then the
/// latest error/warning diagnostics, capped in lines and characters.
fn render_live_state(
    live: &HashMap<String, VecDeque<EditSummary>>,
    diagnostics: &HashMap<String, Vec<Diag>>,
) -> String {
    let mut edits: Vec<&EditSummary> = live.values().flat_map(|ring| ring.iter()).collect();
    edits.sort_by(|a, b| b.ts.cmp(&a.ts));
    let mut out = String::new();
    for e in edits.iter().take(LIVE_STATE_EDITS) {
        let _ = writeln!(
            out,
            "{} changed {} line(s) at {} | {}",
            e.ts, e.changed_lines, e.first_line, e.excerpt
        );
    }
    let mut diags: Vec<String> = Vec::new();
    for (uri, items) in diagnostics {
        for d in items {
            if !matches!(d.severity.as_str(), "error" | "warning") {
                continue;
            }
            diags.push(format!(
                "{} line {}: [{}] {}",
                uri.rsplit('/').next().unwrap_or(uri),
                d.line + 1,
                d.severity,
                d.message
            ));
            if diags.len() >= LIVE_STATE_DIAGS {
                break;
            }
        }
        if diags.len() >= LIVE_STATE_DIAGS {
            break;
        }
    }
    for d in diags {
        let _ = writeln!(out, "{d}");
    }
    excerpt(out.trim_end(), LIVE_STATE_MAX_CHARS)
}

/// One recorded edit of the live state: when `gather` ran, how many lines
/// changed, where the first change sits and what that line says.
struct EditSummary {
    ts: String,
    changed_lines: i64,
    first_line: usize,
    excerpt: String,
}

/// Lines on each side of the last edited line shown to the conversation
/// trigger.
const CONVERSATION_CONTEXT_LINES: usize = 20;

/// Edit summaries kept per uri in the live state.
const LIVE_STATE_EDITS: usize = 5;

/// Diagnostics lines kept in the live state, in total across uris.
const LIVE_STATE_DIAGS: usize = 20;

/// Total character cap of the rendered `{{live_state}}`.
const LIVE_STATE_MAX_CHARS: usize = 1500;

/// One trigger's gathered context: everything the pipeline inputs, the
/// templates and the log record need.
struct TriggerContext {
    uri: String,
    event: &'static str,
    version: i32,
    changed_lines: i64,
    since_last_remark_ms: i64,
    has_errors: bool,
    has_intent: bool,
    has_conversation: bool,
    intent_text: String,
    language_id: String,
    diff: String,
    context_text: String,
    diagnostics_text: String,
    conversation_text: String,
    live_state_text: String,
}

impl TriggerContext {
    fn template_inputs(&self) -> std::collections::BTreeMap<String, String> {
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

    fn prefilter_inputs(&self) -> std::collections::BTreeMap<String, serde_json::Value> {
        std::collections::BTreeMap::from([
            (
                "changed_lines".to_string(),
                serde_json::json!(self.changed_lines),
            ),
            (
                "since_last_remark_ms".to_string(),
                serde_json::json!(self.since_last_remark_ms),
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

    /// Cancels any in-flight evaluation for a document.
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
                inner.evaluate(uri, TriggerKind::Idle).await;
            }
        });
    }

    /// Reloads the pipeline from disk (hot reload on didSave under
    /// `.navigator/` or the user dir). The whole snapshot is validated
    /// before it replaces the active one, so a load error keeps the previous
    /// snapshot running and surfaces in the status bar.
    async fn reload_pipeline(self: &Arc<Self>) {
        let workspace = self.workspace.lock().await.clone();
        let user_dir = self.user_dir.clone();
        match load_snapshot(workspace.as_deref(), user_dir.as_deref()) {
            Ok(snapshot) => {
                let hash = snapshot.hash().to_string();
                let source = snapshot.source().as_str();
                *self.pipeline.lock().await = Pipeline::new(snapshot);
                tracing::info!(hash, source, "pipeline reloaded");
            }
            Err(e) => {
                self.set_status(
                    "error",
                    Some(format!(
                        "pipeline reload failed ({origin}): {e}",
                        origin = e.origin()
                    )),
                )
                .await;
            }
        }
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

    /// The open documents keyed by absolute path (current editor text): the
    /// metacognition's `read` tool answers from these instead of disk.
    async fn open_buffers(&self) -> std::collections::BTreeMap<PathBuf, String> {
        let state = self.state.lock().await;
        state
            .iter()
            .filter_map(|(uri, doc)| {
                crate::navigator::uri_to_path_str(uri).map(|p| (PathBuf::from(p), doc.text.clone()))
            })
            .collect()
    }

    /// Gathers everything one trigger needs from the document state.
    async fn gather(&self, uri: &str, event: TriggerKind, now: Instant) -> Option<TriggerContext> {
        let (changed_lines, since_last_remark_ms, version, doc_text, baseline, last_edit_line) = {
            let state = self.state.lock().await;
            let doc = state.get(uri)?;
            let since = doc
                .last_remark
                .map(|t| (now.duration_since(t)).as_millis() as i64)
                .unwrap_or(NO_REMARK_MS);
            (
                doc.changed_lines_since_baseline() as i64,
                since,
                doc.version,
                doc.text.clone(),
                doc.baseline.clone(),
                doc.last_edit_line,
            )
        };
        let language_id = self
            .language_ids
            .lock()
            .await
            .get(uri)
            .cloned()
            .unwrap_or_else(|| "plaintext".to_string());
        let diags = self
            .diagnostics
            .lock()
            .await
            .get(uri)
            .cloned()
            .unwrap_or_default();
        let has_errors = diags.iter().any(|d| d.severity == "error");
        let diagnostics_text = diags
            .iter()
            .map(|d| format!("line {}: [{}] {}", d.line + 1, d.severity, d.message))
            .collect::<Vec<_>>()
            .join("\n");
        let (intent_text, has_intent) = self.current_intent().await;
        // Reuse the prototype's diff/excerpt builders for the {{diff}} and
        // {{context}} template variables.
        let mut ctx =
            RemarkContext::build(uri, &language_id, &baseline, &doc_text, Vec::new(), None);
        if event == TriggerKind::Conversation {
            // The change is in the conversation: no diff, and the context is
            // the code around the last edited line (the top of the file when
            // nothing was edited yet).
            ctx.diff = String::new();
            ctx.text_excerpt =
                text_around_line(&doc_text, last_edit_line, CONVERSATION_CONTEXT_LINES);
        }
        ctx.workspace = self.workspace.lock().await.clone();
        ctx.open_buffers = self.open_buffers().await;
        // The live state: this trigger's edit summary joins the ring, then
        // the whole ring plus the diagnostics render as plain text.
        let (first_line, first_line_text) = first_changed_line(&baseline, &doc_text);
        let summary = EditSummary {
            ts: now_iso8601(),
            changed_lines,
            first_line,
            excerpt: excerpt(&first_line_text, 120),
        };
        let live_state_text = {
            let mut live = self.live_state.lock().await;
            // A conversation trigger is not an edit: it adds no summary.
            if event != TriggerKind::Conversation {
                let ring = live.entry(uri.to_string()).or_default();
                ring.push_back(summary);
                while ring.len() > LIVE_STATE_EDITS {
                    ring.pop_front();
                }
            }
            let diagnostics = self.diagnostics.lock().await;
            render_live_state(&live, &diagnostics)
        };
        // The conversation digest the fast layers judge against: the feed's
        // bounded view of what the driver and the cognition last agreed.
        let conversation_text = self
            .conversation
            .as_ref()
            .map(|f| f.digest())
            .unwrap_or_default();
        let has_conversation = !conversation_text.trim().is_empty();
        Some(TriggerContext {
            uri: uri.to_string(),
            event: "",
            version,
            changed_lines,
            since_last_remark_ms,
            has_errors,
            has_intent,
            has_conversation,
            intent_text,
            language_id,
            diff: ctx.diff,
            context_text: ctx.text_excerpt,
            diagnostics_text,
            conversation_text,
            live_state_text,
        })
    }

    /// Runs the pipeline for one trigger and delivers the branch's action.
    /// Logs every trigger decision (remark and skip alike) as a full record.
    async fn evaluate(self: &Arc<Self>, uri: String, event: TriggerKind) {
        let config = self.config.lock().await.clone();
        if !config.enabled {
            return;
        }
        // An event no trigger of the pipeline listens to: nothing to decide,
        // nothing worth logging.
        let pipeline = self.pipeline.lock().await.clone();
        let Some(questions) = pipeline.questions_for(event) else {
            return;
        };
        let mut ctx = match self.gather(&uri, event, Instant::now()).await {
            Some(ctx) => ctx,
            None => return,
        };
        ctx.event = event.as_str();

        let template_inputs = ctx.template_inputs();
        let inputs = TriggerInputs {
            changed_lines: ctx.changed_lines,
            since_last_remark_ms: ctx.since_last_remark_ms,
            has_errors: ctx.has_errors,
            has_intent: ctx.has_intent,
            has_conversation: ctx.has_conversation,
        };
        let workspace_str = self
            .workspace
            .lock()
            .await
            .as_ref()
            .map(|p| p.display().to_string());
        let mut pending = PendingTrigger::new(
            &self.run_id,
            workspace_str,
            &ctx.uri,
            ctx.version,
            ctx.event,
            ctx.prefilter_inputs(),
        );

        // No instinct wired: the pipeline cannot run.
        let Some(decision) = self.decision.as_ref().map(Arc::clone) else {
            pending.prefilter_outcome = "skip";
            pending.prefilter_reason = Some("instinct_unavailable");
            pending.finish(
                &self.log,
                DeliveryOutcome::None {
                    reason: "instinct_unavailable",
                },
            );
            return;
        };

        // One in-flight pipeline evaluation per uri; a concurrent didChange
        // cancels it (the SAME critical section stores the handle).
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

        self.set_status("thinking", None).await;
        let started = Instant::now();

        let questions_json =
            Some(serde_json::to_value(&questions).unwrap_or(serde_json::Value::Null));

        let evaluation = pipeline
            .evaluate(event, inputs, &template_inputs, decision.as_ref(), &cancel)
            .await;
        let evaluation = match evaluation {
            Ok(eval) => eval,
            Err(model_error) => {
                self.clear_in_flight(&uri).await;
                let cancelled = matches!(
                    model_error,
                    ModelError::Provider(crate::error::ProviderError::Canceled { .. })
                );
                pending.model = self.instinct_model.clone();
                if cancelled {
                    pending.finish(&self.log, DeliveryOutcome::Cancelled { reason: None });
                } else {
                    let message = model_error.to_string();
                    self.log.append(&LogRecord::Error {
                        uri: &ctx.uri,
                        message: &message,
                    });
                    pending.finish(
                        &self.log,
                        DeliveryOutcome::ProviderError {
                            reason: Some("instinct"),
                        },
                    );
                    self.set_status("error", Some(message)).await;
                }
                return;
            }
        };

        // The instinct call record is emitted only if evaluation actually
        // asked the model (a pre-filter/no-trigger skip has no call).
        let model_id = self
            .instinct_model
            .clone()
            .unwrap_or_else(|| pipeline.model_alias().to_string());
        if evaluation.answers.is_some() {
            self.log.append(&LogRecord::Call {
                layer: "instinct",
                model: &model_id,
                latency_ms: started.elapsed().as_millis(),
                input_tokens: None,
                output_tokens: None,
                cancelled: false,
                purpose: None,
            });
        }
        pending.pipeline_hash = Some(evaluation.hash.clone());
        pending.template_inputs = Some(
            serde_json::to_value(&evaluation.template_inputs).unwrap_or(serde_json::Value::Null),
        );
        pending.rendered_state = Some(evaluation.rendered_state.clone());
        pending.questions = questions_json;
        pending.model = Some(model_id);
        pending.answers = evaluation
            .answers
            .as_ref()
            .map(|a| serde_json::to_value(a).unwrap_or(serde_json::Value::Null));
        pending.branch = evaluation.branch_index;
        pending.requested_action = Some(evaluation.requested_action.as_str());
        pending.prefilter_outcome = evaluation.prefilter.as_str();
        pending.prefilter_reason = evaluation.prefilter.skip_reason();

        if let Some(reason) = evaluation.prefilter.skip_reason() {
            self.clear_in_flight(&uri).await;
            if reason == "too_few_changed_lines" {
                // Unchanged text (an idle after an undo or a rebase) teaches no
                // candidate pipeline anything: not logged. A small change is kept
                // for counting, without the ~10 KiB of template inputs and
                // rendered state (2026-09-30 log: 547 of 641 such skips had 0
                // changed lines).
                if ctx.changed_lines == 0 {
                    self.set_status("idle", None).await;
                    return;
                }
                pending.template_inputs = None;
                pending.rendered_state = None;
            }
            pending.requested_action = None;
            pending.finish(&self.log, DeliveryOutcome::None { reason });
            self.set_status("idle", None).await;
            return;
        }

        match evaluation.requested_action {
            Action::Silent => {
                self.clear_in_flight(&uri).await;
                // A pre-filter skip or no matching branch: nothing beyond
                // the instinct was requested.
                let reason = pending
                    .prefilter_reason
                    .or(if evaluation.branch_index.is_none() {
                        Some("no_branch")
                    } else {
                        None
                    });
                pending.finish(&self.log, DeliveryOutcome::Silent { reason });
                // Silence rebases: this diff was considered and judged fine.
                // A conversation trigger considered no diff.
                if event != TriggerKind::Conversation {
                    let mut state = self.state.lock().await;
                    if let Some(doc) = state.get_mut(&uri) {
                        doc.rebase();
                    }
                }
                self.set_status("idle", None).await;
            }
            Action::WakeCognition => {
                self.clear_in_flight(&uri).await;
                self.deliver_wake(uri, ctx, pending).await;
            }
            Action::Remark => {
                let prompt =
                    pipeline.render_remark_branch(event, evaluation.branch_index, &template_inputs);
                self.deliver_remark(uri, ctx, pending, prompt, cancel).await;
            }
        }
    }

    /// Delivers a `remark` branch: calls the metacognition with the rendered
    /// branch prompt and writes the completed trigger record.
    async fn deliver_remark(
        self: &Arc<Self>,
        uri: String,
        ctx: TriggerContext,
        pending: PendingTrigger,
        prompt: String,
        cancel: CancellationToken,
    ) {
        let remark_ctx = RemarkContext {
            uri: ctx.uri.clone(),
            language_id: ctx.language_id.clone(),
            diff: ctx.diff.clone(),
            text_excerpt: ctx.context_text.clone(),
            diagnostics: Vec::new(),
            intent: if ctx.intent_text.is_empty() {
                None
            } else {
                Some(ctx.intent_text.clone())
            },
            prompt: Some(prompt),
            workspace: self.workspace.lock().await.clone(),
            open_buffers: self.open_buffers().await,
        };
        let started = Instant::now();
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => None, // superseded by a newer edit
            r = self.metacognition.remark(&remark_ctx, &cancel) => Some(r),
        };
        self.clear_in_flight(&uri).await;

        let Some(result) = result else {
            // Cancelled: a newer edit took over. Stay quiet.
            pending.finish(&self.log, DeliveryOutcome::Cancelled { reason: None });
            return;
        };

        let model = self
            .metacognition_model
            .clone()
            .unwrap_or_else(|| "metacognition".to_string());
        match result {
            Ok(Some(remark)) => {
                // A remark about a stale document version is dropped; a
                // current one stamps the cooldown clock and rebases.
                {
                    let mut state = self.state.lock().await;
                    match state.get_mut(&uri) {
                        Some(doc) if doc.version == ctx.version => {
                            doc.last_remark = Some(Instant::now());
                            // A conversation remark considered no diff.
                            if ctx.event != TriggerKind::Conversation.as_str() {
                                doc.rebase();
                            }
                        }
                        _ => {
                            tracing::info!(uri = %ctx.uri, "dropping remark for stale version");
                            pending.finish(
                                &self.log,
                                DeliveryOutcome::Cancelled {
                                    reason: Some("stale_version"),
                                },
                            );
                            return;
                        }
                    }
                }
                let id = crate::navigator::new_ulid_string();
                let latency = started.elapsed().as_millis();
                self.log.append(&LogRecord::Call {
                    layer: "metacognition",
                    model: &model,
                    latency_ms: latency,
                    input_tokens: None,
                    output_tokens: None,
                    cancelled: false,
                    purpose: None,
                });
                self.log.append(&LogRecord::Remark {
                    id: &id,
                    uri: &ctx.uri,
                    line: remark.line,
                    text: &remark.text,
                    model: &model,
                    latency_ms: latency,
                });
                self.client
                    .send_notification::<RemarkNotification>(RemarkParams {
                        id: id.clone(),
                        uri: uri.clone(),
                        line: remark.line,
                        text: remark.text,
                    })
                    .await;
                pending.finish(&self.log, DeliveryOutcome::Remark { remark_id: id });
                self.set_status("idle", None).await;
            }
            Ok(None) => {
                self.log.append(&LogRecord::Call {
                    layer: "metacognition",
                    model: &model,
                    latency_ms: started.elapsed().as_millis(),
                    input_tokens: None,
                    output_tokens: None,
                    cancelled: false,
                    purpose: None,
                });
                self.log.append(&LogRecord::Silent {
                    uri: &ctx.uri,
                    model: &model,
                    latency_ms: started.elapsed().as_millis(),
                });
                // Silence rebases: this diff was considered and judged fine.
                // A conversation trigger considered no diff.
                if ctx.event != TriggerKind::Conversation.as_str() {
                    let mut state = self.state.lock().await;
                    if let Some(doc) = state.get_mut(&uri) {
                        doc.rebase();
                    }
                }
                pending.finish(&self.log, DeliveryOutcome::Silent { reason: None });
                self.set_status("idle", None).await;
            }
            Err(error) => {
                self.log.append(&LogRecord::Call {
                    layer: "metacognition",
                    model: &model,
                    latency_ms: started.elapsed().as_millis(),
                    input_tokens: None,
                    output_tokens: None,
                    cancelled: false,
                    purpose: None,
                });
                // A malformed answer is a model failure, not a provider
                // failure: different delivered outcome in the log.
                if let ModelError::Malformed(raw) = &error {
                    self.log.append(&LogRecord::Malformed {
                        uri: &ctx.uri,
                        model: &model,
                        latency_ms: started.elapsed().as_millis(),
                        raw_excerpt: &excerpt(raw, 400),
                    });
                    pending.finish(&self.log, DeliveryOutcome::Malformed);
                    self.set_status(
                        "error",
                        Some("model answer violated the prompt contract".to_string()),
                    )
                    .await;
                } else {
                    let message = error.to_string();
                    self.log.append(&LogRecord::Error {
                        uri: &ctx.uri,
                        message: &message,
                    });
                    pending.finish(&self.log, DeliveryOutcome::ProviderError { reason: None });
                    self.set_status("error", Some(message)).await;
                }
            }
        }
    }

    /// Delivers a `wake_cognition` branch: builds the wake message from
    /// what the instinct judged and posts it to the active conversation
    /// through the feed. The guard is the feed's: while the cognition
    /// waits on the driver's `ask` answer, the wake is queued (latest
    /// wins) and delivered right after the driver's next message.
    async fn deliver_wake(
        self: &Arc<Self>,
        uri: String,
        ctx: TriggerContext,
        pending: PendingTrigger,
    ) {
        const UNAVAILABLE: DeliveryOutcome = DeliveryOutcome::None {
            reason: "cognition_unavailable",
        };
        let Some(feed) = self.conversation.clone() else {
            pending.finish(&self.log, UNAVAILABLE);
            self.set_status("idle", None).await;
            return;
        };
        if feed.active_session().is_none() {
            pending.finish(&self.log, UNAVAILABLE);
            self.set_status("idle", None).await;
            return;
        }
        let message = wake_message(&ctx, &feed.digest());
        let outcome = feed.post_wake(&message).await;
        if matches!(outcome, WakeOutcome::Posted(_) | WakeOutcome::Queued(_)) {
            // A wake consumes the edit like a remark does: stamp the cooldown
            // and rebase, or the idle trigger right after the save wakes the
            // cognition a second time about the same change (observed
            // 2026-10-01: two identical wakes 1.5 s apart).
            let mut state = self.state.lock().await;
            if let Some(doc) = state.get_mut(&uri) {
                if doc.version == ctx.version {
                    doc.last_remark = Some(Instant::now());
                    doc.rebase();
                }
            }
        }
        match outcome {
            WakeOutcome::Posted(session_id) => {
                pending.finish(
                    &self.log,
                    DeliveryOutcome::Wake {
                        session: session_id.to_string(),
                    },
                );
            }
            WakeOutcome::Queued(session_id) => {
                pending.finish(
                    &self.log,
                    DeliveryOutcome::WakeQueued {
                        session: session_id.to_string(),
                    },
                );
            }
            // The session exists but refused the post (stopping, full
            // inbox): an honest outcome, logged for tuning like any skip.
            WakeOutcome::Failed(_, error) => {
                tracing::warn!(uri = %uri, error = %error, "wake post failed");
                pending.finish(&self.log, UNAVAILABLE);
            }
            WakeOutcome::NoSession => pending.finish(&self.log, UNAVAILABLE),
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

    /// `navigator/diagnostics`: keep the latest set per uri and run the
    /// diagnostics trigger.
    async fn on_diagnostics(self: &Arc<Self>, params: DiagnosticsParams) {
        let items: Vec<Diag> = params
            .items
            .into_iter()
            .map(|d: DiagnosticItem| Diag {
                line: d.line,
                severity: d.severity,
                message: d.message,
            })
            .collect();
        self.diagnostics
            .lock()
            .await
            .insert(params.uri.clone(), items);
        self.evaluate(params.uri, TriggerKind::Diagnostics).await;
    }

    /// `navigator/intent`: the user's current goal, global to the session.
    async fn on_intent(&self, params: IntentParams) {
        let text = params.text.trim().to_string();
        *self.intent.lock().await = if text.is_empty() { None } else { Some(text) };
    }

    /// True when a didSave of this uri should hot-reload the pipeline: any
    /// file under the workspace `.navigator/` or the user navigator dir.
    fn is_navigator_config(
        uri: &str,
        workspace: Option<&std::path::Path>,
        user_dir: Option<&std::path::Path>,
    ) -> bool {
        let Some(path) = crate::navigator::uri_to_path_str(uri) else {
            return false;
        };
        let path = std::path::Path::new(&path);
        if let Some(ws) = workspace {
            if path.starts_with(ws.join(".navigator")) {
                return true;
            }
        }
        if let Some(user) = user_dir {
            if path.starts_with(user) {
                return true;
            }
        }
        false
    }

    async fn on_did_open(self: &Arc<Self>, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        let uri = crate::navigator::uri_key(&doc.uri);
        self.language_ids
            .lock()
            .await
            .insert(uri.clone(), doc.language_id.clone());
        let version;
        {
            let mut state = self.state.lock().await;
            let entry = state
                .entry(uri.clone())
                .or_insert_with(|| DocumentState::new(String::new(), 0));
            entry.apply_change(doc.text, doc.version);
            entry.rebase();
            version = entry.version;
        }
        *self.focused.lock().await = Some(uri.clone());
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
        {
            let mut state = self.state.lock().await;
            match state.get_mut(&uri) {
                Some(doc) => {
                    Self::cancel_in_flight(doc);
                    let (line, _) = first_changed_line(&doc.text, &text);
                    if line > 0 {
                        doc.last_edit_line = line;
                    }
                    doc.apply_change(text, version);
                }
                None => {
                    let mut doc = DocumentState::new(text, version);
                    doc.rebase();
                    state.insert(uri.clone(), doc);
                }
            }
        }
        *self.focused.lock().await = Some(uri.clone());
        self.spawn_idle_timer(uri, version);
    }

    async fn on_did_save(self: &Arc<Self>, params: DidSaveTextDocumentParams) {
        let uri_key = crate::navigator::uri_key(&params.text_document.uri);
        *self.focused.lock().await = Some(uri_key.clone());
        {
            let workspace = self.workspace.lock().await.clone();
            let user_dir = self.user_dir.clone();
            if Self::is_navigator_config(&uri_key, workspace.as_deref(), user_dir.as_deref()) {
                self.reload_pipeline().await;
            }
        }
        self.refresh_reflex_context(&uri_key, None).await;
        self.evaluate(uri_key, TriggerKind::Save).await;
    }

    async fn on_did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = crate::navigator::uri_key(&params.text_document.uri);
        if let Some(mut doc) = self.state.lock().await.remove(&uri) {
            Self::cancel_in_flight(&mut doc);
        }
        {
            let mut focused = self.focused.lock().await;
            if focused.as_deref() == Some(uri.as_str()) {
                *focused = None;
            }
        }
        self.language_ids.lock().await.remove(&uri);
        self.diagnostics.lock().await.remove(&uri);
        self.live_state.lock().await.remove(&uri);
        self.client
            .send_notification::<ClearNotification>(ClearParams { uri })
            .await;
    }

    /// One reflex layer model call, for the cost log. Usage tokens are not
    /// surfaced by the service seam yet; latency is.
    fn log_reflex_call(&self, latency_ms: u128) {
        if let Some(model) = self.reflex_model.as_deref() {
            self.log.append(&LogRecord::Call {
                layer: "reflex",
                model,
                latency_ms,
                input_tokens: None,
                output_tokens: None,
                cancelled: false,
                purpose: None,
            });
        }
    }
}

impl Inner {
    /// The text served as the reflex context: the metacognition's refreshed
    /// text once one exists, else the intent summary.
    async fn reflex_context_text(&self) -> String {
        if let Some(text) = self.reflex_context.lock().await.text.clone() {
            return text;
        }
        self.current_intent().await.0
    }

    /// Starts a background refresh of the reflex context when it is due:
    /// not already running, the latest attempt older than a minute, and the
    /// inputs (intent summary, conversation digest, active file) changed
    /// since the current text was computed. Never waits on the model: the
    /// current text keeps being served meanwhile. A failed or empty refresh
    /// keeps the previous text, is logged, and is retried after the interval.
    async fn refresh_reflex_context(self: &Arc<Self>, uri: &str, cursor_line: Option<u32>) {
        let Some(model) = self.reflex_context_model.clone() else {
            return;
        };
        let (intent, _) = self.current_intent().await;
        let conversation = self
            .conversation
            .as_ref()
            .map(|feed| feed.digest())
            .unwrap_or_default();
        let inputs = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (&intent, &conversation, uri).hash(&mut hasher);
            hasher.finish()
        };
        let now = Instant::now();
        let line = {
            let mut cache = self.reflex_context.lock().await;
            if let Some(line) = cursor_line {
                cache.cursor = Some((uri.to_string(), line));
            }
            if cache.refreshing
                || cache.inputs == Some(inputs)
                || cache
                    .last_attempt
                    .is_some_and(|t| now.duration_since(t) < REFLEX_CONTEXT_MIN_INTERVAL)
            {
                return;
            }
            cache.refreshing = true;
            cache.last_attempt = Some(now);
            cache
                .cursor
                .as_ref()
                .filter(|(u, _)| u == uri)
                .map_or(0, |(_, l)| *l)
        };
        let text = self.state.lock().await.get(uri).map(|d| d.text.clone());
        let Some(text) = text else {
            self.reflex_context.lock().await.refreshing = false;
            return;
        };
        let language_id = self
            .language_ids
            .lock()
            .await
            .get(uri)
            .cloned()
            .unwrap_or_else(|| "plaintext".to_string());
        let request = ReflexContextRequest {
            intent,
            conversation,
            file: uri.to_string(),
            language_id,
            excerpt: numbered_excerpt(&text, line as usize, REFLEX_EXCERPT_RADIUS),
        };
        let inner = Arc::clone(self);
        let uri = uri.to_string();
        tokio::spawn(async move {
            let started = Instant::now();
            let result = model.refresh(&request).await;
            inner.log.append(&LogRecord::Call {
                layer: "metacognition",
                model: inner.metacognition_model.as_deref().unwrap_or("unknown"),
                latency_ms: started.elapsed().as_millis(),
                input_tokens: None,
                output_tokens: None,
                cancelled: false,
                purpose: Some("reflex_context"),
            });
            let mut cache = inner.reflex_context.lock().await;
            cache.refreshing = false;
            match result {
                Ok(text) if !text.trim().is_empty() => {
                    cache.text = Some(crate::navigator::log::excerpt(
                        text.trim(),
                        REFLEX_CONTEXT_MAX_CHARS,
                    ));
                    cache.inputs = Some(inputs);
                }
                Ok(_) => inner.log.append(&LogRecord::Error {
                    uri: &uri,
                    message: "reflex context refresh: empty answer",
                }),
                Err(e) => inner.log.append(&LogRecord::Error {
                    uri: &uri,
                    message: &format!("reflex context refresh: {e}"),
                }),
            }
        });
    }
}

/// The lines within `radius` of `line` (0-based), each prefixed by its
/// 1-based number.
fn numbered_excerpt(text: &str, line: usize, radius: usize) -> String {
    let start = line.saturating_sub(radius);
    text.lines()
        .enumerate()
        .skip(start)
        .take(radius * 2 + 1)
        .map(|(i, l)| format!("{:>4} | {l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

impl LanguageServer for NavigatorBackend {
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> tower_lsp_server::jsonrpc::Result<InitializeResult> {
        if let Some(options) = &params.initialization_options {
            self.inner.apply_config(options.clone()).await;
        }
        // rootUri wins, then the first workspace folder; the process cwd the
        // binary passed (if any) is the last resort.
        {
            #[allow(deprecated)]
            let from_root = params
                .root_uri
                .as_ref()
                .and_then(crate::navigator::uri_to_path);
            let from_folders = params.workspace_folders.as_ref().and_then(|folders| {
                folders
                    .first()
                    .and_then(|f| crate::navigator::uri_to_path(&f.uri))
            });
            let mut workspace = self.inner.workspace.lock().await;
            if from_root.is_some() {
                *workspace = from_root;
            } else if from_folders.is_some() {
                *workspace = from_folders;
            }
            let ws = workspace.clone();
            drop(workspace);
            // The workspace may unlock a project pipeline: reload quietly
            // (a failure keeps the built-in running; the status
            // notification may not be deliverable this early anyway).
            if ws.is_some() {
                self.inner.reload_pipeline().await;
            }
        }
        if !self.inner.disabled_layers.is_empty() {
            self.inner
                .set_status(
                    "error",
                    Some(format!(
                        "layer{} disabled: {}",
                        if self.inner.disabled_layers.len() == 1 {
                            ""
                        } else {
                            "s"
                        },
                        self.inner.disabled_layers.join("; ")
                    )),
                )
                .await;
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

    async fn shutdown(&self) -> tower_lsp_server::jsonrpc::Result<()> {
        Ok(())
    }

    /// `initialized`: announce the cognition chat server's endpoint when the
    /// binary started one, so the extension can bridge the chat panel.
    async fn initialized(&self, _params: InitializedParams) {
        if let Some(info) = self.inner.server_info.clone() {
            self.inner
                .client
                .send_notification::<ServerInfoNotification>(info)
                .await;
        }
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
    /// inline-completion provider. The reflex layer answers or returns an
    /// empty item list.
    pub async fn inline_completion(
        &self,
        params: crate::navigator::protocol::InlineCompletionParams,
    ) -> tower_lsp_server::jsonrpc::Result<crate::navigator::protocol::InlineCompletionResult> {
        let Some(reflex) = &self.inner.reflex else {
            return Ok(InlineCompletionResult {
                items: Vec::new(),
                request_id: None,
            });
        };
        let text = {
            let state = self.inner.state.lock().await;
            match state.get(&params.uri) {
                Some(doc) => doc.text.clone(),
                None => {
                    return Ok(InlineCompletionResult {
                        items: Vec::new(),
                        request_id: None,
                    })
                }
            }
        };
        let language_id = self
            .inner
            .language_ids
            .lock()
            .await
            .get(&params.uri)
            .cloned()
            .unwrap_or_else(|| "plaintext".to_string());
        let pos = Position {
            line: params.line,
            character: params.character,
        };
        // The reflex context: the metacognition's refreshed text, else the
        // intent summary. The refresh runs in the background; the service
        // caps the text.
        let context = self.inner.reflex_context_text().await;
        self.inner
            .refresh_reflex_context(&params.uri, Some(params.line))
            .await;
        let started = Instant::now();
        match reflex
            .suggest(
                &params.uri,
                &text,
                params.version,
                pos,
                &context,
                &language_id,
            )
            .await
        {
            Ok(suggestion) => {
                self.inner.log_reflex_call(started.elapsed().as_millis());
                Ok(to_result(&suggestion))
            }
            Err(outcome) => {
                // A debounced or out-of-range request never reached the
                // model: logging it would bill a call that did not happen.
                if outcome.called_model() {
                    self.inner.log_reflex_call(started.elapsed().as_millis());
                }
                Ok(InlineCompletionResult {
                    items: Vec::new(),
                    request_id: None,
                })
            }
        }
    }

    pub async fn reflex_shown(&self, params: ReflexShownParams) {
        self.inner.on_reflex_shown(params).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigator_ids_are_unique() {
        assert_ne!(
            crate::navigator::new_ulid_string(),
            crate::navigator::new_ulid_string()
        );
    }

    #[test]
    fn first_changed_line_finds_the_first_insert() {
        let (line, text) = first_changed_line("a\nb\n", "a\nb\nc\nd\n");
        assert_eq!(line, 3);
        assert_eq!(text, "c");
        // A pure deletion quotes nothing.
        assert_eq!(first_changed_line("a\nb\n", "a\n"), (0, String::new()));
        assert_eq!(first_changed_line("a\n", "a\n"), (0, String::new()));
    }

    #[test]
    fn live_state_renders_edits_newest_first_and_diagnostics() {
        let mut live: HashMap<String, VecDeque<EditSummary>> = HashMap::new();
        let ring = live.entry("file:///ws/a.rs".to_string()).or_default();
        for (ts, line) in [
            ("2026-10-01T10:00:00.000Z", 1),
            ("2026-10-01T10:01:00.000Z", 2),
        ] {
            ring.push_back(EditSummary {
                ts: ts.to_string(),
                changed_lines: 4,
                first_line: line,
                excerpt: "let x = 1;".to_string(),
            });
        }
        let mut diagnostics: HashMap<String, Vec<Diag>> = HashMap::new();
        diagnostics.insert(
            "file:///ws/a.rs".to_string(),
            vec![
                Diag {
                    line: 4,
                    severity: "error".to_string(),
                    message: "cannot find value `y`".to_string(),
                },
                Diag {
                    line: 9,
                    severity: "info".to_string(),
                    message: "unrelated hint".to_string(),
                },
            ],
        );
        let text = render_live_state(&live, &diagnostics);
        // Newest edit first.
        let first = text.lines().next().unwrap();
        assert!(first.starts_with("2026-10-01T10:01:00.000Z"), "{text}");
        // Only errors and warnings make it in.
        assert!(text.contains("[error] cannot find value `y`"), "{text}");
        assert!(!text.contains("unrelated hint"), "{text}");
    }

    #[test]
    fn navigator_config_uris_match_project_and_user_dirs() {
        let ws = std::path::Path::new("/ws");
        let user = std::path::Path::new("/home/u/.config/rust-agent/navigator");
        assert!(Inner::is_navigator_config(
            "file:///ws/.navigator/pipeline.toml",
            Some(ws),
            Some(user)
        ));
        assert!(Inner::is_navigator_config(
            "file:///ws/.navigator/prompts/state.md",
            Some(ws),
            Some(user)
        ));
        assert!(Inner::is_navigator_config(
            "file:///home/u/.config/rust-agent/navigator/pipeline.toml",
            Some(ws),
            Some(user)
        ));
        assert!(!Inner::is_navigator_config(
            "file:///ws/src/main.rs",
            Some(ws),
            Some(user)
        ));
        assert!(!Inner::is_navigator_config(
            "untitled:Untitled-1",
            Some(ws),
            Some(user)
        ));
    }
}
