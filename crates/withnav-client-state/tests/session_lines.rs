//! The two transcript lines the session view adds: what `/remember` stored, and the note a goal
//! check queued for the agent. Both come from `command.result` events.
use serde_json::{Value, json};
use withnav_client_api::event::{EventEnvelope, EventKind};
use withnav_client_state::{BlockKind, EnvelopeContext, SessionState, apply_envelope};

fn result(seq: u64, name: &str, ok: bool, data: Value) -> EventEnvelope {
    let mut payload = json!({"operation_id": "o", "name": name, "ok": ok});
    if ok {
        payload["data"] = data;
    } else {
        payload["error"] = data;
    }
    EventEnvelope::new(ulid::Ulid(1), seq, EventKind::CommandResult, payload)
}

fn replay(envelopes: Vec<EventEnvelope>) -> SessionState {
    let mut state = SessionState::default();
    let mut mcp = None;
    for envelope in envelopes {
        apply_envelope(
            &mut state,
            envelope,
            EnvelopeContext {
                own_session: None,
                read_only: false,
                default_agent: None,
                catalog: &[],
                mcp: &mut mcp,
            },
        )
        .expect("the envelope applies");
    }
    state
}

fn notices(state: &SessionState) -> Vec<String> {
    state
        .conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::Notice(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_remembered_item_is_a_line_with_its_scope_and_how_to_undo_it() {
    let item = json!({"id": "mem_abc", "scope": "project", "current_version": 1,
                      "current": {"title": "Use conventional commits"}});
    let state = replay(vec![result(
        1,
        "remember",
        true,
        json!({"scope_label": "project WithNav", "item": item}),
    )]);
    assert_eq!(
        notices(&state),
        vec![
            "Remembered: \"Use conventional commits\" · project WithNav · undo with /forget mem_abc"
        ]
    );
}

#[test]
fn a_remembered_item_with_a_notice_keeps_the_notice_and_controls_are_stripped() {
    let item = json!({"id": "m", "scope": "session", "current_version": 1,
                      "current": {"title": "a\u{1b}[2Jb"}});
    let state = replay(vec![result(
        1,
        "remember",
        true,
        json!({"scope_label": "session", "item": item,
               "notice": "stored for this session only: project unknown"}),
    )]);
    assert_eq!(
        notices(&state),
        vec![
            "Remembered: \"ab\" · session · undo with /forget m\n\
             stored for this session only: project unknown"
        ]
    );
}

#[test]
fn a_refused_remember_stays_a_command_error() {
    let state = replay(vec![result(
        1,
        "remember",
        false,
        json!("usage: /remember"),
    )]);
    assert!(notices(&state).is_empty());
    assert!(matches!(
        &state.conversation.blocks[0].kind,
        BlockKind::Command { name, ok: false, .. } if name == "remember"
    ));
}

#[test]
fn the_other_register_commands_keep_their_command_line() {
    let item = json!({"id": "m", "current_version": 2, "current": {"title": "T"}});
    let state = replay(vec![result(
        1,
        "forget",
        true,
        json!({"scope_label": "global", "item": item}),
    )]);
    assert!(notices(&state).is_empty());
    assert!(matches!(
        &state.conversation.blocks[0].kind,
        BlockKind::Command { text, .. } if text == "Forgot \"T\" · global · m v2"
    ));
}

#[test]
fn a_goal_note_is_marked_as_an_automatic_observation() {
    let state = replay(vec![result(
        1,
        "goal_check",
        true,
        json!({"revision": 3, "turn_id": "T", "statuses": [], "claims": [], "note_sent": true,
               "note": "Goal check (revision 3): criterion 2 is contradicted.\u{7}"}),
    )]);
    assert_eq!(
        notices(&state),
        vec![
            "Goal note · automatic observation, not a Captain message\n\
             Goal check (revision 3): criterion 2 is contradicted."
        ]
    );
}

#[test]
fn a_goal_check_that_queued_no_note_adds_no_line() {
    let state = replay(vec![result(
        1,
        "goal_check",
        true,
        json!({"revision": 3, "turn_id": "T", "statuses": [], "claims": [], "note_sent": false}),
    )]);
    assert!(state.conversation.blocks.is_empty());
}
