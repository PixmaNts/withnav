//! `Service::list_commands` and `Service::run_command`: the built-in session
//! commands, run by the service on behalf of any client. Every run publishes
//! one durable `command.result` on the session stream.

use std::sync::Arc;

use serde_json::{json, Value};
use ulid::Ulid;

use super::commands::{self, CommandOutcome, CommandSpec};
use super::service::{exact_variant, Service, SessionHandle, SessionId, VariantChoice, CLEAR_WORD};
use crate::agent::selection::SwitchError;
use crate::bus::event::{CommandResult, EventKind, SessionAgentChanged};
use crate::bus::runtime::{CompactReply, ModelReply};
use crate::bus::sequencer::Candidate;
use crate::error::ServerError;

/// How many alias names `/model` lists; the rest is `GET /v1/models`.
const MODEL_CATALOG_CAP: usize = 50;

/// Why an agent or variant change waits for a compaction to finish.
const COMPACTING_ERROR: &str =
    "a compaction is running: wait for it to finish, then change the agent or variant";

/// Why an agent, variant, compaction or model change waits for a model switch.
/// Why a model switch waits for a compaction to finish.
const COMPACTING_MODEL_ERROR: &str =
    "a compaction is running: wait for it to finish, then switch model";

/// Why a variant change read against a model that was switched meanwhile is
/// not applied.
const MODEL_CHANGED_ERROR: &str =
    "the model was switched while the variant was being checked: run /variant again";

const SWITCHING_ERROR: &str = "a model switch is running: wait for it to finish, then retry";

/// Longest unknown command name echoed into the result.
const UNKNOWN_NAME_CAP: usize = 64;

