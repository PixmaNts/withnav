//! The terminal client's production runtime against a real in-process session server and scripted
//! providers, with no terminal: keys go through `route_key`, effects through the executor, events
//! through the HTTP client and the SSE stream, and the result is read from the reducer's state and
//! from a test-backend render.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, Terminal};
use serde_json::json;
use tokio::time::Instant;

use navai::provider::Usage;
use navai::tools::ToolStatus;
use navai::tui::client::Client;
use navai::tui::protocol::{ClientEvent, Effect};
use navai::tui::runtime::Runtime;
use navai::tui::state::{Activity, Block, BlockKind, ConnectionStatus};
use navai::tui::TuiOptions;

mod common;
use common::server::{spawn_server, TestServerConfig};
use common::sse::{
    http_sse, spawn_capturing_provider, spawn_responding_provider, sse_body, tool_call_chunk,
};

/// The longest any single wait may take.
const WAIT: Duration = Duration::from_secs(20);

/// Points the XDG directories at a temp dir for this process, once, so nothing a test touches by
/// accident lands in the user's data or config.
fn isolate_xdg() {
    static DIRS: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIRS.get_or_init(|| {
        let dir = tempfile::tempdir().expect("xdg tempdir");
        std::env::set_var("XDG_DATA_HOME", dir.path().join("data"));
        std::env::set_var("XDG_CONFIG_HOME", dir.path().join("config"));
        std::env::set_var("HOME", dir.path());
        dir
    });
}

/// The workspace directory inside a test root; known before the server starts, so a scripted
/// provider can name files in it.
fn workspace_of(root: &tempfile::TempDir) -> PathBuf {
    root.path().join("ws")
}

/// A server over a two-alias `models.json` (`alpha` and `beta`) and a workspace holding
/// `notes.txt`. Dropping it stops the server.
struct Env {
    base: String,
    ws: PathBuf,
    task: tokio::task::JoinHandle<()>,
    _root: tempfile::TempDir,
}

impl Env {
    async fn start(root: tempfile::TempDir, provider_url: &str) -> Self {
        isolate_xdg();
        let state = TestServerConfig::mock(root.path(), provider_url)
            .model_alias("alpha", "mock-alpha")
            .extra_alias("beta", "mock-beta")
            .chat_tools(100_000)
            .state();
        let (addr, task) = spawn_server(state).await;
        let ws = workspace_of(&root);
        std::fs::create_dir_all(&ws).expect("workspace dir");
        std::fs::write(ws.join("notes.txt"), "hello from the notes\n").expect("notes.txt");
        Self {
            base: format!("http://{addr}"),
            ws,
            task,
            _root: root,
        }
    }

    fn runtime(&self) -> Runtime {
        Runtime::new(&TuiOptions {
            url: self.base.clone(),
            token: None,
            workspace: self.ws.clone(),
            initial_model: None,
            initial_session: None,
            embedded: false,
        })
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Applies events one at a time until `done` holds. `done` is asked first with no event (state
/// already good), then after each applied event. Panics naming `what` after [`WAIT`].
async fn until(
    what: &str,
    rt: &mut Runtime,
    mut done: impl FnMut(Option<&ClientEvent>, &Runtime) -> bool,
) {
    let deadline = Instant::now() + WAIT;
    if done(None, rt) {
        return;
    }
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = tokio::time::timeout(left, rt.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .unwrap_or_else(|| panic!("event channel closed while waiting for {what}"));
        rt.apply_event(event.clone());
        if done(Some(&event), rt) {
            return;
        }
    }
}

fn press(rt: &mut Runtime, code: KeyCode) {
    rt.handle_terminal_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn type_and_send(rt: &mut Runtime, text: &str) {
    rt.editor.set_text(text);
    press(rt, KeyCode::Enter);
}

fn steps(rt: &Runtime) -> Vec<(String, String)> {
    rt.state
        .conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::AssistantStep { text, thinking, .. } => {
                Some((text.clone(), thinking.clone()))
            }
            _ => None,
        })
        .collect()
}

fn has_final(rt: &Runtime, text: &str) -> bool {
    rt.state.activity == Activity::Idle && steps(rt).iter().any(|(t, _)| t == text)
}

fn assert_unique_ids(blocks: &[Block]) {
    let mut ids: Vec<_> = blocks.iter().map(|b| &b.id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), blocks.len(), "duplicate block ids: {blocks:?}");
}

fn screen(rt: &mut Runtime) -> String {
    let (w, h) = (100, 40);
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
    terminal.draw(|f| rt.render(f)).expect("draw");
    let buffer = terminal.backend().buffer();
    (0..h)
        .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn chunk(delta: serde_json::Value) -> String {
    json!({"choices": [{"index": 0, "delta": delta, "finish_reason": null}]}).to_string()
}

fn usage_chunk(finish: &str, usage: Usage) -> String {
    json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
        "usage": {
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "total_tokens": usage.total_tokens,
        },
    })
    .to_string()
}

