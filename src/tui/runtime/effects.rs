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
}

/// Owns the client, the event channel and the one live event stream.
pub struct Executor {
    client: Client,
    tx: mpsc::Sender<ClientEvent>,
    stream: Option<ActiveStream>,
}

impl Executor {
    pub fn new(client: Client, tx: mpsc::Sender<ClientEvent>) -> Self {
        Self {
            client,
            tx,
            stream: None,
        }
    }

    /// Cancels the live stream if it belongs to another generation than
    /// `generation`.
    pub fn retire_stale_stream(&mut self, generation: &Generation) {
        if self
            .stream
            .as_ref()
            .is_some_and(|s| &s.generation != generation)
        {
            self.cancel_stream();
        }
    }

    /// Stops the live stream, if any.
    pub fn cancel_stream(&mut self) {
        if let Some(active) = self.stream.take() {
            active.cancel.cancel();
        }
    }

    /// A sender into the loop's event channel, for tests that inject events.
    pub fn sender(&self) -> mpsc::Sender<ClientEvent> {
        self.tx.clone()
    }

    pub fn has_stream(&self) -> bool {
        self.stream.is_some()
    }

    pub fn execute(&mut self, effect: Effect, ctx: &Context, editor: &mut Editor) -> Flow {
        let generation = ctx.generation.clone();
        match effect {
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
            Effect::Subscribe { session_id, cursor } => {
                self.subscribe(session_id, cursor, generation);
            }
            Effect::Unsubscribe => self.cancel_stream(),
            // Legacy variant (never emitted by the reducer): no editor revision to echo.
            Effect::Send(request) => self.send(request, 0, ctx),
            Effect::SendAt { request, revision } => self.send(request, revision, ctx),
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

    fn subscribe(&mut self, session_id: String, cursor: Option<u64>, generation: Generation) {
        self.cancel_stream();
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
        self.stream = Some(ActiveStream { cancel, generation });
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
        self.cancel_stream();
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
