//! `Service::list_commands` and `Service::run_command`: the built-in session
//! commands, run by the service on behalf of any client. Every run publishes
//! one durable `command.result` on the session stream.

use serde_json::{json, Value};
use ulid::Ulid;

use super::commands::{self, CommandOutcome, CommandSpec};
use super::service::{Service, SessionHandle, SessionId};
use crate::bus::event::{CommandResult, EventKind};
use crate::bus::sequencer::Candidate;
use crate::error::ServerError;

impl Service {
    /// The registered commands, in listing order.
    pub fn list_commands(&self) -> &'static [CommandSpec] {
        &commands::COMMANDS
    }

    /// Runs the command `name` (a leading `/` is ignored) for a live session.
    ///
    /// A command that cannot run (unknown, not built yet, refused during a
    /// pending `ask`, failed) is an `Ok` outcome with `ok: false` and an
    /// `error` text; `Err` is only for a session that cannot take commands
    /// (unknown, stopping, stopped). Either way one durable `command.result`
    /// is published on the session stream, with the operation id returned.
    /// A command never becomes a message and is never queued: during a
    /// pending `ask` every command is refused (`/stop` included: the stop
    /// endpoint still ends the session).
    pub async fn run_command(
        &self,
        session_id: SessionId,
        name: &str,
        arguments: &str,
    ) -> Result<CommandOutcome, ServerError> {
        let handle = self.routable_handle(session_id).await?;
        let name = name.strip_prefix('/').unwrap_or(name).trim().to_string();
        let operation_id = Ulid::new();
        let result = self
            .dispatch(session_id, &handle, &name, arguments.trim())
            .await;
        let (ok, data, error) = match result {
            Ok(data) => (true, Some(CommandResult::bound_data(data)), None),
            Err(CommandFailure::Refused(error)) => (false, None, Some(error)),
            Err(CommandFailure::Server(err)) => return Err(err),
        };
        let payload = serde_json::to_value(CommandResult {
            operation_id,
            name: name.clone(),
            ok,
            data: data.clone(),
            error: error.clone(),
        })
        .unwrap_or(Value::Null);
        handle
            .sequencer
            .publish(Candidate::new(EventKind::CommandResult, payload))
            .map_err(|_| ServerError::SessionStopping)?;
        if ok && name == "stop" {
            self.close_session(session_id).await?;
        }
        Ok(CommandOutcome {
            operation_id,
            name,
            ok,
            data,
            error,
        })
    }

    async fn dispatch(
        &self,
        session_id: SessionId,
        handle: &SessionHandle,
        name: &str,
        arguments: &str,
    ) -> Result<Value, CommandFailure> {
        let Some(spec) = commands::find(name) else {
            return Err(CommandFailure::Refused(commands::unknown_error(name)));
        };
        if handle.status.pending_ask() {
            return Err(CommandFailure::Refused(
                commands::ASK_PENDING_ERROR.to_string(),
            ));
        }
        if commands::NOT_AVAILABLE.contains(&spec.name) {
            return Err(CommandFailure::Refused(commands::not_available_error(
                spec.name,
            )));
        }
        match spec.name {
            "help" => Ok(json!({ "commands": commands::COMMANDS })),
            "model" => Ok(model_data(handle)),
            "context" => handle
                .status
                .context_snapshot()
                .and_then(|snapshot| serde_json::to_value(snapshot).ok())
                .ok_or_else(|| CommandFailure::Refused("no context snapshot yet".to_string())),
            "new" => self.new_conversation(session_id, handle, arguments).await,
            // The stop itself is done by `run_command` after the result is
            // published: the stream ends with the session.
            "stop" => Ok(json!({ "stopped": true })),
            other => Err(CommandFailure::Refused(commands::not_available_error(
                other,
            ))),
        }
    }

    /// `/new [alias]`: a fresh session in the same workspace, with the
    /// alias given or this session's own. This one is left as it is.
    async fn new_conversation(
        &self,
        session_id: SessionId,
        handle: &SessionHandle,
        arguments: &str,
    ) -> Result<Value, CommandFailure> {
        let mut words = arguments.split_whitespace();
        let alias = words.next();
        if words.next().is_some() {
            return Err(CommandFailure::Refused("usage: /new [alias]".to_string()));
        }
        let alias = alias.unwrap_or(&handle.model).to_string();
        let profile = self
            .history()
            .meta(session_id)
            .await
            .map_err(|e| CommandFailure::Server(ServerError::History(e)))?
            .and_then(|meta| meta.profile);
        match self
            .create_session(&handle.workspace, Some(&alias), profile.as_deref())
            .await
        {
            Ok(id) => Ok(json!({ "session_id": id.to_string(), "model": alias })),
            Err(err @ ServerError::History(_)) => Err(CommandFailure::Server(err)),
            Err(err) => Err(CommandFailure::Refused(err.to_string())),
        }
    }
}

/// Why a command produced no data.
enum CommandFailure {
    /// The command's own error: reported in the result.
    Refused(String),
    /// The session cannot take commands: the caller's error.
    Server(ServerError),
}

/// `/model`: the alias and what the saved resolution says about it.
fn model_data(handle: &SessionHandle) -> Value {
    match &handle.resolution {
        Some(r) => json!({
            "alias": handle.model,
            "model": r.model,
            "provider": r.provider,
            "window": r.bounds.context_window,
            "window_source": r.provenance.context_window,
            "max_input_tokens": r.bounds.max_input_tokens,
        }),
        None => json!({ "alias": handle.model }),
    }
}
