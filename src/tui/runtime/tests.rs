//! Effect execution and generation tagging, with no terminal and no server:
//! the client points at a closed port, so every request fails fast and the
//! tagged result is what is observed.
use std::time::Duration;

use tokio::sync::mpsc;

use super::*;
use crate::tui::protocol::{CreateSessionRequest, Generation, Overlay, SubmitRequest};

const DEAD_URL: &str = "http://127.0.0.1:1";

#[tokio::test]
async fn deletion_reply_survives_origin_tab_closure_and_missing_is_silent() {
    let mut runtime = Runtime::new(&options(None));
    let generation = runtime.state.generation.clone();
    runtime.dispatch(Action::NewConversation);
    runtime.apply_event(ClientEvent::SessionDeleted {
        generation,
        session_id: "already-gone".into(),
        result: Ok(false),
    });
    assert_eq!(runtime.ui.overlay, Some(Overlay::Sessions));
    assert!(runtime.state.conversation.blocks.is_empty());
    loop {
        if matches!(next(&mut runtime.rx).await, ClientEvent::Sessions { .. }) {
            break;
        }
    }
}

#[tokio::test]
async fn archive_replay_fullscreen_permission_is_passive_and_y_copies() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;
    for (read_only, promptable) in [(true, true), (false, false)] {
        let mut runtime = Runtime::new(&options(None));
        crate::tui::reducer::handle_action(
            &mut runtime.state,
            &mut runtime.ui,
            Action::ResumeSession("archive".into()),
        );
        let generation = runtime.state.generation.clone();
        let metadata = serde_json::from_value(json!({
            "session_id": "archive",
            "actor_state": "stopped",
            "workspace": "/ws",
            "model": "gateway/archive",
            "agent": "build",
            "variant": null,
            "source": "config",
            "variant_source": "model_default",
            "current_seq": 0,
            "read_only": read_only,
            "promptable": promptable,
            "resume_disabled_reason": "unproven identity"
        }))
        .unwrap();
        let effects = crate::tui::reducer::update(
            &mut runtime.state,
            ClientEvent::Metadata {
                generation: generation.clone(),
                result: Ok(metadata),
            },
        )
        .unwrap();
        assert!(!effects.iter().any(|e| matches!(e, Effect::Resume(_))));
        for (seq, kind, payload) in [
            (
                1,
                crate::bus::event::EventKind::AgentYield,
                json!({"kind":"watchdog","question":"Historical pause","details":{}}),
            ),
            (
                2,
                crate::bus::event::EventKind::PermissionRequested,
                json!({"request_id":"R","tool_call_id":"C","tool":"bash","agent":"build","arguments":"{\"command\":\"echo history\"}","scope":{"kind":"folder","value":"/ws"},"reason":"Historical review"}),
            ),
        ] {
            let envelope =
                crate::bus::event::EventEnvelope::new(ulid::Ulid::generate(), seq, kind, payload);
            crate::tui::reducer::update(
                &mut runtime.state,
                ClientEvent::Envelope {
                    generation: generation.clone(),
                    envelope,
                },
            )
            .unwrap();
        }
        assert!(runtime.state.open_permission().is_some());
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| runtime.render(frame)).unwrap();
        let text = (0..40)
            .map(|y| {
                (0..120)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in [
            "alt+c continue",
            "esc stop",
            "allow once",
            "always folder",
            " deny",
            "its parent",
            "waiting for your answer",
        ] {
            assert!(!text.contains(forbidden), "{forbidden}: {text}");
        }
        assert!(text.contains("historical request"), "{text}");
        runtime.ui.selected_block = Some("permission:R".into());
        assert!(runtime.selection_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(
            runtime
                .state
                .notice
                .as_deref()
                .unwrap()
                .starts_with("Copied ")
        );
        for key in [KeyCode::Char('a'), KeyCode::Char('n'), KeyCode::Esc] {
            if let Some(action) = crate::tui::input::route_key(
                KeyEvent::new(key, KeyModifiers::NONE),
                &runtime.state,
                &mut runtime.ui,
                &mut runtime.editor,
            ) {
                assert!(
                    crate::tui::reducer::handle_action(&mut runtime.state, &mut runtime.ui, action)
                        .is_empty()
                );
            }
        }
        assert!(runtime.ui.child_confirm.is_none());
    }
}

#[tokio::test]
async fn initial_model_catalog_preview_uses_default_variant_without_sending_an_override() {
    let mut runtime = Runtime::new(&options(Some("gateway/model")));
    let body=serde_json::from_value(serde_json::json!({"models":[{"name":"gateway/model","gateway":"gateway","upstream_id":"model","provider_group":"gateway","variants":["low","high"],"default_variant":"high","context_window":1000000}],"default_agent":"build"})).unwrap();
    crate::tui::reducer::update(
        &mut runtime.state,
        ClientEvent::Models {
            generation: Generation(0),
            workspace: "/ws".into(),
            request: 1,
            result: Ok(body),
        },
    )
    .unwrap();
    assert_eq!(runtime.state.variant.as_deref(), Some("high"));
    assert_eq!(runtime.state.variant_source, "model_default");
    assert!(runtime.state.start_variant.is_none());
}

#[tokio::test]
async fn ctrl_shift_c_copies_a_block_and_plain_y_is_never_intercepted() {
    use crate::tui::state::{Block, BlockKind, Focus};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut runtime = Runtime::new(&options(None));
    let mut p = crate::tui::view::test_prompt();
    runtime.state.conversation.blocks.push(Block {
        id: "permission".into(),
        kind: BlockKind::Permission(p.clone()),
    });
    runtime.ui.focus = Focus::Transcript;
    runtime.ui.selected_block = Some("permission".into());
    assert!(!runtime.selection_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)));
    assert!(runtime.selection_key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT
    )));
    assert!(
        runtime
            .state
            .notice
            .as_deref()
            .unwrap()
            .starts_with("Copied ")
    );
    p.resolution = None;
    runtime.state.conversation.blocks[0].kind = BlockKind::Permission(p);
    runtime
        .state
        .conversation
        .open_permissions
        .push("permission".into());
    assert!(!runtime.selection_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)));
}

