//! Document-scoped flow effects reuse the navigator's anchored delivery paths.
use super::{trigger::TriggerContext, wake::WakeReason, Runtime};
use crate::{
    flows::runtime::{ContextSnapshot, FlowEffects},
    navigator::{log::PendingTrigger, TriggerKind},
};
use instinct_flow::{ApplyError, BoxFuture, Event, ProposedAction};
use serde_json::json;
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct EditorEffects {
    runtime: Weak<Runtime>,
    context: Mutex<Option<(TriggerContext, PendingTrigger)>>,
}
impl FlowEffects for EditorEffects {
    fn effect<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        action: &'a ProposedAction,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), ApplyError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(ApplyError("editor event cancelled".into()));
            }
            let runtime = self
                .runtime
                .upgrade()
                .ok_or_else(|| ApplyError("editor closed".into()))?;
            if !matches!(action.action.as_str(), "remark" | "wake") {
                return Err(ApplyError("unsupported editor effect".into()));
            }
            let (ctx, mut pending) = self
                .context
                .lock()
                .await
                .clone()
                .ok_or_else(|| ApplyError("editor event already delivered".into()))?;
            let uri = ctx.uri.clone();
            self.context.lock().await.take();
            match action.action.as_str() {
                "remark" => {
                    let prompt = action
                        .params
                        .get("prompt")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ApplyError("missing remark prompt".into()))?;
                    pending.requested_action = Some("remark");
                    runtime
                        .deliver_remark(uri, ctx, pending, prompt.into(), cancel.clone())
                        .await;
                }
                "wake" => {
                    pending.requested_action = Some("wake_cognition");
                    runtime.clear_in_flight(&uri).await;
                    runtime
                        .deliver_wake(uri, ctx, pending, WakeReason::Other(0.0))
                        .await;
                }
                _ => unreachable!(),
            }
            Ok(())
        })
    }
}

impl Runtime {
    /// Returns true once an eligible flow owns the trigger, including skips/failures.
    pub(super) async fn evaluate_flows(self: &Arc<Self>, uri: &str, event: TriggerKind) -> bool {
        let Some(factory) = &self.flows else {
            return false;
        };
        let effects = Arc::new(EditorEffects {
            runtime: Arc::downgrade(self),
            context: Mutex::new(None),
        });
        let flows = factory(effects.clone());
        let trusted = flows.store.trusted();
        if !flows.store.snapshot().values().any(|f| {
            let header = &f.compiled.definition().flow;
            header.enabled
                && header.on == event.as_str()
                && (f.location != crate::flows::store::Location::Workspace || trusted)
        }) {
            return false;
        }
        let Some(mut ctx) = self.gather(uri, event, tokio::time::Instant::now()).await else {
            return true;
        };
        ctx.event = event.as_str();
        if ctx.help_in_flight || ctx.remark_in_flight {
            return true;
        }
        let cancel = CancellationToken::new();
        {
            let mut docs = self.documents.lock().await;
            let Some(doc) = docs.get_mut(uri) else {
                return true;
            };
            if doc.in_flight_version.is_some() {
                return true;
            }
            doc.in_flight_version = Some(ctx.version);
            doc.in_flight_cancel = Some(cancel.clone());
        }
        let workspace = self.workspace.lock().await.clone();
        let mut pending = PendingTrigger::new(
            &self.run_id,
            workspace.as_ref().map(|p| p.display().to_string()),
            uri,
            ctx.version,
            ctx.event,
            ctx.prefilter_inputs(),
        );
        pending.prefilter_outcome = "pass";
        let mut payload = serde_json::Map::new();
        match event {
            TriggerKind::Conversation => {
                payload.insert("turn_id".into(), json!(crate::navigator::new_ulid_string()));
                payload.insert("text".into(), json!(ctx.conversation_text));
                payload.insert("uri".into(), json!(uri));
            }
            _ => {
                payload.extend([
                    ("uri".into(), json!(uri)),
                    ("language_id".into(), json!(ctx.language_id)),
                    ("version".into(), json!(ctx.version)),
                    ("diff".into(), json!(ctx.diff)),
                ]);
                if event == TriggerKind::Idle {
                    payload.insert("idle_ms".into(), json!(self.config.lock().await.idle_ms));
                }
                if event == TriggerKind::Diagnostics {
                    payload.insert("diagnostics".into(), json!([]));
                }
            }
        }
        payload.extend(ctx.prefilter_inputs());
        payload.extend(
            ctx.template_inputs()
                .into_iter()
                .map(|(k, v)| (format!("template_{k}"), json!(v))),
        );
        *effects.context.lock().await = Some((ctx, pending));
        self.set_status("thinking", None).await;
        let (_, handles) = flows
            .evaluate(
                Event {
                    trigger: event.as_str().into(),
                    payload: payload.into(),
                },
                ContextSnapshot {
                    workspace,
                    ..Default::default()
                },
                cancel.clone(),
            )
            .await;
        let mut failed = false;
        let mut skipped = true;
        for handle in handles {
            match handle.await {
                Ok(outcome) => {
                    failed |= matches!(outcome.result, instinct_flow::RunResult::Failure { .. });
                    skipped &= matches!(outcome.result, instinct_flow::RunResult::Skipped);
                }
                Err(error) => {
                    failed = true;
                    tracing::warn!(%error, "editor flow task failed");
                }
            }
        }
        if let Some((_, mut pending)) = effects.context.lock().await.take() {
            pending.requested_action = if failed || skipped {
                None
            } else {
                Some("silent")
            };
            let outcome = if cancel.is_cancelled() {
                crate::navigator::log::DeliveryOutcome::Cancelled { reason: None }
            } else if failed {
                crate::navigator::log::DeliveryOutcome::ProviderError {
                    reason: Some("flow"),
                }
            } else if skipped {
                crate::navigator::log::DeliveryOutcome::None {
                    reason: "flow_skipped",
                }
            } else {
                crate::navigator::log::DeliveryOutcome::Silent { reason: None }
            };
            pending.finish(&self.log, outcome);
            if !failed && !skipped && !cancel.is_cancelled() && event != TriggerKind::Conversation {
                if let Some(doc) = self.documents.lock().await.get_mut(uri) {
                    doc.rebase();
                }
            }
        }
        self.clear_in_flight(uri).await;
        self.set_status("idle", None).await;
        true
    }
}
