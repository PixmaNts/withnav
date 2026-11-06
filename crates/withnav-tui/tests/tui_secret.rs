//! The terminal secret prompt: a `request_secret` opens it, the value is typed masked, moves
//! into the one effect that sends it, and never reaches the conversation state or the screen.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;
use withnav_tui::{
    input::{Editor, route_key, route_paste},
    protocol::*,
    reducer,
    state::{BlockKind, SecretState, State, UiState},
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