#[tokio::test]
async fn transcript_mouse_ignores_sidebar_and_scrolls_after_capture() {
    use crate::tui::state::{Block, BlockKind};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};
    let mut runtime = Runtime::new(&options(None));
    runtime.state.conversation.blocks.push(Block {
        id: "reply".into(),
        kind: BlockKind::Message {
            source: "assistant".into(),
            text: "hello world".into(),
        },
    });
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| runtime.render(frame)).unwrap();
    let area = runtime.cache.transcript.area;
    runtime.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 1,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(runtime.selection.range.is_none());
    runtime.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: area.x,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(runtime.ui.scroll, 3);
    assert!(!runtime.ui.follow_bottom);
    let &(x, y, _, _) = runtime.cache.transcript.cells.first().unwrap();
    runtime.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(runtime.selection.range.is_some());
    runtime.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: x,
        row: area.y - 1,
        modifiers: KeyModifiers::NONE,
    }));
    runtime.on_tick();
    assert_eq!(runtime.ui.scroll, 1);
}

#[tokio::test]
async fn keyboard_viewport_survives_mouse_and_wheel() {
    use crate::tui::state::{Block, BlockKind};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};
    let mut runtime = Runtime::new(&options(None));
    for i in 0..100 {
        runtime.state.conversation.blocks.push(Block {
            id: i.to_string(),
            kind: BlockKind::Message {
                source: "user".into(),
                text: format!("message {i} text"),
            },
        });
    }
    runtime.ui.selected_block = Some("10".into());
    runtime.ui.follow_bottom = false;
    let mut terminal = Terminal::new(TestBackend::new(70, 20)).unwrap();
    terminal.draw(|f| runtime.render(f)).unwrap();
    let scroll = runtime.cache.transcript.scroll;
    let &(x, y, a, b) = runtime.cache.transcript.cells.first().unwrap();
    let clicked = runtime.cache.transcript.text[a..b].to_owned();
    runtime.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }));
    terminal.draw(|f| runtime.render(f)).unwrap();
    assert_eq!(runtime.cache.transcript.scroll, scroll);
    runtime.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: x + 2,
        row: y,
        modifiers: KeyModifiers::NONE,
    }));
    let (a, b) = runtime.selection.range.unwrap();
    assert!(runtime.cache.transcript.text[a..b].starts_with(&clicked));
    runtime.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(runtime.ui.scroll, scroll + 3);
    assert!(runtime.cache.transcript.cells.len() <= 70 * 20);
}

