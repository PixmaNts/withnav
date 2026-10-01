//! Delivery of a trigger's outcome: a `remark` branch calls the
//! metacognition and notifies the editor; a `wake_cognition` branch posts to
//! the active conversation through the feed. Each writes the completed
//! trigger record.

use std::sync::Arc;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::navigator::log::{excerpt, DeliveryOutcome, LogRecord, PendingTrigger};
use crate::navigator::lsp::protocol::{RemarkNotification, RemarkParams};
use crate::navigator::metacognition::RemarkContext;
use crate::navigator::pipeline::TriggerKind;
use crate::navigator::ports::ModelError;
use crate::navigator::WakeOutcome;

use super::trigger::TriggerContext;
use super::Runtime;

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

impl Runtime {
    /// Delivers a `remark` branch: calls the metacognition with the rendered
    /// branch prompt and writes the completed trigger record.
    pub(super) async fn deliver_remark(
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
            open_buffers: self.documents.lock().await.open_buffers(),
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
                    let mut state = self.documents.lock().await;
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
                    let mut state = self.documents.lock().await;
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
    pub(super) async fn deliver_wake(
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
            let mut state = self.documents.lock().await;
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
}
