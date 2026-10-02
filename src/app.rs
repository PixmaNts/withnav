use std::io::Read;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use crate::bus::event::{AgentYield, EventEnvelope, EventKind, LlmDelta, TurnEnd, TurnEndOutcome};
use crate::provider::auth;
use crate::render::{Renderer, UiEvent};

pub const EXIT_DONE: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_YIELDED: i32 = 2;
pub const EXIT_INTERRUPTED: i32 = 130;

#[derive(Parser, Debug)]
#[command(
    name = "rust-agent",
    version,
    about = "A personal, minimal-but-complete coding agent in Rust",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Run(RunArgs),
    Serve(ServeArgs),
    Auth(AuthArgs),
    Tui(TuiArgs),
}

pub mod tui;

/// Logging for `tui`: a file in the data dir, never the terminal.
pub fn init_tui_logging() {
    tui::init_logging();
}

#[derive(Args, Debug)]
pub struct TuiArgs {
    /// Workspace directory the conversation works in.
    #[arg(long, default_value_os_t = std::env::current_dir().unwrap_or_default())]
    pub workspace: PathBuf,
    /// Model alias for a new conversation.
    #[arg(long, conflicts_with = "session")]
    pub model: Option<String>,
    /// Id of a past conversation to resume.
    #[arg(long, conflicts_with = "model")]
    pub session: Option<String>,
    /// Path of the models file for the embedded server.
    #[arg(long, conflicts_with = "url")]
    pub models: Option<PathBuf>,
    /// Connect to a running server instead of starting one. If it needs a bearer token, set
    /// RUST_AGENT_SERVER_TOKEN.
    #[arg(long, conflicts_with = "models")]
    pub url: Option<String>,
}

#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Bind address; loopback-only unless `--allow-non-loopback` is also set.
    #[arg(long, default_value = "127.0.0.1:7310")]
    pub bind: String,

    /// Permit a non-loopback bind (still NO auth/TLS this slice — operator's
    /// reverse-proxy / private-network responsibility).
    #[arg(long)]
    pub allow_non_loopback: bool,

    #[arg(long)]
    pub models: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[arg(short, long)]
    pub model: Option<String>,

    #[arg(short, long)]
    pub prompt: Option<String>,

    #[arg(long)]
    pub workspace: Option<PathBuf>,

    #[arg(long)]
    pub models: Option<PathBuf>,

    #[arg(long)]
    pub show_thinking: bool,
}

#[derive(Args, Debug)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(Subcommand, Debug)]
pub enum AuthCommand {
    Set { provider: String },
    List,
    Remove { provider: String },
}

pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

pub async fn dispatch(cli: Cli) -> anyhow::Result<i32> {
    match cli.command {
        Commands::Run(args) => run_command(args).await,
        Commands::Serve(args) => serve_command(args).await,
        Commands::Auth(args) => auth_command(args),
        Commands::Tui(args) => tui::run(args).await,
    }
}

fn config_dir() -> anyhow::Result<PathBuf> {
    auth::default_config_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve config directory"))
}

fn auth_command(args: AuthArgs) -> anyhow::Result<i32> {
    let dir = config_dir()?;
    match args.command {
        AuthCommand::Set { provider } => {
            let key = rpassword_prompt(&provider)?;
            auth::set_key(&dir, &provider, &key)?;
            println!("stored key for provider '{provider}' (0600)");
            Ok(EXIT_DONE)
        }
        AuthCommand::List => {
            let providers = auth::list_providers(&dir)?;
            if providers.is_empty() {
                println!("(no providers configured)");
            } else {
                for p in providers {
                    println!("{p}");
                }
            }
            Ok(EXIT_DONE)
        }
        AuthCommand::Remove { provider } => {
            if auth::remove_key(&dir, &provider).is_ok() {
                println!("removed key for provider '{provider}'");
            } else {
                println!("no key found for provider '{provider}'");
            }
            Ok(EXIT_DONE)
        }
    }
}