#[tokio::test]
async fn picker_wheel_navigates_without_transcript_scroll_and_motion_is_noop() {
    use crate::tui::protocol::Overlay;
    use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
    let mut runtime = Runtime::new(&options(None));
    runtime.ui.overlay = Some(Overlay::Themes);
    let theme = runtime.ui.theme;
    runtime.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    }));
    assert_ne!(runtime.ui.theme, theme);
    assert_eq!(runtime.ui.scroll, 0);
    runtime.ui.overlay = None;
    runtime.dirty = false;
    runtime.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(!runtime.dirty);
    assert!(!runtime.selection_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('c'),
        KeyModifiers::CONTROL
    )));
}

fn executor() -> (Executor, mpsc::Receiver<ClientEvent>) {
    let (tx, rx) = mpsc::channel(16);
    (Executor::new(Client::new(DEAD_URL, None), tx), rx)
}

fn ctx(generation: u64, session: Option<&str>) -> Context {
    Context {
        generation: Generation(generation),
        session_id: session.map(str::to_string),
        workspace: "/ws".into(),
        config_dir: None,
    }
}

async fn next(rx: &mut mpsc::Receiver<ClientEvent>) -> ClientEvent {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("event in time")
        .expect("channel open")
}

fn submit() -> SubmitRequest {
    SubmitRequest {
        answer: None,
        target_actor_id: "agent:main".into(),
        mode: "next-turn".into(),
        content: vec![],
    }
}

#[tokio::test]
async fn delete_effect_reports_errors_with_session_and_generation() {
    let (mut exec, mut rx) = executor();
    exec.execute(
        Effect::DeleteSession("session /?".into()),
        &ctx(7, None),
        &mut Editor::new(),
    );
    match next(&mut rx).await {
        ClientEvent::SessionDeleted {
            generation,
            session_id,
            result,
        } => {
            assert_eq!(generation, Generation(7));
            assert_eq!(session_id, "session /?");
            assert!(result.is_err());
        }
        other => panic!("unexpected event {other:?}"),
    }
}

#[tokio::test]
async fn delete_effect_encodes_ids_and_treats_missing_as_already_deleted() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };
    for (status, deleted) in [(204, true), (404, false)] {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/sessions/session%20%2F%3F"))
            .and(header("authorization", "Bearer secret"))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&server)
            .await;
        let (tx, mut rx) = mpsc::channel(16);
        let mut exec = Executor::new(Client::new(server.uri(), Some("secret".into())), tx);
        exec.execute(
            Effect::DeleteSession("session /?".into()),
            &ctx(8, None),
            &mut Editor::new(),
        );
        match next(&mut rx).await {
            ClientEvent::SessionDeleted { result, .. } => assert_eq!(result, Ok(deleted)),
            other => panic!("unexpected event {other:?}"),
        }
    }
}

