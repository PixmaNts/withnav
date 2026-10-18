//! The terminal client's production runtime against a real in-process session server and scripted
//! providers, with no terminal: keys go through `route_key`, effects through the executor, events
//! through the HTTP client and the SSE stream, and the result is read from the reducer's state and
//! from a test-backend render.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use tokio::time::Instant;

use withnav::provider::Usage;
use withnav::tools::ToolStatus;
use withnav::tui::TuiOptions;
use withnav::tui::protocol::{ClientEvent, Effect};
use withnav::tui::runtime::Runtime;
use withnav::tui::state::{Activity, Block, BlockKind, ConnectionStatus};
use withnav_client::client::Client;

mod common;
use common::server::{TestServerConfig, spawn_server};
use common::sse::{
    StreamingProvider, http_sse, spawn_capturing_provider, spawn_responding_provider,
    spawn_streaming_provider, sse_body, tool_call_chunk,
};

/// The longest any single wait may take.
const WAIT: Duration = Duration::from_secs(20);

/// Points the XDG directories at a temp dir for this process, once, so nothing a test touches by
/// accident lands in the user's data or config.
fn isolate_xdg() {
    static DIRS: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIRS.get_or_init(|| {
        let dir = tempfile::tempdir().expect("xdg tempdir");
        // SAFETY: runs once, before any server starts. Every test of this binary calls this
        // first, and the `OnceLock` holds the others until the variables are set.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.path().join("data"));
            std::env::set_var("XDG_CONFIG_HOME", dir.path().join("config"));
            std::env::set_var("HOME", dir.path());
        }
        dir
    });
}

/// The workspace directory inside a test root; known before the server starts, so a scripted
/// provider can name files in it.
fn workspace_of(root: &tempfile::TempDir) -> PathBuf {
    root.path().join("ws")
}

