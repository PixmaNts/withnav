use crate::error::ServerError;
use crate::session::service::{Service, SessionId};

impl Service {
    /// Root cognition may decide only its own watchdog doubt; permissions stay with the driver.
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
        if !handle.status.watchdog.is_doubt() {
            return Err(ServerError::MalformedBody(
                "child is not parked by a watchdog doubt".into(),
            ));
        }
        if stop {
            self.stop_watchdog_child(parent, child).await
        } else {
            if let Some(note) = note.filter(|note| !note.trim().is_empty()) {
                handle.status.steer(note.to_string(), false);
            }
            if !handle.status.watchdog.resume() {
                return Err(ServerError::MalformedBody(
                    "child is not parked by a watchdog doubt".into(),
                ));
            }
            Ok(())
        }
    }
}
