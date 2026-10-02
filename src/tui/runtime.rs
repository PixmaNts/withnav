//! Owns the UI event loop; effect execution lives in `effects`, drawing in
//! view. The runtime is the only owner of `State`, `UiState`, the editor, the
//! render cache, the client and the event stream.
mod effects;

use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};

use super::{
    client::Client,
    input::{route_key, route_paste, Editor},
    protocol::{Action, ClientEvent, Effect},
    reducer,
    state::{Activity, ConnectionStatus, State, UiState},
    terminal::TerminalGuard,
    view::{self, RenderCache},
    TuiError, TuiOptions,
};
pub use effects::{Context, Executor, Flow};

/// Capacity of the channel the effect tasks and the stream report on. Senders
/// wait when it is full; the loop itself never waits on it.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;
/// Most events applied between two looks at the terminal and the clock.
pub const MAX_EVENTS_PER_ITERATION: usize = 64;
/// Time between ticks, and so the longest gap between two frames (~30 fps).
pub const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Everything the loop owns, with no terminal attached: tests drive it
/// directly.
pub struct Runtime {
    pub state: State,
    pub ui: UiState,
    pub editor: Editor,
    cache: RenderCache,
    exec: Executor,
    rx: mpsc::Receiver<ClientEvent>,
    tick: u64,
    dirty: bool,
    needs_clear: bool,
    quit: bool,
}

impl Runtime {
    pub fn new(options: &TuiOptions) -> Self {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let client = Client::new(options.url.clone(), options.token.clone());
        let workspace = options.workspace.to_string_lossy().into_owned();
        let state = State {
            workspace,
            selected_model_alias: options.initial_model.clone(),
            ..State::default()
        };
        let ui = UiState {
            follow_bottom: true,
            ..UiState::default()
        };
        Self {
            state,
            ui,
            editor: Editor::new(),
            cache: Default::default(),
            exec: Executor::new(client, tx),
            rx,
            tick: 0,
            dirty: true,
            needs_clear: false,
            quit: false,
        }
    }

    /// Asks for the model catalog and the session list, and resumes the
    /// session named on the command line, if any.
    pub fn start(&mut self, session: Option<String>) {
        self.run_effects(vec![Effect::FetchModels, Effect::ListSessions]);
        if let Some(id) = session {
            self.dispatch(Action::ResumeSession(id));
        }
    }

    pub fn executor(&self) -> &Executor {
        &self.exec
    }

    /// A sender into the loop's event channel.
    pub fn exec_sender(&self) -> mpsc::Sender<ClientEvent> {
        self.exec.sender()
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Counts ticks since the start; a view that animates reads it through
    /// here. Marks the screen dirty while something on it moves.
    pub fn on_tick(&mut self) -> u64 {
        self.tick = self.tick.wrapping_add(1);
        let animating = self.state.activity == Activity::Running
            || self.state.pending_submit.is_some()
            || matches!(
                self.state.connection,
                ConnectionStatus::Connecting | ConnectionStatus::Reconnecting
            );
        self.dirty |= animating;
        self.tick
    }

    /// Next event from the effect tasks and the stream.
    pub async fn recv(&mut self) -> Option<ClientEvent> {
        self.rx.recv().await
    }

    /// Applies `first` and then whatever else is already queued, up to the
    /// per-iteration cap.
    pub fn apply_batch(&mut self, first: ClientEvent) {
        self.apply_event(first);
        for _ in 1..MAX_EVENTS_PER_ITERATION {
            match self.rx.try_recv() {
                Ok(event) => self.apply_event(event),
                Err(_) => break,
            }
        }
    }

    pub fn apply_event(&mut self, event: ClientEvent) {
        self.dirty = true;
        match reducer::update(&mut self.state, event) {
            Ok(effects) => self.run_effects(effects),
            // A bad event is shown, never fatal.
            Err(err) => self.state.last_protocol_error = Some(err.0),
        }
    }

    pub fn handle_terminal_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.dirty = true;
                if let Some(action) = route_key(key, &self.state, &mut self.ui, &mut self.editor) {
                    self.dispatch(action);
                }
            }
            Event::Paste(text) => {
                self.dirty = true;
                route_paste(text, &self.ui, &mut self.editor);
            }
            Event::Resize(..) => {
                self.dirty = true;
                self.needs_clear = true;
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, action: Action) {
        self.dirty = true;
        let effects = reducer::handle_action(&mut self.state, &mut self.ui, action);
        self.run_effects(effects);
    }

    /// Executes `effects` the way the loop does after the reducer returned them, tagged with the
    /// current generation; tests use it to inject effects such as a re-subscription.
    pub fn run_effects(&mut self, effects: Vec<Effect>) {
        let ctx = Context {
            generation: self.state.generation.clone(),
            session_id: self.state.session.as_ref().map(|s| s.session_id.clone()),
            workspace: self.state.workspace.clone(),
        };
        for effect in effects {
            if self.exec.execute(effect, &ctx, &mut self.editor) == Flow::Quit {
                self.quit = true;
            }
        }
        self.exec.retire_stale_stream(&ctx.generation);
    }

    pub fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        view::draw(frame, &self.state, &self.ui, &self.editor, &mut self.cache);
    }

    fn draw(&mut self, guard: &mut TerminalGuard) -> Result<(), TuiError> {
        if std::mem::take(&mut self.needs_clear) {
            guard.invalidate().map_err(draw_error)?;
        }
        guard.draw(|frame| self.render(frame)).map_err(draw_error)?;
        self.dirty = false;
        Ok(())
    }
}

fn draw_error(err: std::io::Error) -> TuiError {
    TuiError::Other(format!("cannot draw the screen: {err}"))
}

/// Runs the client until the user quits or the terminal goes away.
pub async fn run(options: TuiOptions) -> Result<(), TuiError> {
    let mut guard = TerminalGuard::enter()?;
    let mut runtime = Runtime::new(&options);
    runtime.start(options.initial_session.clone());
    let result = event_loop(&mut runtime, &mut guard).await;
    runtime.exec.cancel_stream();
    drop(guard);
    result
}

async fn event_loop(runtime: &mut Runtime, guard: &mut TerminalGuard) -> Result<(), TuiError> {
    let mut input = EventStream::new();
    let mut ticker = interval(FRAME_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let signal = termination_signal();
    tokio::pin!(signal);
    runtime.draw(guard)?;
    while !runtime.should_quit() {
        tokio::select! {
            biased;
            _ = &mut signal => break,
            _ = ticker.tick() => {
                runtime.on_tick();
                if runtime.dirty {
                    runtime.draw(guard)?;
                }
            }
            event = input.next() => match event {
                Some(Ok(event)) => runtime.handle_terminal_event(event),
                Some(Err(err)) => {
                    return Err(TuiError::Other(format!("cannot read the terminal: {err}")));
                }
                None => break,
            },
            Some(event) = runtime.recv() => runtime.apply_batch(event),
        }
    }
    Ok(())
}

/// Resolves on SIGINT or SIGTERM sent from outside: in raw mode Ctrl+C is a
/// key, so a signal means someone else wants the process gone.
async fn termination_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let term = async {
            match signal(SignalKind::terminate()) {
                Ok(mut sig) => {
                    sig.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests;
