//! When the terminal client reads the session ledger: on each stream connection, after each
//! turn and after each goal change, one read at a time.
use serde_json::json;
use withnav_client_api::event::{EventEnvelope, EventKind};
use withnav_client_api::ledger::SessionLedger;

use super::update;
use crate::protocol::{ClientEvent, Effect};
use crate::state::State;

fn with_session(id: &str) -> State {
    let mut s = State::default();
    s.tabs[0].session = Some(
        serde_json::from_value(json!({
            "session_id": id,
            "actor_state": "idle",
            "workspace": "/ws",
            "model": "mock/model",
            "current_seq": 0,
        }))
        .unwrap(),
    );
    s
}

fn turn_end(s: &State, seq: u64) -> ClientEvent {
    let session = ulid::Ulid::generate();
    let mut envelope = EventEnvelope::new(
        session,
        seq,
        EventKind::TurnEnd,
        json!({"turn_id": ulid::Ulid::generate(), "actor_id": "agent:main",
               "outcome": "done", "duration_ms": 1}),
    );
    envelope.actor_id = "agent:main".into();
    ClientEvent::Envelope {
        generation: s.generation.clone(),
        envelope,
    }
}

fn ledger_reads(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::FetchLedger(_)))
        .count()
}

fn answered(s: &State, turns: u64) -> ClientEvent {
    let mut ledger = SessionLedger::default();
    ledger.turns.count = turns;
    ClientEvent::Ledger {
        generation: s.generation.clone(),
        result: Ok(ledger),
    }
}

#[test]
fn the_ledger_is_read_when_the_stream_connects() {
    let mut s = with_session("S1");
    let connected = ClientEvent::StreamConnected {
        generation: s.generation.clone(),
    };
    let effects = update(&mut s, connected).unwrap();
    assert!(
        effects.contains(&Effect::FetchLedger("S1".into())),
        "{effects:?}"
    );
}

#[test]
fn a_turn_end_reads_the_ledger_again_one_read_at_a_time() {
    let mut s = with_session("S1");
    let first = turn_end(&s, 1);
    assert_eq!(ledger_reads(&update(&mut s, first).unwrap()), 1);
    // Two more turns end while the read runs: one more read once it returns, not two.
    for seq in [2, 3] {
        let ev = turn_end(&s, seq);
        assert_eq!(ledger_reads(&update(&mut s, ev).unwrap()), 0);
    }
    let ev = answered(&s, 1);
    assert_eq!(
        update(&mut s, ev).unwrap(),
        vec![Effect::FetchLedger("S1".into())]
    );
    assert_eq!(s.conversation.ledger.as_ref().unwrap().turns.count, 1);
    let ev = answered(&s, 3);
    assert!(update(&mut s, ev).unwrap().is_empty());
    assert_eq!(s.conversation.ledger.as_ref().unwrap().turns.count, 3);
}

#[test]
fn a_failed_read_keeps_what_was_shown() {
    let mut s = with_session("S1");
    let ev = answered(&s, 2);
    update(&mut s, ev).unwrap();
    let failed = ClientEvent::Ledger {
        generation: s.generation.clone(),
        result: Err("404 not found".into()),
    };
    update(&mut s, failed).unwrap();
    assert_eq!(s.conversation.ledger.as_ref().unwrap().turns.count, 2);
    assert!(!s.conversation.ledger_reading);
}

#[test]
fn nothing_is_read_before_the_conversation_has_a_session() {
    let mut s = State::default();
    let ev = turn_end(&s, 1);
    assert_eq!(ledger_reads(&update(&mut s, ev).unwrap()), 0);
}

#[test]
fn a_goal_change_reads_the_ledger_again() {
    for (kind, payload) in [
        (
            EventKind::GoalUpdated,
            json!({"revision": 1, "objective": "ship", "criteria": [], "status": "active",
                   "change": "set"}),
        ),
        (EventKind::GoalCleared, json!({"revision": 1})),
    ] {
        let mut s = with_session("S1");
        let envelope = EventEnvelope::new(ulid::Ulid::generate(), 1, kind, payload);
        let ev = ClientEvent::Envelope {
            generation: s.generation.clone(),
            envelope,
        };
        assert_eq!(
            ledger_reads(&update(&mut s, ev).unwrap()),
            1,
            "{}",
            kind.as_str()
        );
    }
}

