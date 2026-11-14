//! `nav run`: one task through the session server, then exit.
use std::time::Duration;

use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use withnav_client::stream::StreamEvent;

use super::*;
use crate::app::connect::{Wanted, ensure_server};

/// `run`: an HTTP + SSE client of the background owner that owns the database.
/// Order: preflight -> ensure_server -> wait_for_mcp -> create session -> bypass if --yes ->
/// open event stream -> wait for Connected -> post prompt/command -> render-until-terminal -> exit.
pub(super) async fn run_command(args: RunArgs) -> anyhow::Result<i32> {
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

    let wanted = Wanted::from_env(args.models.as_deref())?;
    let owner = ensure_server(&wanted).await?;
    let client = withnav_client::client::Client::new(owner.url.clone(), Some(owner.token.clone()));

    // The run's only session freezes its tool inventory when it is created:
    // let configured MCP servers finish starting first (bounded).
    wait_for_mcp(&client).await;

    // A session that cannot start (no window, unknown alias, no key) ends the
    // run the way a failed turn does: the `error:` line, exit 1.
    let variant = match crate::session::VariantChoice::from_word(args.variant.as_deref()) {
        crate::session::VariantChoice::Default => None,
        crate::session::VariantChoice::Clear => Some(None),
        crate::session::VariantChoice::Exact(name) => Some(Some(name)),
    };
    let session = match client
        .create(withnav_client_api::protocol::CreateSessionRequest {
            workspace: workspace.display().to_string(),
            model: args.model.clone(),
            resume: None,
            agent: args.agent.clone(),
            variant,
        })
        .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("\nerror: {e}");
            return Ok(EXIT_ERROR);
        }
    };
    let session_id = session.session_id;

    // Bypass is on before the first prompt; no fallback if it cannot be.
    if args.yes
        && let Err(e) = client.set_bypass(&session_id, true).await
    {
        eprintln!("\nerror: cannot enable bypass: {e}");
        let _ = client.delete_session(&session_id).await;
        return Ok(EXIT_ERROR);
    }

    // Subscribe BEFORE posting: deltas are live-only, so a late
    // subscriber would drop the initial streamed text.
    let stream_cancel = CancellationToken::new();
    let mut stream = withnav_client::stream::subscribe(
        client.clone(),
        session_id.clone(),
        None,
        stream_cancel.clone(),
    );

    // Wait for the event stream to connect before submitting any message or command.
    let connect_timeout = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(connect_timeout);
    loop {
        tokio::select! {
            _ = &mut connect_timeout => {
                eprintln!("\nerror: timed out waiting for event stream connection");
                stream_cancel.cancel();
                let _ = client.delete_session(&session_id).await;
                return Ok(EXIT_ERROR);
            }
            event = stream.next() => match event {
                Some(StreamEvent::Connected) => break,
                Some(StreamEvent::Disconnected) | Some(StreamEvent::ProtocolError(_)) => {
                    eprintln!("\nerror: event stream disconnected before connection established");
                    stream_cancel.cancel();
                    let _ = client.delete_session(&session_id).await;
                    return Ok(EXIT_ERROR);
                }
                None => {
                    eprintln!("\nerror: event stream ended prematurely");
                    stream_cancel.cancel();
                    let _ = client.delete_session(&session_id).await;
                    return Ok(EXIT_ERROR);
                }
                _ => {}
            }
        }
    }

    // A leading `/name` runs a session command and prints its result; `//text`
    // sends the literal message `/text`.
    let prompt = match crate::session::route_line(&prompt) {
        crate::session::Routed::Command { name, arguments } => {
            let exit = match client
                .run_command(
                    &session_id,
                    withnav_client_api::protocol::CommandRequest {
                        name: name.clone(),
                        arguments,
                    },
                )
                .await
            {
                Ok(resp) => {
                    let outcome = crate::session::CommandOutcome {
                        operation_id: resp.operation_id.parse().unwrap_or_default(),
                        name,
                        ok: resp.ok,
                        data: resp.data,
                        error: resp.error,
                    };
                    if outcome.ok {
                        println!("{}", outcome.render_text());
                        EXIT_DONE
                    } else {
                        eprintln!("error: {}", outcome.render_text());
                        EXIT_ERROR
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    EXIT_ERROR
                }
            };
            stream_cancel.cancel();
            let _ = client.delete_session(&session_id).await;
            return Ok(exit);
        }
        crate::session::Routed::Message(text) => text,
    };

    if let Err(e) = client
        .send(
            &session_id,
            withnav_client_api::protocol::SubmitRequest {
                answer: None,
                target_actor_id: "agent:main".into(),
                mode: "next-turn".into(),
                content: vec![withnav_client_api::command::ContentBlock::Text { text: prompt }],
            },
        )
        .await
    {
        eprintln!("\nerror: post failed: {e}");
        stream_cancel.cancel();
        let _ = client.delete_session(&session_id).await;
        return Ok(EXIT_ERROR);
    }

    let mut renderer = Renderer::new(args.show_thinking);

    // Ctrl-C stops this specific session on the owner process and triggers an interrupted exit.
    let client_for_sigint = client.clone();
    let session_id_for_sigint = session_id.clone();
    let sigint_token = CancellationToken::new();
    let sigint_token_clone = sigint_token.clone();
    let sigint_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = client_for_sigint.stop(&session_id_for_sigint).await;
            tokio::time::sleep(Duration::from_millis(1500)).await;
            sigint_token_clone.cancel();
        }
    });

    let exit = tokio::select! {
        _ = sigint_token.cancelled() => {
            eprint!("\ninterrupted");
            EXIT_INTERRUPTED
        }
        exit = render_until_terminal(&mut stream, &mut renderer) => exit,
    };

    sigint_task.abort();
    stream_cancel.cancel();
    let _ = client.delete_session(&session_id).await;
    Ok(exit)
}