fn write_test_config(path: &Path, default_agent: &str, model: &str) {
    std::fs::write(
        path,
        json!({
            "default_agent": default_agent,
            "agents": {
                "build": {"model": model},
                "plan": {"model": model}
            },
            "permissions": {"scratch_dirs": []}
        })
        .to_string(),
    )
    .expect("config.json");
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
        Self::start_with(root, provider_url, |config| config).await
    }

    /// Like [`Env::start`], with the server config adjusted by `adjust` first.
    async fn start_with(
        root: tempfile::TempDir,
        provider_url: &str,
        adjust: impl FnOnce(TestServerConfig) -> TestServerConfig,
    ) -> Self {
        isolate_xdg();
        let config = TestServerConfig::mock(root.path(), provider_url)
            .model_alias("mock/mock-alpha", "mock-alpha")
            .extra_alias("mock/mock-beta", "mock-beta")
            .chat_tools(100_000);
        let state = adjust(config).state();
        write_test_config(
            &root.path().join("cfg/config.json"),
            "build",
            "mock/mock-alpha",
        );
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
            config_dir: None,
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

/// Types `text` key by key through the terminal input (`⏎` is enter).
fn type_keys(rt: &mut Runtime, text: &str) {
    for c in text.chars() {
        let code = if c == '⏎' {
            KeyCode::Enter
        } else {
            KeyCode::Char(c)
        };
        press(rt, code);
    }
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

/// Opens the model picker with F2, moves to the entry at `index`, chooses it and takes the
/// preselected variant (the default, or none) in the second step.
fn pick_model(rt: &mut Runtime, index: usize) {
    press(rt, KeyCode::F(2));
    let row = withnav::tui::catalog::rows(&rt.state.catalog, &rt.ui)
        .iter()
        .enumerate()
        .filter(|(_, row)| matches!(row, withnav::tui::catalog::Row::Model(_)))
        .nth(index)
        .unwrap()
        .0;
    for _ in 0..row {
        press(rt, KeyCode::Down);
    }
    press(rt, KeyCode::Enter);
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
    assert_eq!(
        rt.state.selected_model_id.as_deref(),
        Some("mock/mock-beta")
    );
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
    assert!(shown.contains("▸ Captain"), "{shown}");
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

fn final_text_of(tab: &withnav::tui::state::Tab, text: &str) -> bool {
    tab.activity == Activity::Idle
        && tab
            .conversation
            .blocks
            .iter()
            .any(|b| matches!(&b.kind, BlockKind::AssistantStep { text: t, .. } if t == text))
}

type Answer = tokio::sync::mpsc::UnboundedSender<String>;

/// Applies the runtime's events until the provider receives a request; returns its body and the
/// sender that writes its answer.
async fn next_request(rt: &mut Runtime, provider: &mut StreamingProvider) -> (String, Answer) {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            request = provider.requests.recv() => return request.expect("provider open"),
            event = tokio::time::timeout(left, rt.recv()) => {
                let event = event
                    .expect("timed out waiting for a provider request")
                    .expect("event channel open");
                rt.apply_event(event);
            }
        }
    }
}

fn streamed_text(tab: &withnav::tui::state::Tab) -> String {
    tab.conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::AssistantStep { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// Given a provider whose two answers stay open and are written a piece at a time
// When a message is sent in one tab, ctrl+n opens a second tab and a message is sent there
// Then the pieces of both answers, interleaved, land each in its own tab while both stream, one
//      stream per tab, and the tab in the background is marked unread
// And closing the second tab mid-turn stops its stream only: the first keeps streaming to the end,
//      and the closed conversation stays on the server
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_tabs_stream_their_own_conversations_at_once() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut provider = spawn_streaming_provider().await;
    let env = Env::start(root, &provider.url).await;
    let mut rt = started(&env).await;
    pick_model(&mut rt, 0);

    type_and_send(&mut rt, "first question");
    let (body, first) = next_request(&mut rt, &mut provider).await;
    assert!(body.contains("first question"), "{body}");
    rt.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('n'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!((rt.state.tabs.len(), rt.state.active), (2, 1));
    until("the new tab assignments", &mut rt, |_, rt| {
        rt.state.selected_model_id.is_some()
    })
    .await;
    press(&mut rt, KeyCode::Enter);
    assert!(
        rt.editor.text().is_empty(),
        "the new tab starts with an empty editor"
    );
    type_and_send(&mut rt, "second question");
    let (body, second) = next_request(&mut rt, &mut provider).await;
    assert!(body.contains("second question"), "{body}");

    // Interleaved pieces: each reaches its own tab while both answers are still open.
    for (n, piece) in ["one ", "two "].iter().enumerate() {
        first
            .send(chunk(json!({"content": format!("alpha {piece}")})))
            .unwrap();
        second
            .send(chunk(json!({"content": format!("beta {piece}")})))
            .unwrap();
        let (want_a, want_b) = match n {
            0 => ("alpha one ", "beta one "),
            _ => ("alpha one alpha two ", "beta one beta two "),
        };
        until("both pieces", &mut rt, |_, rt| {
            streamed_text(&rt.state.tabs[0]) == want_a && streamed_text(&rt.state.tabs[1]) == want_b
        })
        .await;
    }
    assert_eq!(rt.state.tabs[0].activity, Activity::Running);
    assert_eq!(rt.state.tabs[1].activity, Activity::Running);
    assert!(rt.state.tabs[0].unread, "the background tab got news");
    assert!(!rt.state.tabs[1].unread, "the tab on screen is read");
    assert_eq!(rt.executor().stream_count(), 2);
    let shown = screen(&mut rt);
    assert!(shown.contains("beta one beta two"), "{shown}");
    assert!(!shown.contains("alpha one"), "{shown}");
    let second_id = session_id(&rt);

    // Close the second tab mid-turn: its stream stops, the first keeps streaming.
    rt.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('w'),
        KeyModifiers::ALT,
    )));
    assert_eq!(rt.state.tabs.len(), 1);
    assert_eq!(rt.executor().stream_count(), 1);
    let notice = rt
        .state
        .conversation
        .blocks
        .iter()
        .rev()
        .find_map(|b| match &b.kind {
            BlockKind::Notice(n) => Some(n.clone()),
            _ => None,
        });
    assert!(
        notice
            .as_deref()
            .is_some_and(|n| n.contains("keeps running on the server")),
        "{notice:?}"
    );
    second
        .send(chunk(json!({"content": "beta three"})))
        .unwrap();
    first
        .send(chunk(json!({"content": "alpha three"})))
        .unwrap();
    first.send(usage_chunk("stop", usage(10, 3))).unwrap();
    drop(first);
    until("the first answer to finish", &mut rt, |_, rt| {
        final_text_of(&rt.state.tabs[0], "alpha one alpha two alpha three")
    })
    .await;
    assert!(!streamed_text(&rt.state.tabs[0]).contains("beta"));
    drop(second);
    let listed = Client::new(env.base.clone(), None)
        .sessions(&env.ws.to_string_lossy())
        .await
        .expect("session list");
    assert!(listed.sessions.iter().any(|s| s.id == second_id));
}

