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
        let payload = serde_json::json!({
            "notice_kind": "watchdog", "child_session_id": child,
            "agent": link.agent, "verdict": checked.verdict,
            "reason": checked.reason.map(|reason| reason.reason()), "checked_at": now_rfc3339(),
            "delivery": "next_turn", "evidence": evidence,
        });
        let notice = if checked.verdict == Verdict::Doubt {
            if !handle.status.watchdog.park() {
                return Ok(());
            }
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
            Verdict::Stuck => self.stop_watchdog_child(parent, child).await?,
        }
        Ok(())
    }

    pub(crate) async fn stop_watchdog_child(
        &self,
        parent: SessionId,
        child: SessionId,
    ) -> Result<(), ServerError> {
        let Some(handle) = self.handle(child) else {
            return Err(ServerError::ChildNotParked);
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
        // Commit the outcome before cancelling so the natural interruption callback cannot win the CAS.
        if let Err(error) = self
            .finish_child(
                parent,
                child,
                "stopped",
                Some("watchdog".into()),
                format!("stopped by watchdog\n{partial}"),
            )
            .await
        {
            handle
                .status
                .watchdog
                .stopped
                .store(false, Ordering::Release);
            handle.status.watchdog.resume();
            return Err(error);
        }
        let target = {
            let _admission = self.admission_gate.lock().await;
            let target = self.mark_stopping(child);
            if let Some(handle) = &target {
                handle.session_cancel.cancel();
            }
            target
        };
        if let Some(target) = target {
            self.fence_and_remove(child, target, "watchdog").await;
        }
        Ok(())
    }
}