fn usage(prompt: u64, completion: u64) -> Usage {
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
    }
}

/// Reasoning, text, then a native `read` call on `path`.
fn step_with_read(path: &Path) -> Vec<u8> {
    let call = tool_call_chunk("call_read", "read", &json!({ "path": path }));
    http_sse(&sse_body(&[
        &chunk(json!({"reasoning_content": "I should read the notes."})),
        &chunk(json!({"content": "Let me open the file."})),
        &call,
        &usage_chunk("tool_calls", usage(100, 10)),
    ]))
}

fn final_answer(text: &str, usage: Usage) -> Vec<u8> {
    http_sse(&sse_body(&[
        &chunk(json!({"content": text})),
        &usage_chunk("stop", usage),
    ]))
}

fn rate_limited() -> Vec<u8> {
    let body = r#"{"error":{"message":"Quota preflight blocked"}}"#;
    format!(
        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Opens the model picker with F2, moves to the entry at `index` and chooses it.
fn pick_model(rt: &mut Runtime, index: usize) {
    press(rt, KeyCode::F(2));
    for _ in 0..index {
        press(rt, KeyCode::Down);
    }
    press(rt, KeyCode::Enter);
}

/// Starts the runtime and waits for the two-alias catalog.
async fn started(env: &Env) -> Runtime {
    let mut rt = env.runtime();
    rt.start(None);
    until("the model catalog", &mut rt, |_, rt| {
        rt.state.catalog.len() == 2
    })
    .await;
    rt
}

fn tool_block(rt: &Runtime, id: &str) -> (serde_json::Value, Option<ToolStatus>, Option<u64>) {
    let wanted = format!("tool:{id}");
    rt.state
        .conversation
        .blocks
        .iter()
        .find_map(|b| match &b.kind {
            BlockKind::ToolCall {
                args,
                status,
                duration_ms,
                ..
            } if b.id == wanted => Some((args.clone(), *status, *duration_ms)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no block {wanted}"))
}

fn user_messages(rt: &Runtime) -> Vec<(String, String)> {
    rt.state
        .conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::Message { source, text } => Some((source.clone(), text.clone())),
            _ => None,
        })
        .collect()
}

fn session_id(rt: &Runtime) -> String {
    rt.state
        .session
        .as_ref()
        .expect("a conversation exists")
        .session_id
        .clone()
}

// Given a catalog of two aliases and a provider that streams reasoning, text and a `read` call,
//       then a final answer with usage
// When the second alias is picked and a message is sent
// Then the conversation is created lazily with that model and the projection shows the user
//      block, the tool with its duration, the answer and the token counters
// And a re-subscription, from the cursor and from the start, adds no block
// And after a stop, the next message resumes the conversation with its history
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pick_send_stream_tool_reconnect_stop_and_resume() {
    let root = tempfile::tempdir().expect("tempdir");
    let notes = workspace_of(&root).join("notes.txt");
    let provider = spawn_capturing_provider(vec![
        step_with_read(&notes),
        final_answer("The notes say hello.", usage(150, 20)),
        final_answer("Welcome back.", usage(200, 5)),
    ])
    .await;
    let env = Env::start(root, &provider.url).await;
    let mut rt = started(&env).await;

    pick_model(&mut rt, 1);
    assert_eq!(rt.state.selected_model_alias.as_deref(), Some("beta"));
    assert!(rt.state.session.is_none(), "picking alone creates nothing");

    type_and_send(&mut rt, "read the notes");
    until("the final answer", &mut rt, |_, rt| {
        has_final(rt, "The notes say hello.")
    })
    .await;

    let messages = user_messages(&rt);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].0.starts_with("user:"), "{messages:?}");
    assert_eq!(messages[0].1, "read the notes");
    let (args, status, duration) = tool_block(&rt, "call_read");
    assert_eq!(args["path"], json!(notes));
    assert_eq!(status, Some(ToolStatus::Ok));
    assert!(duration.is_some(), "tool.post_call carries duration_ms");
    let first_step = &steps(&rt)[0];
    assert_eq!(first_step.1, "I should read the notes.");
    assert_eq!(first_step.0, "Let me open the file.");
    assert_eq!(rt.state.conversation.context_tokens, Some(150));
    assert_eq!(rt.state.conversation.session_tokens, Some(110 + 170));
    assert_unique_ids(&rt.state.conversation.blocks);
    let shown = screen(&mut rt);
    assert!(shown.contains("YOU"), "{shown}");
    assert!(!shown.contains("NAVIGATOR"), "{shown}");
    assert!(shown.contains("The notes say hello."), "{shown}");

    let body: serde_json::Value = serde_json::from_str(&provider.bodies()[0]).expect("body");
    assert_eq!(
        body["model"], "mock-beta",
        "the picked model reached the provider"
    );

    // Let the trailing events (the agent going idle) land: the server's head is the target.
    let first_id = session_id(&rt);
    let head = Client::new(env.base.clone(), None)
        .summary(&first_id)
        .await
        .expect("summary")
        .current_seq;
    until(
        "the projection to reach the server head",
        &mut rt,
        |_, rt| rt.state.last_applied_seq == Some(head),
    )
    .await;

    // Re-subscribe from the cursor: nothing new arrives, nothing is added.
    let before = rt.state.conversation.blocks.clone();
    rt.run_effects(vec![
        Effect::Unsubscribe,
        Effect::Subscribe {
            session_id: first_id.clone(),
            cursor: Some(head),
        },
    ]);
    until(
        "the stream to reconnect from the cursor",
        &mut rt,
        |e, _| matches!(e, Some(ClientEvent::StreamConnected { .. })),
    )
    .await;
    assert_eq!(rt.state.conversation.blocks, before);

    // Re-subscribe from the start: the whole history is replayed and ignored.
    rt.run_effects(vec![
        Effect::Unsubscribe,
        Effect::Subscribe {
            session_id: first_id.clone(),
            cursor: Some(0),
        },
    ]);
    until(
        "the replay to reach the head",
        &mut rt,
        |e, _| matches!(e, Some(ClientEvent::Envelope { envelope, .. }) if envelope.seq == head),
    )
    .await;
    assert_eq!(rt.state.conversation.blocks, before);
    assert_eq!(rt.state.last_applied_seq, Some(head));

    // Stop, then send again: the same conversation resumes.
    press(&mut rt, KeyCode::Esc);
    until("the conversation to stop", &mut rt, |_, rt| {
        rt.state.activity == Activity::Stopped
            && rt.state.connection == ConnectionStatus::Disconnected
    })
    .await;
    type_and_send(&mut rt, "and once more");
    until("the answer after the resume", &mut rt, |_, rt| {
        has_final(rt, "Welcome back.")
    })
    .await;
    assert_eq!(session_id(&rt), first_id, "resume keeps the conversation");
    assert_eq!(user_messages(&rt).len(), 2);
    assert_unique_ids(&rt.state.conversation.blocks);
    let history = &provider.bodies()[2];
    for expected in [
        "read the notes",
        "hello from the notes",
        "The notes say hello.",
        "and once more",
    ] {
        assert!(
            history.contains(expected),
            "the resumed request lacks {expected:?}: {history}"
        );
    }
}

// Given a provider that answers every request with a 429
// When a message is sent
// Then the failed turn shows an error block carrying the code and the provider's message
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rate_limited_provider_shows_an_error_block() {
    let root = tempfile::tempdir().expect("tempdir");
    let provider = spawn_responding_provider(8, |_, _| rate_limited()).await;
    let env = Env::start(root, &provider.url).await;
    let mut rt = started(&env).await;

    pick_model(&mut rt, 0);
    type_and_send(&mut rt, "hello");
    until("the error block", &mut rt, |_, rt| {
        rt.state
            .conversation
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Error(_)))
    })
    .await;

    let text = rt
        .state
        .conversation
        .blocks
        .iter()
        .find_map(|b| match &b.kind {
            BlockKind::Error(text) => Some(text.clone()),
            _ => None,
        })
        .expect("error block");
    assert!(text.starts_with("provider:"), "{text}");
    assert!(text.contains("429"), "{text}");
    assert!(text.contains("Quota preflight blocked"), "{text}");
    let shown = screen(&mut rt);
    assert!(shown.contains("ERROR"), "{shown}");
    assert!(shown.contains("Quota preflight blocked"), "{shown}");
}
