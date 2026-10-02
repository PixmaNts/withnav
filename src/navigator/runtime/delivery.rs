//! Delivery of a trigger's outcome: a `remark` branch calls the
//! metacognition and notifies the editor; a `wake_cognition` branch posts to
//! the active conversation through the feed. Each writes the completed
//! trigger record.

use std::sync::Arc;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::navigator::document::reanchor_line;
use crate::navigator::log::{excerpt, DeliveryOutcome, LogRecord, PendingTrigger};
use crate::navigator::lsp::protocol::{RemarkNotification, RemarkParams};
use crate::navigator::metacognition::{RemarkContext, HELP_TOOL_ROUNDS, REMARK_TOOL_ROUNDS};
use crate::navigator::pipeline::TriggerKind;
use crate::navigator::ports::ModelError;
use crate::navigator::WakeOutcome;

use super::trigger::TriggerContext;
use super::wake::{relative_path, wake_message, WakeReason};
use super::Runtime;

impl Runtime {
    /// Delivers a `remark` branch: calls the metacognition with the rendered
    /// branch prompt and writes the completed trigger record.
    pub(super) async fn deliver_remark(
        self: &Arc<Self>,
        uri: String,
        ctx: TriggerContext,
        pending: PendingTrigger,
        prompt: String,
        instinct_cancel: CancellationToken,
    ) {
        // The instinct stage is over. Typing during it cancelled it (its
        // answer was about text that is gone); from here typing no longer
        // cancels: the remark request runs on its own handle, which only a
        // newer remark for the document or a close cancels.
        let Some(cancel) = self
            .begin_remark(
                &uri,
                &instinct_cancel,
                &ctx.help_calls,
                ctx.event == TriggerKind::Conversation.as_str(),
            )
            .await
        else {
            pending.finish(&self.log, DeliveryOutcome::Cancelled { reason: None });
            return;
        };
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
            open_buffers: self.documents.lock().await.open_buffers(),
            tool_rounds: if ctx.called_for_help {
                HELP_TOOL_ROUNDS
            } else {
                REMARK_TOOL_ROUNDS
            },
        };
        let started = Instant::now();
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => None, // superseded, or the document closed
            r = self.metacognition.remark(&remark_ctx, &cancel) => Some(r),
        };
        self.end_remark(&uri, &cancel).await;

        let Some(result) = result else {
            // Cancelled: a newer remark took over, or the document closed.
            pending.finish(&self.log, DeliveryOutcome::Cancelled { reason: None });
            return;
        };

        let model = self
            .metacognition_model
            .clone()
            .unwrap_or_else(|| "metacognition".to_string());
        match result {
            Ok(Some(remark)) => {
                // Typing while the remark was being written does not kill
                // it: it is delivered when its line survived the edits, at
                // the line's place in the current text. A line that was
                // changed or removed has nothing left to talk about.
                let line = {
                    let mut state = self.documents.lock().await;
                    let anchored = state.get_mut(&uri).and_then(|doc| {
                        let line = if doc.version == ctx.version {
                            Some(remark.line)
                        } else {
                            reanchor_line(&ctx.text, &doc.text, remark.line)
                        }?;
                        doc.last_remark = Some(Instant::now());
                        doc.help.answer(&ctx.help_calls);
                        // A conversation remark considered no diff.
                        if ctx.event != TriggerKind::Conversation.as_str() {
                            doc.rebase_to(&ctx.text);
                        }
                        Some(line)
                    });
                    let Some(line) = anchored else {
                        tracing::info!(uri = %ctx.uri, "dropping remark whose line is gone");
                        pending.finish(
                            &self.log,
                            DeliveryOutcome::Cancelled {
                                reason: Some("stale_version"),
                            },
                        );
                        return;
                    };
                    line
                };
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
                    line,
                    text: &remark.text,
                    model: &model,
                    latency_ms: latency,
                });
                self.client
                    .send_notification::<RemarkNotification>(RemarkParams {
                        id: id.clone(),
                        uri: uri.clone(),
                        line,
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
                // Silence rebases onto the text that was judged: this diff was
                // considered and found fine; later edits stay in the next one.
                // A conversation trigger considered no diff.
                if ctx.event != TriggerKind::Conversation.as_str() {
                    let mut state = self.documents.lock().await;
                    if let Some(doc) = state.get_mut(&uri) {
                        doc.rebase_to(&ctx.text);
                    }
                }
                // The model judged the call and had nothing to say: asking
                // again about the same comment would get the same silence.
                if let Some(doc) = self.documents.lock().await.get_mut(&uri) {
                    doc.help.answer(&ctx.help_calls);
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
    pub(super) async fn deliver_wake(
        self: &Arc<Self>,
        uri: String,
        ctx: TriggerContext,
        pending: PendingTrigger,
        reason: WakeReason,
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
        let workspace = self.workspace.lock().await.clone();
        let message = wake_message(
            &reason,
            &relative_path(&ctx.uri, workspace.as_deref()),
            ctx.first_changed_line,
            &ctx.diff,
            &ctx.intent_text,
        );
        let outcome = feed.post_wake(&message).await;
        if matches!(outcome, WakeOutcome::Posted(_) | WakeOutcome::Queued(_)) {
            // A wake consumes the edit like a remark does: stamp the wake
            // clock and rebase, or the idle trigger right after the save
            // wakes the cognition a second time about the same change
            // (observed 2026-10-01: two identical wakes 1.5 s apart). The
            // remark clock is left alone: wakes and remarks are rate-limited
            // apart, and a wake must not silence the next remark.
            let mut state = self.documents.lock().await;
            if let Some(doc) = state.get_mut(&uri) {
                doc.last_wake = Some(Instant::now());
                doc.rebase_to(&ctx.text);
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
}
