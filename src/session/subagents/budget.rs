//! Parent-visible notice for a child whose step budget ran out.
use crate::bus::event::now_rfc3339;
use crate::bus::runtime::MAX_BUDGET_TOPUPS;
use crate::error::ServerError;
use crate::session::service::{Service, SessionId};

impl Service {
    /// Durably queues the notice that a child used its step budget, then wakes
    /// the owner. The child stays parked; the owner may top it up or stop it.
    pub(crate) async fn notify_budget_park(
        &self,
        parent: SessionId,
        child: SessionId,
        mut evidence: serde_json::Value,
    ) -> Result<(), ServerError> {
        let Some(handle) = self.handle(child) else {
            return Ok(());
        };
        let Some(link) = self.child_metadata(child).await? else {
            return Ok(());
        };
        if link.status != "running" {
            return Ok(());
        }
        evidence["goal"] = serde_json::json!(crate::tools::bound_output(&link.goal, 1024).0);
        let used = evidence["topups_used"].as_u64().unwrap_or_default() as u32;
        let remaining = MAX_BUDGET_TOPUPS.saturating_sub(used);
        let steps = evidence["step_count"].as_u64().unwrap_or_default();
        if !handle.status.is_budget_parked() || handle.status.budget_topups_used() != used {
            return Ok(());
        }
        let payload = serde_json::json!({
            "notice_kind": "budget", "child_session_id": child,
            "agent": link.agent, "checked_at": now_rfc3339(),
            "topups_used": used, "topups_remaining": remaining,
            "delivery": "next_turn", "evidence": evidence, "verdict": "budget",
        });
        let guidance = if remaining == 0 {
            "No parent top-ups remain: only the Captain can continue this child. You may stop it or start a new subagent with a narrower task."
        } else {
            "Use decide_subagent to continue or stop; you may instead start a new subagent with a narrower task."
        };
        let notice = format!(
            "subagent @{} ({child}) used its {steps} steps (top-ups used {used}/{MAX_BUDGET_TOPUPS}): continue, stop, or start a new subagent? {guidance} {}",
            link.agent,
            serde_json::to_string(&evidence).map_err(|e| ServerError::History(e.to_string()))?,
        );
        #[cfg(debug_assertions)]
        let barrier = self
            .budget_notice_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        #[cfg(debug_assertions)]
        if let Some(barrier) = &barrier {
            barrier.reached.notify_one();
            barrier.release.notified().await;
        }
        // A top-up, the Captain's continue or the owner's stop may have landed
        // since the check above: the park is checked again under their gate,
        // atomically with the commit, so a resumed child never leaves a stale
        // "used its steps" notice in the parent's outbox.
        let status = handle.status.clone();
        let committed = self
            .history()
            .enqueue_watchdog_if(parent, payload, notice, true, move |commit| {
                status.while_budget_parked(used, commit)
            })
            .await
            .map_err(ServerError::History);
        // The paused test learns the attempt is over, whether it committed or not.
        #[cfg(debug_assertions)]
        if let Some(barrier) = &barrier {
            barrier.reached.notify_one();
        }
        if !committed? {
            return Ok(());
        }
        self.drain_completions(parent).await;
        if let Some(owner) = self.handle(parent) {
            owner.status.wake_delivery();
        }
        Ok(())
    }
}

impl Service {
    /// Owner stop of a budget-parked child: the same ownership, admission and
    /// terminal compare-and-set as a watchdog stop, with the parked partial text.
    pub(crate) async fn stop_budget_child(
        &self,
        parent: SessionId,
        child: SessionId,
    ) -> Result<(), ServerError> {
        let Some(handle) = self.handle(child) else {
            return Err(ServerError::ChildNotParked);
        };
        self.wait_child_operation(child).await;
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
                || !handle.status.is_budget_parked()
            {
                return Err(ServerError::ChildNotParked);
            }
            if !handle.status.claim_budget_stop() {
                return Err(ServerError::ChildNotParked);
            }
            self.own_child_operation(child)
        };
        if let Err(error) = self.history().claim_watchdog_stop(child).await {
            handle.status.watchdog.rollback_stop(false);
            return Err(ServerError::History(error));
        }
        let target = {
            let _admission = self.admission_gate.lock().await;
            self.mark_stopping(child)
        };
        let Some(target) = target else {
            return Ok(());
        };
        let partial = handle
            .status
            .activity
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .partial_output
            .clone();
        target.session_cancel.cancel();
        self.fence_and_remove(child, target, "budget").await;
        self.finish_child(
            parent,
            child,
            "stopped",
            Some("budget".into()),
            format!("stopped by owner at its step budget\n{partial}"),
        )
        .await
    }
}
