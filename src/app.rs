use std::io::Read;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use crate::agent::response::HumanRequest;
#[cfg(test)]
use crate::agent::run_loop::RunOutcome;
use crate::bus::event::{AgentYield, EventEnvelope, EventKind, LlmDelta, TurnEnd, TurnEndOutcome};
#[cfg(test)]
use crate::error::AgentError;
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
    Tui,
    Auth(AuthArgs),
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
        Commands::Tui => {
            println!("not yet implemented (Phase 2/4)");
            Ok(EXIT_DONE)
        }
        Commands::Auth(args) => auth_command(args),
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

/// `run`: a one-shot IN-PROCESS CLIENT of the same core `serve` uses (SPEC
/// §6.2). It starts the session manager (sequencer + runtime task, NO HTTP
/// server), subscribes to the session's event stream BEFORE posting the
/// prompt (so no initial `llm.delta` is lost — deltas are not in the replay
/// ring), renders each event through the byte-identical `event_to_ui` adapter,
/// and derives the exit code from the terminal events — NOT from a direct
/// `run_agent` call (no shortcut, §6.2). Order: create -> subscribe -> post ->
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
    });
    let session_id = state.create_session(&workspace, args.model.as_deref(), None)?;
    let handle = state
        .handle(session_id)
        .ok_or_else(|| anyhow::anyhow!("session vanished immediately after creation"))?;

    // Ctrl-C cancels the SESSION token the runtime watches, so the in-flight
    // turn is interrupted and the runtime emits turn.end{interrupted} — the
    // render loop then exits 130. (Cancelling a standalone token would not
    // reach the runtime.)
    install_sigint_handler(handle.session_cancel.clone());

    // Subscribe BEFORE posting (O5): deltas are live-only, so a late
    // subscriber would drop the initial streamed text.
    let mut sub = handle
        .sequencer
        .subscribe_since(None)
        .await
        .map_err(|e| anyhow::anyhow!("subscribe failed: {e}"))?;

    post_prompt(&handle, session_id, &prompt).await?;

    let mut renderer = Renderer::new(args.show_thinking);
    let exit = render_until_terminal(&mut sub, &mut renderer).await;

    let _ = state.close_session(session_id).await;
    Ok(exit)
}

/// Post the user prompt as a `next-turn` command through the in-process
/// inbox, sequencing `message.received` before the command is consumed.
async fn post_prompt(
    handle: &crate::server::session::SessionHandle,
    session_id: ulid::Ulid,
    prompt: &str,
) -> anyhow::Result<()> {
    use crate::bus::command::{Command, ContentBlock, DeliveryMode};
    use crate::bus::event::ROOT_ACTOR_ID;
    use crate::bus::sequencer::PendingCommand;

    let permit = handle
        .inbox_tx
        .clone()
        .try_reserve_owned()
        .map_err(|_| anyhow::anyhow!("inbox unavailable"))?;
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
    let pending = PendingCommand {
        command_id: command.command_id,
        content: command.text_content(),
        source: command.source_actor_id.clone(),
        target: command.target_actor_id.clone(),
        mode: command.mode.as_str().to_string(),
    };
    let committed = command.clone();
    handle
        .sequencer
        .accept_command(
            pending,
            Box::new(move || {
                permit.send(committed);
            }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("post failed: {e}"))?;
    Ok(())
}

/// Render events through the `event_to_ui` adapter until a terminal
/// `turn.end`, reproducing the Phase-1 `finish` behavior (stdout via the
/// UiEvents, plus the stderr line + exit code for interrupted/error) from the
/// event stream instead of the raw `RunOutcome`.
async fn render_until_terminal(
    sub: &mut crate::bus::SubscribeReply,
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
    };
    crate::server::routes::serve(config).await?;
    Ok(EXIT_DONE)
}

/// The Phase-1 exit-code + rendering oracle. `run` now derives the same
/// mapping from the event stream via [`exit_code_for`] + [`event_to_ui`],
/// so this stays as the reference the parity test checks against.
#[cfg(test)]
fn finish(outcome: Result<RunOutcome, AgentError>, renderer: &mut Renderer) -> anyhow::Result<i32> {
    match outcome {
        Ok(RunOutcome::Done(_)) => {
            renderer.render(UiEvent::Done);
            Ok(EXIT_DONE)
        }
        Ok(RunOutcome::Yielded(req)) => {
            print_yield(renderer, &req);
            Ok(EXIT_YIELDED)
        }
        Ok(RunOutcome::StepBudgetExhausted) => {
            renderer.render(UiEvent::Yielded {
                question: "step budget exhausted".to_string(),
                kind: Some("budget".to_string()),
            });
            Ok(EXIT_YIELDED)
        }
        Ok(RunOutcome::Interrupted) => {
            eprintln!("\ninterrupted");
            Ok(EXIT_INTERRUPTED)
        }
        Err(e) => {
            eprintln!("\nerror: {e}");
            Ok(EXIT_ERROR)
        }
    }
}

