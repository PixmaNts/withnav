//! The terminal secret prompt: a `request_secret` opens it, the value is typed masked, moves
//! into the one effect that sends it, and never reaches the conversation state or the screen.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;
use withnav_tui::{
    input::{Editor, route_key, route_paste},
    protocol::*,
    reducer,
    state::{BlockKind, Focus, SecretState, State, UiState},
};

const VALUE: &str = "sk-live-7f3a9c2e1d0b4a68";

fn summary() -> SessionMetadata {
    serde_json::from_value(json!({"session_id":"S","actor_state":"idle","workspace":"/ws","agent":"build","model":"m","variant":null,"source":"config","variant_source":"model_default","current_seq":1})).unwrap()
}

fn event(s: &mut State, seq: u64, kind: &str, payload: serde_json::Value) {
    let mut envelope = withnav_client_api::event::EventEnvelope::new(
        ulid::Ulid::generate(),
        seq,
        withnav_client_api::event::EventKind::ModelChanged,
        payload,
    );
    envelope.kind = kind.into();
    reducer::update(
        s,
        ClientEvent::Envelope {
            generation: s.generation.clone(),
            envelope,
        },
    )
    .unwrap();
}

fn asked() -> State {
    let mut s = State::default();
    s.session.replace(summary());
    event(
        &mut s,
        1,
        "turn.start",
        json!({"turn_id":"01JTURN0000000000000000000","actor_id":"agent:main","task_id":"01JTASK0000000000000000000"}),
    );
    event(
        &mut s,
        2,
        "agent.yield",
        json!({"actor_id":"agent:main","turn_id":"01JTURN0000000000000000000","question":"Nav asks for a secret","kind":"secret","question_call_id":"sec_1",
        "details":{"name":"API_KEY","label":"Deploy key","reason":"the staging deploy","default_ttl":"24h","default_scope":"session"}}),
    );
    s
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn press(
    s: &mut State,
    ui: &mut UiState,
    editor: &mut Editor,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Vec<Effect> {
    match route_key(key(code, modifiers), s, ui, editor) {
        Some(action) => reducer::handle_action(s, ui, action),
        None => vec![],
    }
}

#[test]
fn the_prompt_masks_the_value_and_hands_it_to_one_effect_only() {
    let mut s = asked();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    let prompt = withnav_tui::secret::waiting(&s).expect("a waiting request");
    assert_eq!(prompt.request.name, "API_KEY");
    withnav_tui::secret::sync(&s, &mut ui);
    assert!(
        ui.secret_entry.is_some(),
        "a waiting request opens the prompt"
    );

    for c in VALUE[..4].chars() {
        press(
            &mut s,
            &mut ui,
            &mut editor,
            KeyCode::Char(c),
            KeyModifiers::NONE,
        );
    }
    assert!(route_paste(VALUE[4..].to_owned() + "\n", &s, &mut ui, &mut editor).is_none());
    assert_eq!(
        editor.text(),
        "",
        "nothing typed in the prompt reaches the editor"
    );
    let entry = ui.secret_entry.as_ref().unwrap();
    assert_eq!(entry.display(), "•".repeat(VALUE.len()));
    assert!(!format!("{ui:?}").contains(VALUE), "Debug never prints it");
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    );
    assert_eq!(ui.secret_entry.as_ref().unwrap().display(), VALUE);
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Tab,
        KeyModifiers::NONE,
    ); // 24h -> session
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Tab,
        KeyModifiers::NONE,
    ); // session -> 1h
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::BackTab,
        KeyModifiers::SHIFT,
    ); // this workspace

    let effects = press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Enter,
        KeyModifiers::NONE,
    );
    let [
        Effect::SubmitSecret {
            session_id,
            call_id,
            submission,
            ..
        },
    ] = &effects[..]
    else {
        panic!("one effect: {effects:?}");
    };
    assert_eq!((session_id.as_str(), call_id.as_str()), ("S", "sec_1"));
    assert_eq!(submission.value.as_deref(), Some(VALUE));
    assert_eq!(submission.ttl, withnav_client_api::secrets::SecretTtl::Hour);
    assert_eq!(
        submission.scope,
        withnav_client_api::secrets::SecretScope::Workspace
    );
    assert_eq!(submission.name, None, "a waiting request names its secret");
    assert!(
        !format!("{effects:?}").contains(VALUE),
        "an effect's Debug never prints it"
    );
    assert_eq!(
        ui.secret_entry.as_ref().unwrap().value,
        "",
        "the prompt is emptied once sent"
    );
    assert!(!format!("{:?}", s.conversation).contains(VALUE));

    // The server's note settles the request: no Captain message is shown.
    event(
        &mut s,
        3,
        "message.received",
        json!({"command_id":"01JCMD00000000000000000000","source":"user:http","target":"agent:main","mode":"next-turn",
        "content":"stored as $API_KEY","secret":{"request_id":"sec_1","name":"API_KEY","outcome":"stored","scope":"workspace","ttl":"1h","expires_at":"2099-01-01T00:00:00Z"}}),
    );
    withnav_tui::secret::sync(&s, &mut ui);
    assert!(
        ui.secret_entry.is_none(),
        "a settled request closes the prompt"
    );
    assert!(
        !s.conversation
            .blocks
            .iter()
            .any(|b| matches!(&b.kind, BlockKind::Message { .. }))
    );
    let settled = s
        .conversation
        .blocks
        .iter()
        .find_map(|b| match &b.kind {
            BlockKind::Secret(p) => Some(p),
            _ => None,
        })
        .unwrap();
    assert!(matches!(settled.state, SecretState::Stored { .. }));
}

