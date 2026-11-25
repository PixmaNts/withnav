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
