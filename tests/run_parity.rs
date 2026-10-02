//! Direct-vs-bus render parity. `run` goes through
//! the bus. This test drives the SAME scripted mock-provider turn two
//! ways — (1) directly via `run_agent` with the Phase-1 `on_delta` closure,
//! (2) through the runtime task + `event_to_ui` adapter — and asserts the
//! rendered UiEvent sequence + terminal exit code are IDENTICAL, so the bus
//! path does not change the CLI's observable output.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use navai::agent::actor::AgentActor;
use navai::agent::run_loop::{run_agent, AgentEvent, RunOutcome};
use navai::app::{event_to_ui, exit_code_for};
use navai::bus::inbox::ActorInbox;
use navai::bus::runtime;
use navai::bus::sequencer::Sequencer;
use navai::bus::{ResumeOutcome, SessionBus};
use navai::provider::config::{ProviderConfig, ReasoningHistory};
use navai::provider::StreamDelta;
use navai::render::UiEvent;
use navai::tools::bash::BashTool;
use navai::tools::native::ReadTool;
use navai::tools::{Registry, ToolContext};

mod common;
use common::sse::{http_sse, spawn_mock_provider, sse_body};

/// A comparable snapshot of a rendered UiEvent (UiEvent is not PartialEq).
#[derive(Debug, PartialEq, Eq)]
enum Ui {
    Text(String),
    Thinking(String),
    Tool(String),
    Yielded(String, Option<String>),
    Done,
}

