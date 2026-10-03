use super::*;
use clap::CommandFactory;

#[test]
fn cli_debug_assert() {
    Cli::command().debug_assert();
}

#[test]
fn exit_codes_are_pinned() {
    assert_eq!(EXIT_DONE, 0);
    assert_eq!(EXIT_ERROR, 1);
    assert_eq!(EXIT_YIELDED, 2);
    assert_eq!(EXIT_INTERRUPTED, 130);

    // The production mapping, one row per terminal outcome.
    for (outcome, expected) in [
        (TurnEndOutcome::Done, EXIT_DONE),
        (TurnEndOutcome::Yielded, EXIT_YIELDED),
        (TurnEndOutcome::Interrupted, EXIT_INTERRUPTED),
        (TurnEndOutcome::Error, EXIT_ERROR),
    ] {
        assert_eq!(exit_code_for(&turn_end_envelope(outcome)), expected);
    }
    // A turn.end whose payload cannot be parsed is an error, not a success.
    let malformed = kind_envelope(EventKind::TurnEnd, serde_json::json!({}));
    assert_eq!(exit_code_for(&malformed), EXIT_ERROR);
}

#[test]
fn read_prompt_prefers_literal() {
    assert_eq!(read_prompt(Some("hi there".into())).unwrap(), "hi there");
}

#[test]
fn cli_lists_its_subcommands() {
    let cmd = Cli::command();
    let names: Vec<_> = cmd.get_subcommands().map(|s| s.get_name()).collect();
    for expected in ["run", "serve", "auth", "tui"] {
        assert!(names.contains(&expected), "missing subcommand: {expected}");
    }
}

#[test]
fn tui_flags_parse_and_conflicts_are_enforced() {
    let cli = Cli::try_parse_from([
        "navai",
        "tui",
        "--workspace",
        "/tmp",
        "--model",
        "fast",
        "--url",
        "http://localhost",
    ]);
    assert!(cli.is_ok());
    assert!(
        Cli::try_parse_from(["navai", "tui", "--model", "fast", "--session", "01ABC"]).is_err()
    );
    assert!(Cli::try_parse_from([
        "navai",
        "tui",
        "--models",
        "models.json",
        "--url",
        "http://localhost"
    ])
    .is_err());
    let help = Cli::command().render_long_help().to_string();
    assert!(help.contains("tui"));
}

#[test]
fn bogus_subcommand_is_error() {
    let result = Cli::try_parse_from(["navai", "bogus-subcommand"]);
    assert!(result.is_err());
}

// --- event_to_ui render-parity ---

use crate::bus::event::{
    AgentYield as AgentYieldPayload, LlmDelta as LlmDeltaPayload, TurnEnd as TurnEndPayload,
    TurnError,
};
use crate::provider::StreamDelta;
use ulid::Ulid;

fn delta_envelope(delta: StreamDelta) -> EventEnvelope {
    let payload = LlmDeltaPayload {
        turn_id: Ulid::new(),
        step_id: Ulid::new(),
        delta,
    };
    EventEnvelope::new(
        Ulid::new(),
        1,
        EventKind::LlmDelta,
        serde_json::to_value(&payload).unwrap(),
    )
}

fn kind_envelope(kind: EventKind, payload: serde_json::Value) -> EventEnvelope {
    EventEnvelope::new(Ulid::new(), 2, kind, payload)
}

fn turn_end_envelope(outcome: TurnEndOutcome) -> EventEnvelope {
    let payload = TurnEndPayload {
        turn_id: Ulid::new(),
        actor_id: "agent:main".into(),
        outcome,
        usage: None,
        duration_ms: 5,
        error: None,
    };
    kind_envelope(EventKind::TurnEnd, serde_json::to_value(&payload).unwrap())
}