fn rpassword_prompt(provider: &str) -> anyhow::Result<String> {
    eprint!("API key for '{provider}' (input hidden not supported; reads a line from stdin): ");
    use std::io::Write;
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// `run`: a one-shot IN-PROCESS CLIENT of the same core `serve` uses.
/// It starts the session manager (sequencer + runtime task, NO HTTP
/// server), subscribes to the session's event stream BEFORE posting the
/// prompt (so no initial `llm.delta` is lost — deltas are not in the replay
/// ring), renders each event through the `event_to_ui` adapter,
/// and derives the exit code from the terminal events — NOT from a direct
/// `run_agent` call (no shortcut). Order: create -> subscribe -> post ->
/// render-until-terminal -> close.
async fn run_command(args: RunArgs) -> anyhow::Result<i32> {
    use crate::server::session::ServerConfig;

    let workspace = match args.workspace {
        Some(w) => w,
        None => std::env::current_dir()?,
    };
    let cfg_dir = config_dir()?;
    let prompt = read_prompt(args.prompt)?;

    let state = crate::server::session::AppState::new(ServerConfig {
        bind: "127.0.0.1:0".parse().expect("loopback"),
        models_path: args.models.clone(),
        config_dir: cfg_dir,
        allow_non_loopback: false,
        bearer_token: None,
        history_db: history_db_path()?,
    })?;
    let session_id = state
        .create_session(&workspace, args.model.as_deref(), None)
        .await?;
    let handle = state
        .handle(session_id)
        .ok_or_else(|| anyhow::anyhow!("session vanished immediately after creation"))?;

    // Ctrl-C cancels the SESSION token the runtime watches, so the in-flight
    // turn is interrupted and the runtime emits turn.end{interrupted} — the
    // render loop then exits 130. (Cancelling a standalone token would not
    // reach the runtime.)
    install_sigint_handler(handle.session_cancel.clone());

    // Subscribe BEFORE posting: deltas are live-only, so a late
    // subscriber would drop the initial streamed text.
    let mut sub = state
        .subscribe(session_id, None)
        .await
        .map_err(|e| anyhow::anyhow!("subscribe failed: {e}"))?;

    post_prompt(&state, session_id, &prompt).await?;

    let mut renderer = Renderer::new(args.show_thinking);
    let exit = render_until_terminal(&mut sub, &mut renderer).await;

    let _ = state.close_session(session_id).await;
    Ok(exit)
}

/// Post the user prompt as a `next-turn` command through the in-process
/// inbox, sequencing `message.received` before the command is consumed.
async fn post_prompt(
    state: &crate::server::session::AppState,
    session_id: ulid::Ulid,
    prompt: &str,
) -> anyhow::Result<()> {
    use crate::bus::command::{Command, ContentBlock, DeliveryMode};
    use crate::bus::event::ROOT_ACTOR_ID;

    let command = Command {
        command_id: ulid::Ulid::new(),
        session_id,
        source_actor_id: "user:cli".to_string(),
        target_actor_id: ROOT_ACTOR_ID.to_string(),
        mode: DeliveryMode::NextTurn,
        content: vec![ContentBlock::Text {
            text: prompt.to_string(),
        }],
        created_at: std::time::SystemTime::now(),
    };
    state
        .send(command)
        .await
        .map_err(|e| anyhow::anyhow!("post failed: {e}"))?;
    Ok(())
}

/// Render events through the `event_to_ui` adapter until a terminal
/// `turn.end`, reproducing the Phase-1 `finish` behavior (stdout via the
/// UiEvents, plus the stderr line + exit code for interrupted/error) from the
/// event stream instead of the raw `RunOutcome`.
async fn render_until_terminal(
    sub: &mut crate::session::Subscription,
    renderer: &mut Renderer,
) -> i32 {
    if let crate::bus::ResumeOutcome::Replay(events) = &sub.replay {
        for env in events {
            if let Some(ui) = event_to_ui(env) {
                renderer.render(ui);
            }
        }
    }
    let Some(live) = sub.live.as_mut() else {
        return EXIT_ERROR;
    };
    // Ctrl-C cancels the session token, so cancellation arrives here as a real
    // turn.end{interrupted} event — just read the live stream to the terminal.
    while let Some(env) = live.recv().await {
        if let Some(ui) = event_to_ui(&env) {
            renderer.render(ui);
        }
        if env.kind == EventKind::TurnEnd.as_str() {
            return exit_code_for(&env);
        }
    }
    EXIT_ERROR
}

/// Derive the process exit code from a `turn.end` envelope AND print the same
/// stderr line Phase-1 `finish` did (Interrupted/Error). The exact stderr text
/// is produced by [`terminal_stderr_line`] so a parity test can assert the
/// REAL string the production path prints. Done->0, Yielded->2,
/// Interrupted->130 (+stderr), Error->1 (+stderr with the bounded summary).
pub fn exit_code_for(env: &EventEnvelope) -> i32 {
    let Ok(payload) = serde_json::from_value::<TurnEnd>(env.payload.clone()) else {
        return EXIT_ERROR;
    };
    if let Some(line) = terminal_stderr_line(&payload) {
        eprint!("{line}");
    }
    match payload.outcome {
        TurnEndOutcome::Done => EXIT_DONE,
        TurnEndOutcome::Yielded => EXIT_YIELDED,
        TurnEndOutcome::Interrupted => EXIT_INTERRUPTED,
        TurnEndOutcome::Error => EXIT_ERROR,
    }
}

/// The stderr line the terminal outcome prints, matching Phase-1 `finish`:
/// `\ninterrupted` for Interrupted, `\nerror: {message}` for Error, and
/// nothing (stdout-only via `event_to_ui`) for Done/Yielded.
pub fn terminal_stderr_line(payload: &TurnEnd) -> Option<String> {
    match payload.outcome {
        TurnEndOutcome::Interrupted => Some("\ninterrupted".to_string()),
        TurnEndOutcome::Error => {
            let msg = payload
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .unwrap_or_default();
            Some(format!("\nerror: {msg}"))
        }
        TurnEndOutcome::Done | TurnEndOutcome::Yielded => None,
    }
}

/// The shared conversation database, in the navigator's data dir.
fn history_db_path() -> anyhow::Result<std::path::PathBuf> {
    crate::paths::migrate_legacy_data();
    crate::paths::history_db()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve the navigator data dir"))
}

async fn serve_command(args: ServeArgs) -> anyhow::Result<i32> {
    use crate::server::session::ServerConfig;

    let bind: std::net::SocketAddr = args
        .bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --bind address {:?}: {e}", args.bind))?;
    let cfg_dir = config_dir()?;
    let config = ServerConfig {
        bind,
        models_path: args.models,
        config_dir: cfg_dir,
        allow_non_loopback: args.allow_non_loopback,
        bearer_token: None,
        history_db: history_db_path()?,
    };
    crate::server::routes::serve(
        config,
        std::sync::Arc::new(crate::navigator::NavigatorHooks::new()),
    )
    .await?;
    Ok(EXIT_DONE)
}

/// Render-behind-a-subscriber adapter : maps a bus event
/// onto the SAME UiEvent the Phase-1 `on_delta` closure + `finish` produced,
/// so `render.rs` and CLI stdout do not change.
/// Returns `None` for events with no CLI rendering (session/turn lifecycle,
/// llm.response, tool calls, agent.state, task.done, message.received) — the
/// Phase-1 path rendered none of those either.
pub fn event_to_ui(envelope: &EventEnvelope) -> Option<UiEvent> {
    let kind = EventKind::from_wire(&envelope.kind)?;
    match kind {
        EventKind::LlmDelta => {
            let payload: LlmDelta = serde_json::from_value(envelope.payload.clone()).ok()?;
            match payload.delta {
                crate::provider::StreamDelta::Text { text } => Some(UiEvent::AssistantText(text)),
                crate::provider::StreamDelta::Thinking { text } => Some(UiEvent::Thinking(text)),
                crate::provider::StreamDelta::ToolCallStarted { name, .. } => {
                    Some(UiEvent::ToolStarted { name })
                }
                crate::provider::StreamDelta::ToolCallArgs { .. } => None,
            }
        }
        EventKind::AgentYield => {
            let payload: AgentYield = serde_json::from_value(envelope.payload.clone()).ok()?;
            Some(UiEvent::Yielded {
                question: payload.question,
                kind: payload.kind,
            })
        }
        EventKind::TurnEnd => {
            let payload: TurnEnd = serde_json::from_value(envelope.payload.clone()).ok()?;
            match payload.outcome {
                TurnEndOutcome::Done => Some(UiEvent::Done),
                // `finish` renders the same Yielded UiEvent for both (the
                // budget case pins question="step budget exhausted",
                // kind="budget"), and prints "interrupted"/"error" to stderr
                // without a UiEvent — so neither maps to a UiEvent here.
                TurnEndOutcome::Yielded | TurnEndOutcome::Interrupted | TurnEndOutcome::Error => {
                    None
                }
            }
        }
        EventKind::SessionStart
        | EventKind::SessionEnd
        | EventKind::MessageReceived
        | EventKind::TurnStart
        | EventKind::LlmResponse
        | EventKind::ToolPreCall
        | EventKind::ToolPostCall
        | EventKind::AgentState
        | EventKind::TaskDone => None,
    }
}

fn read_prompt(prompt: Option<String>) -> anyhow::Result<String> {
    match prompt {
        Some(p) if p != "-" => Ok(p),
        _ => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf.trim().to_string())
        }
    }
}

fn install_sigint_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
    });
}

#[cfg(test)]
mod tests;
