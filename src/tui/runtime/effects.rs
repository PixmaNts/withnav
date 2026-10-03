//! Runs the effects the reducer asks for. HTTP effects run in spawned tasks
//! that report back as tagged `ClientEvent`s; nothing here touches the
//! terminal, so tests drive it without one.
use std::future::Future;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::tui::{
    client::Client,
    input::Editor,
    protocol::{ClientEvent, Effect, Generation, SubmitRequest, SubmitResponse},
    stream,
};

/// What the loop does after an effect ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit,
}

/// The values an effect is tagged with when it runs.
#[derive(Debug, Clone)]
pub struct Context {
    /// The state's current generation: results of an older one are dropped
    /// by the reducer.
    pub generation: Generation,
    /// The conversation `SendAt` and `Stop` talk to, if one exists.
    pub session_id: Option<String>,
    /// `State.workspace`: the single source for model, session-list and resume requests.
    pub workspace: String,
    /// Where the user settings are saved; `None` saves nothing.
    pub config_dir: Option<std::path::PathBuf>,
}

struct ActiveStream {
    cancel: CancellationToken,
    generation: Generation,
    child_session_id: Option<String>,
}

/// Owns the client, the event channel and the live event streams, one per open tab.
pub struct Executor {
    client: Client,
    tx: mpsc::Sender<ClientEvent>,
    streams: Vec<ActiveStream>,
}

impl Executor {
    pub fn new(client: Client, tx: mpsc::Sender<ClientEvent>) -> Self {
        Self {
            client,
            tx,
            streams: Vec::new(),
        }
    }

    /// Cancels every stream whose generation is not in `live`: a closed tab, or a tab that moved
    /// to another conversation.
    pub fn retire_streams(&mut self, live: &[Generation]) {
        self.streams.retain(|s| {
            let keep = live.contains(&s.generation);
            if !keep {
                s.cancel.cancel();
            }
            keep
        });
    }

    /// Stops the stream of `generation`, if any.
    pub fn cancel_stream(&mut self, generation: &Generation) {
        self.retire_streams_where(|g| g == generation);
    }

    /// Stops every stream.
    pub fn cancel_streams(&mut self) {
        self.retire_streams_where(|_| true);
    }

    fn retire_streams_where(&mut self, stop: impl Fn(&Generation) -> bool) {
        self.streams.retain(|s| {
            let gone = stop(&s.generation);
            if gone {
                s.cancel.cancel();
            }
            !gone
        });
    }

    /// A sender into the loop's event channel, for tests that inject events.
    pub fn sender(&self) -> mpsc::Sender<ClientEvent> {
        self.tx.clone()
    }

    pub fn has_stream(&self) -> bool {
        !self.streams.is_empty()
    }

