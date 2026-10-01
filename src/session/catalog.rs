//! Listing and deleting conversations behind the session service: the
//! stored conversations of a workspace joined with the registry's
//! liveness, and the full removal of one (live teardown, memory, store).

use super::service::{Lifecycle, Service, SessionId, SessionSlot};
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
    pub live: bool,
}

impl Service {
    /// The workspace's stored conversations, newest `updated_at` first.
    /// Without a history store the list is empty.
    pub async fn list(&self, workspace: &str) -> Result<Vec<SessionListing>, ServerError> {
        let Some(store) = self.history() else {
            return Ok(Vec::new());
        };
        let rows = store.list(workspace).await.map_err(ServerError::History)?;
        Ok(rows
            .into_iter()
            .map(|row| SessionListing {
                live: matches!(
                    self.slot(row.meta.id),
                    Some(SessionSlot::Live(handle)) if handle.lifecycle == Lifecycle::Active
                ),
                id: row.meta.id,
                title: row.meta.title,
                created_at: row.meta.created_at,
                updated_at: row.meta.updated_at,
                message_count: row.message_count,
            })
            .collect())
    }

    /// Stops the session if live, removes it from memory and from the
    /// store. [`ServerError::SessionNotFound`] only when the id is neither
    /// in memory nor on disk.
    pub async fn delete(&self, id: SessionId) -> Result<(), ServerError> {
        let known_to_memory = self.slot(id).is_some();
        match self.close_session(id).await {
            Ok(()) => {}
            Err(ServerError::SessionNotFound(_)) if !known_to_memory => {
                // Disk-only id: it must exist in the store.
                let on_disk = match self.history() {
                    Some(store) => store.exists(id).await.map_err(ServerError::History)?,
                    None => false,
                };
                if !on_disk {
                    return Err(ServerError::SessionNotFound(id.to_string()));
                }
            }
            Err(err) => return Err(err),
        }
        self.discard(id);
        if let Some(store) = self.history() {
            store.delete(id).await.map_err(ServerError::History)?;
        }
        Ok(())
    }
}