fn command_blocks(tab: &withnav::tui::state::Tab) -> Vec<(String, bool, String)> {
    tab.conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::Command { name, ok, text } => Some((name.clone(), *ok, text.clone())),
            _ => None,
        })
        .collect()
}

// Given a conversation that answered once
// When the Captain types, key by key, `/context`, then an unknown `/contxt`, then `//context`
// Then the first two run on the server and come back as command blocks (the editor cleared once
//      the server took them), and the third is sent as the message "/context"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_server_commands_run_and_show_their_outcome() {
    let root = tempfile::tempdir().expect("tempdir");
    let provider = spawn_capturing_provider(vec![
        final_answer("Hi.", usage(10, 2)),
        final_answer("A literal slash.", usage(12, 2)),
    ])
    .await;
    let env = Env::start(root, &provider.url).await;
    let mut rt = started(&env).await;
    until("the command list read at start", &mut rt, |_, rt| {
        rt.state.server_commands.iter().any(|c| c.name == "context")
    })
    .await;
    pick_model(&mut rt, 0);
    type_and_send(&mut rt, "hello");
    until("the first answer", &mut rt, |_, rt| has_final(rt, "Hi.")).await;

    type_keys(&mut rt, "/context⏎");
    until("the context outcome", &mut rt, |_, rt| {
        command_blocks(&rt.state)
            .iter()
            .any(|(n, _, _)| n == "context")
    })
    .await;
    let (_, ok, text) = command_blocks(&rt.state).pop().unwrap();
    assert!(ok, "{text}");
    assert!(text.contains(" / ") && text.contains('%'), "{text}");
    until("the editor to clear", &mut rt, |_, rt| {
        rt.editor.text().is_empty()
    })
    .await;

    type_keys(&mut rt, "/contxt⏎");
    until("the unknown command's answer", &mut rt, |_, rt| {
        command_blocks(&rt.state)
            .iter()
            .any(|(n, _, _)| n == "contxt")
    })
    .await;
    let (_, ok, text) = command_blocks(&rt.state).pop().unwrap();
    assert!(!ok);
    assert!(text.contains("/context"), "names the close one: {text}");
    let shown = screen(&mut rt);
    assert!(shown.contains("/contxt · error"), "{shown}");
    until("the editor to clear", &mut rt, |_, rt| {
        rt.editor.text().is_empty()
    })
    .await;

    type_keys(&mut rt, "//context⏎");
    until("the literal message's answer", &mut rt, |_, rt| {
        has_final(rt, "A literal slash.")
    })
    .await;
    assert!(
        user_messages(&rt)
            .iter()
            .any(|(_, text)| text == "/context")
    );
}

fn press_tab(rt: &mut Runtime) {
    press(rt, KeyCode::Tab);
}

