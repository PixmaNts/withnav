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
    request: u64,
    saves: mpsc::UnboundedSender<Box<dyn FnOnce() -> Option<ClientEvent> + Send>>,
    /// [`SAVE_WAIT`]; tests shorten it.
    save_wait: std::time::Duration,
}

impl Executor {
    pub fn new(client: Client, tx: mpsc::Sender<ClientEvent>) -> Self {
        let (saves, mut pending) =
            mpsc::unbounded_channel::<Box<dyn FnOnce() -> Option<ClientEvent> + Send>>();
        let events = tx.clone();
        tokio::spawn(async move {
            while let Some(save) = pending.recv().await {
                match tokio::task::spawn_blocking(save).await {
                    Ok(Some(event)) => {
                        let _ = events.send(event).await;
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!("settings save worker failed: {error}"),
                }
            }
        });
        Self {
            client,
            tx,
            streams: Vec::new(),
            request: 0,
            saves,
            save_wait: SAVE_WAIT,
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

    #[cfg(test)]
    pub async fn flush_saves(&self) {
        self.save_barrier().unwrap().await.unwrap();
    }

    fn save_barrier(&self) -> Result<tokio::sync::oneshot::Receiver<()>, String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.saves
            .send(Box::new(move || {
                let _ = tx.send(());
                None
            }))
            .map_err(|_| "settings save worker stopped".to_string())?;
        Ok(rx)
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
            Effect::FetchConfig(request) => {
                let barrier = self.save_barrier();
                let deadline = self.save_wait;
                let client = self.client.clone();
                let workspace = ctx.workspace.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = match wait_for_saves(barrier, deadline).await {
                        Ok(()) => client.settings(&workspace).await,
                        Err(error) => Err(error),
                    };
                    let _ = tx
                        .send(ClientEvent::ConfigLoaded {
                            generation,
                            request,
                            result,
                        })
                        .await;
                });
            }
            Effect::WriteConfig(request) => {
                let barrier = self.save_barrier();
                let deadline = self.save_wait;
                let client = self.client.clone();
                let workspace = ctx.workspace.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = match wait_for_saves(barrier, deadline).await {
                        Ok(()) => client.write_setting(&workspace, &request).await,
                        Err(error) => Err(crate::tui::config::SettingsError::Other(error)),
                    };
                    let _ = tx
                        .send(ClientEvent::ConfigWritten {
                            generation,
                            key: request.key,
                            value: request.value,
                            result,
                        })
                        .await;
                });
            }
            Effect::TestGateway(name) => {
                let client = self.client.clone();
                let workspace = ctx.workspace.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = client.test_gateway(&workspace, &name).await;
                    let _ = tx
                        .send(ClientEvent::GatewayTested {
                            generation,
                            name,
                            result,
                        })
                        .await;
                });
            }
            Effect::ApplyConfigTheme(_) | Effect::LeaveConfig => {}
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
                self.request += 1;
                let request = self.request;
                let (client, workspace) = (self.client.clone(), ctx.workspace.clone());
                self.spawn_event(async move {
                    ClientEvent::Models {
                        workspace: workspace.clone(),
                        request,
                        generation,
                        result: client.models(&workspace).await,
                    }
                });
            }
            Effect::FetchAgents(session_id) => {
                self.request += 1;
                let request = self.request;
                let (client, workspace) = (self.client.clone(), ctx.workspace.clone());
                self.spawn_event(async move {
                    let result = client.agents(&workspace, session_id.as_deref()).await;
                    ClientEvent::Agents {
                        workspace: workspace.clone(),
                        request,
                        generation,
                        session_id,
                        result,
                    }
                });
            }
            Effect::FetchLayers => {
                self.request += 1;
                let request = self.request;
                let (client, workspace) = (self.client.clone(), ctx.workspace.clone());
                self.spawn_event(async move {
                    ClientEvent::Layers {
                        workspace: workspace.clone(),
                        request,
                        generation,
                        result: client.layers(&workspace).await,
                    }
                });
            }
            Effect::FetchSkills(session_id) => {
                let client = self.client.clone();
                self.spawn_event(async move {
                    ClientEvent::Skills {
                        generation,
                        result: client.skills(&session_id).await,
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
            Effect::SetBypass {
                session_id,
                enabled,
            } => {
                let client = self.client.clone();
                self.spawn_event(async move {
                    ClientEvent::BypassSet {
                        generation,
                        result: client.set_bypass(&session_id, enabled).await,
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
                pause_token,
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
                                .continue_child_with_token(
                                    &parent_session_id,
                                    &child_session_id,
                                    pause_token,
                                )
                                .await
                        }
                    };
                    ClientEvent::ChildControlled {
                        generation,
                        child_session_id,
                        control,
                        result,
                        pause_token,
                    }
                });
            }
            Effect::SetEditor { text, revision } => editor.restore(&text, revision),
            Effect::SaveTheme(theme) => {
                // Saves run off-loop in pick order. A failure is shown, not fatal:
                // the theme stays in effect and picking it again retries the save.
                if let Some(dir) = &ctx.config_dir {
                    let dir = dir.clone();
                    let _ = self.saves.send(Box::new(move || {
                        if let Err(err) = crate::settings::save_tui_theme(&dir, theme.id()) {
                            tracing::warn!("cannot save the theme: {err}");
                            let message = format!(
                            "could not confirm theme {} was saved; pick it again to retry: {err}",
                            theme.name()
                        );
                            return Some(ClientEvent::SettingsNotSaved {
                                generation,
                                message,
                            });
                        }
                        None
                    }));
                }
            }
            Effect::SaveFavoriteModels(names) => {
                // Same ordered off-loop worker as the theme; a failure is shown, the list in
                // memory stays and the next toggle saves it again.
                if let Some(dir) = &ctx.config_dir {
                    let dir = dir.clone();
                    let _ = self.saves.send(Box::new(move || {
                        if let Err(err) = crate::settings::save_tui_favorite_models(&dir, &names) {
                            tracing::warn!("cannot save the favorite models: {err}");
                            let message = format!("could not save the favorite models: {err}");
                            return Some(ClientEvent::SettingsNotSaved {
                                generation,
                                message,
                            });
                        }
                        None
                    }));
                }
            }
            Effect::ReadGit => {
                let workspace = std::path::PathBuf::from(&ctx.workspace);
                self.spawn_event(async move {
                    ClientEvent::Git {
                        generation,
                        status: crate::workspace::git::read(&workspace).await,
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

/// How long a Settings request waits for theme or favorites saves queued before it. Such a save
/// can itself wait on `config.json.lock` held by another Navai process; past this the request
/// fails like any unreachable server (staged changes kept, ctrl+s retries) instead of leaving
/// `/config` loading or writing forever. It never goes ahead without the barrier: that would let
/// an older queued theme land after a newer `/config` write.
const SAVE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

async fn wait_for_saves(
    barrier: Result<tokio::sync::oneshot::Receiver<()>, String>,
    deadline: std::time::Duration,
) -> Result<(), String> {
    match tokio::time::timeout(deadline, barrier?).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err("settings save worker stopped".to_string()),
        Err(_) => Err(format!(
            "an earlier theme or favorites save is still waiting for config.json after {} s; \
             try again",
            deadline.as_secs()
        )),
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;
    use crate::tui::{config::WriteRequest, theme::ThemeId};
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn queued_theme_save_precedes_settings_get_and_put_value_is_final() {
        let dir = tempfile::tempdir().unwrap();
        let saved_dir = dir.path().to_path_buf();
        let (observed, mut requests) = mpsc::unbounded_channel();
        let app = axum::Router::new().route(
            "/v1/settings",
            axum::routing::get({
                let dir = saved_dir.clone();
                let observed = observed.clone();
                move || {
                    let dir = dir.clone();
                    let observed = observed.clone();
                    async move {
                        let theme = crate::settings::load(&dir).unwrap().tui_theme().map(str::to_owned);
                        observed.send(("GET", theme)).unwrap();
                        axum::Json(json!({"revision":"read", "paths":{"config":"config.json","models":null,"agents":[]},"fields":[],"gateways":[]}))
                    }
                }
            }).put({
                let dir = saved_dir.clone();
                move |axum::Json(request): axum::Json<serde_json::Value>| {
                    let dir = dir.clone();
                    let observed = observed.clone();
                    async move {
                        let theme = crate::settings::load(&dir).unwrap().tui_theme().map(str::to_owned);
                        observed.send(("PUT", theme)).unwrap();
                        crate::settings::save_tui_theme(&dir, request["value"].as_str().unwrap()).unwrap();
                        axum::Json(json!({"revision":"written", "paths":{"config":"config.json","models":null,"agents":[]},"fields":[],"gateways":[]}))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (tx, mut events) = mpsc::channel(10);
        let mut executor = Executor::new(Client::new(url, None), tx);
        let ctx = Context {
            generation: Generation(0),
            session_id: None,
            workspace: "/ws".into(),
            config_dir: Some(saved_dir.clone()),
        };
        let mut editor = Editor::default();
        for (effect, method, old_theme) in [
            (Effect::FetchConfig(1), "GET", ThemeId::AyuLight),
            (
                Effect::WriteConfig(WriteRequest {
                    revision: "read".into(),
                    key: "tui.theme".into(),
                    value: json!("nord-dark"),
                }),
                "PUT",
                ThemeId::NordLight,
            ),
        ] {
            let (release, blocked) = std::sync::mpsc::channel();
            let (started, ready) = tokio::sync::oneshot::channel();
            executor
                .saves
                .send(Box::new(move || {
                    started.send(()).unwrap();
                    blocked.recv().unwrap();
                    None
                }))
                .unwrap();
            ready.await.unwrap();
            executor.execute(Effect::SaveTheme(old_theme), &ctx, &mut editor);
            executor.execute(effect, &ctx, &mut editor);
            let premature_request =
                tokio::time::timeout(Duration::from_millis(50), requests.recv()).await;
            release.send(()).unwrap();
            assert!(
                premature_request.is_err(),
                "HTTP must wait for queued saves"
            );
            let (actual_method, theme) =
                tokio::time::timeout(Duration::from_secs(10), requests.recv())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(actual_method, method);
            assert_eq!(theme.as_deref(), Some(old_theme.id()));
            let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .unwrap()
                .unwrap();
            match event {
                ClientEvent::ConfigLoaded {
                    request: 1,
                    result: Ok(_),
                    ..
                } if method == "GET" => {}
                ClientEvent::ConfigWritten { result: Ok(_), .. } if method == "PUT" => {}
                other => panic!("unexpected settings result: {other:?}"),
            }
        }
        executor.flush_saves().await;
        assert_eq!(
            crate::settings::load(&saved_dir).unwrap().tui_theme(),
            Some("nord-dark")
        );
        server.abort();
    }

    #[tokio::test]
    async fn blocked_save_worker_fails_settings_requests_in_bounded_time_then_retry_works() {
        let (observed, mut requests) = mpsc::unbounded_channel::<&'static str>();
        let body = json!({"revision":"r", "paths":{"config":"config.json","models":null,"agents":[]},"fields":[],"gateways":[]});
        let app = axum::Router::new().route(
            "/v1/settings",
            axum::routing::get({
                let observed = observed.clone();
                let body = body.clone();
                move || {
                    observed.send("GET").unwrap();
                    let body = body.clone();
                    async move { axum::Json(body) }
                }
            })
            .put({
                let observed = observed.clone();
                move || {
                    observed.send("PUT").unwrap();
                    let body = body.clone();
                    async move { axum::Json(body) }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (tx, mut events) = mpsc::channel(10);
        let mut executor = Executor::new(Client::new(url, None), tx);
        executor.save_wait = Duration::from_millis(100);
        let ctx = Context {
            generation: Generation(0),
            session_id: None,
            workspace: "/ws".into(),
            config_dir: None,
        };
        let mut editor = Editor::default();
        async fn next_event(events: &mut mpsc::Receiver<ClientEvent>) -> ClientEvent {
            tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("a settings result arrives in bounded time")
                .unwrap()
        }

        // A save stuck behind config.json.lock held by another process.
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let (started, ready) = tokio::sync::oneshot::channel();
        executor
            .saves
            .send(Box::new(move || {
                started.send(()).unwrap();
                blocked.recv().unwrap();
                None
            }))
            .unwrap();
        ready.await.unwrap();

        executor.execute(Effect::FetchConfig(1), &ctx, &mut editor);
        match next_event(&mut events).await {
            ClientEvent::ConfigLoaded {
                request: 1,
                result: Err(error),
                ..
            } => assert!(error.contains("still waiting"), "{error}"),
            other => panic!("expected a bounded load failure, got {other:?}"),
        }
        let write = WriteRequest {
            revision: "r".into(),
            key: "tui.theme".into(),
            value: json!("nord-dark"),
        };
        executor.execute(Effect::WriteConfig(write.clone()), &ctx, &mut editor);
        match next_event(&mut events).await {
            ClientEvent::ConfigWritten { result: Err(_), .. } => {}
            other => panic!("expected a bounded write failure, got {other:?}"),
        }
        assert!(
            requests.try_recv().is_err(),
            "no HTTP request may pass the barrier while an earlier save is pending"
        );

        // Once the earlier save finishes, the same requests go through.
        release.send(()).unwrap();
        executor.flush_saves().await;
        executor.execute(Effect::FetchConfig(2), &ctx, &mut editor);
        match next_event(&mut events).await {
            ClientEvent::ConfigLoaded {
                request: 2,
                result: Ok(_),
                ..
            } => {}
            other => panic!("expected the retried load to succeed, got {other:?}"),
        }
        executor.execute(Effect::WriteConfig(write), &ctx, &mut editor);
        match next_event(&mut events).await {
            ClientEvent::ConfigWritten { result: Ok(_), .. } => {}
            other => panic!("expected the retried write to succeed, got {other:?}"),
        }
        assert_eq!(requests.recv().await, Some("GET"));
        assert_eq!(requests.recv().await, Some("PUT"));
        server.abort();
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
