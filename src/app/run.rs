//! `nav run`: one task through the session server, then exit.
use super::*;

/// `run`: a one-shot IN-PROCESS CLIENT of the same core `serve` uses.
/// It starts the session manager (sequencer + runtime task, NO HTTP
/// server), subscribes to the session's event stream BEFORE posting the
/// prompt (so no initial `llm.delta` is lost — deltas are not in the replay
/// ring), renders each event through the `event_to_ui` adapter,
/// and derives the exit code from the terminal events — NOT from a direct
/// `run_agent` call (no shortcut). Order: create -> subscribe -> post ->
/// render-until-terminal -> close.
pub(super) async fn run_command(args: RunArgs) -> anyhow::Result<i32> {
    use crate::server::session::ServerConfig;

    let workspace = match args.workspace {
        Some(w) => w,
        None => std::env::current_dir()?,
    };
    let cfg_dir = config_dir()?;
    let rules_roots = RulesRoots::from_user_home();
    let client_cache = std::sync::Arc::new(crate::provider::openai::ClientCache::new());
    crate::session::preflight::check(
        &cfg_dir,
        args.models.as_deref(),
        &workspace,
        &rules_roots,
        client_cache.clone(),
    )
    .await?;
    let prompt = read_prompt(args.prompt)?;

    let state = crate::server::session::AppState::new(
        ServerConfig {
            bind: "127.0.0.1:0".parse().expect("loopback"),
            models_path: args.models.clone(),
            config_dir: cfg_dir,
            allow_non_loopback: false,
            bearer_token: None,
            history_db: history_db_path()?,
            data_dir: crate::paths::data_dir()
                .ok_or_else(|| anyhow::anyhow!("cannot resolve data dir"))?,
            rules_roots,
            web: None,
        },
        client_cache,
    )?;
    // The run's only session freezes its tool inventory when it is created:
    // let configured MCP servers finish starting first (bounded).
    state.service().wait_for_mcp().await;
    // A session that cannot start (no window, unknown alias, no key) ends the
    // run the way a failed turn does: the `error:` line, exit 1.
    let session_id = match state
        .create_session_with(
            &workspace,
            args.model.as_deref(),
            false,
            crate::session::VariantChoice::from_word(args.variant.as_deref()),
            args.agent.as_deref(),
        )
        .await
    {
        Ok(id) => id,
        Err(e) => {
            eprintln!("\nerror: {e}");
            return Ok(EXIT_ERROR);
        }
    };
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

    // A leading `/name` runs a session command and prints its result; `//text`
    // sends the literal message `/text`.
    let prompt = match crate::session::route_line(&prompt) {
        crate::session::Routed::Command { name, arguments } => {
            let exit = match state.run_command(session_id, &name, &arguments).await {
                Ok(outcome) if outcome.ok => {
                    println!("{}", outcome.render_text());
                    EXIT_DONE
                }
                Ok(outcome) => {
                    eprintln!("error: {}", outcome.render_text());
                    EXIT_ERROR
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    EXIT_ERROR
                }
            };
            let _ = state.close_session(session_id).await;
            return Ok(exit);
        }
        crate::session::Routed::Message(text) => text,
    };

    // Bypass is on before the first prompt; no fallback if it cannot be.
    if args.yes
        && let Err(e) = state
            .set_bypass(
                session_id,
                true,
                crate::bus::runtime::bypass::BypassOrigin::Cli,
            )
            .await
    {
        eprintln!("\nerror: cannot enable bypass: {e}");
        let _ = state.close_session(session_id).await;
        return Ok(EXIT_ERROR);
    }

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
        command_id: ulid::Ulid::generate(),
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
///
/// A `permission.requested` ends the run with [`EXIT_PERMISSION_REQUIRED`] and
/// a `permission_required: <tool> <scope>: <reason>` line on stderr. Nothing
/// is ever approved here: with `--yes` the session's bypass already answered
/// the ordinary requests, so one that arrives is one the Captain must decide.
pub(super) async fn render_until_terminal(
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
        if env.kind == EventKind::PermissionRequested.as_str() {
            let Ok(request) = serde_json::from_value::<crate::bus::event::PermissionRequested>(
                env.payload.clone(),
            ) else {
                return EXIT_ERROR;
            };
            eprintln!(
                "\npermission_required: {} {}: {}",
                request.tool, request.scope, request.reason
            );
            return EXIT_PERMISSION_REQUIRED;
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

/// Render-behind-a-subscriber adapter : maps a bus event
/// onto the SAME UiEvent the Phase-1 `on_delta` closure + `finish` produced,
/// so `render.rs` and CLI stdout do not change.
/// Returns `None` for events with no CLI rendering (session/turn lifecycle,
/// llm.response, tool calls, agent.state, task.done, message.received) — the
/// Phase-1 path rendered none of those either.
pub fn event_to_ui(envelope: &EventEnvelope) -> Option<UiEvent> {
    let kind = EventKind::from_wire(&envelope.kind)?;
    match kind {
        EventKind::CommandResult if envelope.payload["name"] == "watchdog" => {
            Some(UiEvent::AssistantText(format!(
                "\n{}\n",
                envelope.payload["data"]["message"].as_str()?
            )))
        }
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
        | EventKind::TaskDone
        | EventKind::CommandResult
        | EventKind::SessionAgentChanged
        | EventKind::PermissionRequested
        | EventKind::PermissionResolved
        | EventKind::ModelChanged
        | EventKind::SubagentSpawned
        | EventKind::SubagentDone
        | EventKind::SubagentWatchdog
        | EventKind::ContextCompacted => None,
        EventKind::ContextCompacting => Some(UiEvent::AssistantText(format!(
            "Compacting… {}/{}",
            envelope.payload["pass"], envelope.payload["total"]
        ))),
        EventKind::ContextUpdated | EventKind::SessionBypass | EventKind::McpStatus => None,
    }
}

pub(super) fn read_prompt(prompt: Option<String>) -> anyhow::Result<String> {
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
