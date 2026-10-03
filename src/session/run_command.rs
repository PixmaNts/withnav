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

/// How many alias names `/model` lists; the rest is `GET /v1/models`.
const MODEL_CATALOG_CAP: usize = 50;

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
    /// included: the stop endpoint still ends the session).
    ///
    /// The run holds the lifecycle gate from the session check to the
    /// acknowledged publication of its result, so a stop or delete of the
    /// session waits for the command and never lands between its work and
    /// its result. When the result cannot be published (the session closed
    /// on its own) the command fails and a session it created is closed
    /// again.
    pub async fn run_command(
        &self,
        session_id: SessionId,
        name: &str,
        arguments: &str,
    ) -> Result<CommandOutcome, ServerError> {
        let _gate = self.lifecycle_gate().await;
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
                self.dispatch(session_id, &handle, spec, arguments.trim())
                    .await
            }
            None => Err(CommandFailure::Refused(commands::unknown_error(&name))),
        };
        let (ok, data, error) = match result {
            Ok(done) => {
                created = done.created;
                (true, Some(CommandResult::bound_data(done.data)), None)
            }
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
    ) -> Result<Done, CommandFailure> {
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
        if spec.args_hint.is_empty() && !arguments.is_empty() {
            return Err(CommandFailure::Refused(match spec.name {
                "model" => "switching model inside a conversation is not available yet: \
                            use /new <alias>"
                    .to_string(),
                name => format!("usage: /{name}"),
            }));
        }
        match spec.name {
            "help" => Ok(Done::data(json!({ "commands": commands::COMMANDS }))),
            "model" => Ok(Done::data(self.model_data(handle))),
            "new" => self.new_conversation(session_id, handle, arguments).await,
            // The stop itself is done by `run_command` after the result is
            // published: the stream ends with the session.
            "stop" => Ok(Done::data(json!({ "stopped": true }))),
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
    ) -> Result<Done, CommandFailure> {
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
            Ok(id) => Ok(Done {
                data: json!({ "session_id": id.to_string(), "model": alias }),
                created: Some(id),
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
}

impl Done {
    fn data(data: Value) -> Self {
        Self { data, created: None }
    }
}

impl Service {
    /// `/model`: the alias, what the saved resolution says about it, and a
    /// bounded view of the chat aliases a new session could use.
    fn model_data(&self, handle: &SessionHandle) -> Value {
        let mut data = match &handle.resolution {
            Some(r) => json!({
                "alias": handle.model,
                "model": r.model,
                "provider": r.provider,
                "window": r.bounds.context_window,
                "window_source": r.provenance.context_window,
                "max_input_tokens": r.bounds.max_input_tokens,
            }),
            None => json!({ "alias": handle.model }),
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
            r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1","api_key_env":"K",
            "headers":{},"connect_timeout_ms":100,"request_timeout_ms":100}},
            "models":{"default":{"provider":"mock","model":"m","supports_tools":true,
            "reasoning_history":"preserve","context_window":1000}}}"#,
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