#[test]
fn esc_declines_ctrl_e_puts_it_aside_and_secrets_lists_and_revokes() {
    let mut s = asked();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    withnav_tui::secret::sync(&s, &mut ui);
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Char('e'),
        KeyModifiers::CONTROL,
    );
    withnav_tui::secret::sync(&s, &mut ui);
    assert!(
        ui.secret_entry.is_none(),
        "put aside to write instead, it stays closed"
    );
    let effects = press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Char('k'),
        KeyModifiers::ALT,
    );
    assert!(effects.is_empty());
    assert!(ui.secret_entry.is_some(), "alt+k opens it again");
    let effects = press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Esc,
        KeyModifiers::NONE,
    );
    let [Effect::SubmitSecret { submission, .. }] = &effects[..] else {
        panic!("{effects:?}")
    };
    assert!(submission.decline);
    assert_eq!(submission.value, None);

    assert_eq!(
        withnav_tui::commands::route("/secrets", Mode::NextTurn),
        withnav_tui::commands::Routed::Local(Action::ListSecrets)
    );
    assert_eq!(
        withnav_tui::commands::route("/secrets revoke API_KEY", Mode::NextTurn),
        withnav_tui::commands::Routed::Local(Action::RevokeSecret("API_KEY".into()))
    );
    assert_eq!(
        reducer::handle_action(&mut s, &mut ui, Action::ListSecrets),
        vec![Effect::ListSecrets("S".into())]
    );
    let generation = s.generation.clone();
    reducer::update(&mut s, ClientEvent::SecretsListed { generation, result: Ok(serde_json::from_value(json!({"secrets":[
        {"name":"API_KEY","scope":"session","ttl":"24h","expires_at":"2099-01-01T00:00:00Z","created_at":"2026-01-01T00:00:00Z","use_count":2,"last4":"4a68"}]})).unwrap()) }).unwrap();
    let listed = s
        .conversation
        .blocks
        .iter()
        .rev()
        .find_map(|b| match &b.kind {
            BlockKind::Notice(t) => Some(t.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        listed.contains("$API_KEY ••••4a68 · this session · expires in"),
        "{listed}"
    );
    assert!(listed.contains("used 2×"));
}

#[test]
fn ctrl_c_quits_while_the_prompt_is_open() {
    let s = asked();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    withnav_tui::secret::sync(&s, &mut ui);
    assert!(ui.secret_entry.is_some());
    let ctrl_c = || key(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(route_key(ctrl_c(), &s, &mut ui, &mut editor).is_none());
    assert!(
        ui.quit_confirm.is_some(),
        "the first ctrl+c asks to confirm"
    );
    assert!(matches!(
        route_key(ctrl_c(), &s, &mut ui, &mut editor),
        Some(withnav_tui::protocol::Action::Quit)
    ));
}

/// The prompt keeps a value at the server's byte limit and refuses, whole, anything past it.
#[test]
fn a_paste_is_kept_at_the_server_byte_limit_and_refused_past_it() {
    use withnav_tui::secret::MAX_VALUE_BYTES;

    let mut s = asked();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    withnav_tui::secret::sync(&s, &mut ui);

    // A multibyte paste landing exactly on the limit is accepted.
    let every_other = "é".repeat(MAX_VALUE_BYTES / 2);
    assert!(route_paste(every_other.clone(), &s, &mut ui, &mut editor).is_none());
    let entry = ui.secret_entry.as_ref().unwrap();
    assert_eq!(entry.value.len(), MAX_VALUE_BYTES);
    assert!(entry.error.is_none());
    assert_eq!(editor.text(), "", "a paste never reaches the editor");

    // Past the limit: refused whole, the value is untouched, the refusal names the limit.
    assert!(route_paste("a".into(), &s, &mut ui, &mut editor).is_none());
    let entry = ui.secret_entry.as_ref().unwrap();
    assert_eq!(entry.value, every_other, "the value is left untouched");
    let error = entry.error.as_deref().expect("the refusal is shown");
    assert!(error.contains(&MAX_VALUE_BYTES.to_string()), "{error}");
    assert!(!error.contains("é"), "the refusal never quotes the value");

    // Two bytes of room left: the multibyte character fits exactly, the next is refused.
    let mut ui = UiState::default();
    withnav_tui::secret::sync(&s, &mut ui);
    assert!(route_paste("a".repeat(MAX_VALUE_BYTES - 2), &s, &mut ui, &mut editor).is_none());
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Char('é'),
        KeyModifiers::NONE,
    );
    assert!(ui.secret_entry.as_ref().unwrap().error.is_none());
    assert_eq!(
        ui.secret_entry.as_ref().unwrap().value.len(),
        MAX_VALUE_BYTES
    );
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Char('é'),
        KeyModifiers::NONE,
    );
    assert_eq!(
        ui.secret_entry.as_ref().unwrap().value.len(),
        MAX_VALUE_BYTES
    );
    assert!(ui.secret_entry.as_ref().unwrap().error.is_some());

    // One byte of room left, a two-byte character offered: refused whole, nothing added.
    let mut ui = UiState::default();
    withnav_tui::secret::sync(&s, &mut ui);
    let held = "a".repeat(MAX_VALUE_BYTES - 1);
    assert!(route_paste(held.clone(), &s, &mut ui, &mut editor).is_none());
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Char('é'),
        KeyModifiers::NONE,
    );
    let entry = ui.secret_entry.as_ref().unwrap();
    assert_eq!(entry.value, held, "the held bytes are untouched");
    assert!(entry.error.is_some());

    // An accepted edit forgets the refusal; the held bytes are what is submitted.
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Backspace,
        KeyModifiers::NONE,
    );
    assert!(ui.secret_entry.as_ref().unwrap().error.is_none());
    let effects = press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Enter,
        KeyModifiers::NONE,
    );
    let [Effect::SubmitSecret { submission, .. }] = &effects[..] else {
        panic!("{effects:?}");
    };
    assert_eq!(
        submission.value.as_deref().map(str::len),
        Some(MAX_VALUE_BYTES - 2)
    );
}

