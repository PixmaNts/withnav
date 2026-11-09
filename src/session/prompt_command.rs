//! `/<server>:<prompt>`: an MCP server's prompt run as a Captain command. The prompt's
//! messages reach the agent in the Captain's own message, as a JSON block the permission
//! flow never reads as the Captain's words.

use std::time::SystemTime;

use serde_json::json;
use ulid::Ulid;

use super::commands;
use super::run_command::{CommandFailure, Done};
use super::service::{Service, SessionHandle, SessionId};
use crate::bus::command::{Command, ContentBlock, DeliveryMode};
use crate::bus::event::ROOT_ACTOR_ID;
use crate::error::ServerError;

/// The Captain's ingress for a command posted over HTTP: the message a prompt sends is the
/// Captain's, like the command that asked for it.
const PROMPT_SOURCE: &str = "user:http";

impl Service {
    /// Every ready MCP server's prompts, as commands a client lists after the built-ins.
    pub fn prompt_commands(&self) -> Vec<crate::mcp::PromptCommand> {
        self.mcp.prompt_commands()
    }

    /// The MCP prompt a command name designates, if any ready server offers it.
    pub(super) fn find_prompt(&self, name: &str) -> Option<crate::mcp::PromptRun> {
        self.mcp.prompt(commands::normalize_name(name))
    }

    /// Runs one prompt: the arguments are checked under the lifecycle gate, which is let go
    /// while the server answers (a slow server must not hold a stop or another session's
    /// command), then taken again to send the message and publish the result. A session
    /// stopped or replaced meanwhile gets nothing.
    pub(super) async fn prompt_command<'a>(
        &'a self,
        session_id: SessionId,
        handle: &SessionHandle,
        prompt: crate::mcp::PromptRun,
        arguments: &str,
        gate: &mut Option<tokio::sync::MutexGuard<'a, ()>>,
    ) -> Result<Done, CommandFailure> {
        pending_refusal(handle)?;
        let values = prompt
            .arguments(arguments)
            .map_err(CommandFailure::Refused)?;
        drop(gate.take());
        let secrets = self.secret_access(self.secret_owner(session_id), &handle.workspace);
        let fetched = prompt
            .fetch(values, &handle.session_cancel, &|text| {
                secrets.redact(text).into_owned()
            })
            .await;
        *gate = Some(self.lifecycle_gate().await);
        let current = self
            .routable_handle(session_id)
            .await
            .map_err(CommandFailure::Server)?;
        if current.incarnation != handle.incarnation {
            return Err(CommandFailure::Server(ServerError::SessionStopping));
        }
        let fetched = fetched.map_err(CommandFailure::Refused)?;
        // Every field was redacted before encoding; admission redacts the whole text again. A
        // stored secret that matches the framing itself (the block's tag, its JSON keys) would
        // rewrite it there, and the server text would no longer be told apart from the
        // Captain's words: refuse rather than send that.
        if secrets.redact(&fetched.text) != fetched.text {
            return Err(CommandFailure::Refused(
                "the prompt cannot be sent: one of your stored secrets matches its framing".into(),
            ));
        }
        pending_refusal(&current)?;
        let command = Command {
            command_id: Ulid::generate(),
            session_id,
            source_actor_id: PROMPT_SOURCE.into(),
            target_actor_id: ROOT_ACTOR_ID.into(),
            mode: DeliveryMode::NextTurn,
            content: vec![ContentBlock::Text { text: fetched.text }],
            created_at: SystemTime::now(),
        };
        let command_id = command.command_id;
        match self.send(command).await {
            Ok(_) => {}
            Err(err @ (ServerError::SessionStopping | ServerError::SessionNotFound(_))) => {
                return Err(CommandFailure::Server(err));
            }
            Err(err) => {
                return Err(CommandFailure::Refused(format!(
                    "the prompt could not be sent: {err}"
                )));
            }
        }
        let command = prompt.command();
        let (server, name) = command.split_once(':').unwrap_or((&command, ""));
        Ok(Done::data(json!({
            "server": server,
            "prompt": name,
            "command_id": command_id.to_string(),
            "messages": fetched.messages,
            "truncated": fetched.truncated,
        })))
    }
}

/// A prompt is the Captain's message: it would answer a waiting question in place of the
/// Captain's reply, so it is refused while one waits, like every other command.
fn pending_refusal(handle: &SessionHandle) -> Result<(), CommandFailure> {
    if handle.status.pending_permission().is_some() {
        return Err(CommandFailure::Refused(
            commands::PERMISSION_PENDING_ERROR.to_string(),
        ));
    }
    if handle.status.pending_ask() {
        return Err(CommandFailure::Refused(
            commands::ASK_PENDING_ERROR.to_string(),
        ));
    }
    Ok(())
}