#[cfg(test)]
fn print_yield(renderer: &mut Renderer, req: &HumanRequest) {
    renderer.render(UiEvent::Yielded {
        question: req.question.clone(),
        kind: req.kind.clone(),
    });
}

/// Render-behind-a-subscriber adapter (ticket 05, option a): maps a bus event
/// onto the SAME UiEvent the Phase-1 `on_delta` closure + `finish` produced,
/// so `render.rs` stays byte-identical and CLI stdout does not change.
/// Returns `None` for events with no CLI rendering (session/turn lifecycle,
/// llm.response, tool calls, agent.state, task.done, message.received) — the
/// Phase-1 path rendered none of those either.
pub fn event_to_ui(envelope: &EventEnvelope) -> Option<UiEvent> {
    let kind = EventKind::from_wire(&envelope.kind)?;
    match kind {
        EventKind::LlmDelta => {
            let payload: LlmDelta = serde_json::from_value(envelope.payload.clone()).ok()?;
            match payload.delta {
                crate::provider::StreamDelta::Text(t) => Some(UiEvent::AssistantText(t)),
                crate::provider::StreamDelta::Thinking(t) => Some(UiEvent::Thinking(t)),
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
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_debug_assert() {
        Cli::command().debug_assert();
    }

    #[test]
    fn exit_codes_are_pinned() {
        assert_eq!(EXIT_DONE, 0);
        assert_eq!(EXIT_ERROR, 1);
        assert_eq!(EXIT_YIELDED, 2);
        assert_eq!(EXIT_INTERRUPTED, 130);
    }

    #[test]
    fn finish_maps_outcomes_to_exit_codes() {
        let mut r = Renderer::new(false);
        assert_eq!(
            finish(Ok(RunOutcome::Done(Default::default())), &mut r).unwrap(),
            EXIT_DONE
        );
        assert_eq!(
            finish(
                Ok(RunOutcome::Yielded(HumanRequest {
                    tool_call_id: "a".into(),
                    question: "q".into(),
                    kind: None,
                    details: None,
                })),
                &mut r
            )
            .unwrap(),
            EXIT_YIELDED
        );
        assert_eq!(
            finish(Ok(RunOutcome::StepBudgetExhausted), &mut r).unwrap(),
            EXIT_YIELDED
        );
        assert_eq!(
            finish(Ok(RunOutcome::Interrupted), &mut r).unwrap(),
            EXIT_INTERRUPTED
        );
    }

    #[test]
    fn read_prompt_prefers_literal() {
        assert_eq!(read_prompt(Some("hi there".into())).unwrap(), "hi there");
    }

    #[test]
    fn cli_lists_four_subcommands() {
        let cmd = Cli::command();
        let names: Vec<_> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        for expected in ["run", "serve", "tui", "auth"] {
            assert!(names.contains(&expected), "missing subcommand: {expected}");
        }
    }

    #[test]
    fn bogus_subcommand_is_error() {
        let result = Cli::try_parse_from(["rust-agent", "bogus-subcommand"]);
        assert!(result.is_err());
    }

    // --- event_to_ui render-parity (todo 4 deliverable 3) ---

    use crate::bus::event::{
        AgentYield as AgentYieldPayload, LlmDelta as LlmDeltaPayload, TurnEnd as TurnEndPayload,
        TurnError,
    };
    use crate::provider::StreamDelta;
    use ulid::Ulid;

    fn delta_envelope(delta: StreamDelta) -> EventEnvelope {
        let payload = LlmDeltaPayload {
            turn_id: Ulid::new(),
            step_id: Ulid::new(),
            delta,
        };
        EventEnvelope::new(
            Ulid::new(),
            1,
            EventKind::LlmDelta,
            serde_json::to_value(&payload).unwrap(),
        )
    }

    fn kind_envelope(kind: EventKind, payload: serde_json::Value) -> EventEnvelope {
        EventEnvelope::new(Ulid::new(), 2, kind, payload)
    }

    // Given the scripted delta sequence of a simple Phase-1 run (thinking,
    // text, a tool starting, more text) followed by a Done turn end
    // When each envelope passes through event_to_ui
    // Then the UiEvent sequence equals the Phase-1 on_delta + finish output
    #[test]
    fn event_to_ui_matches_phase1_on_delta_and_finish_sequence() {
        let envelopes = [
            delta_envelope(StreamDelta::Thinking("pondering".into())),
            delta_envelope(StreamDelta::Text("hello ".into())),
            delta_envelope(StreamDelta::ToolCallStarted {
                index: 0,
                name: "bash".into(),
            }),
            delta_envelope(StreamDelta::ToolCallArgs {
                index: 0,
                fragment: "ls".into(),
            }),
            delta_envelope(StreamDelta::Text("world".into())),
            kind_envelope(
                EventKind::TurnEnd,
                serde_json::to_value(&TurnEndPayload {
                    turn_id: Ulid::new(),
                    actor_id: "agent:main".into(),
                    outcome: TurnEndOutcome::Done,
                    usage: None,
                    duration_ms: 5,
                    error: None,
                })
                .unwrap(),
            ),
        ];

        let ui: Vec<UiEvent> = envelopes.iter().filter_map(event_to_ui).collect();
        assert_eq!(ui.len(), 5);
        assert!(matches!(&ui[0], UiEvent::Thinking(t) if t == "pondering"));
        assert!(matches!(&ui[1], UiEvent::AssistantText(t) if t == "hello "));
        assert!(matches!(&ui[2], UiEvent::ToolStarted { name } if name == "bash"));
        // ToolCallArgs rendered nothing in Phase 1 (empty on_delta branch):
        assert!(matches!(&ui[3], UiEvent::AssistantText(t) if t == "world"));
        assert!(matches!(ui[4], UiEvent::Done));
    }

    // Given a turn that ended Yielded with an agent.yield carrying the question
    // When the envelopes pass through event_to_ui
    // Then the Yielded UiEvent carries question + kind exactly as print_yield
    #[test]
    fn event_to_ui_maps_agent_yield_to_the_yielded_ui_event() {
        let yield_payload = AgentYieldPayload {
            actor_id: "agent:main".into(),
            turn_id: Ulid::new(),
            question: "insert SD card?".into(),
            kind: Some("confirm".into()),
            details: None,
        };
        let yield_env = kind_envelope(
            EventKind::AgentYield,
            serde_json::to_value(&yield_payload).unwrap(),
        );
        // The Phase-1 finish() rendered Yielded from agent.yield, and the
        // turn.end{yielded} itself carried no additional UiEvent:
        let turn_end_env = kind_envelope(
            EventKind::TurnEnd,
            serde_json::to_value(&TurnEndPayload {
                turn_id: yield_payload.turn_id,
                actor_id: "agent:main".into(),
                outcome: TurnEndOutcome::Yielded,
                usage: None,
                duration_ms: 9,
                error: None,
            })
            .unwrap(),
        );

        assert!(matches!(
            event_to_ui(&yield_env),
            Some(UiEvent::Yielded { question, kind })
                if question == "insert SD card?" && kind.as_deref() == Some("confirm")
        ));
        assert!(event_to_ui(&turn_end_env).is_none());
    }

    // Given the lifecycle events with no Phase-1 rendering
    // When they pass through event_to_ui
    // Then none produces a UiEvent
    #[test]
    fn event_to_ui_returns_none_for_unrendered_kinds() {
        let session_env = kind_envelope(EventKind::SessionStart, serde_json::json!({}));
        let state_env = kind_envelope(EventKind::AgentState, serde_json::json!({}));
        let message_env = kind_envelope(EventKind::MessageReceived, serde_json::json!({}));
        assert!(event_to_ui(&session_env).is_none());
        assert!(event_to_ui(&state_env).is_none());
        assert!(event_to_ui(&message_env).is_none());
    }

    // Given an interrupted or errored turn end
    // When it passes through event_to_ui
    // Then no UiEvent is produced (Phase-1 printed those to stderr, not render)
    #[test]
    fn event_to_ui_maps_interrupted_and_error_turn_ends_to_none() {
        for outcome in [TurnEndOutcome::Interrupted, TurnEndOutcome::Error] {
            let env = kind_envelope(
                EventKind::TurnEnd,
                serde_json::to_value(&TurnEndPayload {
                    turn_id: Ulid::new(),
                    actor_id: "agent:main".into(),
                    outcome,
                    usage: None,
                    duration_ms: 3,
                    error: Some(TurnError {
                        code: "provider".into(),
                        message: "boom".into(),
                    }),
                })
                .unwrap(),
            );
            assert!(
                event_to_ui(&env).is_none(),
                "{outcome:?} rendered to stderr in Phase 1"
            );
        }
    }

    // Manual-QA observable: the exact UiEvent sequence event_to_ui produces
    // for a scripted simple run (thinking + two text deltas + tool + Done).
    #[test]
    fn event_to_ui_scripted_run_eprints_the_ui_sequence() {
        let envelopes = vec![
            delta_envelope(StreamDelta::Text("counting: ".into())),
            delta_envelope(StreamDelta::Text("1 2 3".into())),
            delta_envelope(StreamDelta::ToolCallStarted {
                index: 0,
                name: "read".into(),
            }),
            delta_envelope(StreamDelta::Text(" done".into())),
            kind_envelope(
                EventKind::TurnEnd,
                serde_json::to_value(&TurnEndPayload {
                    turn_id: Ulid::new(),
                    actor_id: "agent:main".into(),
                    outcome: TurnEndOutcome::Done,
                    usage: None,
                    duration_ms: 1,
                    error: None,
                })
                .unwrap(),
            ),
        ];
        for env in &envelopes {
            if let Some(ui) = event_to_ui(env) {
                eprintln!("UIEVENT {ui:?}");
            }
        }
    }
}