fn snap(ui: &UiEvent) -> Ui {
    match ui {
        UiEvent::AssistantText(t) => Ui::Text(t.clone()),
        UiEvent::Thinking(t) => Ui::Thinking(t.clone()),
        UiEvent::ToolStarted { name } => Ui::Tool(name.clone()),
        UiEvent::Yielded { question, kind } => Ui::Yielded(question.clone(), kind.clone()),
        UiEvent::Done => Ui::Done,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_via_bus_renders_identically_to_the_direct_path() {
    // A one-turn script: thinking + streamed text, then Done.
    let done = r#"{"choices":[{"index":0,"delta":{"reasoning_content":"pondering","content":"hello world"},"finish_reason":"stop"}]}"#;

    // --- Path 1: DIRECT (Phase-1 on_delta closure), capturing UiEvents. ---
    let direct_server = spawn_mock_provider(vec![http_sse(&sse_body(&[done]))]).await;
    let mut direct_ui: Vec<Ui> = Vec::new();
    let mut actor = make_actor(direct_server.url.clone());
    actor.push_user("go");
    let outcome = {
        let mut on_delta = |e: AgentEvent| {
            let AgentEvent::Delta(d) = e else { return };
            match d {
                StreamDelta::Text { text } => direct_ui.push(Ui::Text(text)),
                StreamDelta::Thinking { text } => direct_ui.push(Ui::Thinking(text)),
                StreamDelta::ToolCallStarted { name, .. } => direct_ui.push(Ui::Tool(name)),
                StreamDelta::ToolCallArgs { .. } => {}
            }
        };
        run_agent(&mut actor, &mut on_delta)
            .await
            .expect("direct run")
    };
    // The Phase-1 `finish` renders Done for a Done outcome.
    if matches!(outcome, RunOutcome::Done(_)) {
        direct_ui.push(Ui::Done);
    }
    let direct_exit = phase1_exit(&outcome);

    // --- Path 2: THROUGH THE BUS (runtime task + event_to_ui). ---
    let bus_server = spawn_mock_provider(vec![http_sse(&sse_body(&[done]))]).await;
    let session_id = Ulid::new();
    let sequencer = Sequencer::spawn(session_id, SessionBus::default(), 0);
    let (inbox_tx, inbox) = ActorInbox::channel();
    let session_cancel = CancellationToken::new();
    let bus_actor = make_actor(bus_server.url.clone());

    let sub = sequencer.subscribe_since(None).await.expect("subscribe");
    let task = runtime::spawn(
        session_id,
        bus_actor,
        inbox,
        sequencer.clone(),
        session_cancel.clone(),
        None,
    );

    inbox_tx
        .send(user_command(session_id, "go"))
        .await
        .expect("post");

    let (bus_ui, bus_exit) = collect_bus_render(sub).await;

    session_cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;

    // --- Parity: identical rendered UiEvent sequence + exit code. ---
    assert_eq!(
        bus_ui, direct_ui,
        "bus render sequence != direct render sequence"
    );
    assert_eq!(bus_exit, direct_exit, "bus exit code != direct exit code");
    assert_eq!(direct_exit, 0, "a Done turn exits 0");
}

/// Drive the bus event stream through `event_to_ui`, collecting rendered
/// UiEvents until the terminal turn.end, and derive the exit code the same
/// way `run` does (via the public `exit_code_for`).
async fn collect_bus_render(sub: navai::bus::SubscribeReply) -> (Vec<Ui>, i32) {
    let mut out = Vec::new();
    if let ResumeOutcome::Replay(events) = &sub.replay {
        for env in events {
            if let Some(ui) = event_to_ui(env) {
                out.push(snap(&ui));
            }
        }
    }
    let mut exit = 1;
    if let Some(mut live) = sub.live {
        while let Ok(Some(env)) = tokio::time::timeout(Duration::from_secs(5), live.recv()).await {
            if let Some(ui) = event_to_ui(&env) {
                out.push(snap(&ui));
            }
            if env.kind == "turn.end" {
                exit = exit_code_for(&env);
                break;
            }
        }
    }
    (out, exit)
}

fn phase1_exit(outcome: &RunOutcome) -> i32 {
    match outcome {
        RunOutcome::Done(_) => 0,
        RunOutcome::Yielded(_) | RunOutcome::StepBudgetExhausted => 2,
        RunOutcome::Interrupted => 130,
    }
}

/// Stderr parity on the ERROR path: Phase-1 `finish` prints `\nerror: {e}` and
/// exits 1; the bus path's `exit_code_for` reads `turn.end.error.message` and
/// prints the SAME line + exit 1. This pins that the bounded error summary the
/// runtime puts on `turn.end` reproduces the Phase-1 stderr line, so a
/// client rendering from events sees byte-identical error output.
#[test]
fn error_turn_end_reproduces_the_phase1_stderr_line_and_exit_code() {
    use navai::bus::event::{EventEnvelope, EventKind, TurnEnd, TurnEndOutcome, TurnError};

    let message = "provider API error (status 500): boom".to_string();
    // The Phase-1 `finish` stderr for `Err(e)` is `\nerror: {e}`.
    let phase1_stderr = format!("\nerror: {message}");

    let payload = TurnEnd {
        turn_id: Ulid::new(),
        actor_id: "agent:main".to_string(),
        outcome: TurnEndOutcome::Error,
        usage: None,
        duration_ms: 1,
        error: Some(TurnError {
            code: "provider".to_string(),
            message: message.clone(),
        }),
    };
    let env = EventEnvelope::new(
        Ulid::new(),
        7,
        EventKind::TurnEnd,
        serde_json::to_value(&payload).unwrap(),
    );

    // The bus path derives exit 1 from the same envelope.
    assert_eq!(
        exit_code_for(&env),
        1,
        "error turn.end exits 1 (== Phase-1)"
    );
    // Assert the ACTUAL string the production path emits — the shared
    // `terminal_stderr_line` that `exit_code_for` prints — equals the Phase-1
    // `finish` line, not an independent reconstruction.
    let bus_stderr =
        navai::app::terminal_stderr_line(&payload).expect("error outcome produces a stderr line");
    assert_eq!(
        bus_stderr, phase1_stderr,
        "bus stderr line != Phase-1 stderr line for the same error"
    );
}

/// Subprocess capture of the REAL production stderr: run the actual
/// `navai run` binary against a mock provider that returns HTTP 500, and
/// assert the process stderr carries the `error:` line and the process exits
/// 1. This exercises the `exit_code_for` -> `eprint!(terminal_stderr_line)`
/// path end-to-end, so deleting the production `eprint!` would FAIL this test
/// (unlike a direct call to `terminal_stderr_line`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_binary_prints_the_error_line_to_stderr_and_exits_1() {
    // A mock provider that answers every request with a bare HTTP 500.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let body = "{\"error\":\"boom\"}";
            let resp = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("ws")).unwrap();
    let data_home = root.join("data");
    let config_home = root.join("config");
    let models = root.join("models.json");
    std::fs::write(
        &models,
        serde_json::json!({
            "providers": { "mock": {
                "base_url": format!("http://{addr}/v1"),
                "api_key_env": "RUST_AGENT_RUN_PARITY_KEY",
                "headers": {}, "connect_timeout_ms": 2000, "request_timeout_ms": 30000
            }},
            "models": { "default": {
                "provider": "mock", "model": "m", "reasoning_history": "preserve"
            }}
        })
        .to_string(),
    )
    .unwrap();

    let out = tokio::task::spawn_blocking({
        let models = models.clone();
        let ws = root.join("ws");
        move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_navai"))
                .args([
                    "run",
                    "--workspace",
                    ws.to_str().unwrap(),
                    "--models",
                    models.to_str().unwrap(),
                    "--prompt",
                    "go",
                ])
                .env("RUST_AGENT_RUN_PARITY_KEY", "test-key")
                // The binary opens the conversation history in the XDG data
                // dir and migrates legacy files there. Without these two, a
                // test run wrote into the driver's real database and moved
                // their data (observed 2026-10-01).
                .env("XDG_DATA_HOME", data_home.as_os_str())
                .env("XDG_CONFIG_HOME", config_home.as_os_str())
                .output()
                .expect("run the binary")
        }
    })
    .await
    .expect("join");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error:"),
        "the run binary printed the error line to stderr; got stderr={stderr:?}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "run exits 1 on a provider error"
    );
}

fn user_command(session_id: Ulid, text: &str) -> navai::bus::command::Command {
    use navai::bus::command::{Command, ContentBlock, DeliveryMode};
    Command {
        command_id: Ulid::new(),
        session_id,
        source_actor_id: "user:cli".to_string(),
        target_actor_id: "agent:main".to_string(),
        mode: DeliveryMode::NextTurn,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        created_at: std::time::SystemTime::now(),
    }
}

fn make_actor(base_url: String) -> AgentActor {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    let mut registry = Registry::new();
    registry.register(Box::new(ReadTool));
    registry.register(Box::new(BashTool));
    AgentActor::new(
        registry,
        ToolContext {
            workspace: ws.clone(),
            writable_roots: vec![ws],
        },
        ProviderConfig {
            base_url,
            api_key_env: None,
            headers: BTreeMap::new(),
            connect_timeout_ms: 2000,
            request_timeout_ms: 600_000,
        },
        "test-key".to_string(),
        "test-model".to_string(),
        ReasoningHistory::Preserve,
        Some(256),
        cancel_placeholder(),
    )
}

fn cancel_placeholder() -> CancellationToken {
    CancellationToken::new()
}