// Given a conversation that answered once
// When tab is pressed on an empty message
// Then the server switches it to plan and the status line says so; tab again goes back to build
// And while a turn is running the switch is refused and the refusal shows, the agent unchanged
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tab_switches_build_and_plan_through_the_server() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut provider = spawn_streaming_provider().await;
    let env = Env::start(root, &provider.url).await;
    let mut rt = started(&env).await;
    pick_model(&mut rt, 0);
    type_and_send(&mut rt, "hello");
    let (_, answer) = next_request(&mut rt, &mut provider).await;

    // a turn is running: the server refuses the switch
    // tab switches the agent only on an empty message: the sent one must be cleared first
    until(
        "the turn to run and the editor to clear",
        &mut rt,
        |_, rt| rt.state.activity == Activity::Running && rt.editor.text().is_empty(),
    )
    .await;
    press_tab(&mut rt);
    until("the refusal", &mut rt, |_, rt| {
        command_blocks(&rt.state)
            .iter()
            .any(|(n, ok, _)| n == "agent" && !ok)
    })
    .await;
    assert_eq!(rt.state.agent_name(), "build");
    answer.send(chunk(json!({"content": "Hi."}))).unwrap();
    answer.send(usage_chunk("stop", usage(10, 2))).unwrap();
    drop(answer);
    until("the answer", &mut rt, |_, rt| has_final(rt, "Hi.")).await;

    press_tab(&mut rt);
    until("the switch to plan", &mut rt, |_, rt| {
        rt.state.agent_name() == "plan"
    })
    .await;
    let shown = screen(&mut rt);
    assert!(shown.contains("▌◎ plan"), "{shown}");
    assert!(shown.contains("◎ Nav · idle"), "{shown}");
    let summary = Client::new(env.base.clone(), None)
        .summary(&session_id(&rt))
        .await
        .expect("summary");
    assert_eq!(summary.agent.as_deref(), Some("plan"));

    press_tab(&mut rt);
    until("the switch back", &mut rt, |_, rt| {
        rt.state.agent_name() == "build"
    })
    .await;
}

// Given an alias `alpha` offering `low` and `high`, `high` its default
// When `alpha` is picked with `low` in the variant step and a message is sent
// Then the conversation starts on `low` (the status line and the provider request say so)
// And `/variant` from the palette, moved to `high`, switches it through the server: the status
//      line follows the answer
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn variant_step_and_slash_variant_drive_the_server() {
    let root = tempfile::tempdir().expect("tempdir");
    let provider = spawn_capturing_provider(vec![
        final_answer("On low.", usage(10, 2)),
        final_answer("On high.", usage(10, 2)),
    ])
    .await;
    let variants = json!({
        "low": {"reasoning_effort": "low"},
        "high": {"reasoning_effort": "high"},
    });
    let env = Env::start_with(root, &provider.url, |c| c.variants(variants, Some("high"))).await;
    let mut rt = started(&env).await;
    let alpha = rt
        .state
        .catalog
        .iter()
        .find(|m| m.name == "mock/mock-alpha")
        .expect("alpha")
        .clone();
    assert_eq!(alpha.variants, ["high", "low"]);
    assert_eq!(alpha.default_variant.as_deref(), Some("high"));

    // F2, alpha, then the variant step (high, low, off): the default is preselected; down is `low`.
    press(&mut rt, KeyCode::F(2));
    press(&mut rt, KeyCode::Down);
    press(&mut rt, KeyCode::Enter);
    press(&mut rt, KeyCode::Down);
    press(&mut rt, KeyCode::Enter);
    assert_eq!(rt.state.variant.as_deref(), Some("low"));
    type_and_send(&mut rt, "hello");
    until("the first answer", &mut rt, |_, rt| {
        has_final(rt, "On low.")
    })
    .await;
    let body: serde_json::Value = serde_json::from_str(&provider.bodies()[0]).expect("body");
    assert_eq!(body["reasoning_effort"], "low", "{body}");
    until("the summary's variant", &mut rt, |_, rt| {
        rt.state.session.as_ref().and_then(|m| m.variant.as_deref()) == Some("low")
    })
    .await;
    assert!(screen(&mut rt).contains("· low ·"));

    // /variant from the palette: the list starts on `low`; up is `high`.
    rt.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL,
    )));
    for c in "variant".chars() {
        press(&mut rt, KeyCode::Char(c));
    }
    press(&mut rt, KeyCode::Enter);
    // the list opens on the server's answer: what this conversation offers
    until("the variant list", &mut rt, |_, rt| {
        rt.ui.overlay == Some(withnav::tui::protocol::Overlay::Variants)
    })
    .await;
    assert_eq!(
        withnav::tui::commands::variant_choices(
            &rt.state,
            &withnav::tui::state::VariantPick::Current
        ),
        (
            vec!["high".into(), "low".into(), "off".into(), "inherit".into()],
            1
        )
    );
    press(&mut rt, KeyCode::Up);
    press(&mut rt, KeyCode::Enter);
    until("the switch to high", &mut rt, |_, rt| {
        rt.state.variant.as_deref() == Some("high")
    })
    .await;
    let shown = screen(&mut rt);
    assert!(shown.contains("/variant · ok"), "{shown}");
    assert!(shown.contains("· high ·"), "{shown}");
    type_and_send(&mut rt, "again");
    until("the second answer", &mut rt, |_, rt| {
        has_final(rt, "On high.")
    })
    .await;
    let body: serde_json::Value = serde_json::from_str(&provider.bodies()[1]).expect("body");
    assert_eq!(body["reasoning_effort"], "high", "{body}");

    // typed key by key: `/variant low⏎` switches back
    until("the editor to clear", &mut rt, |_, rt| {
        rt.editor.text().is_empty()
    })
    .await;
    type_keys(&mut rt, "/variant low⏎");
    until("the switch to low", &mut rt, |_, rt| {
        rt.state.variant.as_deref() == Some("low")
    })
    .await;
}

