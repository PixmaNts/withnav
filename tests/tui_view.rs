use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use withnav::{
    tools::ToolStatus,
    tui::{
        input::{Editor, route_key, route_paste},
        protocol::{Action, ModelEntry, Overlay, SessionListEntry, SessionMetadata},
        state::{Activity, Block, BlockKind, ConnectionStatus, State, Tab, UiState},
        theme::{Theme, ThemeId},
        view::{self, RenderCache, tool},
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

#[test]
fn choice_question_requires_explicit_selection_and_replays_answered() {
    use withnav::tools::control::{AskAnswer, AskChoices, AskOption};
    use withnav::tui::{reducer::handle_action, state::QuestionPrompt};
    let mut s = state();
    s.conversation.blocks.push(Block {
        id: "ask".into(),
        kind: BlockKind::Question(QuestionPrompt {
            text: "Where should the changes go?".into(),
            call_id: "call".into(),
            choices: AskChoices {
                header: Some("Next step".into()),
                options: vec![
                    AskOption {
                        label: "Develop".into(),
                        description: Some("Integration branch".into()),
                    },
                    AskOption {
                        label: "Leave open".into(),
                        description: Some("Wait for review".into()),
                    },
                ],
                multiple: true,
                allow_custom: true,
                recommended: Some(0),
            },
            answer: None,
            cursor: 0,
            selected: vec![],
            sending: false,
            dismissed: false,
            custom: false,
            error: None,
        }),
    });
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    let empty = route_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    )
    .unwrap();
    assert!(handle_action(&mut s, &mut ui, empty).is_empty());
    handle_action(&mut s, &mut ui, Action::QuestionMove(1));
    handle_action(&mut s, &mut ui, Action::QuestionToggle);
    let screen = render("question-choices", (100, 30), &s, &ui, &editor);
    assert!(screen.contains("[ ] Develop"));
    assert!(screen.contains("[x] Leave open"));
    assert_eq!(screen.matches("space select").count(), 1);
    if let BlockKind::Question(p) = &mut s.conversation.blocks[0].kind {
        p.choices.header = Some("\x1b[31mSafe\x1b[0m\x1b]0;owned\x07".into());
    }
    let screen = render("question-header-controls", (100, 30), &s, &ui, &editor);
    assert!(screen.contains("Safe · awaiting answer"));
    assert!(!screen.contains("owned"));
    assert!(!screen.contains('\x1b'));
    let action = route_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    )
    .unwrap();
    assert!(
        matches!(action, Action::AnswerQuestion(AskAnswer { selected, .. }) if selected == vec![1])
    );
    handle_action(&mut s, &mut ui, Action::QuestionMove(1));
    assert_eq!(
        route_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        ),
        Some(Action::QuestionCustom)
    );
}

#[test]
fn session_delete_confirmation_row_and_keys() {
    let mut s = state();
    let id = s.sessions[0].id.clone();
    let mut child = s.sessions[0].clone();
    child.id = "child".into();
    child.parent_session_id = Some(id.clone());
    s.sessions.push(child);
    let mut ui = UiState {
        overlay: Some(Overlay::Sessions),
        ..Default::default()
    };
    let mut editor = Editor::new();
    assert_eq!(
        route_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &s,
            &mut ui,
            &mut editor
        ),
        Some(Action::RequestDeleteSession(id.clone()))
    );
    ui.delete_confirm = Some(id);
    render("sessions-delete-confirmation", (160, 30), &s, &ui, &editor);
    assert_eq!(
        route_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        ),
        Some(Action::CancelDeleteSession)
    );
    assert_eq!(
        route_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        ),
        Some(Action::ConfirmDeleteSession)
    );
}

#[test]
fn bypass_header_sidebar_and_notice() {
    let mut s = state();
    s.bypass = true;
    s.conversation.blocks.push(Block {
        id: "bypass:42".into(),
        kind: BlockKind::Notice(
            "bypass on: ordinary permission prompts are skipped; protected requests still ask"
                .into(),
        ),
    });
    render(
        "bypass-on",
        (100, 24),
        &s,
        &UiState::default(),
        &Editor::new(),
    );
}

#[test]
fn a_child_permission_preview_never_uses_a_colliding_parent_call() {
    let mut s = state();
    let mut parent = tool_block(None);
    parent.id = "tool:call-1".into();
    if let BlockKind::ToolCall { args, .. } = &mut parent.kind {
        *args = json!({"command":"rm parent-file"});
    }
    let mut child = prompt(
        "bash",
        json!({"command":"cat child-file"}),
        ("folder", "/ws"),
    );
    if let BlockKind::Permission(p) = &mut child.kind {
        p.tool_call_id = "call-1".into();
        p.child_session_id = Some("child".into());
    }
    s.conversation.blocks = vec![parent, child];
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|f| {
            view::draw(
                f,
                &s,
                &UiState::default(),
                &Editor::new(),
                &mut RenderCache::default(),
            )
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text = (0..30)
        .map(|y| {
            (0..100)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("cat child-file"));
    let preview = text.split("PERMISSION").last().unwrap();
    assert!(!preview.contains("rm parent-file"));
}

#[test]
fn a_narrow_terminal_reaches_children_and_keeps_the_transcript_read_only() {
    use withnav::tui::{
        reducer::handle_action,
        state::{ChildAgent, ChildStatus},
    };
    let mut s = state();
    s.children.push(ChildAgent {
        session_id: "child-session".into(),
        name: "vigie".into(),
        goal: "Read parser tests".into(),
        status: ChildStatus::Running,
        ..Default::default()
    });
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    let action = route_key(
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT),
        &s,
        &mut ui,
        &mut editor,
    )
    .unwrap();
    handle_action(&mut s, &mut ui, action);
    assert_eq!(ui.overlay, Some(Overlay::Children));
    let action = route_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    )
    .unwrap();
    assert_eq!(action, Action::OpenChild("child-session".into()));
    handle_action(&mut s, &mut ui, action);
    assert!(s.read_only());
    assert!(
        route_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        )
        .is_none()
    );
    route_paste("can't send this".into(), &s, &mut ui, &mut editor);
    assert!(editor.text().is_empty());
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    terminal
        .draw(|frame| view::draw(frame, &s, &ui, &editor, &mut RenderCache::default()))
        .unwrap();
    let line = |y| row_text(&terminal, y, 60).trim_end().to_string();
    insta::assert_snapshot!(line(0), @" ◆ subagent of Fix parser  ▌◉ vigie · —");
    insta::assert_snapshot!(line(15), @" alt+w parent · drag copy text · alt+s subagents");
    assert!((0..16).any(|y| line(y).contains("can't be written to")));
    assert!(!(0..16).any(|y| line(y).contains("Conversations")));
    let action = route_key(
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::ALT),
        &s,
        &mut ui,
        &mut editor,
    )
    .unwrap();
    handle_action(&mut s, &mut ui, action);
    assert!(!s.read_only());
}