/// A refusal does not survive the submission that clears the value, nor ctrl+u.
#[test]
fn a_refusal_does_not_survive_submission_or_ctrl_u() {
    use withnav_tui::secret::MAX_VALUE_BYTES;

    let mut s = asked();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    withnav_tui::secret::sync(&s, &mut ui);
    assert!(route_paste("sk-abc".into(), &s, &mut ui, &mut editor).is_none());
    assert!(route_paste("a".repeat(MAX_VALUE_BYTES), &s, &mut ui, &mut editor).is_none());
    assert!(ui.secret_entry.as_ref().unwrap().error.is_some());
    let effects = press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Enter,
        KeyModifiers::NONE,
    );
    let [Effect::SubmitSecret { submission, .. }] = &effects[..] else {
        panic!("{effects:?}");
    };
    assert_eq!(
        submission.value.as_deref(),
        Some("sk-abc"),
        "the value held before the refusal is the one sent"
    );
    assert!(
        ui.secret_entry.as_ref().unwrap().error.is_none(),
        "submission clears the refusal"
    );

    let mut s = asked();
    let mut ui = UiState::default();
    withnav_tui::secret::sync(&s, &mut ui);
    assert!(route_paste("x".repeat(MAX_VALUE_BYTES + 1), &s, &mut ui, &mut editor).is_none());
    assert!(ui.secret_entry.as_ref().unwrap().error.is_some());
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    );
    assert!(ui.secret_entry.as_ref().unwrap().value.is_empty());
    assert!(ui.secret_entry.as_ref().unwrap().error.is_none());
}

