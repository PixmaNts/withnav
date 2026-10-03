use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use navai::{
    tools::ToolStatus,
    tui::{
        input::{route_key, route_paste, Editor},
        protocol::{Action, ModelEntry, Overlay, SessionListEntry, SessionMetadata},
        state::{Activity, Block, BlockKind, ConnectionStatus, State, Tab, UiState},
        theme::{Theme, ThemeId},
        view::{self, tool, RenderCache},
    },
};
use ratatui::{backend::TestBackend, Terminal};
use serde_json::json;

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
    let tab = Tab {
        connection: ConnectionStatus::Connected,
        selected_model_alias: Some("cognition".into()),
        selected_model_name: Some("cc/claude-opus-5-5".into()),
        context_window: Some(200_000),
        session: Some(SessionMetadata {
            session_id: "01JABCDEFGH".into(),
            actor_state: "idle".into(),
            workspace: "/home/pixma/dev/project".into(),
            model: "cognition".into(),
            agent: None,
            variant: None,
            current_seq: 42,
        }),
        ..Tab::default()
    };
    State {
        tabs: vec![tab],
        catalog: vec![
            ModelEntry {
                alias: "cognition".into(),
                model: "cc/claude-opus-5-5".into(),
                context_window: Some(200_000),
                ..Default::default()
            },
            ModelEntry {
                alias: "fast".into(),
                model: "cc/flash".into(),
                context_window: None,
                ..Default::default()
            },
        ],
        sessions: vec![SessionListEntry {
            id: "01JABCDEFGH".into(),
            title: Some("Fix parser".into()),
            created_at: "2026-10-01T08:00:00Z".into(),
            updated_at: "2026-10-02T10:00:00Z".into(),
            message_count: 3,
            agent: None,
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
    let mut ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    let mut editor = Editor::new();
    route_paste("line one\nline two".into(), &s, &mut ui, &mut editor);
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
    assert_eq!(editor.text(), "/", "the slash is typed into the editor");
    assert_eq!(
        route_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        ),
        Some(Action::NewConversation)
    );
    assert!(
        editor.text().is_empty(),
        "a client command clears the line it ran from"
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
    let enter = key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(route_key(enter, &s, &mut ui, &mut editor), None);
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
    let enter = key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        route_key(enter, &s, &mut ui, &mut editor),
        None,
        "the variant step"
    );
    assert_eq!(ui.overlay, Some(Overlay::Variants));
    assert_eq!(
        route_key(enter, &s, &mut ui, &mut editor),
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

#[test]
fn the_status_gauge_shows_the_server_estimate_with_its_percent() {
    let mut s = done();
    // Before any snapshot: the provider's last prompt count over the window.
    assert!(buffer_text(&s).contains("· 12.4k / 200k"));
    s.conversation.context = Some(navai::bus::event::ContextSnapshot {
        alias: "cognition".into(),
        variant: None,
        window: 1_000_000,
        window_source: "models.json".into(),
        estimated: true,
        estimate_total: 42_000,
        breakdown: Default::default(),
        output_reserve: 8192,
        margin: 50_000,
        input_budget: 941_808,
        percent: 4,
        last_usage: None,
        skills: Default::default(),
        rule_files: 0,
        incarnation: ulid::Ulid::new(),
        revision: 1,
    });
    let text = buffer_text(&s);
    assert!(text.contains("· ~42k / 1M · 4%"), "{text}");
    assert!(!text.contains("12.4k"), "{text}");
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

#[test]
fn markdown_repaints_when_the_theme_changes_and_back() {
    let mut s = state();
    s.conversation.blocks = vec![message(
        "md",
        "assistant",
        "## Heading\n\n```rust\nfn main() {}\n```",
    )];
    let editor = Editor::new();
    let mut cache = RenderCache::default();
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    let mut draw = |id: ThemeId| {
        let ui = UiState {
            follow_bottom: true,
            theme: id,
            ..UiState::default()
        };
        terminal
            .draw(|f| view::draw(f, &s, &ui, &editor, &mut cache))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let find = |needle: &str| {
            (0..20)
                .find_map(|y| {
                    let row: String = (0..60).map(|x| buffer[(x, y)].symbol()).collect();
                    row.find(needle)
                        .map(|byte| buffer[(row[..byte].chars().count() as u16, y)].clone())
                })
                .expect(needle)
        };
        (find("Heading").fg, find("main").fg, find("main").bg)
    };
    let dark = draw(ThemeId::AyuDark);
    let light = draw(ThemeId::AyuLight);
    let back = draw(ThemeId::AyuDark);
    assert_eq!(dark.0, Theme::ayu_dark().accent_text);
    assert_eq!(light.0, Theme::ayu_light().accent_text);
    assert_ne!(dark.1, light.1, "code is highlighted with the light scheme");
    assert_eq!(back, dark, "going back repaints exactly as before");
}

#[test]
fn tab_keys_switch_close_and_hide_the_bar() {
    let s = state();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    editor.set_text("draft");
    let alt = |c| key(KeyCode::Char(c), KeyModifiers::ALT);
    assert_eq!(
        route_key(alt('1'), &s, &mut ui, &mut editor),
        Some(Action::SwitchTab(0))
    );
    assert_eq!(
        route_key(alt('9'), &s, &mut ui, &mut editor),
        Some(Action::SwitchTab(8))
    );
    assert_eq!(
        route_key(alt('w'), &s, &mut ui, &mut editor),
        Some(Action::CloseTab)
    );
    assert_eq!(
        route_key(alt('b'), &s, &mut ui, &mut editor),
        Some(Action::ToggleSidebar)
    );
    assert_eq!(editor.text(), "draft", "tab keys type nothing");
    // plain digits still type
    route_key(
        key(KeyCode::Char('1'), KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(editor.text(), "1draft");
    let help: Vec<_> = navai::tui::commands::KEYS.iter().map(|(k, _)| *k).collect();
    for wanted in ["alt+1 … alt+9", "alt+w", "alt+b"] {
        assert!(help.contains(&wanted), "{help:?}");
    }
}

/// Three tabs: the active "Fix parser", a running one with news, and a blank one.
fn three_tabs() -> State {
    let mut s = done();
    let mut running = s.tabs[0].clone();
    running.session.as_mut().unwrap().session_id = "01JZZZZZZZZZZ".into();
    running.activity = Activity::Running;
    running.unread = true;
    running.conversation.blocks.clear();
    let blank = Tab::default();
    s.tabs.extend([running, blank]);
    s
}

fn row_text(t: &Terminal<TestBackend>, y: u16, width: u16) -> String {
    (0..width)
        .map(|x| t.backend().buffer()[(x, y)].symbol())
        .collect()
}

#[test]
fn sidebar_lists_tabs_with_state_unread_and_active_tint() {
    let s = three_tabs();
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    render("sidebar-three-tabs", (100, 24), &s, &ui, &Editor::new());
    let t = cells(&s, &ui, (100, 24));
    let theme = Theme::ayu_dark();
    assert!(row_text(&t, 1, 22).starts_with("▌1 Fix parser"));
    assert!(row_text(&t, 2, 22).contains("build · idle"));
    assert!(
        row_text(&t, 3, 22).contains("2 01JZZZZZ"),
        "short id for an untitled tab"
    );
    assert!(row_text(&t, 3, 22).contains('●'), "unread mark");
    assert!(row_text(&t, 4, 22).contains("build · running"));
    assert!(row_text(&t, 5, 22).contains("3 new conversation"));
    assert!(row_text(&t, 23, 22).contains("alt+b hide bar"));
    let buffer = t.backend().buffer();
    // the active rows carry the accent tint and the accent bar; the others the panel
    let tint = theme.selection_bg_on_panel();
    for y in [1, 2] {
        assert_eq!(buffer[(5, y)].bg, tint, "row {y}");
        assert_eq!(buffer[(0, y)].fg, theme.accent, "bar on row {y}");
        assert_eq!(buffer[(0, y)].symbol(), "▌");
    }
    for y in [3, 4] {
        assert_eq!(buffer[(5, y)].bg, theme.panel, "row {y}");
        assert_ne!(buffer[(0, y)].symbol(), "▌");
    }
    // states and the unread mark use theme roles
    let running_x = row_text(&t, 4, 22).find("running").unwrap();
    let running_x = row_text(&t, 4, 22)[..running_x].chars().count() as u16;
    assert_eq!(buffer[(running_x, 4)].fg, theme.accent_text);
    let dot_x = row_text(&t, 3, 22).chars().position(|c| c == '●').unwrap() as u16;
    assert_eq!(buffer[(dot_x, 3)].fg, theme.accent);
    // the transcript starts right of the bar
    assert_eq!(buffer[(21, 1)].symbol(), "│");
}

#[test]
fn sidebar_hides_on_demand_and_on_narrow_terminals() {
    let s = three_tabs();
    let mut ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    let narrow = cells(&s, &ui, (79, 24));
    assert!(
        !row_text(&narrow, 1, 79).contains("Fix parser · "),
        "header unchanged"
    );
    assert!(
        !(0..24).any(|y| row_text(&narrow, y, 79).contains("build · ")),
        "no sidebar under 80 columns"
    );
    let wide = cells(&s, &ui, (80, 24));
    assert!(row_text(&wide, 2, 80).contains("build · idle"));
    ui.sidebar_hidden = true;
    render("sidebar-hidden", (100, 24), &s, &ui, &Editor::new());
    let hidden = cells(&s, &ui, (100, 24));
    assert!(row_text(&hidden, 0, 100).starts_with(" navai"));
    assert!(!(0..24).any(|y| row_text(&hidden, y, 100).contains("build · ")));
}

#[test]
fn sidebar_shows_waiting_and_error_states() {
    let mut s = three_tabs();
    s.tabs[1].activity = Activity::AwaitingUser;
    s.tabs[2].conversation.blocks = vec![Block {
        id: "e".into(),
        kind: BlockKind::Error("provider: 429".into()),
    }];
    let ui = UiState::default();
    let t = cells(&s, &ui, (100, 24));
    let theme = Theme::ayu_dark();
    assert!(row_text(&t, 4, 22).contains("build · waiting"));
    assert!(row_text(&t, 6, 22).contains("build · error"));
    let err_x = row_text(&t, 6, 22).find("error").unwrap();
    let err_x = row_text(&t, 6, 22)[..err_x].chars().count() as u16;
    assert_eq!(t.backend().buffer()[(err_x, 6)].fg, theme.red);
}

#[test]
fn the_gauge_follows_the_active_tab() {
    let gauge = |estimate_total: u64| navai::bus::event::ContextSnapshot {
        alias: "cognition".into(),
        variant: None,
        window: 1_000_000,
        window_source: "models.json".into(),
        estimated: true,
        estimate_total,
        breakdown: Default::default(),
        output_reserve: 8192,
        margin: 50_000,
        input_budget: 941_808,
        percent: estimate_total * 100 / 1_000_000,
        last_usage: None,
        skills: Default::default(),
        rule_files: 0,
        incarnation: ulid::Ulid::new(),
        revision: 1,
    };
    let mut s = done();
    s.conversation.context = Some(gauge(42_000));
    let mut background = s.tabs[0].clone();
    background.conversation.context = Some(gauge(120_000));
    s.tabs.push(background);
    // Wide enough for the sidebar and the whole status line.
    let screen = |s: &State| {
        let t = cells(s, &UiState::default(), (120, 24));
        (0..24)
            .map(|y| row_text(&t, y, 120))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let text = screen(&s);
    assert!(text.contains("· ~42k / 1M · 4%"), "{text}");
    assert!(!text.contains("120k"), "{text}");
    s.active = 1;
    let text = screen(&s);
    assert!(text.contains("· ~120k / 1M · 12%"), "{text}");
}

#[test]
fn tabs_with_the_same_block_id_render_their_own_text_and_height() {
    let mut s = state();
    s.tabs[0].generation = navai::tui::protocol::Generation(1);
    s.tabs[0].conversation.blocks = vec![message("7", "assistant", "first tab line")];
    let mut other = s.tabs[0].clone();
    other.generation = navai::tui::protocol::Generation(2);
    other.conversation.blocks = vec![message(
        "7",
        "assistant",
        "second tab one\n\nsecond tab two\n\nsecond tab three",
    )];
    s.tabs.push(other);
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    let mut cache = RenderCache::default();
    let mut screen = |s: &State| {
        let mut t = Terminal::new(TestBackend::new(100, 24)).unwrap();
        t.draw(|f| view::draw(f, s, &ui, &Editor::new(), &mut cache))
            .unwrap();
        (0..24)
            .map(|y| row_text(&t, y, 100))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let first = screen(&s);
    assert!(first.contains("first tab line"), "{first}");
    s.active = 1;
    let second = screen(&s);
    assert!(!second.contains("first tab line"), "{second}");
    for line in ["second tab one", "second tab two", "second tab three"] {
        assert!(second.contains(line), "{second}");
    }
    s.active = 0;
    let again = screen(&s);
    assert!(again.contains("first tab line"), "{again}");
    assert!(!again.contains("second tab"), "{again}");
}

#[test]
fn a_short_sidebar_scrolls_to_keep_the_active_tab_in_view() {
    let mut s = done();
    for n in 2..=9 {
        s.tabs.push(Tab {
            pending_submit_text: Some(format!("tab {n}")),
            ..Tab::default()
        });
    }
    s.active = 8;
    let ui = UiState::default();
    // 14 rows: the title row and 4 key rows leave room for 4 tabs of 2 rows
    let t = cells(&s, &ui, (100, 14));
    let bar: Vec<String> = (0..14).map(|y| row_text(&t, y, 22)).collect();
    let all = bar.join("\n");
    assert!(all.contains("▌9 starting…"), "{all}");
    assert!(!all.contains(" 1 Fix parser"), "{all}");
    let theme = Theme::ayu_dark();
    let row = bar.iter().position(|r| r.starts_with("▌9")).unwrap() as u16;
    assert_eq!(
        t.backend().buffer()[(5, row)].bg,
        theme.selection_bg_on_panel()
    );
    // with the first tab active, the list starts at the top again
    s.active = 0;
    let t = cells(&s, &ui, (100, 14));
    assert!(row_text(&t, 1, 22).starts_with("▌1 Fix parser"));
    render(
        "sidebar-scrolled",
        (100, 14),
        &{
            let mut s = s.clone();
            s.active = 8;
            s
        },
        &ui,
        &Editor::new(),
    );
}

#[test]
fn command_outcomes_render_as_compact_blocks() {
    let mut s = done();
    s.conversation.blocks.push(Block {
        id: "c1".into(),
        kind: BlockKind::Command {
            name: "variant".into(),
            ok: true,
            text: "variant high; on offer: low, high".into(),
        },
    });
    s.conversation.blocks.push(Block {
        id: "c2".into(),
        kind: BlockKind::Command {
            name: "agent".into(),
            ok: false,
            text: "the agent is working: wait for the turn to end".into(),
        },
    });
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    let screen = render("command-results", (100, 30), &s, &ui, &Editor::new());
    assert!(screen.contains("/variant · ok"), "{screen}");
    assert!(screen.contains("/agent · error"), "{screen}");
    let t = cells(&s, &ui, (100, 30));
    let theme = Theme::ayu_dark();
    let row = (0..30)
        .find(|y| row_text(&t, *y, 100).contains("/agent · error"))
        .unwrap();
    let line = row_text(&t, row, 100);
    let x = line[..line.find("error").unwrap()].chars().count() as u16;
    assert_eq!(t.backend().buffer()[(x, row)].fg, theme.red);
    let row = (0..30)
        .find(|y| row_text(&t, *y, 100).contains("/variant · ok"))
        .unwrap();
    let line = row_text(&t, row, 100);
    let x = line[..line.find("· ok").unwrap()].chars().count() as u16 + 2;
    assert_eq!(t.backend().buffer()[(x, row)].fg, theme.green);
}

#[test]
fn enter_on_a_server_command_runs_it_and_a_double_slash_sends_text() {
    let s = state();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    editor.set_text("/context");
    let action = route_key(
        key(KeyCode::Enter, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(
        action,
        Some(Action::RunCommand {
            name: "context".into(),
            arguments: String::new(),
            revision: Some(editor.revision()),
        })
    );
    assert_eq!(editor.text(), "/context", "kept until the server took it");
    editor.set_text("//context is a word");
    let action = route_key(
        key(KeyCode::Enter, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(
        action,
        Some(Action::SubmitAt {
            text: "/context is a word".into(),
            revision: editor.revision(),
        })
    );
}

#[test]
fn tab_on_an_empty_message_switches_the_agent_otherwise_it_moves_the_focus() {
    let s = state();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    let tab = key(KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(
        route_key(tab, &s, &mut ui, &mut editor),
        Some(Action::ToggleAgent)
    );
    editor.set_text("draft");
    assert_eq!(route_key(tab, &s, &mut ui, &mut editor), None);
    assert_eq!(ui.focus, navai::tui::state::Focus::Transcript);
    assert_eq!(route_key(tab, &s, &mut ui, &mut editor), None);
    assert_eq!(ui.focus, navai::tui::state::Focus::Editor);
    editor.set_text("");
    let back = key(KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(route_key(back, &s, &mut ui, &mut editor), None);
    assert_eq!(ui.focus, navai::tui::state::Focus::Transcript);
    let help: Vec<_> = navai::tui::commands::KEYS.iter().map(|(k, _)| *k).collect();
    assert!(
        help.contains(&"tab") && help.contains(&"shift+tab"),
        "{help:?}"
    );
}

#[test]
fn the_agent_shows_in_the_status_line_and_the_sidebar() {
    let theme = Theme::ayu_dark();
    let mut s = done();
    let ui = UiState::default();
    let t = cells(&s, &ui, (100, 30));
    let status = (0..30)
        .find(|y| row_text(&t, *y, 100).contains("▌build  cognition"))
        .expect("status line");
    let x = row_text(&t, status, 100)
        .chars()
        .position(|c| c == '▌')
        .unwrap() as u16;
    assert_eq!(t.backend().buffer()[(x, status)].fg, theme.accent);
    assert_eq!(t.backend().buffer()[(x + 1, status)].fg, theme.accent_text);
    s.agent = Some("plan".into());
    s.variant = Some("high".into());
    s.conversation.context = None;
    render("agent-plan", (100, 30), &s, &ui, &Editor::new());
    let t = cells(&s, &ui, (100, 30));
    let status = (0..30)
        .find(|y| row_text(&t, *y, 100).contains("▌plan  cognition"))
        .expect("status line");
    let line = row_text(&t, status, 100);
    assert!(line.contains("· high · 12.4k / 200k"), "{line}");
    let x = line.chars().position(|c| c == '▌').unwrap() as u16;
    assert_eq!(t.backend().buffer()[(x, status)].fg, theme.blue);
    assert!(row_text(&t, 2, 22).contains("plan · idle"));
    let x = row_text(&t, 2, 22).find("plan").unwrap() as u16;
    let x = row_text(&t, 2, 22)[..x as usize].chars().count() as u16;
    assert_eq!(t.backend().buffer()[(x, 2)].fg, theme.blue);
}

/// A catalog of 295 aliases; `opus` offers variants with `high` as default, `flash` none.
fn big_catalog() -> State {
    let mut s = state();
    s.catalog = (0..293)
        .map(|n| ModelEntry {
            alias: format!("alias-{n:03}"),
            model: format!("vendor/model-{n:03}"),
            context_window: Some(128_000),
            ..Default::default()
        })
        .collect();
    s.catalog.push(ModelEntry {
        alias: "opus".into(),
        model: "cc/claude-opus-5-5".into(),
        context_window: Some(1_000_000),
        variants: vec!["low".into(), "high".into(), "max".into()],
        default_variant: Some("high".into()),
        ..Default::default()
    });
    s.catalog.push(ModelEntry {
        alias: "flash".into(),
        model: "cc/flash".into(),
        context_window: None,
        ..Default::default()
    });
    s
}

fn type_text(text: &str, s: &State, ui: &mut UiState, editor: &mut Editor) {
    for c in text.chars() {
        route_key(key(KeyCode::Char(c), KeyModifiers::NONE), s, ui, editor);
    }
}

#[test]
fn the_model_picker_filters_a_long_catalog_and_shows_window_and_variants() {
    let s = big_catalog();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    route_key(
        key(KeyCode::F(2), KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(ui.overlay, Some(Overlay::Models));
    // the selection can walk the whole list; the popup scrolls to keep it in view
    for _ in 0..300 {
        route_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor,
        );
    }
    let t = cells(&s, &ui, (100, 30));
    let all: String = (0..30).map(|y| row_text(&t, y, 100) + "\n").collect();
    assert!(
        all.contains("▌flash"),
        "the last row is shown selected: {all}"
    );
    type_text("opus", &s, &mut ui, &mut editor);
    render("model-picker-filtered", (100, 30), &s, &ui, &editor);
    let t = cells(&s, &ui, (100, 30));
    let row = (0..30)
        .map(|y| row_text(&t, y, 100))
        .find(|r| r.contains("▌opus"))
        .expect("the filtered row");
    assert!(row.contains("cc/claude-opus-5-5"), "{row}");
    assert!(row.contains("1M"), "{row}");
    assert!(row.contains("low high max"), "{row}");
    assert!(!all.contains("alias-") || !row.contains("alias-"));
    // the filter also matches the model name
    ui.filter = "model-29".into();
    let t = cells(&s, &ui, (100, 30));
    let shown: Vec<String> = (0..30)
        .map(|y| row_text(&t, y, 100))
        .filter(|r| r.contains("alias-29"))
        .collect();
    assert_eq!(shown.len(), 3, "alias-290 … alias-292: {shown:?}");
}

#[test]
fn picking_a_model_with_variants_asks_which_one_default_first() {
    let s = big_catalog();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    route_key(
        key(KeyCode::F(2), KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    type_text("opus", &s, &mut ui, &mut editor);
    let enter = key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(route_key(enter, &s, &mut ui, &mut editor), None);
    assert_eq!(ui.overlay, Some(Overlay::Variants));
    assert_eq!(ui.filter, "", "the step starts unfiltered");
    render("variant-step", (100, 30), &s, &ui, &editor);
    let t = cells(&s, &ui, (100, 30));
    let rows: Vec<String> = (0..30).map(|y| row_text(&t, y, 100)).collect();
    let selected = rows
        .iter()
        .find(|r| r.contains('▌') && r.contains("high"))
        .cloned();
    assert!(selected.is_some_and(|r| r.contains("default")), "{rows:#?}");
    assert!(rows
        .iter()
        .any(|r| r.contains("off") && r.contains("no variant")));
    assert!(rows.iter().any(|r| r.contains("Start on variant")));
    // esc goes back to the models with the same one selected
    route_key(
        key(KeyCode::Esc, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(ui.overlay, Some(Overlay::Models));
    assert_eq!(route_key(enter, &s, &mut ui, &mut editor), None);
    route_key(
        key(KeyCode::Down, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(
        route_key(enter, &s, &mut ui, &mut editor),
        Some(Action::PickVariant("max".into()))
    );
}

#[test]
fn a_model_without_variants_shows_a_note_and_starts_without_one() {
    let s = big_catalog();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    route_key(
        key(KeyCode::F(2), KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    type_text("flash", &s, &mut ui, &mut editor);
    let enter = key(KeyCode::Enter, KeyModifiers::NONE);
    route_key(enter, &s, &mut ui, &mut editor);
    assert_eq!(ui.overlay, Some(Overlay::Variants));
    let t = cells(&s, &ui, (100, 30));
    let all: String = (0..30).map(|y| row_text(&t, y, 100) + "\n").collect();
    assert!(all.contains("this model has no variants"), "{all}");
    assert!(!all.contains("off "), "nothing to choose: {all}");
    assert_eq!(
        route_key(enter, &s, &mut ui, &mut editor),
        Some(Action::PickModel("flash".into()))
    );
    assert_eq!(ui.overlay, None);
}

#[test]
fn the_variant_list_of_a_running_conversation_says_it_applies_next_turn() {
    let mut s = big_catalog();
    s.selected_model_alias = Some("opus".into());
    s.variant = Some("high".into());
    s.variant_offer = Some(vec!["low".into(), "high".into(), "max".into()]);
    s.activity = Activity::Running;
    let mut ui = UiState {
        overlay: Some(Overlay::Variants),
        variant_pick: Some(navai::tui::state::VariantPick::Current),
        ..UiState::default()
    };
    // opened as `/variant` opens it: on the variant in use
    let (_, start) =
        navai::tui::commands::variant_choices(&s, &navai::tui::state::VariantPick::Current);
    ui.selection.insert("Variants".into(), start);
    render(
        "variant-current-running",
        (100, 30),
        &s,
        &ui,
        &Editor::new(),
    );
    let t = cells(&s, &ui, (100, 30));
    let rows: Vec<String> = (0..30).map(|y| row_text(&t, y, 100)).collect();
    assert!(rows.iter().any(|r| r.contains(" Variant ")));
    assert!(
        rows.iter().any(|r| r.contains("applies next turn")),
        "{rows:#?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("● high")),
        "the one in use is marked"
    );
    s.activity = Activity::Idle;
    let t = cells(&s, &ui, (100, 30));
    assert!(!(0..30).any(|y| row_text(&t, y, 100).contains("applies next turn")));
}

#[test]
fn the_status_line_reads_model_variant_and_gauge() {
    let mut s = done();
    s.variant = Some("high".into());
    s.conversation.context = Some(navai::bus::event::ContextSnapshot {
        alias: "cognition".into(),
        variant: Some("high".into()),
        window: 1_000_000,
        window_source: "models.json".into(),
        estimated: true,
        estimate_total: 42_000,
        breakdown: Default::default(),
        output_reserve: 8192,
        margin: 50_000,
        input_budget: 941_808,
        percent: 4,
        last_usage: None,
        skills: Default::default(),
        rule_files: 0,
        incarnation: ulid::Ulid::new(),
        revision: 1,
    });
    let t = cells(&s, &UiState::default(), (120, 30));
    let line = (0..30)
        .map(|y| row_text(&t, y, 120))
        .find(|r| r.contains("▌build"))
        .unwrap();
    assert!(
        line.contains("cognition · cc/claude-opus-5-5 · high · ~42k / 1M · 4%"),
        "{line}"
    );
    // on a narrow screen the model name is cut before the variant or the gauge
    let t = cells(&s, &UiState::default(), (70, 20));
    let line = (0..20)
        .map(|y| row_text(&t, y, 70))
        .find(|r| r.contains("▌build"))
        .unwrap();
    assert!(line.contains("· high · ~42k / 1M · 4%"), "{line}");
}

// ---------------------------------------------------------------- typing commands

fn with_server_commands() -> State {
    let mut s = state();
    s.server_commands = [
        "help", "model", "new", "stop", "context", "variant", "compact", "agent",
    ]
    .iter()
    .map(|n| navai::tui::protocol::ServerCommand {
        name: n.to_string(),
        description: format!("Server {n}"),
        args_hint: match *n {
            "variant" => "[name|off]".into(),
            "agent" => "[id]".into(),
            _ => String::new(),
        },
    })
    .collect();
    s
}

/// Types `keys` (a char per key; `⏎` is enter) through the input handler; returns the actions.
fn keys(text: &str, s: &State, ui: &mut UiState, editor: &mut Editor) -> Vec<Action> {
    text.chars()
        .filter_map(|c| {
            let code = if c == '⏎' {
                KeyCode::Enter
            } else {
                KeyCode::Char(c)
            };
            route_key(key(code, KeyModifiers::NONE), s, ui, editor)
        })
        .collect()
}

#[test]
fn typing_a_slash_shows_client_and_server_commands_and_filters_by_name() {
    let s = with_server_commands();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    keys("/", &s, &mut ui, &mut editor);
    assert_eq!(ui.overlay, Some(Overlay::Commands));
    render("palette-typed", (100, 30), &s, &ui, &editor);
    let t = cells(&s, &ui, (100, 30));
    let all: String = (0..30).map(|y| row_text(&t, y, 100) + "\n").collect();
    for row in ["/context", "/agent [id]", "/compact", "/sessions"] {
        assert!(all.contains(row), "{row}: {all}");
    }
    assert_eq!(
        all.matches("/variant [name|off]").count(),
        1,
        "listed once: {all}"
    );
    keys("co", &s, &mut ui, &mut editor);
    assert_eq!(editor.text(), "/co");
    assert_eq!(ui.filter, "co");
    let t = cells(&s, &ui, (100, 30));
    let all: String = (0..30).map(|y| row_text(&t, y, 100) + "\n").collect();
    assert!(
        all.contains("/context") && all.contains("/compact"),
        "{all}"
    );
    assert!(!all.contains("/sessions"), "{all}");
}

#[test]
fn slash_context_enter_runs_it_on_the_server() {
    let s = with_server_commands();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    let actions = keys("/context⏎", &s, &mut ui, &mut editor);
    assert_eq!(
        actions.last(),
        Some(&Action::RunCommand {
            name: "context".into(),
            arguments: String::new(),
            revision: Some(editor.revision()),
        })
    );
    assert_eq!(editor.text(), "/context", "kept until the server took it");
    assert_eq!(ui.overlay, None);
}

#[test]
fn an_argument_closes_the_palette_and_enter_sends_the_line() {
    let s = with_server_commands();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    keys("/variant", &s, &mut ui, &mut editor);
    assert_eq!(ui.overlay, Some(Overlay::Commands));
    keys(" ", &s, &mut ui, &mut editor);
    assert_eq!(
        ui.overlay, None,
        "a space after the name puts the list away"
    );
    let actions = keys("high⏎", &s, &mut ui, &mut editor);
    assert_eq!(
        actions.last(),
        Some(&Action::RunCommand {
            name: "variant".into(),
            arguments: "high".into(),
            revision: Some(editor.revision()),
        })
    );
}

#[test]
fn an_unknown_name_closes_the_palette_and_goes_to_the_server() {
    let s = with_server_commands();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    keys("/contx", &s, &mut ui, &mut editor);
    assert_eq!(ui.overlay, None, "no command starts with it");
    let actions = keys("t⏎", &s, &mut ui, &mut editor);
    assert_eq!(
        actions.last(),
        Some(&Action::RunCommand {
            name: "contxt".into(),
            arguments: String::new(),
            revision: Some(editor.revision()),
        })
    );
}

#[test]
fn a_double_slash_types_a_literal_slash_and_sends_a_message() {
    let s = with_server_commands();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    keys("//", &s, &mut ui, &mut editor);
    assert_eq!(ui.overlay, None);
    assert_eq!(editor.text(), "//");
    let actions = keys("hello⏎", &s, &mut ui, &mut editor);
    assert_eq!(
        actions.last(),
        Some(&Action::SubmitAt {
            text: "/hello".into(),
            revision: editor.revision(),
        })
    );
}

#[test]
fn ctrl_p_opens_the_palette_and_enter_runs_a_server_row() {
    let s = with_server_commands();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    editor.set_text("a draft");
    let action = route_key(
        key(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(action, Some(Action::OpenOverlay(Overlay::Commands)));
    assert!(!ui.palette_typed);
    keys("context", &s, &mut ui, &mut editor);
    assert_eq!(
        editor.text(),
        "a draft",
        "the palette filter is not the editor"
    );
    let actions = keys("⏎", &s, &mut ui, &mut editor);
    assert_eq!(
        actions,
        [Action::RunCommand {
            name: "context".into(),
            arguments: String::new(),
            revision: None,
        }]
    );
    assert_eq!(editor.text(), "a draft");
    // a row that needs an argument is put in the editor to finish
    let mut editor = Editor::new();
    route_key(
        key(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &s,
        &mut ui,
        &mut editor,
    );
    keys("compact", &s, &mut ui, &mut editor);
    // `/compact` takes an optional focus: it runs bare
    assert!(matches!(
        keys("⏎", &s, &mut ui, &mut editor).as_slice(),
        [Action::RunCommand { name, .. }] if name == "compact"
    ));
}

#[test]
fn esc_and_backspace_in_the_typed_palette_keep_the_line() {
    let s = with_server_commands();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    keys("/ag", &s, &mut ui, &mut editor);
    route_key(
        key(KeyCode::Backspace, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!((editor.text().as_str(), ui.filter.as_str()), ("/a", "a"));
    assert_eq!(ui.overlay, Some(Overlay::Commands));
    route_key(
        key(KeyCode::Esc, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(ui.overlay, None);
    assert_eq!(editor.text(), "/a");
    // backspace over the slash leaves an empty line and no list
    let mut editor = Editor::new();
    keys("/", &s, &mut ui, &mut editor);
    route_key(
        key(KeyCode::Backspace, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(editor.text(), "");
    assert_eq!(ui.overlay, None);
}

#[test]
fn a_typed_local_command_with_an_argument_is_refused_with_a_notice() {
    let s = with_server_commands();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    let actions = keys("/stop now⏎", &s, &mut ui, &mut editor);
    assert_eq!(
        actions.last(),
        Some(&Action::ShowNotice("/stop takes no arguments".into()))
    );
    assert_eq!(editor.text(), "/stop now", "the line stays to fix");
    let mut editor = Editor::new();
    let actions = keys("/model fast⏎", &s, &mut ui, &mut editor);
    assert_eq!(actions.last(), Some(&Action::PickModel("fast".into())));
    assert_eq!(editor.text(), "", "a client command clears its line");
}

#[test]
fn a_paste_after_a_typed_slash_edits_the_line_the_palette_follows() {
    let s = with_server_commands();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    keys("/", &s, &mut ui, &mut editor);
    assert_eq!(
        route_paste("context".into(), &s, &mut ui, &mut editor),
        None
    );
    assert_eq!(editor.text(), "/context");
    assert_eq!(
        (ui.overlay, ui.filter.as_str()),
        (Some(Overlay::Commands), "context")
    );
    let actions = keys("⏎", &s, &mut ui, &mut editor);
    assert_eq!(
        actions,
        [Action::RunCommand {
            name: "context".into(),
            arguments: String::new(),
            revision: Some(editor.revision()),
        }]
    );
    // a pasted path is not a command name: the list goes away, the whole line stays
    let (mut ui, mut editor) = (UiState::default(), Editor::new());
    keys("/", &s, &mut ui, &mut editor);
    let action = route_paste("etc/hosts is wrong".into(), &s, &mut ui, &mut editor);
    assert_eq!(action, Some(Action::CloseOverlay));
    assert_eq!(ui.overlay, None);
    assert_eq!(editor.text(), "/etc/hosts is wrong");
    // another overlay still keeps pastes out
    let (mut ui, mut editor) = (UiState::default(), Editor::new());
    route_key(
        key(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &s,
        &mut ui,
        &mut editor,
    );
    route_paste("context".into(), &s, &mut ui, &mut editor);
    assert_eq!(editor.text(), "");
}

// ---------------------------------------------------------------- permission prompt

fn prompt(tool: &str, arguments: serde_json::Value, scope: (&str, &str)) -> Block {
    Block {
        id: "permission:R1".into(),
        kind: BlockKind::Permission(navai::tui::state::PermissionPrompt {
            request_id: "R1".into(),
            tool_call_id: "c1".into(),
            tool: tool.into(),
            arguments: arguments.to_string(),
            scope_kind: scope.0.into(),
            scope: scope.1.into(),
            agent: "build".into(),
            reason: "it changes files".into(),
            resolution: None,
            message: None,
            stale: false,
        }),
    }
}

/// `done()` with an open request for `block` at the end.
fn asking(block: Block) -> State {
    let mut s = done();
    s.activity = Activity::AwaitingUser;
    s.conversation.open_permissions = vec![block.id.clone()];
    s.conversation.blocks.push(block);
    s
}

fn screen_text(s: &State, ui: &UiState, size: (u16, u16)) -> String {
    let t = cells(s, ui, size);
    (0..size.1)
        .map(|y| row_text(&t, y, size.0))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_write_prompt_shows_the_path_the_first_lines_and_the_keys() {
    let content = (1..=9)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let s = asking(prompt(
        "write",
        json!({"path": "/home/pixma/dev/project/src/a.rs", "content": content}),
        ("folder", "/home/pixma/dev/project/src"),
    ));
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    let shown = render("permission-write", (100, 36), &s, &ui, &Editor::new());
    for want in [
        "PERMISSION · build wants to run write in /home/pixma/dev/project/src",
        "/home/pixma/dev/project/src/a.rs",
        "+ line 1",
        "+ line 6",
        "… 3 more lines",
        "y allow once   a always for this folder   n deny",
    ] {
        assert!(shown.contains(want), "{want}: {shown}");
    }
    assert!(!shown.contains("+ line 7"), "{shown}");
    // the open prompt has the accent rule
    let t = cells(&s, &ui, (100, 36));
    let theme = Theme::ayu_dark();
    let row = (0..36)
        .find(|y| row_text(&t, *y, 100).contains("PERMISSION"))
        .unwrap();
    assert_eq!(t.backend().buffer()[(23, row)].symbol(), "│");
    assert_eq!(t.backend().buffer()[(23, row)].fg, theme.accent);
    let x = row_text(&t, row, 100).find("PERMISSION").unwrap();
    let x = row_text(&t, row, 100)[..x].chars().count() as u16;
    assert_eq!(t.backend().buffer()[(x, row)].fg, theme.accent_text);
}

#[test]
fn bash_edit_and_fetch_prompts_show_what_they_would_do() {
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    let s = asking(prompt(
        "bash",
        json!({"command": "cargo test -p navai"}),
        ("folder", "/ws"),
    ));
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(shown.contains("wants to run bash in /ws"), "{shown}");
    assert!(shown.contains("$ cargo test -p navai"), "{shown}");
    let s = asking(prompt(
        "edit",
        json!({"path": "/ws/a.rs", "old_string": "let x = 1;", "new_string": "let x = 2;"}),
        ("folder", "/ws"),
    ));
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(
        shown.contains("- let x = 1;") && shown.contains("+ let x = 2;"),
        "{shown}"
    );
    let s = asking(prompt(
        "fetch",
        json!({"url": "https://docs.rs/ratatui"}),
        ("origin", "https://docs.rs"),
    ));
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(
        shown.contains("wants to run fetch to https://docs.rs"),
        "{shown}"
    );
    assert!(shown.contains("https://docs.rs/ratatui"), "{shown}");
    assert!(shown.contains("a always for this site"), "{shown}");
}

/// An open prompt for `args` as the server sends it (its copy of the arguments cut to 4 KiB),
/// with the tool call itself in the transcript when `with_call`.
fn asking_for(tool: &str, args: serde_json::Value, with_call: bool) -> State {
    let call = navai::provider::ToolCall {
        id: "c1".into(),
        name: tool.into(),
        args: args.clone(),
        args_valid: true,
    };
    let request = navai::agent::permissions::PermissionRequest::new(
        &call,
        navai::agent::permissions::Scope::Folder("/ws".into()),
        "build",
    );
    let mut block = prompt(tool, json!({}), ("folder", "/ws"));
    if let BlockKind::Permission(p) = &mut block.kind {
        p.arguments = request.arguments;
    }
    let mut s = asking(block);
    if with_call {
        let at = s.conversation.blocks.len() - 1;
        s.conversation.blocks.insert(
            at,
            Block {
                id: "tool:c1".into(),
                kind: BlockKind::ToolCall {
                    id: "c1".into(),
                    name: tool.into(),
                    args,
                    status: None,
                    output: String::new(),
                    metadata: serde_json::Value::Null,
                    duration_ms: None,
                    exit_code: None,
                },
            },
        );
    }
    s
}

#[test]
fn a_large_write_shows_its_path_and_first_lines() {
    let content = (0..2_100)
        .map(|n| format!("line {n:04} {}", "x".repeat(40)))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(content.len() > 100 * 1024);
    let args = json!({"content": content, "path": "/ws/big.txt"});
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    // from the call in the transcript: the whole preview
    let s = asking_for("write", args.clone(), true);
    render("permission-write-large", (100, 36), &s, &ui, &Editor::new());
    let shown = screen_text(&s, &ui, (100, 36));
    for want in [
        "/ws/big.txt",
        "+ line 0000",
        "+ line 0005",
        "… 2094 more lines",
    ] {
        assert!(shown.contains(want), "{want}: {shown}");
    }
    // from the server's cut copy only: the path still on its own line, the rest labelled
    let s = asking_for("write", args, false);
    let shown = screen_text(&s, &ui, (100, 36));
    assert!(shown.contains("│  /ws/big.txt"), "{shown}");
    assert!(shown.contains("(content too long to show here)"), "{shown}");
    assert!(!shown.contains("\\n"), "no raw JSON: {shown}");
}

#[test]
fn a_long_command_and_control_characters_are_shown_safely() {
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    let command = format!("echo {} && echo done", "a".repeat(20_000));
    let s = asking_for("bash", json!({"command": command}), true);
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(shown.contains("$ echo aaaa"), "{shown}");
    let s = asking_for("bash", json!({"command": command}), false);
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(shown.contains("(command too long to show here)"), "{shown}");
    // escape sequences and other controls are written out, never sent to the terminal
    let s = asking_for(
        "write",
        json!({"path": "/ws/a\u{1b}[2J.txt", "content": "ok\u{7}\u{1b}]0;title\u{7}\tend"}),
        true,
    );
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(shown.contains("/ws/a\\u{1b}[2J.txt"), "{shown}");
    assert!(
        shown.contains("+ ok\\u{7}\\u{1b}]0;title\\u{7}    end"),
        "{shown}"
    );
    assert!(!shown.contains('\u{1b}'), "{shown}");
}

#[test]
fn n_asks_for_a_message_then_enter_denies_with_it() {
    let s = asking(prompt(
        "bash",
        json!({"command": "rm -rf target"}),
        ("folder", "/ws"),
    ));
    let mut ui = UiState {
        follow_bottom: true,
        prompt_shown: Some("R1".into()),
        ..UiState::default()
    };
    let mut editor = Editor::new();
    let answer = |decision: &str, message: Option<&str>| Action::AnswerPermission {
        request_id: "R1".into(),
        answer: navai::tui::protocol::PermissionAnswer {
            decision: decision.into(),
            message: message.map(str::to_string),
        },
    };
    assert_eq!(
        keys("y", &s, &mut ui, &mut editor),
        [answer("allow_once", None)]
    );
    assert_eq!(
        keys("a", &s, &mut ui, &mut editor),
        [answer("allow_folder", None)]
    );
    assert!(keys("n", &s, &mut ui, &mut editor).is_empty());
    assert_eq!(ui.deny_draft("R1"), Some(""));
    keys("use cargo clean", &s, &mut ui, &mut editor);
    assert_eq!(
        editor.text(),
        "",
        "the message is not typed into the editor"
    );
    render("permission-deny-message", (100, 30), &s, &ui, &editor);
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(
        shown.contains("deny with a message: use cargo clean"),
        "{shown}"
    );
    assert!(
        shown.contains("enter send · esc deny without a message"),
        "{shown}"
    );
    assert_eq!(
        keys("⏎", &s, &mut ui, &mut editor),
        [answer("deny", Some("use cargo clean"))]
    );
    assert_eq!(ui.deny_message, None);
    // esc denies without one
    keys("n", &s, &mut ui, &mut editor);
    let esc = route_key(
        key(KeyCode::Esc, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(esc, Some(answer("deny", None)));
}

/// `s` with its open request `R1` replaced by `R2` (R1 ended elsewhere).
fn next_request(s: &State) -> State {
    let mut s = s.clone();
    let mut second = prompt("bash", json!({"command": "rm -rf /"}), ("folder", "/"));
    second.id = "permission:R2".into();
    if let BlockKind::Permission(p) = &mut second.kind {
        p.request_id = "R2".into();
    }
    if let BlockKind::Permission(p) = &mut s.conversation.blocks.last_mut().unwrap().kind {
        p.resolution = Some("cancelled".into());
    }
    s.conversation.open_permissions = vec![second.id.clone()];
    s.conversation.blocks.push(second);
    s
}

#[test]
fn a_deny_message_does_not_carry_over_to_the_next_request() {
    let s = asking(prompt("bash", json!({"command": "ls"}), ("folder", "/ws")));
    let mut ui = UiState {
        follow_bottom: true,
        prompt_shown: Some("R1".into()),
        ..UiState::default()
    };
    let mut editor = Editor::new();
    keys("nnot here", &s, &mut ui, &mut editor);
    assert_eq!(ui.deny_draft("R1"), Some("not here"));
    let s2 = next_request(&s);
    // the message is not shown on R2, and enter does not deny R2 with it
    let shown = screen_text(&s2, &ui, (100, 30));
    assert!(!shown.contains("not here"), "{shown}");
    assert!(shown.contains("y allow once"), "{shown}");
    let enter = keys("⏎", &s2, &mut ui, &mut editor);
    assert!(
        !enter
            .iter()
            .any(|a| matches!(a, Action::AnswerPermission { .. })),
        "{enter:?}"
    );
    assert_eq!(ui.deny_message, None);
    // a paste is not added to R1's message either
    let mut ui = UiState {
        prompt_shown: Some("R1".into()),
        ..UiState::default()
    };
    let mut editor = Editor::new();
    keys("nold", &s, &mut ui, &mut editor);
    route_paste("pasted".into(), &s2, &mut ui, &mut editor);
    assert_eq!(ui.deny_message, None);
    assert_eq!(editor.text(), "pasted", "it goes to the editor");
    // on R2, n starts a message of its own
    let mut ui = UiState {
        prompt_shown: Some("R1".into()),
        ..UiState::default()
    };
    let mut editor = Editor::new();
    keys("nold", &s, &mut ui, &mut editor);
    ui.prompt_shown = Some("R2".into());
    keys("nnew", &s2, &mut ui, &mut editor);
    assert_eq!(ui.deny_draft("R2"), Some("new"));
    assert_eq!(
        keys("⏎", &s2, &mut ui, &mut editor),
        [Action::AnswerPermission {
            request_id: "R2".into(),
            answer: navai::tui::protocol::PermissionAnswer {
                decision: "deny".into(),
                message: Some("new".into()),
            },
        }]
    );
}

/// Draws `s` like the runtime does, and keeps what the keys need to know of the screen.
fn draw_into(s: &State, ui: &mut UiState, size: (u16, u16)) {
    let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
    let mut cache = RenderCache::default();
    terminal
        .draw(|f| view::draw(f, s, ui, &Editor::new(), &mut cache))
        .unwrap();
    ui.prompt_shown = cache.prompt_shown.clone();
}

#[test]
fn a_key_on_a_prompt_scrolled_out_of_view_first_brings_it_back() {
    let s = asking(prompt("bash", json!({"command": "ls"}), ("folder", "/ws")));
    let mut ui = UiState {
        follow_bottom: false,
        scroll: 200,
        ..UiState::default()
    };
    let mut editor = Editor::new();
    draw_into(&s, &mut ui, (100, 16));
    assert_eq!(ui.prompt_shown, None, "scrolled to the top");
    assert!(keys("a", &s, &mut ui, &mut editor).is_empty(), "no grant");
    assert_eq!(editor.text(), "");
    assert_eq!(ui.selected_block.as_deref(), Some("permission:R1"));
    draw_into(&s, &mut ui, (100, 16));
    assert_eq!(ui.prompt_shown.as_deref(), Some("R1"), "now in view");
    assert_eq!(
        keys("a", &s, &mut ui, &mut editor),
        [Action::AnswerPermission {
            request_id: "R1".into(),
            answer: navai::tui::protocol::PermissionAnswer {
                decision: "allow_folder".into(),
                message: None,
            },
        }]
    );
    // in view from the start: the first key answers
    let mut ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    draw_into(&s, &mut ui, (100, 30));
    assert_eq!(keys("y", &s, &mut ui, &mut editor).len(), 1);
}

#[test]
fn an_answered_prompt_says_it_was_sent_and_takes_no_more_keys() {
    let mut s = asking(prompt("bash", json!({"command": "ls"}), ("folder", "/ws")));
    s.answered_permission = Some("R1".into());
    let mut ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    let mut editor = Editor::new();
    assert!(keys("yyan", &s, &mut ui, &mut editor).is_empty());
    assert_eq!(editor.text(), "", "the keys are not typed either");
    assert_eq!(ui.deny_message, None);
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(shown.contains("sent, waiting for the server"), "{shown}");
    assert!(!shown.contains("y allow once"), "{shown}");
}

#[test]
fn a_prompt_the_server_no_longer_waits_on_is_muted_until_it_says_how_it_ended() {
    let mut s = asking(prompt("bash", json!({"command": "ls"}), ("folder", "/ws")));
    s.conversation.open_permissions.clear();
    if let BlockKind::Permission(p) = &mut s.conversation.blocks.last_mut().unwrap().kind {
        p.stale = true;
    }
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    render("permission-stale", (100, 30), &s, &ui, &Editor::new());
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(shown.contains("no longer waiting"), "{shown}");
    assert!(!shown.contains("waiting for your answer"), "{shown}");
    assert!(!shown.contains("y allow once"), "{shown}");
    let t = cells(&s, &ui, (100, 30));
    let theme = Theme::ayu_dark();
    let row = (0..30)
        .find(|y| row_text(&t, *y, 100).contains("PERMISSION"))
        .unwrap();
    assert_eq!(
        t.backend().buffer()[(23, row)].fg,
        theme.border,
        "no accent rule"
    );
    let line = row_text(&t, row, 100);
    let x = line[..line.find("PERMISSION").unwrap()].chars().count() as u16;
    assert_ne!(t.backend().buffer()[(x, row)].fg, theme.accent_text);
    let mut editor = Editor::new();
    let mut keys_ui = UiState::default();
    assert!(keys("y", &s, &mut keys_ui, &mut editor).is_empty());
    assert_eq!(editor.text(), "y", "the key types: nothing is open");
    // then the server's resolution
    if let BlockKind::Permission(p) = &mut s.conversation.blocks.last_mut().unwrap().kind {
        p.resolution = Some("allow_once".into());
    }
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(shown.contains("allowed once"), "{shown}");
    assert!(!shown.contains("no longer waiting"), "{shown}");
}

#[test]
fn prompt_keys_type_normally_when_the_message_has_text_or_nothing_is_open() {
    let s = asking(prompt("bash", json!({"command": "ls"}), ("folder", "/ws")));
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    editor.set_text("why ");
    assert!(keys("yan", &s, &mut ui, &mut editor).is_empty());
    assert_eq!(editor.text(), "yanwhy ", "typed at the cursor");
    // a resolved request takes no keys
    let mut s = s;
    if let BlockKind::Permission(p) = &mut s.conversation.blocks.last_mut().unwrap().kind {
        p.resolution = Some("allow_once".into());
    }
    s.conversation.open_permissions.clear();
    let mut editor = Editor::new();
    assert!(keys("y", &s, &mut ui, &mut editor).is_empty());
    assert_eq!(editor.text(), "y");
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(shown.contains("allowed once"), "{shown}");
    assert!(!shown.contains("y allow once"), "{shown}");
}

#[test]
fn the_prompt_reads_on_a_light_theme() {
    let s = asking(prompt(
        "write",
        json!({"path": "/ws/a.txt", "content": "hello"}),
        ("folder", "/ws"),
    ));
    let ui = UiState {
        follow_bottom: true,
        theme: ThemeId::AyuLight,
        ..UiState::default()
    };
    render("permission-light", (100, 30), &s, &ui, &Editor::new());
    let t = cells(&s, &ui, (100, 30));
    let theme = Theme::ayu_light();
    let row = (0..30)
        .find(|y| row_text(&t, *y, 100).contains("PERMISSION"))
        .unwrap();
    let line = row_text(&t, row, 100);
    let x = line[..line.find("PERMISSION").unwrap()].chars().count() as u16;
    let cell = &t.backend().buffer()[(x, row)];
    assert_eq!(cell.fg, theme.accent_text, "the readable accent on light");
    assert_eq!(cell.bg, theme.bg);
    let keys_row = (0..30)
        .find(|y| row_text(&t, *y, 100).contains("y allow once"))
        .unwrap();
    let line = row_text(&t, keys_row, 100);
    let x = line[..line.find("y allow").unwrap()].chars().count() as u16;
    assert_eq!(t.backend().buffer()[(x, keys_row)].fg, theme.text);
}

#[test]
fn sending_text_over_an_open_request_asks_for_a_second_enter() {
    let s = asking(prompt("bash", json!({"command": "ls"}), ("folder", "/ws")));
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    editor.set_text("no, list src instead");
    let warn = keys("⏎", &s, &mut ui, &mut editor);
    assert_eq!(
        warn,
        [Action::ShowNotice(
            "sending text cancels the pending permission; press enter again to send".into()
        )]
    );
    assert_eq!(editor.text(), "no, list src instead");
    let send = keys("⏎", &s, &mut ui, &mut editor);
    assert_eq!(
        send,
        [Action::SubmitAt {
            text: "no, list src instead".into(),
            revision: editor.revision(),
        }]
    );
    // an edit in between asks again
    let mut ui = UiState::default();
    keys("⏎", &s, &mut ui, &mut editor);
    keys("!", &s, &mut ui, &mut editor);
    assert!(matches!(
        keys("⏎", &s, &mut ui, &mut editor).as_slice(),
        [Action::ShowNotice(_)]
    ));
    // with nothing open, enter sends at once
    let mut plain = s.clone();
    plain.conversation.open_permissions.clear();
    let mut ui = UiState::default();
    assert!(matches!(
        keys("⏎", &plain, &mut ui, &mut editor).as_slice(),
        [Action::SubmitAt { .. }]
    ));
}