impl Service {
    /// The registered commands, in listing order.
    pub fn list_commands(&self) -> &'static [CommandSpec] {
        &commands::COMMANDS
    }

    /// Runs the command `name` (one leading `/` and surrounding whitespace
    /// are ignored; anything else must match a registered name exactly) for
    /// a live session.
    ///
    /// A command that cannot run (unknown, not built yet, refused during a
    /// pending `ask`, bad arguments, failed) is an `Ok` outcome with
    /// `ok: false` and an `error` text; `Err` is only for a session that
    /// cannot take commands (unknown, stopping, stopped). Either way one
    /// durable `command.result` is published on the session stream, with the
    /// operation id returned. A command never becomes a message and is never
    /// queued: during a pending `ask` every command is refused (`/stop`
    /// included: the stop endpoint still ends the session); during a pending
    /// permission request every command but `/stop` is, and `/stop` cancels
    /// the request.
    ///
    /// The run holds the lifecycle gate from the session check to the
    /// acknowledged publication of its result, so a stop or delete of the
    /// session waits for the command and never lands between its work and
    /// its result. `/compact` is the exception: it hands its work to the
    /// session runtime and lets go of the gate before it waits for the
    /// summary, so a stop, a shutdown and the other sessions never wait for
    /// the provider; the runtime, which a stop cancels, guarantees that a
    /// stopped session commits nothing. When the result cannot be published (the session closed
    /// on its own) the command fails and a session it created is closed
    /// again.
    pub async fn run_command(
        &self,
        session_id: SessionId,
        name: &str,
        arguments: &str,
    ) -> Result<CommandOutcome, ServerError> {
        let mut gate = Some(self.lifecycle_gate().await);
        let handle = self.routable_handle(session_id).await?;
        let spec = commands::find(name);
        let name = match spec {
            Some(spec) => spec.name.to_string(),
            None => commands::normalize_name(name)
                .chars()
                .take(UNKNOWN_NAME_CAP)
                .collect(),
        };
        let operation_id = Ulid::new();
        let mut created = None;
        let result = match spec {
            Some(spec) => {
                self.dispatch(
                    session_id,
                    &handle,
                    spec,
                    arguments.trim(),
                    operation_id,
                    &mut gate,
                )
                .await
            }
            None => Err(CommandFailure::Refused(commands::unknown_error(&name))),
        };
        // `/compact` is published by the runtime, in order with the rest of
        // its operation, before it answers.
        let mut published = false;
        let (ok, data, error) = match result {
            Ok(done) => {
                created = done.created;
                published = done.published;
                (true, Some(CommandResult::bound_data(done.data)), None)
            }
            Err(CommandFailure::Refused(error)) => (false, None, Some(error)),
            Err(CommandFailure::Published(error)) => {
                published = true;
                (false, None, Some(error))
            }
            Err(CommandFailure::Server(err)) => return Err(err),
        };
        if published {
            return Ok(CommandOutcome {
                operation_id,
                name,
                ok,
                data,
                error,
            });
        }
        let payload = serde_json::to_value(CommandResult {
            operation_id,
            name: name.clone(),
            ok,
            data: data.clone(),
            error: error.clone(),
        })
        .unwrap_or(Value::Null);
        self.before_publish_hook().await;
        if handle
            .sequencer
            .publish_acked(Candidate::new(EventKind::CommandResult, payload))
            .await
            .is_err()
        {
            if let Some(id) = created {
                if let Err(err) = self.close_session_gated(id).await {
                    tracing::warn!(session_id = %id, error = %err, "closing the session of a failed /new");
                }
            }
            return Err(ServerError::SessionStopping);
        }
        if ok && name == "stop" {
            self.close_session_gated(session_id).await?;
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
        spec: &'static CommandSpec,
        arguments: &str,
        operation_id: Ulid,
        gate: &mut Option<tokio::sync::MutexGuard<'_, ()>>,
    ) -> Result<Done, CommandFailure> {
        // A waiting permission request also reads as a waiting actor; `/stop`
        // goes through it and cancels the request.
        if handle.status.pending_permission().is_some() {
            if spec.name != "stop" {
                return Err(CommandFailure::Refused(
                    commands::PERMISSION_PENDING_ERROR.to_string(),
                ));
            }
        } else if handle.status.pending_ask() {
            return Err(CommandFailure::Refused(
                commands::ASK_PENDING_ERROR.to_string(),
            ));
        }
        if commands::NOT_AVAILABLE.contains(&spec.name) {
            return Err(CommandFailure::Refused(commands::not_available_error(
                spec.name,
            )));
        }
        if spec.args_hint.is_empty() && !arguments.is_empty() {
            return Err(CommandFailure::Refused(format!("usage: /{}", spec.name)));
        }
        match spec.name {
            "help" => Ok(Done::data(json!({ "commands": commands::COMMANDS }))),
            "model" if arguments.is_empty() => Ok(Done::data(self.model_data(handle))),
            "model" => {
                self.model_command(handle, arguments, operation_id, gate)
                    .await
            }
            "context" => handle
                .status
                .context_snapshot()
                .and_then(|snapshot| serde_json::to_value(snapshot).ok())
                .map(Done::data)
                .ok_or_else(|| CommandFailure::Refused("no context snapshot yet".to_string())),
            "variant" => self.variant_command(session_id, handle, arguments).await,
            "new" => self.new_conversation(session_id, handle, arguments).await,
            "agent" => self.agent_command(session_id, handle, arguments).await,
            "compact" => {
                self.compact_command(handle, arguments, operation_id, gate)
                    .await
            }
            // The stop itself is done by `run_command` after the result is
            // published: the stream ends with the session.
            "stop" => Ok(Done::data(json!({ "stopped": true }))),
            other => Err(CommandFailure::Refused(commands::not_available_error(
                other,
            ))),
        }
    }

    /// `/compact [focus]`: summarizes the older part of the conversation and
    /// keeps the recent turns. The runtime does it between turns: at once when
    /// the session is idle, after the running turn otherwise (the answer says
    /// so). Refused while an `ask` waits (before this is reached).
    async fn compact_command(
        &self,
        handle: &SessionHandle,
        arguments: &str,
        operation_id: Ulid,
        gate: &mut Option<tokio::sync::MutexGuard<'_, ()>>,
    ) -> Result<Done, CommandFailure> {
        if handle.status.selection.switching() {
            return Err(CommandFailure::Refused(SWITCHING_ERROR.to_string()));
        }
        let focus = (!arguments.is_empty()).then(|| arguments.to_string());
        if focus
            .as_deref()
            .is_some_and(|f| f.chars().count() > crate::bus::runtime::FOCUS_MAX_CHARS)
        {
            return Err(CommandFailure::Refused(format!(
                "the focus is longer than {} characters",
                crate::bus::runtime::FOCUS_MAX_CHARS
            )));
        }
        // The summary is a provider call: nothing else may wait for it.
        drop(gate.take());
        match handle.status.request_compact(focus, operation_id).await {
            Some(CompactReply::Done(done)) => Ok(Done::published(done.wire("now"))),
            Some(CompactReply::Deferred) => {
                Ok(Done::published(crate::bus::runtime::deferred_wire()))
            }
            Some(CompactReply::Refused(why)) => Err(CommandFailure::Published(why)),
            None => Err(CommandFailure::Server(ServerError::SessionStopping)),
        }
    }

    /// `/model <alias>`: moves the conversation to another model. The runtime
    /// does it between turns: at once when the session is idle, after the
    /// running turn otherwise (the answer says so). Refused while an `ask` or
    /// a permission request waits (before this is reached), and while a
    /// compaction is preparing its summary.
    async fn model_command(
        &self,
        handle: &SessionHandle,
        arguments: &str,
        operation_id: Ulid,
        gate: &mut Option<tokio::sync::MutexGuard<'_, ()>>,
    ) -> Result<Done, CommandFailure> {
        let mut words = arguments.split_whitespace();
        let (Some(alias), None) = (words.next(), words.next()) else {
            return Err(CommandFailure::Refused("usage: /model [alias]".to_string()));
        };
        let selection = &handle.status.selection;
        if selection.compacting() {
            return Err(CommandFailure::Refused(COMPACTING_MODEL_ERROR.to_string()));
        }
        if selection.switching() {
            return Err(CommandFailure::Refused(SWITCHING_ERROR.to_string()));
        }
        // Resolving the target is a provider lookup: nothing else may wait
        // for it.
        drop(gate.take());
        match handle
            .status
            .request_model_switch(alias.to_string(), operation_id)
            .await
        {
            Some(ModelReply::Done(done)) => Ok(Done::published(done.wire("now"))),
            Some(ModelReply::Deferred) => Ok(Done::published(
                json!({ "accepted": true, "applies": "after_turn" }),
            )),
            Some(ModelReply::Refused(why)) => Err(CommandFailure::Published(why)),
            None => Err(CommandFailure::Server(ServerError::SessionStopping)),
        }
    }

    /// `/agent [id]`: with no argument, the current primary agent and the
    /// ones that can be selected. With an id, switches to it: only between
    /// turns (a pending `ask` was refused before this), stored before it is
    /// reported, and announced by a durable `session.agent_changed`. The
    /// conversation and the model stay as they are.
    async fn agent_command(
        &self,
        session_id: SessionId,
        handle: &SessionHandle,
        arguments: &str,
    ) -> Result<Done, CommandFailure> {
        let selection = &handle.status.selection;
        let current = selection.current();
        let mut words = arguments.split_whitespace();
        let Some(wanted) = words.next() else {
            let agents: Vec<Value> = selection
                .agents()
                .primary()
                .map(|a| json!({ "id": a.id, "description": a.description, "mode": a.mode.as_str() }))
                .collect();
            return Ok(Done::data(json!({ "agent": current.id, "agents": agents })));
        };
        if words.next().is_some() {
            return Err(CommandFailure::Refused("usage: /agent [id]".to_string()));
        }
        let target = match selection.agents().get(wanted) {
            Some(agent) if agent.mode.is_primary() => agent.clone(),
            _ => {
                return Err(CommandFailure::Refused(format!(
                    "unknown agent {wanted}; the primary agents are {}",
                    super::service::primary_list(selection.agents())
                )))
            }
        };
        if target.id == current.id {
            return Ok(Done::data(json!({ "agent": current.id, "changed": false })));
        }
        let stored = (!target.id.is_build()).then(|| target.id.as_str().to_string());
        let history = self.history().clone();
        let previous = selection
            .switch(target.clone(), async move {
                history
                    .set_primary_agent(session_id, stored.as_deref())
                    .await
            })
            .await
            .map_err(|err| match err {
                SwitchError::AskPending => {
                    CommandFailure::Refused(commands::ASK_PENDING_ERROR.to_string())
                }
                SwitchError::Compacting => CommandFailure::Refused(COMPACTING_ERROR.to_string()),
                SwitchError::Switching => CommandFailure::Refused(SWITCHING_ERROR.to_string()),
                SwitchError::Busy => CommandFailure::Refused(
                    "the agent is working: wait for the turn to end or stop it, then switch"
                        .to_string(),
                ),
                SwitchError::Persist(err) => CommandFailure::Server(ServerError::History(err)),
            })?;
        // The gauge counts the new role at once, not at the next turn.
        if let Some(snapshot) = handle.status.set_context_role(&target) {
            if let Ok(payload) = serde_json::to_value(&snapshot) {
                let _ = handle
                    .sequencer
                    .publish(Candidate::new(EventKind::ContextUpdated, payload));
            }
        }
        let payload = serde_json::to_value(SessionAgentChanged {
            agent: target.id.to_string(),
            previous: previous.id.to_string(),
        })
        .unwrap_or(Value::Null);
        handle
            .sequencer
            .publish_acked(Candidate::new(EventKind::SessionAgentChanged, payload))
            .await
            .map_err(|_| CommandFailure::Server(ServerError::SessionStopping))?;
        Ok(Done::data(json!({
            "agent": target.id,
            "previous": previous.id,
            "changed": true,
        })))
    }

    /// `/variant [name|off]`: shows the variant and the names on offer, or
    /// switches. `off` clears the variant; any other word is the exact name
    /// of an offered variant (`none` included, when the model offers it). A
    /// switch is checked against the offer, saved, and only then handed to
    /// the runtime, which applies it at the next turn boundary.
    async fn variant_command(
        &self,
        session_id: SessionId,
        handle: &SessionHandle,
        arguments: &str,
    ) -> Result<Done, CommandFailure> {
        // One selection for the whole command: the offer read below belongs to
        // this alias, and is only applied if the selection is still this one
        // once the command holds exclusivity.
        let selected = handle.selected();
        let mut offer = handle.current_offer();
        let mut words = arguments.split_whitespace();
        let Some(word) = words.next() else {
            let offered: Vec<&String> = offer.variants.keys().collect();
            return Ok(Done::data(json!({
                "variant": handle.status.selected_variant(),
                "offered": offered,
            })));
        };
        if words.next().is_some() {
            return Err(CommandFailure::Refused(
                "usage: /variant [name|off]".to_string(),
            ));
        }
        // An offer that was unknown when the session started is read again
        // now: the provider may be back.
        if word != CLEAR_WORD && !offer.known {
            match self
                .reread_offer(
                    &session_id.to_string(),
                    &handle.workspace,
                    &selected.alias,
                    selected.resolution.as_deref(),
                )
                .await
            {
                Ok(fresh) if fresh.known => {
                    offer = Arc::new(fresh);
                }
                // The alias now names another model: its variants are not
                // this session's, and nothing is changed.
                Err(e @ ServerError::ModelIdentityChanged { .. }) => {
                    return Err(CommandFailure::Refused(e.to_string()));
                }
                // Still unknown, or unreadable for now: the retryable
                // refusal below applies.
                Ok(_) | Err(_) => {}
            }
        }
        let chosen = if word == CLEAR_WORD {
            None
        } else if !offer.known && !offer.variants.contains_key(word) {
            return Err(CommandFailure::Refused(
                ServerError::VariantOfferUnknown {
                    alias: selected.alias.clone(),
                    variant: word.to_string(),
                }
                .to_string(),
            ));
        } else {
            exact_variant(&selected.alias, word, &offer.variants)
                .map_err(|e| CommandFailure::Refused(e.to_string()))?
        };
        let name = chosen.as_ref().map(|(name, _)| name.clone());
        // No compaction starts, and none is preparing, while this applies.
        let _exclusive = handle
            .status
            .selection
            .exclusive_unless_compacting()
            .await
            .map_err(|err| match err {
                SwitchError::Switching => CommandFailure::Refused(SWITCHING_ERROR.to_string()),
                _ => CommandFailure::Refused(COMPACTING_ERROR.to_string()),
            })?;
        // A model switch may have finished while the offer was being read: the
        // offer and the variant chosen from it belong to the old model.
        if handle.selected().revision != selected.revision {
            return Err(CommandFailure::Refused(MODEL_CHANGED_ERROR.to_string()));
        }
        if !Arc::ptr_eq(&offer, &handle.current_offer()) && offer.known {
            handle.replace_offer((*offer).clone());
        }
        self.history()
            .set_variant(session_id, chosen.as_ref())
            .await
            .map_err(|e| CommandFailure::Server(ServerError::History(e)))?;
        let offered: Vec<&String> = offer.variants.keys().collect();
        let now = handle.status.select_variant(chosen);
        // The gauge shows the variant that is selected, even when it applies
        // from the next turn.
        if let Some(snapshot) = handle.status.set_context_variant(name.clone()) {
            if let Ok(payload) = serde_json::to_value(&snapshot) {
                let _ = handle
                    .sequencer
                    .publish(Candidate::new(EventKind::ContextUpdated, payload));
            }
        }
        Ok(Done::data(json!({
            "variant": name,
            "offered": offered,
            "applies": if now { "now" } else { "next_turn" },
        })))
    }

    /// `/new [alias]`: a fresh session in the same workspace, with the
    /// alias given or this session's own. This one is left as it is.
    async fn new_conversation(
        &self,
        session_id: SessionId,
        handle: &SessionHandle,
        arguments: &str,
    ) -> Result<Done, CommandFailure> {
        let mut words = arguments.split_whitespace();
        let alias = words.next();
        if words.next().is_some() {
            return Err(CommandFailure::Refused("usage: /new [alias]".to_string()));
        }
        let selected = handle.selected();
        let current = selected.alias;
        let alias = alias.unwrap_or(&current).to_string();
        // The same alias keeps this session's variant; another alias starts
        // on its own default.
        let variant = if alias == current {
            match handle.status.selected_variant() {
                Some(name) => VariantChoice::Exact(name),
                None => VariantChoice::Clear,
            }
        } else {
            VariantChoice::Default
        };
        let profile = self
            .history()
            .meta(session_id)
            .await
            .map_err(|e| CommandFailure::Server(ServerError::History(e)))?
            .and_then(|meta| meta.profile);
        match self
            .create_session_with_variant(
                &handle.workspace,
                Some(&alias),
                profile.as_deref(),
                variant,
            )
            .await
        {
            Ok(id) => Ok(Done {
                data: json!({ "session_id": id.to_string(), "model": alias }),
                created: Some(id),
                published: false,
            }),
            Err(err @ ServerError::History(_)) => Err(CommandFailure::Server(err)),
            Err(err) => Err(CommandFailure::Refused(err.to_string())),
        }
    }
}

