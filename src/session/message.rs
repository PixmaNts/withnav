//! Command acceptance: validate one [`Command`], reserve an inbox slot,
//! sequence `message.received`, commit the command and ack. Transport-free:
//! HTTP, the CLI and the navigator's wake poster all arrive here.

use serde::Serialize;

use super::service::SessionHandle;
use crate::bus::command::Command;
use crate::bus::event::ROOT_ACTOR_ID;
use crate::bus::sequencer::PendingCommand;
use crate::error::{BusError, ServerError};

/// The frozen message ack: `notice` carries the visible
/// steer/interrupt downgrade and is absent for next-turn.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MessageAccepted {
    pub command_id: String,
    pub accepted: bool,
    pub actor_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// Validate then route one command into a live session. One root actor this
/// slice: the target is pinned to the session's agent (a foreign target is a
/// malformed ask), and content must be text-only. Nothing is sequenced for a
/// rejected command.
pub(super) async fn accept(
    handle: &SessionHandle,
    command: Command,
) -> Result<MessageAccepted, ServerError> {
    if command.target_actor_id != ROOT_ACTOR_ID {
        return Err(ServerError::MalformedBody(format!(
            "unknown target_actor_id: {} (this server routes to {ROOT_ACTOR_ID})",
            command.target_actor_id
        )));
    }
    command.validate_content()?;
    if command
        .content
        .iter()
        .any(|block| matches!(block, crate::bus::command::ContentBlock::Skill { .. }))
    {
        return Err(ServerError::MalformedBody(
            "skill selection must be resolved by the session service".into(),
        ));
    }
    route(handle, command).await
}

/// The event-before-action route for ONE validated command: reserve an inbox
/// permit up front (a full inbox is a 503 BEFORE the event is sequenced, so
/// there is nothing to un-publish — the rejected permit releases its slot),
/// sequence `message.received` via `accept_command`, commit the command into
/// the reserved permit, ack.
async fn route(handle: &SessionHandle, command: Command) -> Result<MessageAccepted, ServerError> {
    let permit = match handle.inbox_tx.clone().try_reserve_owned() {
        Ok(permit) => permit,
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            return Err(ServerError::InboxFull);
        }
        Err(_) => {
            return Err(ServerError::SessionStopping);
        }
    };
    let command_id = command.command_id;
    let actor_state = handle.actor_state.clone();
    let notice = command.downgrade_notice().map(|n| n.to_string());
    let pending = PendingCommand {
        command_id,
        content: command.text_content(),
        source: command.source_actor_id.clone(),
        target: command.target_actor_id.clone(),
        mode: command.mode.as_str().to_string(),
    };
    let committed = command.clone();
    let status = handle.status.clone();
    handle
        .sequencer
        .accept_command(
            pending,
            Box::new(move || {
                if committed.from_driver() {
                    status.queue_driver();
                }
                permit.send(committed);
            }),
        )
        .await
        .map_err(map_bus_error)?;
    Ok(MessageAccepted {
        command_id: command_id.to_string(),
        accepted: true,
        actor_state,
        notice,
    })
}

/// Map a routing failure onto the typed server error: full inbox stays 503;
/// a sequencer that closed under us means the session is stopping.
fn map_bus_error(err: BusError) -> ServerError {
    match err {
        BusError::InboxFull => ServerError::InboxFull,
        BusError::SequencerClosed | BusError::SessionNotFound(_) | BusError::SeqSpaceExhausted => {
            ServerError::SessionStopping
        }
    }
}