/// One step that calls `write` on `path` with `content`.
fn step_with_write(id: &str, path: &Path, content: &str) -> Vec<u8> {
    let call = tool_call_chunk(id, "write", &json!({ "path": path, "content": content }));
    http_sse(&sse_body(&[
        &call,
        &usage_chunk("tool_calls", usage(50, 5)),
    ]))
}

fn open_request(rt: &Runtime) -> Option<String> {
    rt.state.open_permission().map(|p| p.request_id.clone())
}

// Given a build conversation whose model asks to write a file
// When the prompt appears and the Captain presses o
// Then the file is written, the prompt shows "allowed once" and the turn carries on to the answer
// And when the model asks again and the Captain presses r, types a reason and enter, the file is
//     left alone and the model's next request carries the denial with that reason
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permission_prompt_allows_with_o_and_rejects_with_r_and_a_reason() {
    let root = tempfile::tempdir().expect("tempdir");
    let ws = workspace_of(&root);
    std::fs::create_dir_all(&ws).expect("ws");
    let ws = std::fs::canonicalize(&ws).expect("canonical ws");
    let allowed = ws.join("allowed.txt");
    let denied = ws.join("denied.txt");
    let provider = spawn_capturing_provider(vec![
        step_with_write("w1", &allowed, "hello\nworld"),
        final_answer("Written.", usage(60, 2)),
        step_with_write("w2", &denied, "nope"),
        final_answer("Understood.", usage(70, 2)),
    ])
    .await;
    let env = Env::start(root, &provider.url).await;
    let mut rt = started(&env).await;
    pick_model(&mut rt, 0);

    type_and_send(&mut rt, "write allowed.txt");
    until("the permission prompt", &mut rt, |_, rt| {
        open_request(rt).is_some() && rt.editor.text().is_empty()
    })
    .await;
    assert!(!allowed.exists(), "nothing runs before the answer");
    let shown = screen(&mut rt);
    assert!(shown.contains("wants to run write in"), "{shown}");
    assert!(shown.contains("+ hello"), "{shown}");
    assert!(shown.contains("o once"), "{shown}");
    assert!(shown.contains("permission needed"), "{shown}");

    press(&mut rt, KeyCode::Char('o'));
    until("the answer after the write", &mut rt, |_, rt| {
        has_final(rt, "Written.")
    })
    .await;
    assert!(open_request(&rt).is_none());
    assert_eq!(std::fs::read_to_string(&allowed).unwrap(), "hello\nworld");
    let (_, status, _) = tool_block(&rt, "w1");
    assert_eq!(status, Some(ToolStatus::Ok));
    assert!(screen(&mut rt).contains("allowed once"));

    type_and_send(&mut rt, "now write denied.txt");
    until("the second prompt", &mut rt, |_, rt| {
        open_request(rt).is_some() && rt.editor.text().is_empty()
    })
    .await;
    screen(&mut rt);
    type_keys(&mut rt, "rkeep the folder clean⏎");
    until("the answer after the denial", &mut rt, |_, rt| {
        has_final(rt, "Understood.")
    })
    .await;
    assert!(!denied.exists());
    let (_, status, _) = tool_block(&rt, "w2");
    assert_eq!(status, Some(ToolStatus::Denied));
    let next = &provider.bodies()[3];
    assert!(next.contains("keep the folder clean"), "{next}");
    assert!(next.contains("permission_denied"), "{next}");
    assert!(screen(&mut rt).contains("denied: keep the folder clean"));
}

