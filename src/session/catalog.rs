//! Listing and deleting conversations behind the session service: the
//! stored conversations of a workspace joined with the registry's
//! liveness, and the full removal of one (live teardown, memory, store).

use super::service::{Lifecycle, Service, SessionId};
use crate::error::ServerError;

/// One conversation of a workspace: the stored summary plus whether a live,
/// active session with this id exists in this process.
#[derive(Debug, Clone)]
pub struct SessionListing {
    pub id: SessionId,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Number of driver messages (`message.received` events).
    pub message_count: u64,
    /// The primary agent the session runs as.
    pub agent: String,
    pub live: bool,
}

impl Service {
    /// The workspace's stored conversations, newest `updated_at` first.
    pub async fn list(&self, workspace: &str) -> Result<Vec<SessionListing>, ServerError> {
        let rows = self
            .history()
            .list(workspace)
            .await
            .map_err(ServerError::History)?;
        Ok(rows
            .into_iter()
            .map(|row| SessionListing {
                live: matches!(
                    self.handle(row.meta.id),
                    Some(handle) if handle.lifecycle == Lifecycle::Active
                ),
                id: row.meta.id,
                title: row.meta.title,
                created_at: row.meta.created_at,
                updated_at: row.meta.updated_at,
                message_count: row.message_count,
                agent: row
                    .meta
                    .primary_agent
                    .unwrap_or_else(|| crate::agent::definitions::BUILD.to_string()),
            })
            .collect())
    }

    /// Stops the session if live, removes it from memory and from the
    /// store. [`ServerError::SessionNotFound`] only when the id is neither
    /// in memory nor on disk.
    pub async fn delete(&self, id: SessionId) -> Result<(), ServerError> {
        let _gate = self.lifecycle_gate().await;
        // A live session is closed; a stored one is a no-op; an id the store
        // does not know is a 404.
        self.close_session_gated(id).await?;
        self.discard(id);
        self.history()
            .delete(id)
            .await
            .map_err(ServerError::History)?;
        Ok(())
    }
}