fn command_result(
    s: &State,
    seq: u64,
    name: &str,
    ok: bool,
    data: serde_json::Value,
) -> ClientEvent {
    let mut payload = json!({"operation_id": "o", "name": name, "ok": ok});
    payload["data"] = data;
    ClientEvent::Envelope {
        generation: s.generation.clone(),
        envelope: EventEnvelope::new(
            ulid::Ulid::generate(),
            seq,
            EventKind::CommandResult,
            payload,
        ),
    }
}

fn goal_check_result(s: &State, seq: u64) -> ClientEvent {
    command_result(
        s,
        seq,
        "goal_check",
        true,
        json!({"revision": 1, "turn_id": "T", "statuses": [], "claims": [], "note_sent": false}),
    )
}

#[test]
fn a_goal_check_result_refreshes_the_ledger_once() {
    let mut s = with_session("S1");
    let first = goal_check_result(&s, 1);
    let effects = update(&mut s, first).unwrap();
    assert_eq!(ledger_reads(&effects), 1, "{effects:?}");
    // A second result while the read runs asks for one more read, not another at once.
    let second = goal_check_result(&s, 2);
    assert_eq!(ledger_reads(&update(&mut s, second).unwrap()), 0);
    let ev = answered(&s, 1);
    assert_eq!(
        update(&mut s, ev).unwrap(),
        vec![Effect::FetchLedger("S1".into())]
    );
    let ev = answered(&s, 1);
    assert!(update(&mut s, ev).unwrap().is_empty());
    // The check changes no register item.
    assert_eq!(memory_reads(&effects), 0);
}

fn memory_reads(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::FetchMemory(_)))
        .count()
}

fn memory_answered(s: &State, titles: &[&str]) -> ClientEvent {
    ClientEvent::Memory {
        generation: s.generation.clone(),
        result: Ok(titles
            .iter()
            .map(|t| crate::state::MemoryEntry {
                id: format!("id-{t}"),
                title: (*t).into(),
                scope: "project".into(),
            })
            .collect()),
    }
}

#[test]
fn the_memory_is_read_when_the_stream_connects_and_after_each_turn_one_read_at_a_time() {
    let mut s = with_session("S1");
    let connected = ClientEvent::StreamConnected {
        generation: s.generation.clone(),
    };
    let effects = update(&mut s, connected).unwrap();
    assert!(
        effects.contains(&Effect::FetchMemory("S1".into())),
        "{effects:?}"
    );
    let ev = turn_end(&s, 1);
    assert_eq!(
        memory_reads(&update(&mut s, ev).unwrap()),
        0,
        "one read runs"
    );
    let ev = memory_answered(&s, &["a"]);
    assert_eq!(
        update(&mut s, ev).unwrap(),
        vec![Effect::FetchMemory("S1".into())]
    );
    let ev = memory_answered(&s, &["a", "b"]);
    assert!(update(&mut s, ev).unwrap().is_empty());
    let titles: Vec<_> = s
        .conversation
        .memory
        .as_ref()
        .unwrap()
        .iter()
        .map(|m| m.title.as_str())
        .collect();
    assert_eq!(titles, ["a", "b"]);
}

#[test]
fn a_register_command_reads_the_memory_and_a_refused_one_does_not() {
    for name in ["remember", "forget", "memory"] {
        let mut s = with_session("S1");
        let ev = command_result(&s, 1, name, true, json!({"items": []}));
        let effects = update(&mut s, ev).unwrap();
        assert_eq!(memory_reads(&effects), 1, "{name}: {effects:?}");
        assert_eq!(ledger_reads(&effects), 0, "{name}");
    }
    let mut s = with_session("S1");
    let ev = command_result(&s, 1, "remember", false, json!(null));
    assert!(update(&mut s, ev).unwrap().is_empty());
}

#[test]
fn a_failed_memory_read_keeps_what_was_shown() {
    let mut s = with_session("S1");
    let ev = memory_answered(&s, &["a"]);
    update(&mut s, ev).unwrap();
    let failed = ClientEvent::Memory {
        generation: s.generation.clone(),
        result: Err("404 not found".into()),
    };
    update(&mut s, failed).unwrap();
    assert_eq!(s.conversation.memory.as_ref().unwrap().len(), 1);
    assert!(!s.conversation.memory_reading);
}