    /// How many streams are live: at most one per open tab.
    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    pub fn execute(&mut self, effect: Effect, ctx: &Context, editor: &mut Editor) -> Flow {
        let generation = ctx.generation.clone();
        match effect {
            Effect::UnwatchChild(child_session_id) => {
                self.streams.retain(|s| {
                    let gone = s.child_session_id.as_ref() == Some(&child_session_id);
                    if gone {
                        s.cancel.cancel();
                    }
                    !gone
                });
            }
            Effect::WatchChild {
                child_session_id,
                generation,
                ..
            } => {
                if self
                    .streams
                    .iter()
                    .any(|s| s.child_session_id.as_ref() == Some(&child_session_id))
                {
                    return Flow::Continue;
                }
                let cancel = CancellationToken::new();
                let mut events = stream::subscribe(
                    self.client.clone(),
                    child_session_id.clone(),
                    None,
                    generation.clone(),
                    cancel.clone(),
                );
                let (tx, stop, child, tag) = (
                    self.tx.clone(),
                    cancel.clone(),
                    child_session_id.clone(),
                    generation.clone(),
                );
                tokio::spawn(async move {
                    loop {
                        let next = tokio::select! { _ = stop.cancelled() => break, next = events.next() => next };
                        match next {
                            Some(ClientEvent::Envelope { envelope, .. }) => {
                                if tx
                                    .send(ClientEvent::ChildEnvelope {
                                        generation: tag.clone(),
                                        child_session_id: child.clone(),
                                        envelope,
                                    })
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            None => break,
                            _ => {}
                        }
                    }
                });
                self.streams.push(ActiveStream {
                    cancel,
                    generation,
                    child_session_id: Some(child_session_id),
                });
            }
            Effect::FetchModels => {
                let (client, workspace) = (self.client.clone(), ctx.workspace.clone());
                self.spawn_event(async move {
                    ClientEvent::Models {
                        generation,
                        result: client.models(&workspace).await,
                    }
                });
            }
            Effect::ListSessions => {
                let (client, workspace) = (self.client.clone(), ctx.workspace.clone());
                self.spawn_event(async move {
                    ClientEvent::Sessions {
                        generation,
                        result: client.sessions(&workspace).await,
                    }
                });
            }
            Effect::Create(request) => {
                let client = self.client.clone();
                self.spawn_event(async move {
                    ClientEvent::Created {
                        generation,
                        result: client.create(request).await,
                    }
                });
            }
            Effect::Resume(id) => {
                let (client, workspace) = (self.client.clone(), ctx.workspace.clone());
                self.spawn_event(async move {
                    ClientEvent::Resumed {
                        generation,
                        result: client.resume(&workspace, &id).await,
                    }
                });
            }
            Effect::FetchMetadata(id) => {
                let client = self.client.clone();
                self.spawn_event(async move {
                    ClientEvent::Metadata {
                        generation,
                        result: client.summary(&id).await,
                    }
                });
            }
            Effect::FetchChildTerminalMetadata(id) => {
                let client = self.client.clone();
                self.spawn_event(async move {
                    let mut result = Err("could not refresh the subagent's terminal state".into());
                    for attempt in 0..4 {
                        result = match tokio::time::timeout(
                            std::time::Duration::from_secs(1),
                            client.summary(&id),
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => {
                                Err("timed out refreshing the subagent's terminal state".into())
                            }
                        };
                        if result.as_ref().is_ok_and(|m| {
                            crate::tui::state::ChildStatus::from_wire(
                                m.child_status.as_deref().unwrap_or(""),
                            )
                            .terminal()
                        }) {
                            break;
                        }
                        if attempt < 3 {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }
                    ClientEvent::Metadata { generation, result }
                });
            }
            Effect::FetchContext { session_id, epoch } => {
                let client = self.client.clone();
                self.spawn_event(async move {
                    ClientEvent::Context {
                        generation,
                        epoch,
                        result: client.context(&session_id).await,
                    }
                });
            }
            Effect::Subscribe { session_id, cursor } => {
                self.subscribe(session_id, cursor, generation);
            }
            Effect::Unsubscribe => self.cancel_stream(&generation),
            // Legacy variant (never emitted by the reducer): no editor revision to echo.
            Effect::Send(request) => self.send(request, 0, ctx),
            Effect::SendAt { request, revision } => self.send(request, revision, ctx),
            Effect::AnswerPermission {
                session_id,
                request_id,
                answer,
            } => {
                let client = self.client.clone();
                self.spawn_event(async move {
                    ClientEvent::PermissionAnswered {
                        generation,
                        result: client
                            .answer_permission(&session_id, &request_id, answer)
                            .await,
                        request_id,
                    }
                });
            }
            Effect::FetchCommands => {
                let client = self.client.clone();
                self.spawn_event(async move {
                    ClientEvent::Commands {
                        generation,
                        result: client.commands().await,
                    }
                });
            }
            Effect::RunCommand {
                session_id,
                request,
                revision,
                token,
            } => {
                let client = self.client.clone();
                self.spawn_event(async move {
                    ClientEvent::CommandRan {
                        generation,
                        name: request.name.clone(),
                        revision,
                        token,
                        result: client.run_command(&session_id, request).await,
                    }
                });
            }
            Effect::Stop(id) => {
                let client = self.client.clone();
                self.spawn_event(async move {
                    ClientEvent::Stopped {
                        generation,
                        result: client.stop(&id).await,
                    }
                });
            }
            Effect::ClearEditor { revision } => {
                editor.clear_if_revision(revision);
            }
            Effect::ControlChild {
                parent_session_id,
                child_session_id,
                control,
            } => {
                let client = self.client.clone();
                self.spawn_event(async move {
                    let result = match control {
                        crate::tui::protocol::ChildControl::Stop => {
                            client.stop(&child_session_id).await.and_then(|response| {
                                if response.accepted {
                                    Ok(())
                                } else {
                                    Err(format!("stop was refused: {}", response.actor_state))
                                }
                            })
                        }
                        crate::tui::protocol::ChildControl::Continue => {
                            client
                                .continue_child(&parent_session_id, &child_session_id)
                                .await
                        }
                    };
                    ClientEvent::ChildControlled {
                        generation,
                        child_session_id,
                        control,
                        result,
                    }
                });
            }
            Effect::SetEditor { text, revision } => editor.restore(&text, revision),
            Effect::SaveTheme(theme) => {
                // A few bytes written once per pick: done inline. A failure is shown, not fatal:
                // the theme stays in effect and picking it again retries the save.
                if let Some(dir) = &ctx.config_dir {
                    if let Err(err) = crate::settings::save_tui_theme(dir, theme.id()) {
                        tracing::warn!("cannot save the theme: {err}");
                        let message = format!(
                            "could not confirm theme {} was saved; pick it again to retry: {err}",
                            theme.name()
                        );
                        self.spawn_event(async move {
                            ClientEvent::SettingsNotSaved {
                                generation,
                                message,
                            }
                        });
                    }
                }
            }
            Effect::ReadGit => {
                let workspace = std::path::PathBuf::from(&ctx.workspace);
                self.spawn_event(async move {
                    ClientEvent::Git {
                        generation,
                        status: crate::tui::git::read(&workspace).await,
                    }
                });
            }
            // Applied by the runtime before it reaches here.
            Effect::ShowVariants { .. } => {}
            Effect::Quit => return Flow::Quit,
        }
        Flow::Continue
    }

    fn send(&mut self, request: SubmitRequest, revision: u64, ctx: &Context) {
        let generation = ctx.generation.clone();
        let Some(id) = ctx.session_id.clone() else {
            let event = submit_event(
                generation,
                revision,
                Err(SubmitFailure::Rejected(
                    "no conversation to send the message to".into(),
                )),
            );
            self.spawn_event(async move { event });
            return;
        };
        let client = self.client.clone();
        self.spawn_event(async move {
            submit_event(
                generation,
                revision,
                client.send(&id, request).await.map_err(classify_send_error),
            )
        });
    }

    /// Starts the stream of the tab tagged `generation`, replacing the one it had.
    fn subscribe(&mut self, session_id: String, cursor: Option<u64>, generation: Generation) {
        self.streams.retain(|s| {
            let gone = s.generation == generation && s.child_session_id.is_none();
            if gone {
                s.cancel.cancel();
            }
            !gone
        });
        let cancel = CancellationToken::new();
        let mut events = stream::subscribe(
            self.client.clone(),
            session_id,
            cursor,
            generation.clone(),
            cancel.clone(),
        );
        let (tx, stop) = (self.tx.clone(), cancel.clone());
        tokio::spawn(async move {
            loop {
                let next = tokio::select! {
                    _ = stop.cancelled() => break,
                    next = events.next() => next,
                };
                match next {
                    Some(event) => {
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        });
        self.streams.push(ActiveStream {
            cancel,
            generation,
            child_session_id: None,
        });
    }

    fn spawn_event(&self, work: impl Future<Output = ClientEvent> + Send + 'static) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(work.await).await;
        });
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        self.cancel_streams();
    }
}

/// Why a message POST failed, as far as the loop cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SubmitFailure {
    /// The message was not delivered; the draft can be sent again.
    Rejected(String),
    /// The request may have reached the server; the draft stays and the user
    /// must check the conversation before sending again.
    Uncertain(String),
}

/// The one place that adapts the client's send error to `SubmitFailure`.
pub(super) fn classify_send_error(err: crate::tui::client::SendError) -> SubmitFailure {
    use crate::tui::client::SendError;
    match err {
        SendError::Rejected(m) => SubmitFailure::Rejected(m),
        SendError::Uncertain(m) => SubmitFailure::Uncertain(m),
    }
}

/// Turns the outcome of a message POST into the event the reducer gets:
/// a rejection is a failed `Submitted`, an uncertain delivery is
/// `DeliveryUncertain` (the draft is never retried automatically).
pub(super) fn submit_event(
    generation: Generation,
    revision: u64,
    result: Result<SubmitResponse, SubmitFailure>,
) -> ClientEvent {
    match result {
        Ok(_) | Err(SubmitFailure::Rejected(_)) => ClientEvent::Submitted {
            generation,
            revision,
            result: result.map_err(|e| match e {
                SubmitFailure::Rejected(m) | SubmitFailure::Uncertain(m) => m,
            }),
        },
        Err(SubmitFailure::Uncertain(_)) => ClientEvent::DeliveryUncertain { generation },
    }
}