/// Why a command produced no data.
enum CommandFailure {
    /// The command's own error: reported in the result.
    Refused(String),
    /// Refused, and the runtime already published the result.
    Published(String),
    /// The session cannot take commands: the caller's error.
    Server(ServerError),
}

/// Parks a command run just before its result is published.
#[cfg(test)]
#[derive(Debug, Default)]
pub(super) struct Barrier {
    pub(super) reached: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
}

impl Service {
    /// Test seam: parks here when a barrier is armed.
    #[cfg(test)]
    async fn before_publish_hook(&self) {
        let barrier = self
            .publish_barrier
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        if let Some(barrier) = barrier {
            barrier.reached.notify_one();
            barrier.release.notified().await;
        }
    }

    #[cfg(not(test))]
    async fn before_publish_hook(&self) {}
}

/// What a successful command produced.
struct Done {
    data: Value,
    /// A session the command created, to close again if its result cannot
    /// be published.
    created: Option<SessionId>,
    /// The runtime already published the result.
    published: bool,
}

impl Done {
    fn data(data: Value) -> Self {
        Self {
            data,
            created: None,
            published: false,
        }
    }

    fn published(data: Value) -> Self {
        Self {
            published: true,
            ..Self::data(data)
        }
    }
}

impl Service {
    /// `/model`: the alias, what the saved resolution says about it, and a
    /// bounded view of the chat aliases a new session could use.
    fn model_data(&self, handle: &SessionHandle) -> Value {
        let selected = handle.selected();
        let alias = selected.alias;
        let mut data = match &selected.resolution {
            Some(r) => json!({
                "alias": alias,
                "model": r.model,
                "provider": r.provider,
                "window": r.bounds.context_window,
                "window_source": r.provenance.context_window,
                "max_input_tokens": r.bounds.max_input_tokens,
            }),
            None => json!({ "alias": alias }),
        };
        if let Ok(models) = self.list_models(&handle.workspace) {
            let mut names: Vec<String> = models.into_iter().map(|(alias, _)| alias).collect();
            names.sort();
            let count = names.len();
            names.truncate(MODEL_CATALOG_CAP);
            data["catalog"] = json!({
                "count": count,
                "aliases": names,
                "more": "GET /v1/models lists every alias",
            });
        }
        data
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::session::config::SessionConfig;

    /// A service over a temp directory with one chat alias on an
    /// unreachable provider: sessions are created but never run.
    fn service(dir: &Path) -> Service {
        service_with(dir, "http://127.0.0.1:1/v1", 128000)
    }

    fn service_with(dir: &Path, url: &str, window: u64) -> Service {
        std::fs::create_dir_all(dir.join("cfg")).expect("cfg dir");
        std::fs::write(
            dir.join("cfg/auth.json"),
            r#"{"mock":{"api_key":"test-key"}}"#,
        )
        .expect("auth");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                dir.join("cfg/auth.json"),
                std::fs::Permissions::from_mode(0o600),
            )
            .expect("chmod");
        }
        std::fs::write(
            dir.join("models.json"),
            format!(
                r#"{{"providers":{{"mock":{{"base_url":"{url}","api_key_env":"K",
            "headers":{{}},"connect_timeout_ms":2000,"request_timeout_ms":20000}}}},
            "models":{{"default":{{"provider":"mock","model":"m","supports_tools":true,
            "reasoning_history":"preserve","context_window":{window}}}}}}}"#
            ),
        )
        .expect("models");
        Service::new(SessionConfig {
            models_path: Some(dir.join("models.json")),
            config_dir: dir.join("cfg"),
            history_db: dir.join("history.db"),
            data_dir: dir.join("data"),
            rules_roots: Default::default(),
        })
        .expect("service")
    }

    fn arm(service: &Service) -> Arc<Barrier> {
        let barrier = Arc::new(Barrier::default());
        *service.publish_barrier.lock().unwrap() = Some(barrier.clone());
        barrier
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    // Given a /new parked after its work, just before its result is published
    // When the session is stopped meanwhile
    // Then the stop waits for the command, the command reports success with
    //      its result stored before session.end, and the new session is live
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_waits_for_a_new_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let service = service(tmp.path());
        let id = service
            .create_session(tmp.path(), None, None)
            .await
            .unwrap();
        let barrier = arm(&service);
        let run = tokio::spawn({
            let service = service.clone();
            async move { service.run_command(id, "new", "").await }
        });
        barrier.reached.notified().await;
        let stop = tokio::spawn({
            let service = service.clone();
            async move { service.close_session(id).await }
        });
        settle().await;
        assert!(!stop.is_finished(), "the stop waits for the command");

        barrier.release.notify_one();
        let out = run.await.unwrap().expect("the command ran");
        assert!(out.ok, "{out:?}");
        stop.await.unwrap().expect("stop");

        service.history().flush().await.unwrap();
        let kinds = stored_kinds(&service, id).await;
        let result = kinds.iter().position(|k| k == "command.result").unwrap();
        let end = kinds.iter().position(|k| k == "session.end").unwrap();
        assert!(result < end, "{kinds:?}");
        let fresh: SessionId = out.data.unwrap()["session_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(service.handle(fresh).is_some(), "the new session is live");
    }

    // Given a /new parked before its result is published
    // When the session is deleted meanwhile
    // Then the delete waits, and the command is reported only once its
    //      result is on the stream
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_waits_for_a_new_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let service = service(tmp.path());
        let id = service
            .create_session(tmp.path(), None, None)
            .await
            .unwrap();
        let barrier = arm(&service);
        let run = tokio::spawn({
            let service = service.clone();
            async move { service.run_command(id, "new", "").await }
        });
        barrier.reached.notified().await;
        let delete = tokio::spawn({
            let service = service.clone();
            async move { service.delete(id).await }
        });
        settle().await;
        assert!(!delete.is_finished(), "the delete waits for the command");

        barrier.release.notify_one();
        assert!(run.await.unwrap().expect("the command ran").ok);
        delete.await.unwrap().expect("delete");
        assert!(service.handle(id).is_none());
    }

    // Given a /new parked before its result is published
    // When the session's sequencer closes on its own meanwhile
    // Then the command fails and the session it created is closed again
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_whose_result_cannot_be_published_closes_what_it_created() {
        let tmp = tempfile::tempdir().unwrap();
        let service = service(tmp.path());
        let id = service
            .create_session(tmp.path(), None, None)
            .await
            .unwrap();
        let handle = service.handle(id).unwrap();
        let barrier = arm(&service);
        let run = tokio::spawn({
            let service = service.clone();
            async move { service.run_command(id, "new", "").await }
        });
        barrier.reached.notified().await;
        handle.sequencer.close_session("closed on its own").unwrap();
        handle.sequencer.closed().await;
        barrier.release.notify_one();

        let err = run.await.unwrap().unwrap_err();
        assert!(matches!(err, ServerError::SessionStopping), "{err:?}");
        assert_eq!(service.session_count(), 1, "the created session is closed");
    }

    /// A provider that answers its one request with `text`.
    async fn one_answer_provider(text: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let chunk = json!({"choices": [{"index": 0, "finish_reason": "stop",
            "delta": {"content": text}}]})
        .to_string();
        let response =
            crate::provider::test_support::http_sse(&crate::provider::test_support::sse_body(&[
                &chunk,
            ]));
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 1 << 20];
            let mut seen = 0;
            // The request is complete once its JSON body closes.
            while let Ok(n) = sock.read(&mut buf[seen..]).await {
                if n == 0 {
                    break;
                }
                seen += n;
                if buf[..seen].ends_with(b"}") {
                    break;
                }
            }
            let _ = sock.write_all(&response).await;
            let _ = sock.shutdown().await;
        });
        format!("http://{addr}/v1")
    }

    /// A conversation of four long turns on a window that it overruns.
    async fn seeded_long(service: &Service, dir: &Path) -> SessionId {
        use crate::provider::openai::ChatMessage;
        let id = service.create_session(dir, None, None).await.unwrap();
        service.close_session(id).await.unwrap();
        service.history().flush().await.unwrap();
        let turns: Vec<ChatMessage> = (0..4)
            .flat_map(|n| {
                [
                    ChatMessage::User {
                        content: format!("request {n}"),
                    },
                    ChatMessage::Assistant {
                        content: Some(format!("answer {n} {}", "word ".repeat(1200))),
                        reasoning_content: None,
                        tool_calls: vec![],
                    },
                ]
            })
            .collect();
        service.history().save_transcript(id, turns);
        service.history().flush().await.unwrap();
        service
            .create_or_resume_session(dir, None, None, Some(id))
            .await
            .unwrap();
        id
    }

    // Given an idle /compact that committed, parked before its result is
    //       published
    // When the session is stopped meanwhile
    // Then the stop does not fence the stream before the result: the result
    //      is stored before session.end and the command reports success
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_after_a_commit_never_fences_out_the_compact_result() {
        let tmp = tempfile::tempdir().unwrap();
        let url = one_answer_provider("Goal: finish.").await;
        let service = service_with(tmp.path(), &url, 20_000);
        let id = seeded_long(&service, tmp.path()).await;
        let barrier = service.handle(id).unwrap().status.arm_result_barrier();

        let run = tokio::spawn({
            let service = service.clone();
            async move { service.run_command(id, "compact", "").await }
        });
        barrier.reached.notified().await;
        let stop = tokio::spawn({
            let service = service.clone();
            async move { service.close_session(id).await }
        });
        settle().await;

        barrier.release.notify_one();
        let out = run.await.unwrap().expect("the command reports");
        assert!(out.ok, "{out:?}");
        assert_eq!(out.data.as_ref().unwrap()["applies"], "now");
        stop.await.unwrap().expect("stop");

        service.history().flush().await.unwrap();
        let kinds = stored_kinds(&service, id).await;
        let result = kinds.iter().position(|k| k == "command.result").unwrap();
        let compacted = kinds.iter().position(|k| k == "context.compacted").unwrap();
        let end = kinds.iter().rposition(|k| k == "session.end").unwrap();
        assert!(compacted < result && result < end, "{kinds:?}");
        assert_eq!(
            kinds.iter().filter(|k| *k == "command.result").count(),
            1,
            "{kinds:?}"
        );
    }

    async fn stored_kinds(service: &Service, id: SessionId) -> Vec<String> {
        match service.subscribe(id, None).await {
            Ok(sub) => match sub.replay {
                crate::bus::ResumeOutcome::Replay(events) => {
                    events.into_iter().map(|e| e.kind).collect()
                }
                other => panic!("expected replay, got {other:?}"),
            },
            Err(err) => panic!("subscribe: {err}"),
        }
    }
}