#[tokio::test]
async fn http_effects_report_back_tagged_with_the_context_generation() {
    let (mut exec, mut rx) = executor();
    let mut editor = Editor::new();
    let ctx = ctx(7, Some("s1"));
    for effect in [
        Effect::FetchModels,
        Effect::ListSessions,
        Effect::Create(CreateSessionRequest {
            workspace: String::new(),
            model: None,
            resume: None,
            agent: None,
            variant: None,
        }),
        Effect::Resume("s1".into()),
        Effect::FetchMetadata("s1".into()),
        Effect::Stop("s1".into()),
    ] {
        assert_eq!(exec.execute(effect, &ctx, &mut editor), Flow::Continue);
    }
    let mut seen = Vec::new();
    for _ in 0..6 {
        let (generation, name) = match next(&mut rx).await {
            ClientEvent::Models {
                generation, result, ..
            } => {
                assert!(result.is_err());
                (generation, "models")
            }
            ClientEvent::Sessions { generation, result } => {
                assert!(result.is_err());
                (generation, "sessions")
            }
            ClientEvent::Created { generation, result } => {
                assert!(result.is_err());
                (generation, "created")
            }
            ClientEvent::Resumed { generation, result } => {
                assert!(result.is_err());
                (generation, "resumed")
            }
            ClientEvent::Metadata { generation, result } => {
                assert!(result.is_err());
                (generation, "metadata")
            }
            ClientEvent::Stopped { generation, result } => {
                assert!(result.is_err());
                (generation, "stopped")
            }
            other => panic!("unexpected event {other:?}"),
        };
        assert_eq!(generation, Generation(7), "{name}");
        seen.push(name);
    }
    seen.sort_unstable();
    assert_eq!(
        seen,
        [
            "created", "metadata", "models", "resumed", "sessions", "stopped"
        ]
    );
}

#[tokio::test]
async fn effects_use_the_generation_of_the_moment_they_run() {
    let (mut exec, mut rx) = executor();
    let mut editor = Editor::new();
    exec.execute(Effect::FetchModels, &ctx(1, None), &mut editor);
    exec.execute(Effect::FetchModels, &ctx(2, None), &mut editor);
    let mut generations = Vec::new();
    for _ in 0..2 {
        match next(&mut rx).await {
            ClientEvent::Models { generation, .. } => generations.push(generation.0),
            other => panic!("unexpected event {other:?}"),
        }
    }
    generations.sort_unstable();
    assert_eq!(generations, [1, 2]);
}

#[tokio::test]
async fn a_failed_send_keeps_the_action_revision_and_the_generation() {
    let (mut exec, mut rx) = executor();
    let mut editor = Editor::new();
    exec.execute(
        Effect::SendAt {
            request: submit(),
            revision: 42,
        },
        &ctx(3, Some("s1")),
        &mut editor,
    );
    match next(&mut rx).await {
        ClientEvent::Submitted {
            generation,
            revision,
            result,
        } => {
            assert_eq!((generation, revision), (Generation(3), 42));
            assert!(result.is_err());
        }
        other => panic!("unexpected event {other:?}"),
    }
}

#[tokio::test]
async fn send_without_a_conversation_fails_instead_of_vanishing() {
    let (mut exec, mut rx) = executor();
    let mut editor = Editor::new();
    exec.execute(
        Effect::SendAt {
            request: submit(),
            revision: 0,
        },
        &ctx(1, None),
        &mut editor,
    );
    assert!(matches!(
        next(&mut rx).await,
        ClientEvent::Submitted { result: Err(_), .. }
    ));
}

#[tokio::test]
async fn clear_editor_only_clears_an_unedited_draft() {
    let (mut exec, _rx) = executor();
    let mut editor = Editor::new();
    editor.set_text("hello");
    let sent = editor.revision();
    editor.set_text("hello world");
    exec.execute(
        Effect::ClearEditor { revision: sent },
        &ctx(1, None),
        &mut editor,
    );
    assert_eq!(editor.text(), "hello world");
    let current = editor.revision();
    exec.execute(
        Effect::ClearEditor { revision: current },
        &ctx(1, None),
        &mut editor,
    );
    assert_eq!(editor.text(), "");
}

