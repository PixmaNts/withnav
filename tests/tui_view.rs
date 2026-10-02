use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    backend::TestBackend,
    style::{Color, Modifier},
    Terminal,
};
use rust_agent::{
    tools::ToolStatus,
    tui::{
        input::{route_key, route_paste, Editor},
        protocol::{Action, ModelEntry, Overlay, SessionListEntry, SessionMetadata},
        state::{Activity, Block, BlockKind, ConnectionStatus, State, UiState},
        view::{self, tool, RenderCache},
    },
};

fn message(id: &str, source: &str, text: &str) -> Block {
    Block {
        id: id.into(),
        kind: BlockKind::Message {
            source: source.into(),
            text: text.into(),
        },
    }
}
fn step(text: &str, thinking: &str) -> Block {
    Block {
        id: "step".into(),
        kind: BlockKind::AssistantStep {
            text: text.into(),
            thinking: thinking.into(),
            revision: 1,
        },
    }
}
fn tool_block(status: Option<ToolStatus>) -> Block {
    Block {
        id: "tool".into(),
        kind: BlockKind::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            args: serde_json::json!({"command":"cargo test parser"}),
            status,
            output: "test parser::empty_argument ... FAILED\nexpected Some(\"\"), got None".into(),
            metadata: serde_json::Value::Null,
            duration_ms: Some(1420),
            exit_code: Some(101),
        },
    }
}
fn state() -> State {
    State {
        connection: ConnectionStatus::Connected,
        selected_model_alias: Some("cognition".into()),
        selected_model_name: Some("cc/claude-opus-5-5".into()),
        context_window: Some(200_000),
        session: Some(SessionMetadata {
            session_id: "01JABCDEFGH".into(),
            actor_state: "idle".into(),
            workspace: "/home/pixma/dev/project".into(),
            model: "cognition".into(),
            current_seq: 42,
        }),
        catalog: vec![
            ModelEntry {
                alias: "cognition".into(),
                model: "cc/claude-opus-5-5".into(),
                context_window: Some(200_000),
            },
            ModelEntry {
                alias: "fast".into(),
                model: "cc/flash".into(),
                context_window: None,
            },
        ],
        sessions: vec![SessionListEntry {
            id: "01JABCDEFGH".into(),
            title: Some("Fix parser".into()),
            created_at: "2026-10-01T08:00:00Z".into(),
            updated_at: "2026-10-02T10:00:00Z".into(),
            message_count: 3,
            live: true,
        }],
        ..State::default()
    }
}
fn done() -> State {
    let mut s = state();
    s.conversation.blocks = vec![
        message("user", "user", "Explain the failure, then fix it."),
        step(
            "The parser treats an empty argument as a missing value.",
            "I should check the parser tests first.",
        ),
        tool_block(Some(ToolStatus::Error)),
        message(
            "end",
            "assistant",
            "I found the failing test. **The empty argument** is discarded.",
        ),
    ];
    s.conversation.context_tokens = Some(12_400);
    s.conversation.session_tokens = Some(31_200);
    s
}
fn render(name: &str, size: (u16, u16), state: &State, ui: &UiState, editor: &Editor) -> String {
    let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
    let mut cache = RenderCache::default();
    terminal
        .draw(|frame| view::draw(frame, state, ui, editor, &mut cache))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let snapshot = (0..size.1)
        .map(|y| {
            (0..size.0)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(name, snapshot);
    snapshot
}
#[test]
fn empty_and_streaming() {
    let editor = Editor::new();
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    render("empty-100", (100, 30), &state(), &ui, &editor);
    render("empty-60", (60, 20), &state(), &ui, &editor);
    let mut s = state();
    s.activity = Activity::Running;
    s.conversation.blocks = vec![
        message("u", "user", "Why is this failing?"),
        step(
            "Checking **the parser** as tokens arrive…",
            "First inspect the call site.",
        ),
    ];
    render("streaming-100", (100, 30), &s, &ui, &editor);
    render("streaming-60", (60, 20), &s, &ui, &editor);
}
#[test]
fn done_tools_and_reasoning() {
    let s = done();
    let editor = Editor::new();
    let mut ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    render("done-collapsed", (100, 30), &s, &ui, &editor);
    ui.expanded.insert("tool".into());
    render("done-expanded-tool-100", (100, 30), &s, &ui, &editor);
    render("done-expanded-tool-60", (60, 20), &s, &ui, &editor);
    ui.expanded.insert("step".into());
    render("done-expanded-reasoning", (100, 30), &s, &ui, &editor);
    ui.selected_block = Some("tool".into());
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|f| view::draw(f, &s, &ui, &editor, &mut RenderCache::default()))
        .unwrap();
    assert!(terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .any(|cell| cell.style().add_modifier.contains(Modifier::REVERSED)));
}
#[test]
fn errors_yield_and_connection() {
    let mut s = state();
    s.activity = Activity::AwaitingUser;
    s.conversation.blocks = vec![
        Block {
            id: "err".into(),
            kind: BlockKind::Error("Permission denied".into()),
        },
        Block {
            id: "question".into(),
            kind: BlockKind::YieldQuestion("Choose a file".into()),
        },
        Block {
            id: "notice".into(),
            kind: BlockKind::Notice("Mode downgraded to next-turn".into()),
        },
    ];
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    let editor = Editor::new();
    render("error-yield", (100, 30), &s, &ui, &editor);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|f| view::draw(f, &s, &ui, &editor, &mut RenderCache::default()))
        .unwrap();
    assert!(terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .any(|cell| cell.fg == Color::Red));
    s.connection = ConnectionStatus::Reconnecting;
    s.activity = Activity::Stopped;
    s.delivery_uncertain = true;
    render("stopped-reconnecting", (60, 20), &s, &ui, &editor);
}
#[test]
fn overlays() {
    let s = done();
    let editor = Editor::new();
    for (overlay, name) in [
        (Overlay::Models, "overlay-models"),
        (Overlay::Sessions, "overlay-sessions"),
        (Overlay::Commands, "overlay-commands"),
        (Overlay::Help, "overlay-help"),
    ] {
        let ui = UiState {
            overlay: Some(overlay),
            follow_bottom: true,
            ..UiState::default()
        };
        render(name, (100, 30), &s, &ui, &editor);
    }
}
#[test]
fn unicode_paste_paths_and_unclosed_fence() {
    let mut s = state();
    s.session.as_mut().unwrap().workspace =
        "/a/very/long/path/with/segments/and/日本語/projects/the-last-directory".into();
    s.conversation.blocks = vec![
        message("u", "user", "你好 e\u{301} 👩‍💻 café"),
        step("```rust\nfn main() { println!(\"hi\"); }", ""),
    ];
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    let mut editor = Editor::new();
    route_paste("line one\nline two".into(), &ui, &mut editor);
    assert_eq!(editor.text(), "line one\nline two");
    render("unicode-paste-fence", (60, 20), &s, &ui, &editor);
    render("tiny", (20, 6), &s, &ui, &editor);
}
fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}
#[test]
fn editor_routes_submission_and_newline() {
    let s = state();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    editor.set_text("hello");
    let revision = editor.revision();
    assert_eq!(
        route_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        ),
        Some(Action::Submit("hello".into()))
    );
    assert_eq!(editor.text(), "hello");
    assert_eq!(editor.revision(), revision);
    route_key(
        key(KeyCode::Enter, KeyModifiers::ALT),
        &s,
        &mut ui,
        &mut editor,
    );
    assert!(editor.text().contains('\n'));
    assert!(editor.revision() > revision);
    route_key(
        key(KeyCode::Char('j'), KeyModifiers::CONTROL),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(editor.text().matches('\n').count(), 2);
}
#[test]
fn overlay_precedence_and_empty_slash() {
    let s = state();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    assert_eq!(
        route_key(
            key(KeyCode::Char('/'), KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        ),
        Some(Action::OpenOverlay(Overlay::Commands))
    );
    assert!(editor.text().is_empty());
    assert_eq!(
        route_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        ),
        Some(Action::NewConversation)
    );
    editor.set_text("abc");
    assert_eq!(
        route_key(
            key(KeyCode::Char('/'), KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        ),
        None
    );
    assert_eq!(editor.text(), "/abc");
    ui.overlay = Some(Overlay::Models);
    assert_eq!(
        route_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        ),
        Some(Action::PickModel("cognition".into()))
    );
    assert_eq!(editor.text(), "/abc");
}
#[test]
fn tool_summaries_and_durations() {
    assert_eq!(
        tool::summary("read", &serde_json::json!({"path":"src/main.rs"})),
        "src/main.rs"
    );
    assert_eq!(
        tool::summary("bash", &serde_json::json!({"command":"cargo test"})),
        "cargo test"
    );
    assert_eq!(
        tool::summary("grep", &serde_json::json!({"pattern":"foo"})),
        "foo"
    );
    assert_eq!(tool::duration(None), "—");
    assert_eq!(tool::duration(Some(18)), "18 ms");
    assert_eq!(tool::duration(Some(1420)), "1.42 s");
}