/// Wait until configured MCP servers are ready or failed (bounded at 30 s).
async fn wait_for_mcp(client: &withnav_client::client::Client) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        match client.mcp().await {
            Ok(status) => {
                let any_starting = status.servers.iter().any(|s| s.state == "starting");
                if !any_starting {
                    break;
                }
            }
            Err(_) => break,
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Render events through the `event_to_ui` adapter until a terminal
/// `turn.end`, reproducing the finish behavior (stdout via the
/// UiEvents, plus the stderr line + exit code for interrupted/error) from the
/// client event stream.
///
/// A `permission.requested` ends the run with [`EXIT_PERMISSION_REQUIRED`] and
/// a `permission_required: <tool> <scope>: <reason>` line on stderr.
pub(super) async fn render_until_terminal<S>(stream: &mut S, renderer: &mut Renderer) -> i32
where
    S: futures_util::Stream<Item = StreamEvent> + Unpin + ?Sized,
{
    let mut consecutive_reconnects = 0;
    const MAX_RECONNECTS: usize = 5;

    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::Connected => {
                consecutive_reconnects = 0;
            }
            StreamEvent::Envelope(env) => {
                consecutive_reconnects = 0;
                if let Some(ui) = event_to_ui(&env) {
                    renderer.render(ui);
                }
                if env.kind == EventKind::PermissionRequested.as_str() {
                    let Ok(request) = serde_json::from_value::<
                        crate::bus::event::PermissionRequested,
                    >(env.payload.clone()) else {
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
            StreamEvent::Resync => {
                consecutive_reconnects = 0;
            }
            StreamEvent::Reconnecting => {
                consecutive_reconnects += 1;
                if consecutive_reconnects > MAX_RECONNECTS {
                    eprintln!(
                        "\nerror: lost connection to server (max reconnect attempts exceeded)"
                    );
                    return EXIT_ERROR;
                }
            }
            StreamEvent::Disconnected => {
                return EXIT_ERROR;
            }
            StreamEvent::ProtocolError(err) => {
                eprintln!("\nerror: protocol error: {err}");
                return EXIT_ERROR;
            }
        }
    }
    EXIT_ERROR
}

/// Derive the process exit code from a `turn.end` envelope AND print the same
/// stderr line finish did (Interrupted/Error).
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

/// The stderr line the terminal outcome prints:
/// `\ninterrupted` for Interrupted, `\nerror: {message}` for Error, and
/// nothing for Done/Yielded.
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

/// Render-behind-a-subscriber adapter: maps a bus event onto the SAME UiEvent.
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
