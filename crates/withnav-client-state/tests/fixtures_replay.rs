use std::path::Path;

use withnav_client_api::event::EventEnvelope;
use withnav_client_api::tools::ToolStatus;
use withnav_client_state::{Activity, BlockKind, EnvelopeContext, SessionState, apply_envelope};

fn parse_sse(raw: &str) -> Vec<EventEnvelope> {
    let mut envelopes = Vec::new();
    for block in raw.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        let mut data_lines = Vec::new();
        for line in block.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.trim_start());
            }
        }
        if !data_lines.is_empty() {
            let data = data_lines.join("\n");
            let env: EventEnvelope = serde_json::from_str(&data)
                .unwrap_or_else(|e| panic!("failed to parse envelope: {e}\nData: {data}"));
            envelopes.push(env);
        }
    }
    envelopes
}

fn load_fixture(name: &str) -> Vec<EventEnvelope> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = Path::new(manifest_dir)
        .join("../../tests/fixtures/events")
        .join(name);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()));
    parse_sse(&raw)
}

fn replay(envelopes: &[EventEnvelope]) -> SessionState {
    let mut state = SessionState::default();
    let mut mcp = None;
    for env in envelopes {
        apply_envelope(
            &mut state,
            env.clone(),
            EnvelopeContext {
                own_session: None,
                read_only: false,
                default_agent: None,
                catalog: &[],
                mcp: &mut mcp,
            },
        )
        .expect("apply_envelope must succeed");
    }
    state
}

#[test]
fn replay_write_file_fixture() {
    let envelopes = load_fixture("write-file.sse");
    let state = replay(&envelopes);

    assert_eq!(state.activity, Activity::Idle);

    let tools: Vec<_> = state
        .conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::ToolCall {
                name,
                status,
                output,
                args,
                ..
            } => Some((name.as_str(), *status, output.as_str(), args)),
            _ => None,
        })
        .collect();

    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].0, "glob");
    assert_eq!(tools[0].1, Some(ToolStatus::Ok));
    assert_eq!(tools[0].2, "README.md");
    assert_eq!(tools[0].3["pattern"], "**/*");

    assert_eq!(tools[1].0, "write");
    assert_eq!(tools[1].1, Some(ToolStatus::Ok));
    assert_eq!(tools[1].2, "wrote 2 bytes");
    assert_eq!(tools[1].3["path"], "hello.txt");

    let last_text = state
        .conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::AssistantStep { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .next_back()
        .expect("an assistant step must be present");
    assert!(last_text.contains("hello.txt"));
}

#[test]
fn replay_permission_denied_fixture() {
    let envelopes = load_fixture("permission-denied.sse");
    let state = replay(&envelopes);

    assert_eq!(state.activity, Activity::Idle);

    let permissions: Vec<_> = state
        .conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::Permission(p) => Some((p.tool.as_str(), p.resolution.as_deref())),
            _ => None,
        })
        .collect();

    assert_eq!(permissions.len(), 1);
    assert_eq!(permissions[0].0, "bash");
    assert_eq!(permissions[0].1, Some("deny"));

    let edit_tool = state
        .conversation
        .blocks
        .iter()
        .find_map(|b| match &b.kind {
            BlockKind::ToolCall { name, status, .. } if name == "edit" => *status,
            _ => None,
        });
    assert_eq!(edit_tool, Some(ToolStatus::Ok));
}

#[test]
fn replay_reconnect_gap_fixture() {
    let envelopes = load_fixture("reconnect-gap.sse");
    let state = replay(&envelopes);

    assert_eq!(state.activity, Activity::Idle);

    let step_text = state
        .conversation
        .blocks
        .iter()
        .find_map(|b| match &b.kind {
            BlockKind::AssistantStep { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .expect("assistant step text must be present");

    assert_eq!(
        step_text,
        "Paris is the capital of France and its largest city."
    );
}

#[test]
fn replay_session_end_interrupted_fixture() {
    let envelopes = load_fixture("session-end-interrupted.sse");
    let state = replay(&envelopes);

    // session.end settles the whole conversation: activity stops, the
    // running call is interrupted and the completed one keeps its result.
    assert_eq!(state.activity, Activity::Stopped);
    assert_eq!(state.conversation.current_turn, None);

    let tools: Vec<_> = state
        .conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::ToolCall { name, status, .. } => Some((name.as_str(), *status)),
            _ => None,
        })
        .collect();

    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0], ("glob", Some(ToolStatus::Ok)));
    assert_eq!(tools[1], ("bash", Some(ToolStatus::Interrupted)));
}

#[test]
fn replay_session_end_permission_fixture() {
    let envelopes = load_fixture("session-end-permission.sse");
    let state = replay(&envelopes);

    assert_eq!(state.activity, Activity::Stopped);

    // The gated call never ran: it settles interrupted like every planned
    // call the session abandoned, and the prompt it raised is no longer
    // answerable.
    let permissions: Vec<_> = state
        .conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::Permission(p) => Some((p.tool.as_str(), p.resolution.as_deref(), p.stale)),
            _ => None,
        })
        .collect();
    assert_eq!(permissions.len(), 1);
    assert_eq!(permissions[0], ("bash", Some("cancelled"), false));
    assert!(state.conversation.open_permissions.is_empty());

    let gated = state
        .conversation
        .blocks
        .iter()
        .find_map(|b| match &b.kind {
            BlockKind::ToolCall { name, status, .. } if name == "bash" => *status,
            _ => None,
        });
    assert_eq!(gated, Some(ToolStatus::Interrupted));

    let completed = state
        .conversation
        .blocks
        .iter()
        .find_map(|b| match &b.kind {
            BlockKind::ToolCall { name, status, .. } if name == "glob" => *status,
            _ => None,
        });
    assert_eq!(completed, Some(ToolStatus::Ok));
}

#[test]
fn replayed_session_end_is_idempotent() {
    let envelopes = load_fixture("session-end-permission.sse");
    let mut state = replay(&envelopes);

    // A second end (a resync overlap, a repeated capture) settles nothing
    // twice: the settled prompt keeps its resolution and the completed tool
    // keeps its result. Only the event cursor legitimately moves.
    let before = state.conversation.clone();
    let second = EventEnvelope {
        seq: 10,
        kind: "session.end".into(),
        ..envelopes.last().cloned().unwrap()
    };
    let mut mcp = None;
    apply_envelope(
        &mut state,
        second,
        EnvelopeContext {
            own_session: None,
            read_only: false,
            default_agent: None,
            catalog: &[],
            mcp: &mut mcp,
        },
    )
    .expect("apply_envelope must succeed");
    assert_eq!(state.conversation.blocks, before.blocks);
    assert_eq!(state.conversation.open_permissions, before.open_permissions);
    assert_eq!(state.activity, Activity::Stopped);
}

#[test]
fn replay_turn_error_fixture() {
    let envelopes = load_fixture("turn-error.sse");
    let state = replay(&envelopes);

    assert_eq!(state.activity, Activity::Idle);

    let error_text = state
        .conversation
        .blocks
        .iter()
        .find_map(|b| match &b.kind {
            BlockKind::Error(text) => Some(text.as_str()),
            _ => None,
        })
        .expect("error block must be present");

    assert_eq!(
        error_text,
        "context_length_exceeded: Prompt exceeds maximum context length"
    );
}
