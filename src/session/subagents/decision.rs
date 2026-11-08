use crate::error::ServerError;
use crate::session::service::{Service, SessionId};

impl Service {
    /// Root cognition decides its own child pauses; permissions stay with the Captain.
    pub async fn decide_subagent(
        &self,
        parent: SessionId,
        child: SessionId,
        stop: bool,
        note: Option<&str>,
    ) -> Result<(), ServerError> {
        let owner = self.routable_handle(parent).await?;
        let handle = self.routable_handle(child).await?;
        if owner.parent_session_id.is_some() || handle.parent_session_id != Some(parent) {
            return Err(ServerError::ChildReadOnly);
        }
        if note.is_some_and(|note| note.len() > 16 * 1024) {
            return Err(ServerError::MalformedBody(
                "note must be at most 16 KiB".into(),
            ));
        }
        if handle.status.pending_permission().is_some()
            || handle.status.selection.ask_pending()
            || handle
                .status
                .awaiting_fresh_captain
                .load(std::sync::atomic::Ordering::Acquire)
            || handle.status.watchdog.root_hold().is_some()
        {
            return Err(ServerError::ChildNotParked);
        }
        let doubt = handle.status.watchdog.is_doubt();
        let budget = handle.status.is_budget_parked();
        if !doubt && !budget {
            return Err(ServerError::MalformedBody(
                "child is not parked by a watchdog doubt or its step budget".into(),
            ));
        }
        if stop {
            if doubt {
                if !handle.status.watchdog.claim_stop() {
                    return Err(ServerError::MalformedBody(
                        "child is not parked by a watchdog doubt".into(),
                    ));
                }
                return self.stop_watchdog_child(parent, child).await;
            }
            return self.stop_budget_child(parent, child).await;
        }
        if doubt {
            if !handle.status.watchdog.resume_with(|| {
                if let Some(note) = note.filter(|note| !note.trim().is_empty()) {
                    handle.status.steer(note.to_string(), false);
                }
            }) {
                return Err(ServerError::MalformedBody(
                    "child is not parked by a watchdog doubt".into(),
                ));
            }
            return Ok(());
        }
        handle
            .status
            .topup_budget_with(|| {
                if let Some(note) = note.filter(|note| !note.trim().is_empty()) {
                    handle.status.queue_budget_note(note.to_string());
                }
            })
            .map_err(|error| match error {
                crate::bus::runtime::BudgetTopupError::NotParked => ServerError::ChildNotParked,
                crate::bus::runtime::BudgetTopupError::LimitReached => {
                    ServerError::BudgetTopupLimit
                }
            })?;
        Ok(())
    }
}
