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
    pub parent_session_id: Option<String>,
    pub child_status: Option<String>,
    pub promptable: bool,
}

impl Service {
    /// The workspace's stored conversations, newest `updated_at` first.
    pub async fn list(&self, workspace: &str) -> Result<Vec<SessionListing>, ServerError> {
        let rows = self
            .history()
            .list(workspace)
            .await
            .map_err(ServerError::History)?;
        let mut listings = Vec::new();
        for row in rows {
            let link = self
                .history()
                .child_link(row.meta.id)
                .await
                .map_err(ServerError::History)?;
            if link.is_none()
                && self
                    .unpublished_children
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&row.meta.id)
            {
                continue;
            }
            listings.push(SessionListing {
                live: matches!(
                    self.handle(row.meta.id),
                    Some(handle) if handle.lifecycle == Lifecycle::Active
                ),
                parent_session_id: link.as_ref().map(|child| child.parent_id.to_string()),
                child_status: link.as_ref().map(|child| child.status.clone()),
                promptable: link.is_none(),
                id: row.meta.id,
                title: row.meta.title,
                created_at: row.meta.created_at,
                updated_at: row.meta.updated_at,
                message_count: row.message_count,
                agent: row
                    .meta
                    .primary_agent
                    .unwrap_or_else(|| crate::agent::definitions::BUILD.to_string()),
            });
        }
        Ok(listings)
    }

    /// Stops the session if live, removes it from memory and from the
    /// store. [`ServerError::SessionNotFound`] only when the id is neither
    /// in memory nor on disk.
    pub async fn delete(&self, id: SessionId) -> Result<(), ServerError> {
        let _gate = self.lifecycle_gate().await;
        self.wait_child_operation(id).await;
        // A live session is closed; a stored one is a no-op; an id the store
        // does not know is a 404.
        let parent = self.handle(id).and_then(|handle| handle.parent_session_id);
        self.close_session_gated(id).await?;
        if let Some(parent) = parent {
            self.finish_child(
                parent,
                id,
                "stopped",
                Some("deleted_by_owner".into()),
                String::new(),
            )
            .await?;
        }
        self.discard(id);
        self.history()
            .delete(id)
            .await
            .map_err(ServerError::History)?;
        Ok(())
    }
}