// Given the scripted delta sequence of a simple Phase-1 run (thinking,
// text, a tool starting, more text) followed by a Done turn end
// When each envelope passes through event_to_ui
// Then the UiEvent sequence equals the Phase-1 on_delta + finish output
#[test]
fn event_to_ui_matches_phase1_on_delta_and_finish_sequence() {
    let envelopes = [
        delta_envelope(StreamDelta::Thinking {
            text: "pondering".into(),
        }),
        delta_envelope(StreamDelta::Text {
            text: "hello ".into(),
        }),
        delta_envelope(StreamDelta::ToolCallStarted {
            index: 0,
            name: "bash".into(),
        }),
        delta_envelope(StreamDelta::ToolCallArgs {
            index: 0,
            fragment: "ls".into(),
        }),
        delta_envelope(StreamDelta::Text {
            text: "world".into(),
        }),
        turn_end_envelope(TurnEndOutcome::Done),
    ];

    let ui: Vec<UiEvent> = envelopes.iter().filter_map(event_to_ui).collect();
    assert_eq!(ui.len(), 5);
    assert!(matches!(&ui[0], UiEvent::Thinking(t) if t == "pondering"));
    assert!(matches!(&ui[1], UiEvent::AssistantText(t) if t == "hello "));
    assert!(matches!(&ui[2], UiEvent::ToolStarted { name } if name == "bash"));
    // ToolCallArgs rendered nothing in Phase 1 (empty on_delta branch):
    assert!(matches!(&ui[3], UiEvent::AssistantText(t) if t == "world"));
    assert!(matches!(ui[4], UiEvent::Done));
}

// Given a turn that ended Yielded with an agent.yield carrying the question
// When the envelopes pass through event_to_ui
// Then the Yielded UiEvent carries question + kind exactly as print_yield
#[test]
fn event_to_ui_maps_agent_yield_to_the_yielded_ui_event() {
    let yield_payload = AgentYieldPayload {
        actor_id: "agent:main".into(),
        turn_id: Ulid::new(),
        question: "insert SD card?".into(),
        kind: Some("confirm".into()),
        details: None,
    };
    let yield_env = kind_envelope(
        EventKind::AgentYield,
        serde_json::to_value(&yield_payload).unwrap(),
    );
    // The Phase-1 finish() rendered Yielded from agent.yield, and the
    // turn.end{yielded} itself carried no additional UiEvent:
    let turn_end_env = kind_envelope(
        EventKind::TurnEnd,
        serde_json::to_value(&TurnEndPayload {
            turn_id: yield_payload.turn_id,
            actor_id: "agent:main".into(),
            outcome: TurnEndOutcome::Yielded,
            usage: None,
            duration_ms: 9,
            error: None,
        })
        .unwrap(),
    );

    assert!(matches!(
        event_to_ui(&yield_env),
        Some(UiEvent::Yielded { question, kind })
            if question == "insert SD card?" && kind.as_deref() == Some("confirm")
    ));
    assert!(event_to_ui(&turn_end_env).is_none());
}

// Given the lifecycle events with no Phase-1 rendering
// When they pass through event_to_ui
// Then none produces a UiEvent
#[test]
fn event_to_ui_returns_none_for_unrendered_kinds() {
    let session_env = kind_envelope(EventKind::SessionStart, serde_json::json!({}));
    let state_env = kind_envelope(EventKind::AgentState, serde_json::json!({}));
    let message_env = kind_envelope(EventKind::MessageReceived, serde_json::json!({}));
    assert!(event_to_ui(&session_env).is_none());
    assert!(event_to_ui(&state_env).is_none());
    assert!(event_to_ui(&message_env).is_none());
}

// Given an interrupted or errored turn end
// When it passes through event_to_ui
// Then no UiEvent is produced (Phase-1 printed those to stderr, not render)
#[test]
fn event_to_ui_maps_interrupted_and_error_turn_ends_to_none() {
    for outcome in [TurnEndOutcome::Interrupted, TurnEndOutcome::Error] {
        let env = kind_envelope(
            EventKind::TurnEnd,
            serde_json::to_value(&TurnEndPayload {
                turn_id: Ulid::new(),
                actor_id: "agent:main".into(),
                outcome,
                usage: None,
                duration_ms: 3,
                error: Some(TurnError {
                    code: "provider".into(),
                    message: "boom".into(),
                }),
            })
            .unwrap(),
        );
        assert!(
            event_to_ui(&env).is_none(),
            "{outcome:?} rendered to stderr in Phase 1"
        );
    }
}

#[test]
fn the_html_converter_subcommand_is_hidden_but_parses() {
    let help = Cli::command().render_long_help().to_string();
    assert!(!help.contains("internal-html-convert"));
    assert!(Cli::try_parse_from(["navai", "internal-html-convert", "--format", "text"]).is_ok());
    assert!(Cli::try_parse_from(["navai", "internal-html-convert", "--format", "html"]).is_err());
}