/// A paste loses at most the one line ending a terminal appends; a newline a secret means to
/// keep, at either end, survives.
#[test]
fn a_paste_keeps_intentional_newlines_and_drops_one_line_ending() {
    let s = asked();
    let mut editor = Editor::new();
    let mut value_after = |text: &str| {
        let mut ui = UiState::default();
        withnav_tui::secret::sync(&s, &mut ui);
        assert!(route_paste(text.into(), &s, &mut ui, &mut editor).is_none());
        ui.secret_entry.as_ref().unwrap().value.clone()
    };
    assert_eq!(value_after("sk-abc\n"), "sk-abc");
    assert_eq!(value_after("sk-abc\r\n"), "sk-abc");
    assert_eq!(value_after("sk-abc\r"), "sk-abc");
    assert_eq!(value_after("sk-abc\n\n"), "sk-abc\n");
    assert_eq!(value_after("\nsk-abc"), "\nsk-abc");
    assert_eq!(value_after("\nsk-abc\n"), "\nsk-abc");
    assert_eq!(value_after("\r\n"), "");
}

/// A shown value cannot drive the terminal: escape sequences, an OSC payload, a newline and a
/// tab are sanitized for the display, while the submitted bytes keep them.
#[test]
fn a_shown_value_cannot_drive_the_terminal() {
    use ratatui::{Terminal, backend::TestBackend};
    use withnav_tui::view::{self, RenderCache};

    let mut s = asked();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    withnav_tui::secret::sync(&s, &mut ui);
    let raw = "a\x1b[31mb\n\tc\x1b]0;owned\x07d";
    assert!(route_paste(raw.into(), &s, &mut ui, &mut editor).is_none());
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    );
    assert!(ui.secret_entry.as_ref().unwrap().shown);

    let mut terminal = Terminal::new(TestBackend::new(72, 13)).unwrap();
    terminal
        .draw(|f| view::draw(f, &s, &ui, &editor, &mut RenderCache::default()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
    }
    // The visible value is the sanitized one: no escape sequence residue, newline and tab shown.
    assert!(text.contains("ab··cd"), "{text:?}");
    assert!(!text.contains("[31m"), "{text:?}");
    assert!(!text.contains("]0;owned"), "{text:?}");
    assert!(!text.contains('\x1b'), "{text:?}");
    assert!(
        !text.chars().any(char::is_control),
        "no control character reaches a cell: {text:?}"
    );

    // The submitted bytes are the value's own: display never mutates them.
    let effects = press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Enter,
        KeyModifiers::NONE,
    );
    let [Effect::SubmitSecret { submission, .. }] = &effects[..] else {
        panic!("{effects:?}");
    };
    assert_eq!(submission.value.as_deref(), Some(raw));
}

