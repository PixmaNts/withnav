use super::watchdog::{Checked, Evidence, Verdict};
use crate::bus::event::now_rfc3339;
use crate::error::ServerError;
use crate::session::service::{Service, SessionId};
use std::sync::atomic::Ordering;

impl Service {
    pub(crate) async fn apply_watchdog(
        &self,
        parent: SessionId,
        child: SessionId,
        generation: u64,
        checked: Checked,
        evidence: Evidence,
    ) -> Result<(), ServerError> {
        let Some(handle) = self.handle(child) else {
            return Ok(());
        };
        if !handle.status.watchdog_eligible() || handle.session_cancel.is_cancelled() {
            return Ok(());
        }
        let Some(link) = self.child_metadata(child).await? else {
            return Ok(());
        };
        if link.status != "running" {
            return Ok(());
        }
        if !handle
            .status
            .watchdog
            .accept_verdict(generation, checked.verdict == Verdict::Doubt)
        {
            tracing::debug!(%child, generation, "discarded stale watchdog verdict");
            return Ok(());
        }
        let payload = serde_json::json!({
            "notice_kind": "watchdog", "child_session_id": child,
            "agent": link.agent, "verdict": checked.verdict,
            "reason": checked.reason.map(|reason| reason.reason()), "checked_at": now_rfc3339(),
            "delivery": "next_turn", "evidence": evidence,
        });
        let notice = if checked.verdict == Verdict::Doubt {
            format!(
                "subagent @{} ({child}) may be stuck: continue or stop it?\n{}",
                link.agent,
                serde_json::to_string(&evidence)
                    .map_err(|e| ServerError::History(e.to_string()))?
            )
        } else {
            String::new()
        };
        if let Err(error) = self
            .history()
            .enqueue_watchdog(parent, payload, notice, checked.verdict == Verdict::Doubt)
            .await
            .map_err(ServerError::History)
        {
            handle.status.watchdog.resume();
            return Err(error);
        }
        self.drain_completions(parent).await;
        match checked.verdict {
            Verdict::Active => {}
            Verdict::Doubt => {
                if let Some(owner) = self.handle(parent) {
                    owner.status.wake_delivery();
                }
            }
            Verdict::Stuck => {
                self.stop_watchdog_child_generation(parent, child, Some(generation))
                    .await?
            }
        }
        Ok(())
    }

    pub(crate) async fn stop_watchdog_child(
        &self,
        parent: SessionId,
        child: SessionId,
    ) -> Result<(), ServerError> {
        self.stop_watchdog_child_generation(parent, child, None)
            .await
    }

    async fn stop_watchdog_child_generation(
        &self,
        parent: SessionId,
        child: SessionId,
        generation: Option<u64>,
    ) -> Result<(), ServerError> {
        #[cfg(debug_assertions)]
        {
            let barrier = self
                .watchdog_ownership_barrier
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(barrier) = barrier {
                barrier.reached.notify_one();
                barrier.release.notified().await;
            }
        }
        let Some(handle) = self.handle(child) else {
            return Err(ServerError::ChildNotParked);
        };
        self.wait_child_operation(child).await;
        let was_doubt = handle.status.watchdog.is_doubt();
        let _operation = {
            let _admission = self.admission_gate.lock().await;
            if self
                .child_operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&child)
                || self
                    .handle(child)
                    .is_none_or(|h| h.lifecycle != crate::session::service::Lifecycle::Active)
            {
                return Ok(());
            }
            let claimed = match generation {
                Some(generation) => handle.status.watchdog.claim_stuck(generation),
                None => handle
                    .status
                    .watchdog
                    .claim_terminal(crate::bus::runtime::TerminalOwner::Watchdog),
            };
            if !claimed {
                tracing::debug!(%child, "discarded obsolete watchdog stopping intent");
                return Ok(());
            }
            if handle.session_cancel.is_cancelled() {
                return Ok(());
            }
            self.own_child_operation(child)
        };
        let claim = async {
            #[cfg(debug_assertions)]
            {
                let barrier = self
                    .watchdog_claim_write_barrier
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                if let Some(barrier) = barrier {
                    barrier.reached.notify_one();
                    barrier.release.notified().await;
                }
            }
            #[cfg(debug_assertions)]
            if self.watchdog_stop_failure.swap(false, Ordering::AcqRel) {
                return Err("injected watchdog terminal write failure".into());
            }
            self.history().claim_watchdog_stop(child).await
        }
        .await;
        if let Err(error) = claim {
            if let Some((message, outcome)) = handle.status.watchdog.rollback_stop(was_doubt) {
                use crate::bus::runtime::CompletionOutcome;
                let status = match outcome {
                    CompletionOutcome::Done => "done",
                    CompletionOutcome::Failed => "failed",
                    CompletionOutcome::Stopped => "stopped",
                    CompletionOutcome::Parked => unreachable!("parked outcomes are not terminal"),
                };
                handle.status.watchdog.supervisor_cancel.cancel();
                let text = crate::tools::bound_output(&message.text, 16 * 1024).0;
                self.finish_child(
                    parent,
                    child,
                    status,
                    (outcome != CompletionOutcome::Done).then_some(message.text),
                    text,
                )
                .await?;
            }
            return Err(ServerError::History(error));
        }
        let target = {
            let _admission = self.admission_gate.lock().await;
            self.mark_stopping(child)
        };
        let Some(target) = target else {
            return Ok(());
        };
        handle
            .status
            .watchdog
            .stopped
            .store(true, Ordering::Release);
        let partial = handle
            .status
            .activity
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .partial_output
            .clone();
        target.session_cancel.cancel();
        #[cfg(debug_assertions)]
        {
            let barrier = self
                .watchdog_teardown_barrier
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(barrier) = barrier {
                barrier.reached.notify_one();
                barrier.release.notified().await;
            }
        }
        self.fence_and_remove(child, target, "watchdog").await;
        self.finish_child(
            parent,
            child,
            "stopped",
            Some("watchdog".into()),
            format!("stopped by watchdog\n{partial}"),
        )
        .await?;
        Ok(())
    }
}