#[tokio::test]
async fn quit_ends_the_loop() {
    let (mut exec, _rx) = executor();
    let mut editor = Editor::new();
    assert_eq!(
        exec.execute(Effect::Quit, &ctx(1, None), &mut editor),
        Flow::Quit
    );
}

#[tokio::test]
async fn each_generation_has_its_own_stream_and_unsubscribe_cancels_only_its_own() {
    let (mut exec, _rx) = executor();
    let mut editor = Editor::new();
    let subscribe = |id: &str| Effect::Subscribe {
        session_id: id.into(),
        cursor: None,
    };
    assert!(!exec.has_stream());
    exec.execute(subscribe("a"), &ctx(1, Some("a")), &mut editor);
    exec.execute(subscribe("b"), &ctx(2, Some("b")), &mut editor);
    assert_eq!(exec.stream_count(), 2, "two tabs stream at once");
    exec.execute(subscribe("b"), &ctx(2, Some("b")), &mut editor);
    assert_eq!(
        exec.stream_count(),
        2,
        "a tab's new stream replaces its old one"
    );
    exec.retire_streams(&[Generation(2), Generation(3)]);
    assert_eq!(
        exec.stream_count(),
        1,
        "a generation no tab holds is retired"
    );
    exec.execute(subscribe("c"), &ctx(3, Some("c")), &mut editor);
    exec.execute(Effect::Unsubscribe, &ctx(2, Some("b")), &mut editor);
    assert_eq!(exec.stream_count(), 1);
    exec.cancel_streams();
    assert!(!exec.has_stream());
}

fn options(model: Option<&str>) -> TuiOptions {
    TuiOptions {
        url: DEAD_URL.into(),
        token: None,
        workspace: "/ws".into(),
        initial_model: model.map(str::to_string),
        initial_session: None,
        embedded: false,
        config_dir: None,
    }
}