#[test]
fn delivered_subagent_output_is_not_labelled_as_the_captain() {
    let mut s = state();
    s.conversation.blocks.push(message(
        "result",
        "user:subagent",
        "The parser test passed.",
    ));
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    terminal
        .draw(|frame| {
            view::draw(
                frame,
                &s,
                &UiState::default(),
                &Editor::new(),
                &mut RenderCache::default(),
            )
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text = (0..16)
        .map(|y| (0..60).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("◎ subagent output · untrusted data"));
    assert!(text.contains("The parser test passed."));
    assert!(!text.contains("YOU"));
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
        selected_model_id: Some("cognition".into()),
        selected_model_name: Some("cc/claude-opus-5-5".into()),
        context_window: Some(200_000),
        session: Some(SessionMetadata {
            yolo: false,
            permissions_unavailable: None,
            bypass: false,
            bypass_seq: 0,
            incarnation: None,
            source: "config".into(),
            variant_source: "model_default".into(),
            read_only: false,
            resume_disabled_reason: None,
            session_id: "01JABCDEFGH".into(),
            actor_state: "idle".into(),
            // The status line shows the home folder as `~`, and the snapshots expect
            // `~/dev/project`. A fixed `/home/pixma/...` only matched on a machine whose home
            // is `/home/pixma` (observed 2026-10-12: ten snapshots failed on a runner whose
            // home is `/home/runner`), so build the path from the actual home. Windows usually
            // has no `HOME`: the status line then uses `USERPROFILE`, and so does this fixture.
            workspace: format!(
                "{}/dev/project",
                std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .unwrap()
            ),
            model: "cognition".into(),
            agent: None,
            variant: None,
            parent_session_id: None,
            promptable: true,
            child_status: None,
            current_seq: 42,
        }),
        ..Tab::default()
    };
    State {
        tabs: vec![tab],
        catalog: vec![
            ModelEntry {
                api: withnav::tui::protocol::ApiKind::Chat,
                gateway: "gateway".into(),
                provider_group: "gateway".into(),
                name: "cognition".into(),
                upstream_id: "cc/claude-opus-5-5".into(),
                context_window: Some(200_000),
                ..Default::default()
            },
            ModelEntry {
                api: withnav::tui::protocol::ApiKind::Chat,
                gateway: "gateway".into(),
                provider_group: "gateway".into(),
                name: "fast".into(),
                upstream_id: "cc/flash".into(),
                context_window: None,
                ..Default::default()
            },
        ],
        sessions: vec![SessionListEntry {
            bypass: false,
            bypass_seq: 0,
            incarnation: None,
            read_only: false,
            resume_disabled_reason: None,
            id: "01JABCDEFGH".into(),
            title: Some("Fix parser".into()),
            created_at: "2026-10-01T08:00:00Z".into(),
            updated_at: "2026-10-02T10:00:00Z".into(),
            message_count: 3,
            agent: None,
            live: true,
            parent_session_id: None,
            child_status: None,
            promptable: true,
        }],
        ..State::default()
    }
}

#[test]
fn watchdog_pause_and_budget_are_not_questions_and_permission_keys_take_priority() {
    let mut s = state();
    s.watchdog_paused = true;
    s.activity = Activity::AwaitingUser;
    s.conversation.blocks = vec![Block {
        id: "pause".into(),
        kind: BlockKind::Notice("Watchdog pause · alt+c continue or Stop".into()),
    }];
    let text = screen_text(&s, &UiState::default(), (100, 30));
    assert!(text.contains("Watchdog pause"));
    assert!(text.contains("alt+c"));
    assert!(!text.contains("QUESTION"));
    assert!(text.contains("watchdog paused"));
    assert!(withnav::tui::reducer::switch_blocked(&s.tabs[s.active]).is_none());
    let mut child = s.clone();
    child.parent_session_id = Some("parent".into());
    child.activity = Activity::OutOfSteps;
    child.conversation.blocks[0].kind = BlockKind::Notice("Out of steps · alt+c continue".into());
    let text = screen_text(&child, &UiState::default(), (100, 30));
    assert!(text.contains("alt+c continue"));
    assert!(!text.contains("send a message"));
    s.conversation.blocks[0].kind =
        BlockKind::Notice("Out of steps · send a message to continue".into());
    assert!(!screen_text(&s, &UiState::default(), (100, 30)).contains("QUESTION"));
    let mut permission = asking(prompt("bash", json!({"command":"true"}), ("folder", "/ws")));
    permission.watchdog_paused = true;
    let text = screen_text(&permission, &UiState::default(), (100, 30));
    assert!(text.contains("once"));
    assert!(!text.contains("alt+c continue"));
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
    let snapshot = (0..size.1)
        .map(|y| row_text(&terminal, y, size.0))
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(name, snapshot);
    snapshot
}

fn settings_view_state() -> (State, UiState) {
    use withnav::tui::{
        config::{SettingField, SettingsBody, SettingsPaths},
        protocol::{AgentEntry, ApiKind},
    };
    let mut s = state();
    s.workspace = "/ws/refactor-parser".into();
    s.catalog = vec![ModelEntry {
        name: "omniroute/combo/deepseek-v4-flash".into(),
        upstream_id: "combo/deepseek-v4-flash".into(),
        gateway: "omniroute".into(),
        provider_group: "combo".into(),
        api: ApiKind::Chat,
        variants: vec!["low".into(), "medium".into(), "high".into()],
        context_window: Some(200_000),
        ..Default::default()
    }];
    s.agents = vec![AgentEntry {
        id: "build".into(),
        mode: "primary".into(),
        ..Default::default()
    }];
    let fields = [
        ("layers.reflex", json!("inception/mercury-edit-2")),
        ("layers.instinct", json!("omniroute/typesafe/jev-1.13")),
        (
            "layers.metacognition",
            json!({"model":"omniroute/combo/deepseek-v4-flash","variant":"low"}),
        ),
        ("default_agent", json!("build")),
        (
            "agents.build",
            json!({"model":"omniroute/combo/deepseek-v4-flash","variant":"high"}),
        ),
        (
            "permissions.scratch_dirs",
            json!(["/run/user/1000/withnav", "/tmp/withnav"]),
        ),
        ("subagents.max_concurrent", json!(4)),
        ("subagents.watchdog_interval_secs", json!(300)),
        ("agent.watchdog_interval_secs", json!(300)),
        ("tui.theme", json!("ayu-dark")),
    ]
    .into_iter()
    .map(|(key, value)| SettingField {
        key: key.into(),
        default: value.clone(),
        value,
        set: true,
        error: None,
        applies: "new_sessions".into(),
    })
    .collect();
    s.config.body = Some(SettingsBody {
        revision: "r0".into(),
        paths: SettingsPaths {
            config: "/cfg/withnav/config.json".into(),
            models: Some("/cfg/withnav/models.json".into()),
            agents: vec![],
        },
        fields,
        gateways: vec![withnav::tui::config::GatewayInfo {
            name: "omniroute".into(),
            base_url: "http://localhost:20128/v1".into(),
            api_key_env: Some("OMNIROUTE_API_KEY".into()),
            key_source: "environment".into(),
            catalog_format: Some("omniroute".into()),
            connect_timeout_ms: Some(5000),
            request_timeout_ms: Some(600000),
            headers: vec![],
            model_count: Some(42),
            used_by: vec!["layers.metacognition".into()],
        }],
    });
    (
        s,
        UiState {
            screen: withnav::tui::config::Screen::Config,
            ..Default::default()
        },
    )
}

#[test]
fn settings_layers_metacognition_variant() {
    let (mut s, ui) = settings_view_state();
    s.config.section = withnav::tui::config::Section::Layers;
    s.config.row = 3;
    let text = render(
        "settings-layers-variant-140",
        (140, 40),
        &s,
        &ui,
        &Editor::new(),
    );
    assert!(text.contains("layers.metacognition.variant"));
    assert!(text.contains("low · medium"));
    assert!(text.contains("high"));
}

#[test]
fn settings_pending_two_changes() {
    let (mut s, ui) = settings_view_state();
    s.config.section = withnav::tui::config::Section::Subagents;
    s.config
        .stage("subagents.watchdog_interval_secs".into(), json!(60));
    s.config.stage("subagents.max_concurrent".into(), json!(5));
    let text = render("settings-pending-140", (140, 40), &s, &ui, &Editor::new());
    assert!(text.contains("● 2 unsaved"));
    assert!(text.contains("- 300"));
    assert!(text.contains("+ 60"));
}

#[test]
fn settings_model_picker_for_layer() {
    let (mut s, mut ui) = settings_view_state();
    s.config.section = withnav::tui::config::Section::Layers;
    s.config.row = 2;
    withnav::tui::config::handle(&mut s, &mut ui, withnav::tui::config::ConfigAction::Edit);
    let text = render(
        "settings-model-picker-140",
        (140, 40),
        &s,
        &ui,
        &Editor::new(),
    );
    assert!(text.contains("model for layers.metacognition"));
    assert!(text.contains("writes config.json"));
    assert!(!text.contains("switches the model only"));
}

#[test]
fn settings_gateways_read_only() {
    let (s, ui) = settings_view_state();
    let text = render("settings-gateways-140", (140, 40), &s, &ui, &Editor::new());
    assert!(text.contains("edit in models.json"));
    assert!(text.contains("read-only"));
}

#[test]
fn settings_leave_prompt() {
    let (mut s, ui) = settings_view_state();
    s.config.stage("tui.theme".into(), json!("nord-dark"));
    s.config.leave_prompt = true;
    let text = render("settings-leave-140", (140, 40), &s, &ui, &Editor::new());
    assert!(text.contains("[w] write · [d] discard · [esc] stay"));
}

#[test]
fn settings_row_server_error_and_stale_reload() {
    let (mut s, ui) = settings_view_state();
    s.config.section = withnav::tui::config::Section::Subagents;
    s.config.stage("subagents.max_concurrent".into(), json!(65));
    s.config.field_errors.insert(
        "subagents.max_concurrent".into(),
        "Must be between 1 and 64".into(),
    );
    s.config.status = withnav::tui::config::SaveStatus::Error {
        key: "subagents.max_concurrent".into(),
        message: "Must be between 1 and 64".into(),
    };
    let text = render("settings-row-error-140", (140, 40), &s, &ui, &Editor::new());
    assert!(text.contains("! Must be between 1 and 64"));
    s.config.status = withnav::tui::config::SaveStatus::StaleReloaded(1);
    assert!(
        screen_text(&s, &ui, (140, 40))
            .contains("config.json changed elsewhere; reloaded · 1 still staged")
    );
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
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.bg == Theme::default().selection_bg())
    );
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
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.fg == Theme::default().red)
    );
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
        Some(Action::OpenConfig)
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
    ui.selection.insert("Models".into(), 1);
    let enter = key(KeyCode::Enter, KeyModifiers::NONE);
    // a live conversation switches the model, with no variant step
    ui.selection.insert("Models".into(), 1);
    assert_eq!(
        route_key(enter, &s, &mut ui, &mut editor),
        Some(Action::SwitchModel("cognition".into()))
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
    use withnav::tui::state::Focus;
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
    // an empty tab: the model, then the variant it starts on
    let mut s = state();
    s.session = None;
    let mut ui = UiState {
        overlay: Some(Overlay::Models),
        selection: std::collections::HashMap::from([("Models".into(), 1)]),
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
    ui.selection.insert("Models".into(), 1);
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
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    // Before any snapshot: the provider's last prompt count over the window.
    let terminal = cells(&s, &ui, (60, 20));
    let header = row_text(&terminal, 0, 60);
    assert!(header.contains("· 12.4k / 200k"), "{header}");
    s.conversation.context = Some(withnav::tui::protocol::ContextSnapshot {
        gateway: "gateway".into(),
        upstream_id: "upstream".into(),
        model: "cognition".into(),
        variant: None,
        window: 1_000_000,
        window_source: "models.json".into(),

        estimate_total: 42_000,
        breakdown: Default::default(),
        output_reserve: 8192,
        margin: 50_000,
        input_budget: 941_808,
        percent: 4,
        last_usage: None,
        skills: Default::default(),
        rule_files: 0,
        incarnation: ulid::Ulid::generate(),
        revision: 1,
    });
    let terminal = cells(&s, &ui, (60, 20));
    let header = row_text(&terminal, 0, 60);
    assert!(header.contains("· ~42k / 1M · 4%"), "{header}");
    let text = buffer_text(&s);
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
    (0..20)
        .map(|y| row_text(&terminal, y, 60))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn message_labels_follow_the_source_actor_id() {
    for (source, label) in [
        ("user:http", "▸ Captain"),
        ("user", "▸ Captain"),
        ("driver", "▸ Captain"),
        ("captain", "▸ Captain"),
        ("navigator", "Beacon · Remark"),
        ("navigator:main", "Beacon · Remark"),
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
    assert!(
        buffer
            .content()
            .iter()
            .any(|cell| cell.bg == theme.selection_bg_on_surface())
    );
}

#[test]
fn theme_picker_previews_and_esc_restores() {
    let s = state();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    let apply = |ui: &mut UiState, action: Option<Action>| {
        if let Some(action) = action {
            withnav::tui::reducer::handle_action(&mut state(), ui, action)
        } else {
            vec![]
        }
    };
    let open = Some(Action::OpenOverlay(Overlay::Themes));
    apply(&mut ui, open.clone());
    assert_eq!(ui.selection.get("Themes"), Some(&4));
    ui.selection.insert("Themes".into(), 0);
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
    assert_eq!(ui.theme, ThemeId::TideDark, "esc puts the old theme back");
    assert_eq!(ui.overlay, None);

    apply(&mut ui, open);
    ui.selection.insert("Themes".into(), 0);
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
        vec![withnav::tui::protocol::Effect::SaveTheme(
            ThemeId::NordLight
        )]
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
    withnav::tui::reducer::handle_action(&mut s, &mut ui, Action::OpenOverlay(Overlay::Themes));
    assert_eq!(ui.selection.get("Themes"), Some(&2));
    // Confirming the theme already in effect saves it again, so a failed save can be retried.
    assert_eq!(
        withnav::tui::reducer::handle_action(&mut s, &mut ui, Action::PickTheme(ThemeId::NordDark)),
        vec![withnav::tui::protocol::Effect::SaveTheme(ThemeId::NordDark)]
    );
    assert_eq!(
        withnav::tui::commands::command_action("/theme", Default::default()),
        Some(Action::OpenOverlay(Overlay::Themes))
    );
}

/// Sends `code` to the open theme picker and applies the action it returns, like the runtime.
fn theme_key(ui: &mut UiState, code: KeyCode) -> Option<Action> {
    let s = state();
    let action = route_key(key(code, KeyModifiers::NONE), &s, ui, &mut Editor::new());
    if let Some(action) = action.clone() {
        withnav::tui::reducer::handle_action(&mut state(), ui, action);
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
    withnav::tui::reducer::handle_action(
        &mut state(),
        &mut ui,
        Action::OpenOverlay(Overlay::Themes),
    );
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
    let help: Vec<_> = withnav::tui::commands::KEYS
        .iter()
        .map(|(k, _)| *k)
        .collect();
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
    let theme = Theme::default();
    assert!(row_text(&t, 1, 22).starts_with("▌1 Fix parser"));
    assert!(row_text(&t, 2, 22).contains("◎ Nav · idle"));
    assert!(
        row_text(&t, 3, 22).contains("2 01JZZZZZ"),
        "short id for an untitled tab"
    );
    assert!(row_text(&t, 3, 22).contains('●'), "unread mark");
    assert!(row_text(&t, 4, 22).contains("◎ Nav · running"));
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
    assert!(row_text(&wide, 2, 80).contains("◎ Nav · idle"));
    ui.sidebar_hidden = true;
    render("sidebar-hidden", (100, 24), &s, &ui, &Editor::new());
    let hidden = cells(&s, &ui, (100, 24));
    assert!(row_text(&hidden, 0, 100).starts_with(" 🛡 safe"));
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
    let theme = Theme::default();
    assert!(row_text(&t, 4, 22).contains("◎ Nav · waiting"));
    assert!(row_text(&t, 6, 22).contains("◎ Nav · error"));
    let err_x = row_text(&t, 6, 22).find("error").unwrap();
    let err_x = row_text(&t, 6, 22)[..err_x].chars().count() as u16;
    assert_eq!(t.backend().buffer()[(err_x, 6)].fg, theme.red);
}

#[test]
fn the_gauge_follows_the_active_tab() {
    let gauge = |estimate_total: u64| withnav::tui::protocol::ContextSnapshot {
        gateway: "gateway".into(),
        upstream_id: "upstream".into(),
        model: "cognition".into(),
        variant: None,
        window: 1_000_000,
        window_source: "models.json".into(),

        estimate_total,
        breakdown: Default::default(),
        output_reserve: 8192,
        margin: 50_000,
        input_budget: 941_808,
        percent: estimate_total * 100 / 1_000_000,
        last_usage: None,
        skills: Default::default(),
        rule_files: 0,
        incarnation: ulid::Ulid::generate(),
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
    s.tabs[0].generation = withnav::tui::protocol::Generation(1);
    s.tabs[0].conversation.blocks = vec![message("7", "assistant", "first tab line")];
    let mut other = s.tabs[0].clone();
    other.generation = withnav::tui::protocol::Generation(2);
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
    let theme = Theme::default();
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
    let theme = Theme::default();
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
    assert_eq!(ui.focus, withnav::tui::state::Focus::Transcript);
    assert_eq!(route_key(tab, &s, &mut ui, &mut editor), None);
    assert_eq!(ui.focus, withnav::tui::state::Focus::Editor);
    editor.set_text("");
    let back = key(KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(route_key(back, &s, &mut ui, &mut editor), None);
    assert_eq!(ui.focus, withnav::tui::state::Focus::Transcript);
    let help: Vec<_> = withnav::tui::commands::KEYS
        .iter()
        .map(|(k, _)| *k)
        .collect();
    assert!(
        help.contains(&"tab") && help.contains(&"shift+tab"),
        "{help:?}"
    );
}

#[test]
fn the_agent_shows_in_the_status_line_and_the_sidebar() {
    let theme = Theme::default();
    let mut s = done();
    let ui = UiState::default();
    let t = cells(&s, &ui, (100, 30));
    let status = (0..30)
        .find(|y| row_text(&t, *y, 100).contains("▌◎ build  cognition"))
        .expect("status line");
    let x = row_text(&t, status, 100)
        .chars()
        .position(|c| c == '▌')
        .unwrap() as u16;
    assert_eq!(t.backend().buffer()[(x, status)].fg, theme.nav);
    assert_eq!(t.backend().buffer()[(x + 1, status)].fg, theme.nav);
    s.agent = Some("plan".into());
    s.variant = Some("high".into());
    s.conversation.context = None;
    render("agent-plan", (100, 30), &s, &ui, &Editor::new());
    let t = cells(&s, &ui, (100, 30));
    let status = (0..30)
        .find(|y| row_text(&t, *y, 100).contains("▌◎ plan  cognition"))
        .expect("status line");
    let line = row_text(&t, status, 100);
    assert!(line.contains("· high · 12.4k / 200k"), "{line}");
    let x = line.chars().position(|c| c == '▌').unwrap() as u16;
    assert_eq!(t.backend().buffer()[(x, status)].fg, theme.nav);
    assert!(row_text(&t, 2, 22).contains("◎ Nav · idle"));
    let x = row_text(&t, 2, 22).find("◎ Nav").unwrap() as u16;
    let x = row_text(&t, 2, 22)[..x as usize].chars().count() as u16;
    assert_eq!(t.backend().buffer()[(x, 2)].fg, theme.nav);
}

/// A catalog of 295 aliases; `opus` offers variants with `high` as default, `flash` none.
fn big_catalog() -> State {
    let mut s = state();
    s.catalog = (0..293)
        .map(|n| ModelEntry {
            api: withnav::tui::protocol::ApiKind::Chat,
            gateway: "gateway".into(),
            provider_group: "gateway".into(),
            name: format!("alias-{n:03}"),
            upstream_id: format!("vendor/model-{n:03}"),
            context_window: Some(128_000),
            ..Default::default()
        })
        .collect();
    s.catalog.push(ModelEntry {
        api: withnav::tui::protocol::ApiKind::Chat,
        gateway: "gateway".into(),
        provider_group: "gateway".into(),
        name: "opus".into(),
        upstream_id: "cc/claude-opus-5-5".into(),
        context_window: Some(1_000_000),
        variants: vec!["low".into(), "high".into(), "max".into()],
        default_variant: Some("high".into()),
        ..Default::default()
    });
    s.catalog.push(ModelEntry {
        api: withnav::tui::protocol::ApiKind::Chat,
        gateway: "gateway".into(),
        provider_group: "gateway".into(),
        name: "flash".into(),
        upstream_id: "cc/flash".into(),
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
    for _ in 0..400 {
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
        all.contains("▌  opus"),
        "the last row is shown selected, one level under its direct gateway: {all}"
    );
    type_text("opus", &s, &mut ui, &mut editor);
    ui.selection.insert("Models".into(), 1);
    render("model-picker-filtered", (100, 30), &s, &ui, &editor);
    let t = cells(&s, &ui, (100, 30));
    let row = (0..30)
        .map(|y| row_text(&t, y, 100))
        .find(|r| r.contains("▌  opus"))
        .expect("the filtered row");
    assert!(row.contains("1M"), "{row}");
    assert!(
        !row.contains("low high max"),
        "variants are in the detail line: {row}"
    );
    let detail = (0..30)
        .map(|y| row_text(&t, y, 100))
        .find(|r| r.contains("opus · variants: low high max"))
        .expect("the detail line of the highlighted model");
    assert!(detail.contains("opus"), "{detail}");
    assert!(!all.contains("alias-") || !row.contains("alias-"));
    // the filter also matches the model name
    ui.filter = "alias-29".into();
    let t = cells(&s, &ui, (100, 30));
    let shown: Vec<String> = (0..30)
        .map(|y| row_text(&t, y, 100))
        .filter(|r| r.contains("alias-29") && r.contains("128k"))
        .collect();
    assert_eq!(shown.len(), 3, "alias-290 … alias-292: {shown:?}");
}

#[test]
fn picking_a_model_with_variants_asks_which_one_default_first() {
    // an empty tab: the variant step chooses what the conversation starts on
    let mut s = big_catalog();
    s.session = None;
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    route_key(
        key(KeyCode::F(2), KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    type_text("opus", &s, &mut ui, &mut editor);
    ui.selection.insert("Models".into(), 1);
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
    assert!(
        rows.iter()
            .any(|r| r.contains("off") && r.contains("no variant"))
    );
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
    let mut s = big_catalog();
    s.session = None;
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    route_key(
        key(KeyCode::F(2), KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    type_text("flash", &s, &mut ui, &mut editor);
    ui.selection.insert("Models".into(), 1);
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
    s.selected_model_id = Some("opus".into());
    s.variant = Some("high".into());
    s.variant_offer = Some(vec!["low".into(), "high".into(), "max".into()]);
    s.activity = Activity::Running;
    let mut ui = UiState {
        overlay: Some(Overlay::Variants),
        variant_pick: Some(withnav::tui::state::VariantPick::Current),
        ..UiState::default()
    };
    // opened as `/variant` opens it: on the variant in use
    let (_, start) =
        withnav::tui::commands::variant_choices(&s, &withnav::tui::state::VariantPick::Current);
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
fn the_status_line_sums_up_the_mcp_servers_and_gives_way_first_on_a_narrow_line() {
    use withnav::bus::event::{McpServerView, McpStatus};
    let server = |id: &str, state: &str| McpServerView {
        server_id: id.into(),
        transport: "stdio".into(),
        state: state.into(),
        tool_count: 0,
        skipped_tools: vec![],
        skipped_more: 0,
        error: None,
        stderr_tail: String::new(),
        stderr_truncated: false,
    };
    let mut s = done();
    let status_line = |s: &State, width: u16| {
        let t = cells(s, &UiState::default(), (width, 30));
        (0..30)
            .map(|y| row_text(&t, y, width))
            .find(|r| r.contains(" tok"))
            .unwrap()
    };
    // Before any status, nothing is said.
    assert!(!status_line(&s, 120).contains("mcp "));
    s.mcp = Some(McpStatus {
        incarnation: ulid::Ulid::generate(),
        revision: 1,
        servers: vec![
            server("github", "ready"),
            server("contextd", "failed"),
            server("off", "disabled"),
        ],
    });
    let line = status_line(&s, 120);
    assert!(
        line.contains("mcp 1/2"),
        "disabled servers are not counted: {line}"
    );
    assert!(line.contains("session "), "{line}");
    // The summary goes first when the line is short; the token count stays.
    let narrow = status_line(&s, 32);
    assert!(!narrow.contains("mcp "), "{narrow}");
    assert!(narrow.contains("session "), "{narrow}");
}

#[test]
fn the_status_line_reads_model_variant_and_gauge() {
    let mut s = done();
    s.variant = Some("high".into());
    s.conversation.context = Some(withnav::tui::protocol::ContextSnapshot {
        gateway: "gateway".into(),
        upstream_id: "upstream".into(),
        model: "cognition".into(),
        variant: Some("high".into()),
        window: 1_000_000,
        window_source: "models.json".into(),

        estimate_total: 42_000,
        breakdown: Default::default(),
        output_reserve: 8192,
        margin: 50_000,
        input_budget: 941_808,
        percent: 4,
        last_usage: None,
        skills: Default::default(),
        rule_files: 0,
        incarnation: ulid::Ulid::generate(),
        revision: 1,
    });
    let t = cells(&s, &UiState::default(), (120, 30));
    let line = (0..30)
        .map(|y| row_text(&t, y, 120))
        .find(|r| r.contains("▌◎"))
        .unwrap();
    assert!(
        line.contains("cognition · assigned · high · ~42k / 1M · 4%"),
        "{line}"
    );
    // on a narrow screen the model name is cut before the variant or the gauge
    let t = cells(&s, &UiState::default(), (70, 20));
    let line = (0..20)
        .map(|y| row_text(&t, y, 70))
        .find(|r| r.contains("▌◎"))
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
    .map(|n| withnav::tui::protocol::ServerCommand {
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
    assert_eq!(actions.last(), Some(&Action::SwitchModel("fast".into())));
    assert_eq!(editor.text(), "", "a client command clears its line");
    // `/new <alias>` still opens a tab on it
    let actions = keys("/new fast⏎", &s, &mut ui, &mut editor);
    assert_eq!(actions.last(), Some(&Action::PickAgent("fast".into())));
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
        kind: BlockKind::Permission(withnav::tui::state::PermissionPrompt {
            always_label: None,
            skill_script: None,
            child_session_id: None,
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
            always_saved: true,
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
        "o once",
    ] {
        assert!(shown.contains(want), "{want}: {shown}");
    }
    assert!(!shown.contains("+ line 7"), "{shown}");
    // the open prompt has the accent rule
    let t = cells(&s, &ui, (100, 36));
    let theme = Theme::default();
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
        json!({"command": "cargo test -p withnav"}),
        ("folder", "/ws"),
    ));
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(shown.contains("wants to run bash in /ws"), "{shown}");
    assert!(shown.contains("$ cargo test -p withnav"), "{shown}");
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
    assert!(!shown.contains("a always"), "{shown}");
}

/// An open prompt for `args` as the server sends it (its copy of the arguments cut to 4 KiB),
/// with the tool call itself in the transcript when `with_call`.
fn asking_for(tool: &str, args: serde_json::Value, with_call: bool) -> State {
    let call = withnav::provider::ToolCall {
        id: "c1".into(),
        name: tool.into(),
        args: args.clone(),
        args_valid: true,
    };
    let request = withnav::agent::permissions::PermissionRequest::new(
        &call,
        withnav::agent::permissions::Scope::Folder("/ws".into()),
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

/// An open request of the `plan` agent, as a guarded exception reaches the Captain.
fn plan_exception() -> State {
    let mut block = prompt(
        "write",
        json!({"path": "/ws/src/main.rs", "content": "fn main() {}"}),
        ("folder", "/ws/src"),
    );
    if let BlockKind::Permission(p) = &mut block.kind {
        p.agent = "plan".into();
        p.reason =
            "Instinct judged this outside the plan role. `write` acts in /ws/src (approve this call once, or deny)"
                .into();
    }
    asking(block)
}

#[test]
fn a_prompt_shows_why_it_asks_and_a_plan_exception_offers_only_once_or_deny() {
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    // Any request: the reason sits under the header, muted, on one line.
    let build = asking(prompt("bash", json!({"command": "ls"}), ("folder", "/ws")));
    let shown = render(
        "permission-reason-build",
        (100, 30),
        &build,
        &ui,
        &Editor::new(),
    );
    assert!(shown.contains("it changes files"), "{shown}");
    assert!(!shown.contains("a always for this folder"), "{shown}");

    let plan = plan_exception();
    let shown = render(
        "permission-plan-exception",
        (100, 30),
        &plan,
        &ui,
        &Editor::new(),
    );
    for want in [
        "PERMISSION · plan wants to run write in /ws/src",
        "Instinct judged this outside the plan role.",
        "o once · r reject",
    ] {
        assert!(shown.contains(want), "{want}: {shown}");
    }
    assert!(!shown.contains("always"), "{shown}");
    let t = cells(&plan, &ui, (100, 30));
    let theme = Theme::default();
    let row = (0..30)
        .find(|y| row_text(&t, *y, 100).contains("Instinct judged"))
        .unwrap();
    let x = row_text(&t, row, 100).find("Instinct").unwrap();
    let x = row_text(&t, row, 100)[..x].chars().count() as u16;
    assert_eq!(t.backend().buffer()[(x, row)].fg, theme.muted().fg.unwrap());
    // One row, even for a narrow screen: cut with an ellipsis, never wrapped.
    let narrow = screen_text(&plan, &ui, (60, 30));
    let reason_rows = narrow
        .lines()
        .filter(|l| l.contains("Instinct judged"))
        .count();
    assert_eq!(reason_rows, 1, "{narrow}");
}

#[test]
fn a_does_nothing_for_a_plan_exception_but_says_why() {
    let s = plan_exception();
    let mut ui = UiState {
        follow_bottom: true,
        prompt_shown: Some("R1".into()),
        ..UiState::default()
    };
    let mut editor = Editor::new();
    let a = keys("a", &s, &mut ui, &mut editor);
    assert_eq!(
        a,
        [Action::ShowNotice(
            "Always is not offered for this request".into()
        )],
        "no answer is sent"
    );
    assert!(
        a.iter()
            .all(|action| !matches!(action, Action::AnswerPermission { .. })),
        "{a:?}"
    );
    assert_eq!(
        keys("o", &s, &mut ui, &mut editor),
        [Action::AnswerPermission {
            request_id: "R1".into(),
            answer: withnav::tui::protocol::PermissionAnswer {
                decision: "allow_once".into(),
                message: None,
            },
        }]
    );
}

#[test]
fn reject_draft_limits_utf8_bytes_without_answering_or_truncating() {
    let s = asking(prompt("bash", json!({"command":"true"}), ("folder", "/ws")));
    let mut ui = UiState {
        prompt_shown: Some("R1".into()),
        ..UiState::default()
    };
    let mut editor = Editor::new();
    keys("r", &s, &mut ui, &mut editor);
    assert!(route_paste("é".repeat(1024), &s, &mut ui, &mut editor).is_none());
    assert_eq!(ui.deny_draft("R1").unwrap().len(), 2048);
    assert!(matches!(
        route_paste("a".into(), &s, &mut ui, &mut editor),
        Some(Action::ShowNotice(_))
    ));
    assert!(matches!(
        keys("é", &s, &mut ui, &mut editor).as_slice(),
        [Action::ShowNotice(_)]
    ));
    assert_eq!(ui.deny_draft("R1").unwrap().len(), 2048);
    assert!(matches!(
        keys("⏎", &s, &mut ui, &mut editor).as_slice(),
        [Action::AnswerPermission { .. }]
    ));
    keys("r", &s, &mut ui, &mut editor);
    ui.deny_message.as_mut().unwrap().text = "a".repeat(2047);
    assert!(matches!(
        keys("é", &s, &mut ui, &mut editor).as_slice(),
        [Action::ShowNotice(_)]
    ));
    ui.deny_message.as_mut().unwrap().text = "a".repeat(2049);
    assert!(matches!(
        keys("⏎", &s, &mut ui, &mut editor).as_slice(),
        [Action::ShowNotice(_)]
    ));
    assert_eq!(ui.deny_draft("R1").unwrap().len(), 2049);
}

#[test]
fn a_answers_always_only_when_the_server_supplies_its_label() {
    let mut block = prompt(
        "bash",
        json!({"command":"git push origin topic"}),
        ("folder", "/ws"),
    );
    if let BlockKind::Permission(p) = &mut block.kind {
        p.always_label = Some("Always (project): bash git push *".into());
    }
    let s = asking(block);
    let mut ui = UiState {
        prompt_shown: Some("R1".into()),
        ..UiState::default()
    };
    let mut editor = Editor::new();
    assert!(screen_text(&s, &ui, (200, 30)).contains("Always (project): bash git push *"));
    assert_eq!(
        keys("a", &s, &mut ui, &mut editor),
        [Action::AnswerPermission {
            request_id: "R1".into(),
            answer: withnav::tui::protocol::PermissionAnswer {
                decision: "allow_always".into(),
                message: None
            },
        }]
    );
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
        answer: withnav::tui::protocol::PermissionAnswer {
            decision: decision.into(),
            message: message.map(str::to_string),
        },
    };
    assert_eq!(
        keys("o", &s, &mut ui, &mut editor),
        [answer("allow_once", None)]
    );
    assert_eq!(
        keys("a", &s, &mut ui, &mut editor),
        [Action::ShowNotice(
            "Always is not offered for this request".into()
        )]
    );
    assert!(keys("r", &s, &mut ui, &mut editor).is_empty());
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
        shown.contains("reject with a message: use cargo clean"),
        "{shown}"
    );
    assert!(
        shown.contains("enter send · esc reject without a message"),
        "{shown}"
    );
    assert_eq!(
        keys("⏎", &s, &mut ui, &mut editor),
        [answer("reject", Some("use cargo clean"))]
    );
    assert_eq!(ui.deny_message, None);
    // esc denies without one
    keys("r", &s, &mut ui, &mut editor);
    let esc = route_key(
        key(KeyCode::Esc, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(esc, Some(answer("reject", None)));
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
    keys("rnot here", &s, &mut ui, &mut editor);
    assert_eq!(ui.deny_draft("R1"), Some("not here"));
    let s2 = next_request(&s);
    // the message is not shown on R2, and enter does not deny R2 with it
    let shown = screen_text(&s2, &ui, (100, 30));
    assert!(!shown.contains("not here"), "{shown}");
    assert!(shown.contains("o once"), "{shown}");
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
    keys("rold", &s, &mut ui, &mut editor);
    route_paste("pasted".into(), &s2, &mut ui, &mut editor);
    assert_eq!(ui.deny_message, None);
    assert_eq!(editor.text(), "pasted", "it goes to the editor");
    // on R2, n starts a message of its own
    let mut ui = UiState {
        prompt_shown: Some("R1".into()),
        ..UiState::default()
    };
    let mut editor = Editor::new();
    keys("rold", &s, &mut ui, &mut editor);
    ui.prompt_shown = Some("R2".into());
    keys("rnew", &s2, &mut ui, &mut editor);
    assert_eq!(ui.deny_draft("R2"), Some("new"));
    assert_eq!(
        keys("⏎", &s2, &mut ui, &mut editor),
        [Action::AnswerPermission {
            request_id: "R2".into(),
            answer: withnav::tui::protocol::PermissionAnswer {
                decision: "reject".into(),
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
    // keys queued before the next frame never answer an unseen prompt
    assert!(keys("aao", &s, &mut ui, &mut editor).is_empty(), "no grant");
    assert_eq!(ui.prompt_shown, None, "only a drawn frame marks it shown");
    draw_into(&s, &mut ui, (100, 16));
    assert_eq!(ui.prompt_shown.as_deref(), Some("R1"), "now in view");
    assert_eq!(
        keys("a", &s, &mut ui, &mut editor),
        [Action::ShowNotice(
            "Always is not offered for this request".into()
        )]
    );
    // in view from the start: the first key answers
    let mut ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    draw_into(&s, &mut ui, (100, 30));
    assert_eq!(keys("o", &s, &mut ui, &mut editor).len(), 1);
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
    assert!(keys("ooar", &s, &mut ui, &mut editor).is_empty());
    assert_eq!(editor.text(), "", "the keys are not typed either");
    assert_eq!(ui.deny_message, None);
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(shown.contains("sent, waiting for the server"), "{shown}");
    assert!(!shown.contains("o once"), "{shown}");
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
    assert!(!shown.contains("o once"), "{shown}");
    let t = cells(&s, &ui, (100, 30));
    let theme = Theme::default();
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
    assert!(keys("o", &s, &mut keys_ui, &mut editor).is_empty());
    assert_eq!(editor.text(), "o", "the key types: nothing is open");
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
    assert!(keys("oar", &s, &mut ui, &mut editor).is_empty());
    assert_eq!(editor.text(), "oarwhy ", "typed at the cursor");
    // a resolved request takes no keys
    let mut s = s;
    if let BlockKind::Permission(p) = &mut s.conversation.blocks.last_mut().unwrap().kind {
        p.resolution = Some("allow_once".into());
    }
    s.conversation.open_permissions.clear();
    let mut editor = Editor::new();
    assert!(keys("o", &s, &mut ui, &mut editor).is_empty());
    assert_eq!(editor.text(), "o");
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(shown.contains("allowed once"), "{shown}");
    assert!(!shown.contains("o once"), "{shown}");
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
        .find(|y| row_text(&t, *y, 100).contains("o once"))
        .unwrap();
    let line = row_text(&t, keys_row, 100);
    let x = line[..line.find("o once").unwrap()].chars().count() as u16;
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

// ---------------------------------------------------------------- header

#[test]
fn the_header_shows_the_branch_its_changes_and_the_agent_in_its_colour() {
    let mut s = done();
    s.git = Some(withnav::workspace::git::GitStatus {
        branch: Some("feat/tui-design".into()),
        dirty: 3,
        files: vec![],
    });
    let ui = UiState::default();
    let t = cells(&s, &ui, (140, 20));
    let header = row_text(&t, 0, 140);
    for want in [
        "◆ withnav",
        "~/dev/project",
        "feat/tui-design ●3",
        "▌◎ Nav /build  cognition · assigned · 12.4k / 200k",
    ] {
        assert!(header.contains(want), "{want}: {header}");
    }
    let theme = Theme::default();
    let x = header[..header.find("▌◎ Nav").unwrap()].chars().count() as u16;
    assert_eq!(t.backend().buffer()[(x, 0)].fg, theme.nav);
    s.agent = Some("plan".into());
    let t = cells(&s, &ui, (140, 20));
    let header = row_text(&t, 0, 140);
    let x = header[..header.find("▌◎ Nav").unwrap()].chars().count() as u16;
    assert_eq!(t.backend().buffer()[(x, 0)].fg, theme.nav);
    // not a repository: no branch, nothing made up
    s.git = None;
    let header = row_text(&cells(&s, &ui, (140, 20)), 0, 140);
    assert!(!header.contains('●'), "{header}");
    // narrow: the path and the model give way before the agent and the gauge
    let header = row_text(&cells(&s, &ui, (60, 20)), 0, 60);
    assert!(header.contains("▌◎ plan  cognition"), "{header}");
    assert!(header.contains("/ 200k"), "{header}");
}

// ---------------------------------------------------------------- right column

fn snapshot_with(
    breakdown: withnav::bus::event::Breakdown,
) -> withnav::tui::protocol::ContextSnapshot {
    withnav::tui::protocol::ContextSnapshot {
        gateway: "gateway".into(),
        upstream_id: "upstream".into(),
        model: "cognition".into(),
        variant: None,
        window: 200_000,
        window_source: "models.json".into(),

        estimate_total: 41_200,
        breakdown,
        output_reserve: 8192,
        margin: 10_000,
        input_budget: 181_808,
        percent: 21,
        last_usage: None,
        skills: withnav::bus::event::SkillCounts {
            listed: 12,
            omitted: 3,
        },
        rule_files: 2,
        incarnation: ulid::Ulid(7),
        revision: 1,
    }
}

/// `done()` with a context snapshot, a compaction and git changes: what the column reads.
fn with_panel_data() -> State {
    let mut s = done();
    s.conversation.context = Some(snapshot_with(withnav::bus::event::Breakdown {
        system: 3_100,
        rules: 1_900,
        skills_catalog: 2_400,
        tools: 6_800,
        messages: 27_000,
    }));
    s.conversation.last_compaction = Some(withnav::bus::event::ContextCompacted {
        written_by: None,
        passes: 1,
        generation: 1,
        through_message: 40,
        pruned: 6,
        estimate_before: 150_000,
        estimate_after: 38_000,
        reason: None,
    });
    let files = (0..14)
        .map(|n| withnav::workspace::git::Change {
            mark: ['M', 'A', 'D', '?'][n % 4],
            path: format!("src/tui/view/file_{n}.rs"),
        })
        .take(withnav::workspace::git::MAX_FILES)
        .collect();
    s.git = Some(withnav::workspace::git::GitStatus {
        branch: Some("feat/tui-design".into()),
        dirty: 14,
        files,
    });
    s
}

#[test]
fn the_right_column_reads_the_snapshot_the_compaction_and_git() {
    let s = with_panel_data();
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    render("panel-160", (160, 40), &s, &ui, &Editor::new());
    let shown = screen_text(&s, &ui, (160, 40));
    for want in [
        "AGENTS",
        "● ◎ Nav  in use",
        "rules · 2 files",
        "messages                    ~27k",
        "next request          ~41k · 21%",
        "input budget                182k",
        "compacted ×1 · ~150k → ~38k",
        "skills listed                 12",
        "omitted, no room               3",
        "mcp                            —",
        "M src/tui/view/file_0.rs",
        "CHANGES",
    ] {
        assert!(shown.contains(want), "{want}: {shown}");
    }
    // nothing the server does not report: no MCP status read yet, so no server is listed
    for absent in [
        "INTENT",
        "drift",
        "mcp:",
        "plugin:",
        "vigie",
        "review",
        "Implements the task",
        "assigned:",
    ] {
        assert!(!shown.contains(absent), "{absent}: {shown}");
    }
}

#[test]
fn the_right_column_shows_a_dash_before_any_snapshot_and_no_changes_outside_git() {
    let s = done();
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    let shown = screen_text(&s, &ui, (160, 30));
    assert!(shown.contains("CONTEXT"), "{shown}");
    assert!(shown.contains("SKILLS · MCP · PLUGINS"), "{shown}");
    assert!(shown.contains(" —"), "{shown}");
    assert!(!shown.contains("CHANGES"), "not a repository: {shown}");
}

#[test]
fn the_right_column_lists_mcp_servers_with_their_state_and_the_skills_the_model_loaded() {
    use withnav::bus::event::{McpServerView, McpStatus};
    let mut s = with_panel_data();
    let server = |id: &str, state: &str, tools: usize, error: Option<&str>| McpServerView {
        server_id: id.into(),
        transport: "stdio".into(),
        state: state.into(),
        tool_count: tools,
        skipped_tools: vec![],
        skipped_more: 0,
        error: error.map(Into::into),
        stderr_tail: String::new(),
        stderr_truncated: false,
    };
    s.mcp = Some(McpStatus {
        incarnation: ulid::Ulid::generate(),
        revision: 2,
        servers: vec![
            server("github", "ready", 12, None),
            server("codegraph", "ready", 1, None),
            server("contextd", "failed", 0, Some("MCP startup timed out")),
        ],
    });
    for (id, skill, status) in [
        ("s1", "rust-clippy", Some(ToolStatus::Ok)),
        ("s2", "rust-clippy", Some(ToolStatus::Ok)),
        ("s3", "git-commit", Some(ToolStatus::Ok)),
        ("s4", "missing", Some(ToolStatus::Error)),
    ] {
        s.tabs[0].conversation.blocks.push(Block {
            id: id.into(),
            kind: BlockKind::ToolCall {
                id: id.into(),
                name: "skill".into(),
                args: json!({ "name": skill }),
                status,
                output: String::new(),
                metadata: json!({}),
                duration_ms: None,
                exit_code: None,
            },
        });
    }
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    let shown = screen_text(&s, &ui, (160, 60));
    for want in [
        "SKILLS · MCP · PLUGINS",
        "skills listed",
        "model loaded                   2",
        "rust-clippy · git-commit",
        "● mcp:github  12 tools",
        "● mcp:codegraph  1 tool ",
        "● mcp:contextd  MCP startup",
    ] {
        assert!(shown.contains(want), "{want}: {shown}");
    }
    // A failed `skill` call loaded nothing (the transcript still shows the call).
    assert!(
        !shown.contains("rust-clippy · git-commit · missing"),
        "{shown}"
    );
    assert!(shown.contains("rust-clippy · git-commit "), "{shown}");
}

#[test]
fn the_right_column_hides_under_140_columns_and_alt_r_toggles_it() {
    let s = with_panel_data();
    let mut ui = UiState::default();
    assert!(!screen_text(&s, &ui, (139, 30)).contains("AGENTS"));
    assert!(screen_text(&s, &ui, (140, 30)).contains("AGENTS"));
    // alt+r flips what was on screen
    let mut editor = Editor::new();
    let action = route_key(
        key(KeyCode::Char('r'), KeyModifiers::ALT),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(action, Some(Action::TogglePanel));
    ui.panel_drawn = false;
    let mut state = s.clone();
    withnav::tui::reducer::handle_action(&mut state, &mut ui, Action::TogglePanel);
    assert_eq!(ui.panel_shown, Some(true));
    assert!(
        screen_text(&s, &ui, (120, 30)).contains("AGENTS"),
        "shown on demand"
    );
    ui.panel_drawn = true;
    withnav::tui::reducer::handle_action(&mut state, &mut ui, Action::TogglePanel);
    assert!(
        !screen_text(&s, &ui, (200, 30)).contains("AGENTS"),
        "hidden on demand"
    );
    // too narrow to leave the transcript room: not drawn even when asked for
    ui.panel_shown = Some(true);
    assert!(!screen_text(&s, &ui, (90, 30)).contains("AGENTS"));
}

// ---------------------------------------------------------------- key bar

fn bottom_row(s: &State, ui: &UiState, editor: &Editor, size: (u16, u16)) -> String {
    let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
    terminal
        .draw(|f| view::draw(f, s, ui, editor, &mut RenderCache::default()))
        .unwrap();
    row_text(&terminal, size.1 - 1, size.0)
}

#[test]
fn the_key_bar_follows_what_the_keys_do_now() {
    let s = done();
    let ui = UiState::default();
    let mut editor = Editor::new();
    let bar = bottom_row(&s, &ui, &editor, (200, 20));
    assert!(
        bar.contains(
            "tab build/plan · ctrl+p palette · alt+1…9 tabs · /model · /compact · esc stop · alt+r panel · f1 help"
        ),
        "{bar}"
    );
    // narrow: whole hints only, the first ones kept
    let bar = bottom_row(&s, &ui, &editor, (60, 20));
    assert!(bar.trim_end().ends_with("/model"), "{bar}");
    // typing: sending and newlines
    editor.set_text("hello");
    let bar = bottom_row(&s, &ui, &editor, (200, 20));
    assert!(bar.contains("enter send · alt+enter newline"), "{bar}");
    // a prompt waits: its keys first
    let s = asking(prompt("bash", json!({"command": "ls"}), ("folder", "/ws")));
    let bar = bottom_row(&s, &ui, &Editor::new(), (200, 30));
    assert!(
        bar.contains("o once · r reject · esc stop · ctrl+p palette"),
        "{bar}"
    );
    let theme = Theme::default();
    let mut terminal = Terminal::new(TestBackend::new(200, 30)).unwrap();
    terminal
        .draw(|f| view::draw(f, &s, &ui, &Editor::new(), &mut RenderCache::default()))
        .unwrap();
    let x = bar[..bar.find("o once").unwrap()].chars().count() as u16;
    assert_eq!(
        terminal.backend().buffer()[(x, 29)].fg,
        theme.text,
        "the key"
    );
    assert_eq!(
        terminal.backend().buffer()[(x + 2, 29)].fg,
        theme.muted,
        "its label"
    );
}

// ---------------------------------------------------------------- popups

#[test]
fn every_popup_is_centred_with_an_accent_border_over_a_dimmed_screen() {
    let s = done();
    let theme = Theme::default();
    for overlay in [
        Overlay::Commands,
        Overlay::Models,
        Overlay::Themes,
        Overlay::Sessions,
        Overlay::Help,
    ] {
        let ui = UiState {
            overlay: Some(overlay),
            follow_bottom: true,
            ..UiState::default()
        };
        let (w, h) = (120u16, 36u16);
        let t = cells(&s, &ui, (w, h));
        let buffer = t.backend().buffer();
        let top = (0..h)
            .find(|y| row_text(&t, *y, w).contains('┌'))
            .expect("a popup");
        let line = row_text(&t, top, w);
        let left = line[..line.find('┌').unwrap()].chars().count() as u16;
        let right = line[..line.rfind('┐').unwrap()].chars().count() as u16;
        assert!(
            left.abs_diff(w - 1 - right) <= 1,
            "{overlay:?} centred: {left} {right}"
        );
        assert_eq!(buffer[(left, top)].fg, theme.accent, "{overlay:?} border");
        let bottom = (top..h)
            .find(|y| row_text(&t, *y, w).contains('└'))
            .unwrap();
        assert!(
            row_text(&t, bottom - 1, w).contains("esc"),
            "{overlay:?} keys"
        );
        // behind it, the header text is dimmed
        assert_ne!(buffer[(30, 0)].fg, theme.text, "{overlay:?} backdrop");
    }
}

// ---------------------------------------------------------------- full layout

#[test]
fn the_layout_at_100_140_and_200_columns_in_dark_and_light() {
    let s = with_panel_data();
    for (theme, name) in [(ThemeId::AyuDark, "dark"), (ThemeId::AyuLight, "light")] {
        let ui = UiState {
            follow_bottom: true,
            theme,
            ..UiState::default()
        };
        for width in [100u16, 140, 200] {
            // The text is the same in both themes: one snapshot per width, the colours below.
            let shown = if name == "dark" {
                render(
                    &format!("layout-{width}"),
                    (width, 40),
                    &s,
                    &ui,
                    &Editor::new(),
                )
            } else {
                screen_text(&s, &ui, (width, 40))
            };
            assert!(shown.contains("feat/tui-design ●14"), "{width}: {shown}");
            assert_eq!(shown.contains("AGENTS"), width >= 140, "{width}: {shown}");
            // the theme paints the header panel and the transcript background
            let t = cells(&s, &ui, (width, 40));
            let palette = theme.palette();
            assert_eq!(t.backend().buffer()[(width - 1, 0)].bg, palette.panel);
            assert_eq!(t.backend().buffer()[(30, 10)].bg, palette.bg);
            let header = row_text(&t, 0, width);
            let x = header[..header.find('◆').unwrap()].chars().count() as u16;
            assert_eq!(t.backend().buffer()[(x, 0)].fg, palette.nav, "{name}");
            if width >= 140 {
                let row = (0..40)
                    .find(|y| row_text(&t, *y, width).contains("AGENTS"))
                    .unwrap();
                assert_eq!(t.backend().buffer()[(width - 2, row)].bg, palette.panel);
            }
        }
    }
}

// ---------------------------------------------------------------- wide and combining text

#[test]
fn wide_and_combining_names_never_push_the_gauge_off_the_header() {
    let mut s = with_panel_data();
    let workspace = "/home/pixma/開発/プロジェクト/とても長いフォルダの名前/ソース";
    if let Some(m) = s.session.as_mut() {
        m.workspace = workspace.into();
    }
    s.selected_model_name = Some("ベンダー/とても長いモデルの名前-e\u{301}-v2".into());
    s.git = Some(withnav::workspace::git::GitStatus {
        branch: Some("機能/端末の配置-e\u{301}".into()),
        dirty: 2,
        files: vec![
            withnav::workspace::git::Change {
                mark: 'M',
                path: "源/表示/とても長いファイルの名前です.rs".into(),
            },
            withnav::workspace::git::Change {
                mark: 'A',
                path: "cafe\u{301}/re\u{301}sume\u{301}.md".into(),
            },
        ],
    });
    let ui = UiState::default();
    for width in [80u16, 100, 140, 200] {
        let t = cells(&s, &ui, (width, 40));
        let header = row_text(&t, 0, width);
        assert!(
            header.contains("200k"),
            "{width}: the gauge stays: {header}"
        );
        assert!(
            header.contains("▌◎ Nav /build") || header.contains("▌◎ build"),
            "{width}: {header}"
        );
        // the header ends with the gauge and its trailing space: nothing was cut at the edge
        assert!(
            header.trim_end().ends_with("200k · 21%"),
            "{width}: {header}"
        );
    }
    // Wide characters take two cells, the second one blank: read the text without those.
    let t = cells(&s, &ui, (160, 40));
    let buffer = t.backend().buffer();
    let shown: String = (0..40u16)
        .map(|y| {
            let mut row = String::new();
            let mut x = 0u16;
            while x < 160 {
                let symbol = buffer[(x, y)].symbol();
                row.push_str(symbol);
                x += withnav::tui::view::text::columns(symbol).max(1) as u16;
            }
            row
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        shown.contains("A cafe\u{301}/re\u{301}sume\u{301}.md"),
        "{shown}"
    );
    assert!(shown.contains("M …も長いファイルの名前です.rs"), "{shown}");
    assert!(shown.contains("機能/端末の配置-e\u{301} ●2"), "{shown}");
}

#[test]
fn a_deny_message_keeps_both_keys_on_a_narrow_key_bar() {
    for s in [
        asking(prompt("bash", json!({"command": "ls"}), ("folder", "/ws"))),
        plan_exception(),
    ] {
        let ui = UiState {
            deny_message: Some(withnav::tui::state::DenyDraft {
                request_id: "R1".into(),
                text: "keep it".into(),
            }),
            ..UiState::default()
        };
        for width in [24u16, 28, 40, 80] {
            let row = bottom_row(&s, &ui, &Editor::new(), (width, 30));
            let row = row.rsplit('│').next().unwrap_or(&row).to_string();
            assert!(row.contains("enter reject"), "{width}: {row}");
            assert!(row.contains("esc "), "{width}: {row}");
        }
    }
}

#[test]
fn a_plan_exception_never_offers_always_on_the_key_bar() {
    let s = plan_exception();
    let ui = UiState::default();
    for width in [24u16, 30, 40, 60, 100] {
        let row = bottom_row(&s, &ui, &Editor::new(), (width, 30));
        let row = row.rsplit('│').next().unwrap_or(&row).to_string();
        assert!(!row.contains("always"), "{width}: {row}");
        assert!(!row.split(" · ").any(|h| h.trim() == "a"), "{width}: {row}");
        assert!(row.contains('o') && row.contains('r'), "{width}: {row}");
    }
}

#[test]
fn the_three_permission_choices_stay_on_a_narrow_key_bar() {
    let s = asking(prompt("bash", json!({"command": "ls"}), ("folder", "/ws")));
    let ui = UiState::default();
    let bar = |width: u16| bottom_row(&s, &ui, &Editor::new(), (width, 30));
    let wide = bar(100);
    assert!(
        wide.contains("o once · r reject · esc stop · ctrl+p palette"),
        "{wide}"
    );
    // `o once · r reject` needs 34 columns; below that the short labels
    let full = bar(34);
    assert!(full.contains("o once · r reject"), "{full}");
    let mid = bar(30);
    assert!(mid.contains("o once · r reject"), "{mid}");
    let narrow = bar(24);
    assert!(narrow.contains("o once · r reject"), "{narrow}");
    for width in [24u16, 30, 40, 50, 60, 80] {
        let row = bar(width);
        // At 80 columns and more the sidebar takes the left of the row.
        let row = row.rsplit('│').next().unwrap_or(&row).to_string();
        for key in ["o", "r"] {
            assert!(
                row.split(" · ")
                    .any(|h| h.trim() == key || h.trim().starts_with(&format!("{key} "))),
                "{width}: {key} missing: {row}"
            );
        }
    }
}

// ---------------------------------------------------------------- model switch

#[test]
fn the_header_shows_the_model_in_use_and_a_switch_waiting_for_the_turn() {
    let mut s = done();
    s.activity = Activity::Running;
    s.model_switch = Some(withnav::tui::state::ModelSwitch {
        agent: "build".into(),
        model_id: "luna".into(),
        token: 1,
        operation_id: Some("op".into()),
        deferred: true,
    });
    let ui = UiState {
        follow_bottom: true,
        ..UiState::default()
    };
    render("model-pending", (140, 24), &s, &ui, &Editor::new());
    let header = row_text(&cells(&s, &ui, (140, 24)), 0, 140);
    assert!(
        header.contains(
            "▌◎ Nav /build  cognition · assigned · 12.4k / 200k  → luna (after this turn)"
        ),
        "{header}"
    );
    // narrow: the switch stays, the model name gives way first
    let header = row_text(&cells(&s, &ui, (80, 24)), 0, 80);
    assert!(header.contains("→ luna (after this turn)"), "{header}");
    // the picker on a running conversation says when it applies
    let ui = UiState {
        overlay: Some(Overlay::Models),
        selection: std::collections::HashMap::from([("Models".into(), 1)]),
        ..UiState::default()
    };
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(shown.contains("Switch this conversation to…"), "{shown}");
    assert!(
        shown.contains("the model only, after this turn; /variant after the switch"),
        "{shown}"
    );
    // and refuses while a permission request waits
    let s = asking(prompt("bash", json!({"command": "ls"}), ("folder", "/ws")));
    let shown = screen_text(&s, &ui, (100, 30));
    assert!(shown.contains("waiting for approval"), "{shown}");
}

#[test]
fn enter_in_the_model_list_switches_a_live_tab_and_ctrl_n_opens_a_new_one() {
    let s = done();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    route_key(
        key(KeyCode::F(2), KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    // ctrl+n: the variant step for a new tab
    ui.selection.insert("Models".into(), 1);
    route_key(
        key(KeyCode::Char('n'), KeyModifiers::CONTROL),
        &s,
        &mut ui,
        &mut editor,
    );
    assert_eq!(
        ui.variant_pick,
        Some(withnav::tui::state::VariantPick::NewConversation(
            "cognition".into()
        ))
    );
    let enter = key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        route_key(enter, &s, &mut ui, &mut editor),
        Some(Action::PickModel("cognition".into()))
    );
    // enter: the switch, the model only (no variant step)
    route_key(
        key(KeyCode::F(2), KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    ui.selection.insert("Models".into(), 1);
    assert_eq!(
        route_key(enter, &s, &mut ui, &mut editor),
        Some(Action::SwitchModel("cognition".into()))
    );
    assert_eq!(ui.variant_pick, None);
    assert_eq!(ui.overlay, None);
    // while a question waits, enter says why and switches nothing
    let mut waiting = done();
    waiting.activity = Activity::AwaitingUser;
    let mut ui = UiState::default();
    route_key(
        key(KeyCode::F(2), KeyModifiers::NONE),
        &waiting,
        &mut ui,
        &mut editor,
    );
    ui.selection.insert("Models".into(), 1);
    let action = route_key(enter, &waiting, &mut ui, &mut editor);
    assert!(
        matches!(&action, Some(Action::ShowNotice(why)) if why.contains("waiting for your answer")),
        "{action:?}"
    );
    assert_eq!(ui.variant_pick, None);
}

#[test]
fn the_header_keeps_the_committed_model_when_the_catalog_says_otherwise() {
    let mut s = done();
    // committed by model.changed
    s.selected_model_id = Some("luna".into());
    s.selected_model_name = Some("gpt-5.6-luna".into());
    s.context_window = Some(872_000);
    s.model_seq = Some(10);
    s.conversation.context_tokens = Some(1_000);
    let ev = withnav::tui::protocol::ClientEvent::Models {
        workspace: String::new(),
        request: 0,
        generation: s.generation.clone(),
        result: Ok(withnav::tui::protocol::ModelsBody {
            models: vec![ModelEntry {
                api: withnav::tui::protocol::ApiKind::Chat,
                gateway: "gateway".into(),
                provider_group: "gateway".into(),
                name: "luna".into(),
                upstream_id: "elsewhere/other".into(),
                context_window: Some(32_000),
                ..Default::default()
            }],
            default_agent: None,
        }),
    };
    withnav::tui::reducer::update(&mut s, ev).unwrap();
    let header = row_text(&cells(&s, &UiState::default(), (140, 20)), 0, 140);
    assert!(header.contains("luna · assigned · 1k / 872k"), "{header}");
}

#[test]
fn permission_layout_is_fresh_with_one_render_cache() {
    let mut s = asking(prompt("bash", json!({"command":"ls"}), ("folder", "/ws")));
    let mut ui = UiState {
        follow_bottom: true,
        ..Default::default()
    };
    let editor = Editor::new();
    let mut cache = RenderCache::default();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    for stage in 0..4 {
        if stage == 1 {
            ui.deny_message = Some(withnav::tui::state::DenyDraft {
                request_id: "R1".into(),
                text: "first".into(),
            });
        }
        if stage == 2 {
            ui.deny_message.as_mut().unwrap().text = "edited".into();
        }
        if stage == 3 {
            s.answered_permission = Some("R1".into());
        }
        terminal
            .draw(|f| withnav::tui::view::draw(f, &s, &ui, &editor, &mut cache))
            .unwrap();
        let shown = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            shown.contains(match stage {
                0 => "once",
                1 => "first",
                2 => "edited",
                _ => "sent, waiting for the server",
            }),
            "{shown}"
        );
    }
}

#[test]
fn auto_approval_notes_render_narrow_with_optional_evidence() {
    for source in ["instinct", "metacognition", "captain", "driver", "none"] {
        for detailed in [false, true] {
            let mut s = State::default();
            let mut block = tool_block(Some(ToolStatus::Ok));
            block.id = "review".into();
            if let BlockKind::ToolCall { metadata, .. } = &mut block.kind {
                *metadata = if source == "none" {
                    json!({})
                } else {
                    json!({"permission_gate":{"policy_version":1,"source":source,"reason":"Matches the requested change"}})
                };
                if detailed && source != "none" {
                    metadata["permission_gate"]["verdicts"] =
                        json!({"effect":"read_only","scope":"inside","risk":"low"});
                    metadata["permission_gate"]["overrides"] = json!(["scratch root"]);
                    metadata["permission_gate"]["truncated"] = json!(true);
                    metadata["guard"] =
                        json!({"verdict":"conforms","reason":"Within the plan role"});
                }
            }
            s.conversation.blocks.push(block);
            let mut ui = UiState {
                follow_bottom: true,
                ..Default::default()
            };
            let collapsed = screen_text(&s, &ui, (70, 24));
            match source {
                "instinct" => assert!(collapsed.contains("auto · instinct")),
                "metacognition" => assert!(collapsed.contains("auto · metacognition")),
                "captain" | "driver" => assert!(collapsed.contains("Captain approved")),
                _ => assert!(!collapsed.contains("auto ·")),
            }
            ui.expanded.insert("review".into());
            let shown = screen_text(&s, &ui, (70, 24));
            insta::assert_snapshot!(format!("approval-{source}-{detailed}-70"), shown);
        }
    }
}

#[test]
fn unavailable_permission_rules_are_visible_in_the_header_with_the_open_error() {
    let mut s = state();
    s.session.as_mut().unwrap().permissions_unavailable = Some("withnav.db: open failed".into());
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    let mut cache = RenderCache::default();
    terminal
        .draw(|frame| view::draw(frame, &s, &UiState::default(), &Editor::new(), &mut cache))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("Permission rules unavailable"), "{text}");
    assert!(text.contains("withnav.db: open failed"), "{text}");
}