/// The refusal takes the blank row under the value, so it is drawn without pushing the key help
/// past the modal's fixed height.
#[test]
fn the_refusal_is_visible_and_does_not_clip_the_modal() {
    use withnav_tui::secret::MAX_VALUE_BYTES;

    let s = asked();
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    withnav_tui::secret::sync(&s, &mut ui);
    assert!(route_paste("a".repeat(MAX_VALUE_BYTES + 1), &s, &mut ui, &mut editor).is_none());
    let error = ui
        .secret_entry
        .as_ref()
        .and_then(|e| e.error.clone())
        .expect("a refusal");

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(72, 20)).unwrap();
    terminal
        .draw(|f| {
            withnav_tui::view::draw(
                f,
                &s,
                &ui,
                &editor,
                &mut withnav_tui::view::RenderCache::default(),
            )
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
    }
    assert!(text.contains(&error), "{text:?}");
    assert!(text.contains(&MAX_VALUE_BYTES.to_string()), "{text:?}");
    // The error takes the blank row under the value, so every line after it still fits: the wrap
    // of the key help's last row stays visible.
    assert!(text.contains("ctrl+e write instead"), "{text:?}");
}

fn open_prompt() -> (State, UiState, Editor) {
    let s = asked();
    let mut ui = UiState::default();
    withnav_tui::secret::sync(&s, &mut ui);
    (s, ui, Editor::new())
}

fn type_text(s: &mut State, ui: &mut UiState, editor: &mut Editor, text: &str) {
    for c in text.chars() {
        press(s, ui, editor, KeyCode::Char(c), KeyModifiers::NONE);
    }
}

#[test]
fn a_paste_is_refused_while_the_answer_is_being_sent() {
    let (mut s, mut ui, mut editor) = open_prompt();
    type_text(&mut s, &mut ui, &mut editor, VALUE);
    let effects = press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Enter,
        KeyModifiers::NONE,
    );
    assert_eq!(effects.len(), 1);
    assert!(route_paste("late-paste".into(), &s, &mut ui, &mut editor).is_none());
    assert!(
        ui.secret_entry.as_ref().unwrap().value.is_empty(),
        "nothing lingers after a failed request"
    );
}

#[test]
fn backspace_removes_a_whole_grapheme() {
    let (mut s, mut ui, mut editor) = open_prompt();
    // "e" + combining acute accent is one character to the Captain.
    type_text(&mut s, &mut ui, &mut editor, "ab\u{65}\u{301}");
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Backspace,
        KeyModifiers::NONE,
    );
    assert_eq!(ui.secret_entry.as_ref().unwrap().value, "ab");
}

#[test]
fn ctrl_u_hides_the_value_again() {
    let (mut s, mut ui, mut editor) = open_prompt();
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    );
    type_text(&mut s, &mut ui, &mut editor, "abc");
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    );
    type_text(&mut s, &mut ui, &mut editor, "xyz");
    let entry = ui.secret_entry.as_ref().unwrap();
    assert!(!entry.shown);
    assert_eq!(entry.display(), "•••");
}

#[test]
fn putting_the_prompt_aside_focuses_the_editor() {
    let (mut s, mut ui, mut editor) = open_prompt();
    ui.focus = Focus::Transcript;
    press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Char('e'),
        KeyModifiers::CONTROL,
    );
    assert_eq!(ui.focus, Focus::Editor);
}

#[test]
fn the_submit_effect_carries_the_requested_name_for_the_failure_notice() {
    let (mut s, mut ui, mut editor) = open_prompt();
    type_text(&mut s, &mut ui, &mut editor, VALUE);
    let effects = press(
        &mut s,
        &mut ui,
        &mut editor,
        KeyCode::Enter,
        KeyModifiers::NONE,
    );
    let [
        Effect::SubmitSecret {
            name, submission, ..
        },
    ] = &effects[..]
    else {
        panic!("{effects:?}")
    };
    // A reply to a pending request has no name on the wire; the notice still needs one.
    assert_eq!(submission.name, None);
    assert_eq!(name, "API_KEY");
}