#[tokio::test]
async fn startup_presets_the_model_and_asks_for_catalog_and_sessions() {
    let mut runtime = Runtime::new(&options(Some("fast")));
    assert_eq!(runtime.state.selected_model_id.as_deref(), Some("fast"));
    assert_eq!(runtime.state.model_source, "override");
    assert_eq!(runtime.state.variant_source, "model_default");
    runtime.start(None);
    let required = std::collections::HashSet::from(["agents", "layers", "models", "sessions"]);
    let mut kinds = std::collections::HashSet::new();
    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while kinds != required {
            let event = runtime.recv().await.expect("startup event channel open");
            observed.push(format!("{event:?}"));
            let kind = match event {
                ClientEvent::Models { .. } => "models",
                ClientEvent::Sessions { .. } => "sessions",
                ClientEvent::Agents { .. } => "agents",
                ClientEvent::Layers { .. } => "layers",
                _ => continue,
            };
            assert!(kinds.insert(kind), "duplicate {kind}: {observed:?}");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing startup events; got {kinds:?}; observed: {observed:?}"));
    assert_eq!(kinds, required);
}

#[tokio::test]
async fn startup_resume_fetches_workspace_layers_under_the_resumed_generation() {
    let mut runtime = Runtime::new(&options(None));
    runtime.start(Some("saved".into()));
    let generation = runtime.state.generation.clone();
    let mut got_layers = false;
    let mut got_models = false;
    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !(got_layers && got_models) {
            let event = runtime.recv().await.expect("resume event channel open");
            observed.push(format!("{event:?}"));
            match event {
                ClientEvent::Layers {
                    generation: owner,
                    workspace,
                    ..
                } => {
                    assert_eq!(owner, generation);
                    assert_eq!(workspace, "/ws");
                    got_layers = true;
                }
                ClientEvent::Models {
                    generation: owner,
                    workspace,
                    ..
                } => {
                    assert_eq!(owner, generation);
                    assert_eq!(workspace, "/ws");
                    got_models = true;
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing resumed catalog or layers; observed: {observed:?}"));
    assert!(got_layers && got_models, "observed: {observed:?}");
}

#[tokio::test]
async fn a_protocol_error_event_is_kept_and_the_loop_goes_on() {
    let mut runtime = Runtime::new(&options(None));
    runtime.apply_batch(ClientEvent::ProtocolError {
        generation: runtime.state.generation.clone(),
        message: "bad frame".into(),
    });
    assert_eq!(
        runtime.state.last_protocol_error.as_deref(),
        Some("bad frame")
    );
    assert!(!runtime.should_quit());
}

#[tokio::test]
async fn a_batch_applies_at_most_the_cap_of_queued_events() {
    let mut runtime = Runtime::new(&options(None));
    let tx = runtime.exec_sender();
    for i in 0..(MAX_EVENTS_PER_ITERATION + 5) {
        tx.send(ClientEvent::ProtocolError {
            generation: runtime.state.generation.clone(),
            message: format!("e{i}"),
        })
        .await
        .unwrap();
    }
    let first = runtime.recv().await.unwrap();
    runtime.apply_batch(first);
    assert_eq!(
        runtime.state.last_protocol_error.as_deref(),
        Some(format!("e{}", MAX_EVENTS_PER_ITERATION - 1).as_str())
    );
    assert!(runtime.recv().await.is_some(), "the rest stays queued");
}

#[test]
fn a_rejected_send_is_a_failed_submission() {
    let event = effects::submit_event(
        Generation(5),
        9,
        Err(effects::SubmitFailure::Rejected("HTTP 400".into())),
    );
    assert_eq!(
        event,
        ClientEvent::Submitted {
            generation: Generation(5),
            revision: 9,
            result: Err("HTTP 400".into()),
        }
    );
}

#[test]
fn an_uncertain_send_is_reported_as_delivery_uncertain() {
    let event = effects::submit_event(
        Generation(5),
        9,
        Err(effects::SubmitFailure::Uncertain("timeout".into())),
    );
    assert_eq!(
        event,
        ClientEvent::DeliveryUncertain {
            generation: Generation(5)
        }
    );
}

fn options_in(config_dir: &std::path::Path) -> TuiOptions {
    TuiOptions {
        config_dir: Some(config_dir.to_path_buf()),
        ..options(None)
    }
}

/// Opens the theme picker, moves down `steps` rows and confirms, through the real key routing.
fn pick_theme(runtime: &mut Runtime, steps: usize) {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let press = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
    runtime.dispatch(Action::OpenOverlay(crate::tui::protocol::Overlay::Themes));
    runtime.ui.selection.insert("Themes".into(), 0);
    for _ in 0..steps {
        runtime.handle_terminal_event(press(KeyCode::Down));
    }
    runtime.handle_terminal_event(press(KeyCode::Enter));
}

#[tokio::test]
async fn a_picked_theme_is_saved_and_restored_at_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = Runtime::new(&options_in(dir.path()));
    assert_eq!(runtime.ui.theme, ThemeId::TideDark);
    pick_theme(&mut runtime, 3);
    assert_eq!(runtime.ui.theme, ThemeId::NordLight);
    runtime.executor().flush_saves().await;
    drop(runtime);
    let restarted = Runtime::new(&options_in(dir.path()));
    assert_eq!(restarted.ui.theme, ThemeId::NordLight);
}

#[tokio::test]
async fn a_failed_save_is_shown_and_picking_again_retries() {
    let dir = tempfile::tempdir().unwrap();
    // A directory where the settings file should be: the rename over it fails.
    std::fs::create_dir(crate::settings::path(dir.path())).unwrap();
    std::fs::write(crate::settings::path(dir.path()).join("keep"), "").unwrap();
    let mut runtime = Runtime::new(&options_in(dir.path()));
    pick_theme(&mut runtime, 1);
    assert_eq!(
        runtime.ui.theme,
        ThemeId::AyuLight,
        "the theme applies anyway"
    );
    let event = tokio::time::timeout(Duration::from_secs(10), runtime.recv())
        .await
        .expect("in time")
        .expect("open");
    assert!(
        matches!(event, ClientEvent::SettingsNotSaved { .. }),
        "{event:?}"
    );
    runtime.apply_event(event);
    let notice = runtime.state.conversation.blocks.last().expect("a notice");
    assert!(
        matches!(&notice.kind, crate::tui::state::BlockKind::Notice(text) if text.contains("Ayu Light")),
        "{notice:?}"
    );

    // Once the cause is gone, confirming the same theme again saves it.
    std::fs::remove_dir_all(crate::settings::path(dir.path())).unwrap();
    pick_theme(&mut runtime, 1);
    assert_eq!(runtime.ui.theme, ThemeId::AyuLight);
    runtime.executor().flush_saves().await;
    assert_eq!(
        crate::settings::load(dir.path()).unwrap().tui_theme(),
        Some("ayu-light")
    );
}

#[tokio::test]
async fn rapid_theme_picks_save_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = Runtime::new(&options_in(dir.path()));
    for _ in 0..20 {
        pick_theme(&mut runtime, 1);
    }
    let expected = runtime.ui.theme.id();
    runtime.executor().flush_saves().await;
    assert_eq!(
        crate::settings::load(dir.path()).unwrap().tui_theme(),
        Some(expected)
    );
}

#[tokio::test]
async fn a_tabs_editor_keeps_its_cursor_and_history_across_a_switch() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let key = |code, modifiers| Event::Key(KeyEvent::new(code, modifiers));
    let mut runtime = Runtime::new(&options(None));
    // the first tab holds a conversation, so ctrl+n opens a second one
    runtime.state.tabs[0].resuming = Some("S1".into());
    for c in "hello world".chars() {
        runtime.handle_terminal_event(key(KeyCode::Char(c), KeyModifiers::NONE));
    }
    runtime
        .editor
        .area_mut()
        .move_cursor(ratatui_textarea::CursorMove::Jump(0, 5));
    let cursor = runtime.editor.area().cursor();
    runtime.handle_terminal_event(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert_eq!(runtime.state.active, 1);
    assert_eq!(runtime.editor.text(), "");
    runtime.handle_terminal_event(key(KeyCode::Esc, KeyModifiers::NONE));
    runtime.handle_terminal_event(key(KeyCode::Char('x'), KeyModifiers::NONE));
    runtime.handle_terminal_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
    assert_eq!(runtime.state.active, 0);
    assert_eq!(runtime.editor.text(), "hello world");
    assert_eq!(
        runtime.editor.area().cursor(),
        cursor,
        "typing resumes where it stopped"
    );
    runtime.handle_terminal_event(key(KeyCode::Char(','), KeyModifiers::NONE));
    assert_eq!(runtime.editor.text(), "hello, world");
    // the undo history came back with it
    assert!(runtime.editor.area_mut().undo());
    assert_eq!(runtime.editor.text(), "hello world");
    // and the other tab's editor was kept as well
    runtime.handle_terminal_event(key(KeyCode::Char('2'), KeyModifiers::ALT));
    assert_eq!(runtime.editor.text(), "x");
    assert_eq!(runtime.editor.area().cursor().1, 1);
}

#[tokio::test]
async fn wheel_bursts_accumulate_before_render() {
    use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
    let mut runtime = Runtime::new(&options(None));
    runtime.cache.transcript.area = ratatui::layout::Rect::new(0, 0, 40, 10);
    runtime.ui.follow_bottom = false;
    for kind in [
        MouseEventKind::ScrollUp,
        MouseEventKind::ScrollUp,
        MouseEventKind::ScrollUp,
    ] {
        runtime.handle_terminal_event(Event::Mouse(MouseEvent {
            kind,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }));
    }
    assert_eq!(runtime.ui.scroll, 9);
    runtime.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(runtime.ui.scroll, 6);
}
