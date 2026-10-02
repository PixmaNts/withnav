use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use navai::{
    tools::ToolStatus,
    tui::{
        input::{route_key, route_paste, Editor},
        protocol::{Action, ModelEntry, Overlay, SessionListEntry, SessionMetadata},
        state::{Activity, Block, BlockKind, ConnectionStatus, State, UiState},
        theme::{Theme, ThemeId},
        view::{self, tool, RenderCache},
    },
};
use ratatui::{backend::TestBackend, Terminal};

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
        message("user", "user:http", "Explain the failure, then fix it."),
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
        message("u", "user:http", "Why is this failing?"),
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
        .any(|cell| cell.bg == Theme::ayu_dark().selection_bg()));
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
        .any(|cell| cell.fg == Theme::ayu_dark().red));
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
        (Overlay::Themes, "overlay-themes"),
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
        message("u", "user:http", "你好 e\u{301} 👩‍💻 café"),
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
        Some(Action::SubmitAt {
            text: "hello".into(),
            revision
        })
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
    use navai::tui::state::Focus;
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

fn buffer_text(s: &State) -> String {
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    terminal
        .draw(|f| view::draw(f, s, &ui, &Editor::new(), &mut RenderCache::default()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..20)
        .map(|y| (0..60).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn message_labels_follow_the_source_actor_id() {
    for (source, label) in [
        ("user:http", "YOU"),
        ("user", "YOU"),
        ("navigator", "NAVIGATOR"),
        ("navigator:main", "NAVIGATOR"),
        ("cron:nightly", "cron:nightly"),
    ] {
        let mut s = state();
        s.conversation.blocks = vec![message("m", source, "hi")];
        let text = buffer_text(&s);
        assert!(text.contains(label), "{source}: {text}");
        if label != "NAVIGATOR" {
            assert!(!text.contains("NAVIGATOR"), "{source}: {text}");
        }
    }
}

#[test]
fn tool_only_step_renders_no_assistant_label() {
    let mut s = state();
    s.conversation.blocks = vec![
        message("u", "user:http", "go"),
        step("", ""),
        tool_block(Some(ToolStatus::Ok)),
    ];
    let text = buffer_text(&s);
    assert!(!text.contains("ASSISTANT"), "{text}");
    assert!(text.contains("bash"), "{text}");
}

fn cells(s: &State, ui: &UiState, size: (u16, u16)) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
    terminal
        .draw(|f| view::draw(f, s, ui, &Editor::new(), &mut RenderCache::default()))
        .unwrap();
    terminal
}

#[test]
fn every_theme_paints_its_backgrounds() {
    let s = done();
    for id in ThemeId::ALL {
        let theme = id.palette();
        let ui = UiState {
            follow_bottom: true,
            theme: id,
            ..UiState::default()
        };
        let terminal = cells(&s, &ui, (100, 30));
        let buffer = terminal.backend().buffer();
        // Header row on the panel colour, transcript on the main background.
        assert_eq!(buffer[(50, 0)].bg, theme.panel, "{id:?} header");
        assert_eq!(buffer[(50, 5)].bg, theme.bg, "{id:?} transcript");
        assert_eq!(buffer[(50, 29)].bg, theme.panel, "{id:?} key hints");
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| cell.bg != ratatui::style::Color::Reset),
            "{id:?}: every cell has a theme background"
        );
        // The tool summary is a code reference and its duration a number.
        let row = (0..30)
            .find(|y| {
                (0..100)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("cargo test parser")
            })
            .expect("tool row");
        let line: Vec<_> = (0..100).map(|x| &buffer[(x, row)]).collect();
        let at = |needle: &str| {
            let text: String = line.iter().map(|c| c.symbol()).collect();
            let byte = text.find(needle).unwrap();
            text[..byte].chars().count()
        };
        assert_eq!(line[at("cargo")].fg, theme.blue, "{id:?} code ref");
        assert_eq!(line[at("1.42")].fg, theme.orange, "{id:?} number");
    }
}