#[test]
fn transcript_keys_select_expand_and_follow() {
    use rust_agent::tui::state::Focus;
    let s = done();
    let mut ui = UiState {
        focus: Focus::Transcript,
        follow_bottom: true,
        ..UiState::default()
    };
    let mut editor = Editor::new();
    route_key(
        key(KeyCode::Up, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(ui.selected_block.as_deref(), Some("tool"));
    assert_eq!(
        route_key(
            key(KeyCode::Char(' '), KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        ),
        Some(Action::ToggleBlock("tool".into()))
    );
    route_key(
        key(KeyCode::End, KeyModifiers::CONTROL),
        &s,
        &mut ui,
        &mut editor,
    );
    assert!(ui.follow_bottom);
    route_key(
        key(KeyCode::PageUp, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert!(!ui.follow_bottom);
}

#[test]
fn pickers_filter_and_keep_draft() {
    let s = state();
    let mut ui = UiState {
        overlay: Some(Overlay::Models),
        ..UiState::default()
    };
    let mut editor = Editor::new();
    editor.set_text("draft");
    route_key(
        key(KeyCode::Char('f'), KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(ui.filter, "f");
    assert_eq!(
        route_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        ),
        Some(Action::PickModel("fast".into()))
    );
    assert_eq!(editor.text(), "draft");
    ui.overlay = Some(Overlay::Sessions);
    route_key(
        key(KeyCode::Char('x'), KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(
        route_key(
            key(KeyCode::Esc, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        ),
        Some(Action::CloseOverlay)
    );
    assert_eq!(editor.text(), "draft");
}