// Given an open permission request
// When the Captain types a message and presses enter twice
// Then the first enter only warns; the second sends the text, which cancels the request
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_sent_over_an_open_request_cancels_it_after_a_warning() {
    let root = tempfile::tempdir().expect("tempdir");
    let ws = workspace_of(&root);
    std::fs::create_dir_all(&ws).expect("ws");
    let target = std::fs::canonicalize(&ws).expect("canonical").join("a.txt");
    let provider = spawn_capturing_provider(vec![
        step_with_write("w1", &target, "x"),
        final_answer("Fine, I will not.", usage(60, 2)),
    ])
    .await;
    let env = Env::start(root, &provider.url).await;
    let mut rt = started(&env).await;
    pick_model(&mut rt, 0);
    type_and_send(&mut rt, "write a.txt");
    until("the prompt", &mut rt, |_, rt| {
        open_request(rt).is_some() && rt.editor.text().is_empty()
    })
    .await;
    rt.editor.set_text("actually, do not write anything");
    press(&mut rt, KeyCode::Enter);
    assert!(open_request(&rt).is_some());
    assert!(screen(&mut rt).contains("press enter again to send"));
    press(&mut rt, KeyCode::Enter);
    until("the answer", &mut rt, |_, rt| {
        has_final(rt, "Fine, I will not.")
    })
    .await;
    assert!(open_request(&rt).is_none());
    assert!(!target.exists());
    assert!(screen(&mut rt).contains("cancelled"));
}

// Given a workspace that is a git repository on branch `trial` with one changed file
// When a conversation starts
// Then the header shows the branch and one change, read off the reducer by `git`
// And after a turn that adds a file, the count is read again
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_header_shows_the_workspace_branch_and_its_changes() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("git is not installed: skipped");
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let provider = spawn_capturing_provider(vec![
        final_answer("Hello.", usage(10, 2)),
        final_answer("Again.", usage(12, 2)),
    ])
    .await;
    let env = Env::start(root, &provider.url).await;
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&env.ws)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git");
        assert!(out.status.success(), "{args:?}: {out:?}");
    };
    git(&["init", "-q", "-b", "trial"]);
    git(&["add", "notes.txt"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "notes",
    ]);
    std::fs::write(env.ws.join("notes.txt"), "changed\n").expect("edit");
    let mut rt = started(&env).await;
    pick_model(&mut rt, 0);
    type_and_send(&mut rt, "hello");
    until("the git state", &mut rt, |_, rt| {
        rt.state.git.as_ref().is_some_and(|g| g.dirty == 1)
    })
    .await;
    let header = screen(&mut rt)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    assert!(header.contains("trial ●1"), "{header}");

    std::fs::write(env.ws.join("new.txt"), "new\n").expect("new file");
    until("the first answer", &mut rt, |_, rt| has_final(rt, "Hello.")).await;
    type_and_send(&mut rt, "again");
    until("the count read after the turn", &mut rt, |_, rt| {
        has_final(rt, "Again.") && rt.state.git.as_ref().is_some_and(|g| g.dirty == 2)
    })
    .await;
    assert!(screen(&mut rt).contains("trial ●2"));
}