#[test]
fn overlays_sit_on_the_surface_with_an_accent_border() {
    let s = done();
    let ui = UiState {
        overlay: Some(Overlay::Themes),
        follow_bottom: true,
        theme: ThemeId::NordLight,
        ..UiState::default()
    };
    let terminal = cells(&s, &ui, (100, 30));
    let buffer = terminal.backend().buffer();
    let theme = Theme::nord_light();
    let corner = buffer
        .content()
        .iter()
        .find(|cell| cell.symbol() == "╭" || cell.symbol() == "┌")
        .expect("popup corner");
    assert_eq!(corner.fg, theme.accent);
    assert!(buffer.content().iter().any(|cell| cell.bg == theme.surface));
    assert!(buffer
        .content()
        .iter()
        .any(|cell| cell.bg == theme.selection_bg_on_surface()));
}

#[test]
fn theme_picker_previews_and_esc_restores() {
    let s = state();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    let apply = |ui: &mut UiState, action: Option<Action>| {
        if let Some(action) = action {
            navai::tui::reducer::handle_action(&mut state(), ui, action)
        } else {
            vec![]
        }
    };
    let open = Some(Action::OpenOverlay(Overlay::Themes));
    apply(&mut ui, open.clone());
    assert_eq!(ui.selection.get("Themes"), Some(&0));
    let down = route_key(
        key(KeyCode::Down, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(down, Some(Action::PreviewTheme(ThemeId::AyuLight)));
    apply(&mut ui, down);
    assert_eq!(
        ui.theme,
        ThemeId::AyuLight,
        "the highlighted theme is drawn"
    );
    let esc = route_key(
        key(KeyCode::Esc, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(apply(&mut ui, esc), vec![]);
    assert_eq!(ui.theme, ThemeId::AyuDark, "esc puts the old theme back");
    assert_eq!(ui.overlay, None);

    apply(&mut ui, open);
    for _ in 0..3 {
        let action = route_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor,
        );
        apply(&mut ui, action);
    }
    assert_eq!(ui.theme, ThemeId::NordLight);
    let enter = route_key(
        key(KeyCode::Enter, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(enter, Some(Action::PickTheme(ThemeId::NordLight)));
    assert_eq!(
        apply(&mut ui, enter),
        vec![navai::tui::protocol::Effect::SaveTheme(ThemeId::NordLight)]
    );
    assert_eq!(ui.theme, ThemeId::NordLight);
    assert_eq!(ui.overlay, None);
    assert_eq!(ui.theme_on_open, None);
}

#[test]
fn theme_picker_opens_on_the_current_theme_and_the_palette_lists_it() {
    let mut ui = UiState {
        theme: ThemeId::NordDark,
        ..UiState::default()
    };
    let mut s = state();
    navai::tui::reducer::handle_action(&mut s, &mut ui, Action::OpenOverlay(Overlay::Themes));
    assert_eq!(ui.selection.get("Themes"), Some(&2));
    // Confirming the theme already in effect saves it again, so a failed save can be retried.
    assert_eq!(
        navai::tui::reducer::handle_action(&mut s, &mut ui, Action::PickTheme(ThemeId::NordDark)),
        vec![navai::tui::protocol::Effect::SaveTheme(ThemeId::NordDark)]
    );
    assert_eq!(
        navai::tui::commands::command_action("/theme", Default::default()),
        Some(Action::OpenOverlay(Overlay::Themes))
    );
}

/// Sends `code` to the open theme picker and applies the action it returns, like the runtime.
fn theme_key(ui: &mut UiState, code: KeyCode) -> Option<Action> {
    let s = state();
    let action = route_key(key(code, KeyModifiers::NONE), &s, ui, &mut Editor::new());
    if let Some(action) = action.clone() {
        navai::tui::reducer::handle_action(&mut state(), ui, action);
    }
    action
}

/// The theme the picker highlights, as drawn: the row carrying the `▌` mark.
fn highlighted_theme(ui: &UiState) -> Option<ThemeId> {
    let terminal = cells(&state(), ui, (100, 30));
    let buffer = terminal.backend().buffer();
    (0..30).find_map(|y| {
        let row: String = (0..100).map(|x| buffer[(x, y)].symbol()).collect();
        row.contains('▌')
            .then(|| ThemeId::ALL.into_iter().find(|t| row.contains(t.id())))
            .flatten()
    })
}

#[test]
fn theme_filter_previews_the_highlighted_row() {
    let mut ui = UiState::default();
    navai::tui::reducer::handle_action(&mut state(), &mut ui, Action::OpenOverlay(Overlay::Themes));
    // Typing `n` from Ayu Dark: the first match is Nord Dark, and that is what is drawn.
    assert_eq!(
        theme_key(&mut ui, KeyCode::Char('n')),
        Some(Action::PreviewTheme(ThemeId::NordDark))
    );
    assert_eq!(ui.theme, ThemeId::NordDark);
    assert_eq!(highlighted_theme(&ui), Some(ui.theme));
    theme_key(&mut ui, KeyCode::Char('o'));
    theme_key(&mut ui, KeyCode::Char('r'));
    theme_key(&mut ui, KeyCode::Char('d'));
    theme_key(&mut ui, KeyCode::Char('-'));
    theme_key(&mut ui, KeyCode::Char('l'));
    assert_eq!(ui.theme, ThemeId::NordLight);
    assert_eq!(highlighted_theme(&ui), Some(ThemeId::NordLight));
    // No match: nothing is highlighted and the theme on screen stays.
    assert_eq!(theme_key(&mut ui, KeyCode::Char('x')), None);
    assert_eq!(ui.theme, ThemeId::NordLight);
    assert_eq!(highlighted_theme(&ui), None);
    assert_eq!(theme_key(&mut ui, KeyCode::Enter), None, "no row to pick");
    // Backspace back to `nord-l`, then to `nord-`: the first match is highlighted again.
    theme_key(&mut ui, KeyCode::Backspace);
    assert_eq!(ui.theme, ThemeId::NordLight);
    theme_key(&mut ui, KeyCode::Backspace);
    assert_eq!(ui.theme, ThemeId::NordDark);
    assert_eq!(highlighted_theme(&ui), Some(ThemeId::NordDark));
    assert_eq!(
        theme_key(&mut ui, KeyCode::Enter),
        Some(Action::PickTheme(ThemeId::NordDark)),
        "enter picks the theme that was previewed"
    );
}

#[test]
fn running_tool_label_uses_the_readable_accent() {
    let mut s = state();
    let mut block = tool_block(None);
    if let BlockKind::ToolCall { duration_ms, .. } = &mut block.kind {
        *duration_ms = None;
    }
    s.conversation.blocks = vec![block];
    let theme = Theme::ayu_light();
    let ui = UiState {
        follow_bottom: true,
        theme: ThemeId::AyuLight,
        ..UiState::default()
    };
    let terminal = cells(&s, &ui, (100, 30));
    let buffer = terminal.backend().buffer();
    let (x, y) = (0..30)
        .find_map(|y| {
            let row: String = (0..100).map(|x| buffer[(x, y)].symbol()).collect();
            row.find("running")
                .map(|byte| (row[..byte].chars().count() as u16, y))
        })
        .expect("running label");
    assert_eq!(buffer[(x, y)].fg, theme.accent_text);
    let spinner = (0..x)
        .map(|x| &buffer[(x, y)])
        .find(|cell| cell.symbol() == "◐" || cell.symbol() == "◓")
        .expect("spinner");
    assert_eq!(
        spinner.fg, theme.accent,
        "the glyph keeps the bright accent"
    );
}
