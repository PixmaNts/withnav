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

    assert_eq!(state.activity, Activity::Stopped);

    let tool_status = state
        .conversation
        .blocks
        .iter()
        .find_map(|b| match &b.kind {
            BlockKind::ToolCall { name, status, .. } if name == "bash" => *status,
            _ => None,
        });

    assert_eq!(tool_status, Some(ToolStatus::Interrupted));
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
