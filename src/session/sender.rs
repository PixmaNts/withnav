//! The one capability a session hands to its host: posting a command.

use std::time::SystemTime;

use ulid::Ulid;

use super::message::accept;
use super::service::{SessionHandle, SessionId};
use crate::bus::command::{Command, ContentBlock, DeliveryMode};
use crate::bus::event::ROOT_ACTOR_ID;
use crate::error::ServerError;

/// A cloneable handle that can only post text to the root agent of ONE
/// session incarnation. A session resumed under the same id is a new
/// incarnation: a sender kept from the old one fails with
/// [`ServerError::SessionStopping`] and never reaches the new one.
#[derive(Clone, Debug)]
pub struct SessionSender {
    session_id: SessionId,
    handle: SessionHandle,
}

impl SessionSender {
    pub(super) fn new(session_id: SessionId, handle: SessionHandle) -> Self {
        Self { session_id, handle }
    }

    /// Posts `text` as a next-turn command from `source`, through the same
    /// validation and event-before-action sequencing as `POST /messages`.
    pub async fn send(&self, text: String, source: &str) -> Result<(), ServerError> {
        let command = Command {
            command_id: Ulid::new(),
            session_id: self.session_id,
            source_actor_id: source.to_string(),
            target_actor_id: ROOT_ACTOR_ID.to_string(),
            mode: DeliveryMode::NextTurn,
            content: vec![ContentBlock::Text { text }],
            created_at: SystemTime::now(),
        };
        accept(&self.handle, command).await.map(|_| ())
    }
}