/// A server whose two aliases talk to two providers: `alpha` (provider `a`, 128k) and `luna`
/// (provider `b`, 256k, variants `low` and `high`, default `high`).
async fn two_provider_env(root: tempfile::TempDir, a_url: &str, b_url: &str) -> Env {
    isolate_xdg();
    let config = TestServerConfig::mock(root.path(), a_url)
        .model_alias("alpha", "model-a")
        .chat_tools(128_000)
        .config();
    let entry = |provider: &str, model: &str, window: u64| {
        json!({
            "provider": provider, "model": model, "supports_tools": true,
            "context_window": window, "reasoning_history": "preserve"
        })
    };
    let mut luna = entry("b", "model-b", 256_000);
    luna["variants"] = json!({
        "low": {"reasoning_effort": "low"},
        "high": {"reasoning_effort": "high"}
    });
    luna["default_variant"] = json!("high");
    let provider = |url: &str| {
        json!({"base_url": url, "api_key_env": "WITHNAV_TEST_KEY", "headers": {},
               "connect_timeout_ms": 2000, "request_timeout_ms": 600_000})
    };
    std::fs::write(
        config.models_path.as_ref().expect("models path"),
        json!({
            "providers": {"a": provider(a_url), "b": provider(b_url)},
            "models": {"a/model-a": entry("a", "model-a", 128_000), "b/model-b": luna}
        })
        .to_string(),
    )
    .expect("models.json");
    std::fs::write(
        config.config_dir.join("auth.json"),
        json!({"a": {"api_key": "key-a"}, "b": {"api_key": "key-b"}}).to_string(),
    )
    .expect("auth.json");
    write_test_config(&config.config_dir.join("config.json"), "build", "a/model-a");
    let state = withnav::server::session::AppState::new(config).expect("state");
    let (addr, task) = spawn_server(state).await;
    let ws = workspace_of(&root);
    std::fs::create_dir_all(&ws).expect("workspace dir");
    Env {
        base: format!("http://{addr}"),
        ws,
        task,
        _root: root,
    }
}

