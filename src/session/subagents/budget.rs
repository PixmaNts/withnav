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
        let (used, steps) = match budget_counts(&evidence) {
            Ok(counts) => counts,
            Err(reason) => {
                tracing::warn!(%parent, %child, reason, "malformed budget evidence; notice skipped");
                return Ok(());
            }
        };
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
        let remaining = MAX_BUDGET_TOPUPS.saturating_sub(used);
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
        self.history()
            .enqueue_watchdog(parent, payload, notice, true)
            .await
            .map_err(ServerError::History)?;
        self.drain_completions(parent).await;
        if let Some(owner) = self.handle(parent) {
            owner.status.wake_delivery();
        }
        Ok(())
    }
}

/// The top-ups used and the step count a budget notice reports, read from the evidence the
/// child's runtime recorded. Both are required: a missing or malformed count read as zero would
/// tell the owner it has top-ups it does not have, or that a child stopped after no steps.
fn budget_counts(evidence: &serde_json::Value) -> Result<(u32, u64), String> {
    if !evidence.is_object() {
        return Err("evidence is not an object".into());
    }
    let used = evidence
        .get("topups_used")
        .and_then(serde_json::Value::as_u64)
        .ok_or("topups_used is missing or not a count")?;
    let used = u32::try_from(used)
        .ok()
        .filter(|used| *used <= MAX_BUDGET_TOPUPS)
        .ok_or_else(|| format!("topups_used {used} exceeds {MAX_BUDGET_TOPUPS}"))?;
    let steps = evidence
        .get("step_count")
        .and_then(serde_json::Value::as_u64)
        .ok_or("step_count is missing or not a count")?;
    Ok((used, steps))
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Given the evidence the runtime records when a child exhausts its step budget
    // Then both counts are read as recorded
    #[test]
    fn well_formed_evidence_gives_its_counts() {
        let evidence = json!({"goal": "g", "step_count": 40, "topups_used": 1});
        assert_eq!(budget_counts(&evidence), Ok((1, 40)));
    }

    // Given evidence with a missing, mistyped or out-of-range count, or no object at all
    // Then it is refused, never read as zero
    #[test]
    fn malformed_evidence_is_refused_rather_than_counted_as_zero() {
        for evidence in [
            json!({"step_count": 40}),
            json!({"step_count": 40, "topups_used": "0"}),
            json!({"step_count": 40, "topups_used": -1}),
            json!({"step_count": 40, "topups_used": u64::from(MAX_BUDGET_TOPUPS) + 1}),
            json!({"topups_used": 0}),
            json!({"step_count": "40", "topups_used": 0}),
            json!(null),
            json!([0, 40]),
        ] {
            assert!(
                budget_counts(&evidence).is_err(),
                "{evidence} must be refused"
            );
        }
    }
}
