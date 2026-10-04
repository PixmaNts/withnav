//! Effect execution and generation tagging, with no terminal and no server:
//! the client points at a closed port, so every request fails fast and the
//! tagged result is what is observed.
use std::time::Duration;

use tokio::sync::mpsc;

use super::*;
use crate::tui::protocol::{CreateSessionRequest, Generation, SubmitRequest};

const DEAD_URL: &str = "http://127.0.0.1:1";

#[tokio::test]
async fn transcript_mouse_ignores_sidebar_and_scrolls_after_capture() {
    use crate::tui::state::{Block, BlockKind};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{backend::TestBackend, Terminal};
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
    use ratatui::{backend::TestBackend, Terminal};
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
        target_actor_id: "agent:main".into(),
        mode: "next-turn".into(),
        content: vec![],
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
            ClientEvent::Models { generation, result } => {
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
        ["created", "metadata", "models", "resumed", "sessions", "stopped"]
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
    assert_eq!(runtime.state.selected_model_alias.as_deref(), Some("fast"));
    runtime.start(None);
    let mut kinds = Vec::new();
    for _ in 0..2 {
        let event = tokio::time::timeout(Duration::from_secs(10), runtime.recv())
            .await
            .expect("in time")
            .expect("open");
        kinds.push(match event {
            ClientEvent::Models { .. } => "models",
            ClientEvent::Sessions { .. } => "sessions",
            other => panic!("unexpected event {other:?}"),
        });
    }
    kinds.sort_unstable();
    assert_eq!(kinds, ["models", "sessions"]);
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
    for _ in 0..steps {
        runtime.handle_terminal_event(press(KeyCode::Down));
    }
    runtime.handle_terminal_event(press(KeyCode::Enter));
}

#[tokio::test]
async fn a_picked_theme_is_saved_and_restored_at_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = Runtime::new(&options_in(dir.path()));
    assert_eq!(runtime.ui.theme, ThemeId::AyuDark);
    pick_theme(&mut runtime, 3);
    assert_eq!(runtime.ui.theme, ThemeId::NordLight);
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
    pick_theme(&mut runtime, 0);
    assert_eq!(runtime.ui.theme, ThemeId::AyuLight);
    assert_eq!(
        crate::settings::load(dir.path()).unwrap().tui_theme(),
        Some("ayu-light")
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
