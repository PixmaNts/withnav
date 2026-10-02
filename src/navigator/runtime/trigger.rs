//! One trigger, end to end up to its decision: gathers the context from the
//! document state, runs the pipeline (pre-filters, the instinct call, the
//! branch) and logs the decision; the branch's action is handed to
//! [`delivery`](super::delivery).

use std::sync::Arc;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::navigator::document::{first_changed_line, text_around_line};
use crate::navigator::log::{DeliveryOutcome, LogRecord, PendingTrigger};
use crate::navigator::metacognition::RemarkContext;
use crate::navigator::pipeline::{Action, TriggerInputs, TriggerKind};
use crate::navigator::ports::ModelError;

use super::wake::WakeReason;
use super::Runtime;

/// Lines on each side of the last edited line shown to the conversation
/// trigger.
const CONVERSATION_CONTEXT_LINES: usize = 20;

/// One trigger's gathered context: everything the pipeline inputs, the
/// templates and the log record need.
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
    pub(super) has_errors: bool,
    pub(super) has_intent: bool,
    pub(super) has_conversation: bool,
    pub(super) intent_text: String,
    pub(super) language_id: String,
    pub(super) diff: String,
    /// The document text this trigger judged: a late remark's line is
    /// re-anchored from it onto the current text.
    pub(super) text: String,
    /// 1-based line of the first inserted line since the baseline (0: none).
    pub(super) first_changed_line: usize,
    pub(super) context_text: String,
    pub(super) diagnostics_text: String,
    pub(super) conversation_text: String,
    pub(super) live_state_text: String,
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
    async fn gather(&self, uri: &str, event: TriggerKind, now: Instant) -> Option<TriggerContext> {
        let snapshot = self.documents.lock().await.snapshot(uri, event, now)?;
        let has_errors = snapshot.diagnostics.iter().any(|d| d.severity == "error");
        let diagnostics_text = snapshot
            .diagnostics
            .iter()
            .map(|d| format!("line {}: [{}] {}", d.line + 1, d.severity, d.message))
            .collect::<Vec<_>>()
            .join("\n");
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
        // bounded view of what the driver and the cognition last agreed.
        let conversation_text = self
            .conversation
            .as_ref()
            .map(|f| f.digest())
            .unwrap_or_default();
        let has_conversation = !conversation_text.trim().is_empty();
        // A conversation turn changed nothing in the file; a call already
        // answered (or being answered) is not a new one.
        let called_for_help = !snapshot.help_calls.is_empty();
        let (first_changed_line, _) = first_changed_line(&snapshot.baseline, &snapshot.text);
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
            has_errors,
            has_intent,
            has_conversation,
            intent_text,
            language_id: snapshot.language_id,
            diff: ctx.diff,
            text: snapshot.text,
            first_changed_line,
            context_text: ctx.text_excerpt,
            diagnostics_text,
            conversation_text,
            live_state_text: snapshot.live_state,
        })
    }

    /// Runs the pipeline for one trigger and delivers the branch's action.
    /// Logs every trigger decision (remark and skip alike) as a full record.
    pub(super) async fn evaluate(self: &Arc<Self>, uri: String, event: TriggerKind) {
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
            since_last_wake_ms: ctx.since_last_wake_ms,
            called_for_help: ctx.called_for_help,
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

        // The comment that calls for help stays in the file: while a remark
        // is being written for it, the same call is not a new trigger (it
        // would cancel the answer it is waiting for).
        if ctx.help_in_flight {
            pending.prefilter_outcome = "skip";
            pending.prefilter_reason = Some("help_in_flight");
            pending.finish(
                &self.log,
                DeliveryOutcome::None {
                    reason: "help_in_flight",
                },
            );
            return;
        }

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
            let mut state = self.documents.lock().await;
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
                    let mut state = self.documents.lock().await;
                    if let Some(doc) = state.get_mut(&uri) {
                        doc.rebase();
                    }
                }
                self.set_status("idle", None).await;
            }
            Action::WakeCognition => {
                self.clear_in_flight(&uri).await;
                let reason = WakeReason::from_answers(
                    evaluation.answers.as_ref(),
                    ctx.has_intent,
                    ctx.has_conversation,
                );
                self.deliver_wake(uri, ctx, pending, reason).await;
            }
            Action::Remark => {
                let prompt =
                    pipeline.render_remark_branch(event, evaluation.branch_index, &template_inputs);
                self.deliver_remark(uri, ctx, pending, prompt, cancel).await;
            }
        }
    }
}
