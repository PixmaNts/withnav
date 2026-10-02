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
    /// The conversation `Send` and `Stop` talk to, if one exists.
    pub session_id: Option<String>,
}

struct ActiveStream {
    cancel: CancellationToken,
    generation: Generation,
}

/// Owns the client, the event channel and the one live event stream.
pub struct Executor {
    client: Client,
    workspace: String,
    tx: mpsc::Sender<ClientEvent>,
    stream: Option<ActiveStream>,
    submit_revision: u64,
}

impl Executor {
    pub fn new(client: Client, workspace: String, tx: mpsc::Sender<ClientEvent>) -> Self {
        Self {
            client,
            workspace,
            tx,
            stream: None,
            submit_revision: 0,
        }
    }

    /// Remembers the editor revision of the text being submitted. The
    /// acknowledgement clears the editor only if it still has this revision,
    /// whatever was typed while the request was in flight.
    pub fn note_submit(&mut self, revision: u64) {
        self.submit_revision = revision;
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
                let (client, workspace) = (self.client.clone(), self.workspace.clone());
                self.spawn_event(async move {
                    ClientEvent::Models {
                        generation,
                        result: client.models(&workspace).await,
                    }
                });
            }
            Effect::ListSessions => {
                let (client, workspace) = (self.client.clone(), self.workspace.clone());
                self.spawn_event(async move {
                    ClientEvent::Sessions {
                        generation,
                        result: client.sessions(&workspace).await,
                    }
                });
            }
            Effect::Create(mut request) => {
                if request.workspace.is_empty() {
                    request.workspace = self.workspace.clone();
                }
                let client = self.client.clone();
                self.spawn_event(async move {
                    ClientEvent::Created {
                        generation,
                        result: client.create(request).await,
                    }
                });
            }
            Effect::Resume(id) => {
                let (client, workspace) = (self.client.clone(), self.workspace.clone());
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
            Effect::Send(request) => self.send(request, ctx),
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
            Effect::Quit => return Flow::Quit,
        }
        Flow::Continue
    }

    fn send(&mut self, request: SubmitRequest, ctx: &Context) {
        let generation = ctx.generation.clone();
        let revision = self.submit_revision;
        let Some(id) = ctx.session_id.clone() else {
            let event = submit_event(
                generation,
                revision,
                Err::<SubmitResponse, _>("no conversation to send the message to"),
            );
            self.spawn_event(async move { event });
            return;
        };
        let client = self.client.clone();
        self.spawn_event(async move {
            submit_event(generation, revision, client.send(&id, request).await)
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

/// The single place that turns the outcome of a message POST into an event.
///
/// The client reports every failure as a string for now, so all of them map
/// to a plain failed submission and the draft stays. When `Client::send`
/// returns a typed error, map its "delivery uncertain" variant to
/// `ClientEvent::DeliveryUncertain { generation }` here and keep the
/// rejected variant as `Submitted { result: Err(..) }`.
fn submit_event(
    generation: Generation,
    revision: u64,
    result: Result<SubmitResponse, impl std::fmt::Display>,
) -> ClientEvent {
    ClientEvent::Submitted {
        generation,
        revision,
        result: result.map_err(|e| e.to_string()),
    }
}