// Given a conversation on `alpha` (provider a)
// When the model list switches it to `luna` and the next message is sent
// Then the next request goes to provider b, the header and the tab show luna with its default
//      variant and window, and the transcript shows one switch notice
// And `/model alpha` typed in the editor switches it back, without opening a tab
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_model_list_and_slash_model_switch_the_live_conversation_between_providers() {
    let root = tempfile::tempdir().expect("tempdir");
    let a = spawn_capturing_provider(vec![
        final_answer("From a.", usage(10, 2)),
        final_answer("Back on a.", usage(12, 2)),
    ])
    .await;
    let b = spawn_capturing_provider(vec![final_answer("From b.", usage(11, 2))]).await;
    let env = two_provider_env(root, &a.url, &b.url).await;
    let mut rt = env.runtime();
    rt.start(None);
    until("the model catalog", &mut rt, |_, rt| {
        rt.state.catalog.len() == 2
    })
    .await;
    // on an empty tab, picking starts the conversation on alpha
    pick_model(&mut rt, 0);
    type_and_send(&mut rt, "hello");
    until("the first answer", &mut rt, |_, rt| {
        has_final(rt, "From a.")
    })
    .await;
    assert_eq!(rt.state.selected_model_id.as_deref(), Some("a/model-a"));

    // the model list on a live tab: enter on luna switches the model; the server applies its
    // default variant
    press(&mut rt, KeyCode::F(2));
    let shown = screen(&mut rt);
    assert!(shown.contains("Switch this conversation to…"), "{shown}");
    press(&mut rt, KeyCode::Down);
    press(&mut rt, KeyCode::Down);
    press(&mut rt, KeyCode::Down);
    press(&mut rt, KeyCode::Enter);
    assert_eq!(rt.ui.overlay, None, "no variant step");
    until("the switch", &mut rt, |_, rt| {
        rt.state.selected_model_id.as_deref() == Some("b/model-b")
    })
    .await;
    assert_eq!(rt.state.tabs.len(), 1, "no new tab");
    assert_eq!(rt.state.variant.as_deref(), Some("high"));
    assert_eq!(rt.state.context_window, Some(256_000));
    let header = screen(&mut rt)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    assert!(header.contains("model-b"), "{header}");
    let notices: Vec<_> = rt
        .state
        .conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::Notice(n) if n.starts_with("model:") => Some(n.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(notices, ["model: a/model-a → b/model-b · high · 256k"]);

    type_and_send(&mut rt, "and now?");
    until("the answer from b", &mut rt, |_, rt| {
        has_final(rt, "From b.")
    })
    .await;
    assert_eq!(b.bodies().len(), 1, "the request went to provider b");
    assert!(b.bodies()[0].contains("\"model-b\""), "{}", b.bodies()[0]);
    assert_eq!(a.bodies().len(), 1);

    // `/model alpha` typed in the editor switches back, in the same tab
    type_and_send(&mut rt, "/model a/model-a");
    until("the switch back", &mut rt, |_, rt| {
        rt.state.selected_model_id.as_deref() == Some("a/model-a")
    })
    .await;
    assert_eq!(rt.state.tabs.len(), 1);
    assert_eq!(rt.state.variant, None);
    type_and_send(&mut rt, "again");
    until("the answer from a", &mut rt, |_, rt| {
        has_final(rt, "Back on a.")
    })
    .await;
    assert_eq!(a.bodies().len(), 2);

    // ctrl+n in the model list opens luna in a new tab; the first conversation stays on alpha
    press(&mut rt, KeyCode::F(2));
    press(&mut rt, KeyCode::Down);
    press(&mut rt, KeyCode::Down);
    press(&mut rt, KeyCode::Down);
    rt.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('n'),
        KeyModifiers::CONTROL,
    )));
    press(&mut rt, KeyCode::Enter);
    assert_eq!((rt.state.tabs.len(), rt.state.active), (2, 1));
    assert_eq!(rt.state.selected_model_id.as_deref(), Some("b/model-b"));
    assert!(
        rt.state.session.is_none(),
        "it starts with the first message"
    );
    assert_eq!(
        rt.state.tabs[0].selected_model_id.as_deref(),
        Some("a/model-a")
    );
}

// Given a conversation on `alpha` whose answer is still streaming
// When `/model luna` is typed
// Then the server accepts it for after the turn: the header shows "→ luna (after this turn)"
//      beside alpha; once the turn ends the conversation is on luna and the mark is gone
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_typed_during_a_turn_waits_for_it_and_shows_in_the_header() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut a = spawn_streaming_provider().await;
    let b = spawn_capturing_provider(vec![]).await;
    let env = two_provider_env(root, &a.url, &b.url).await;
    let mut rt = env.runtime();
    rt.start(None);
    until("the model catalog", &mut rt, |_, rt| {
        rt.state.catalog.len() == 2
    })
    .await;
    pick_model(&mut rt, 0);
    type_and_send(&mut rt, "a long one");
    let (_, answer) = next_request(&mut rt, &mut a).await;
    answer.send(chunk(json!({"content": "working"}))).unwrap();
    until("the turn running", &mut rt, |_, rt| {
        rt.state.activity == Activity::Running
    })
    .await;

    type_and_send(&mut rt, "/model b/model-b");
    until("the acceptance", &mut rt, |_, rt| {
        rt.state.pending_model() == Some("b/model-b")
    })
    .await;
    assert_eq!(rt.state.selected_model_id.as_deref(), Some("a/model-a"));
    let header = screen(&mut rt)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    assert!(header.contains("→ b/model-b (after this turn)"), "{header}");
    assert!(header.contains("model-a"), "{header}");

    answer.send(usage_chunk("stop", usage(10, 2))).unwrap();
    drop(answer);
    until("the switch after the turn", &mut rt, |_, rt| {
        rt.state.selected_model_id.as_deref() == Some("b/model-b")
            && rt.state.pending_model().is_none()
    })
    .await;
    let header = screen(&mut rt)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    assert!(!header.contains("after this turn"), "{header}");
    assert!(header.contains("model-b"), "{header}");
}
