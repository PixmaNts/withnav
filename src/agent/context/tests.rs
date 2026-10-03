use super::*;
use crate::agent::prompt::build_request;
use crate::provider::openai::{RequestFunction, RequestToolCall};
use serde_json::json;

fn user(text: &str) -> ChatMessage {
    ChatMessage::User {
        content: text.into(),
    }
}

fn assistant_call() -> ChatMessage {
    ChatMessage::Assistant {
        content: Some("looking".into()),
        reasoning_content: Some("thinking hard".into()),
        tool_calls: vec![RequestToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: RequestFunction {
                name: "read".into(),
                arguments: "{\"path\":\"a\"}".into(),
            },
        }],
    }
}

fn tool_defs() -> Vec<ToolDef> {
    vec![ToolDef::function(
        "read",
        "read a file",
        json!({"type": "object"}),
    )]
}

#[test]
fn text_is_bytes_over_four_rounded_up() {
    assert_eq!(text_tokens(""), 0);
    assert_eq!(text_tokens("a"), 1);
    assert_eq!(text_tokens("abcd"), 1);
    assert_eq!(text_tokens("abcde"), 2);
    // Bytes, not chars: "é" is two bytes, "✓" three.
    assert_eq!(text_tokens("éé"), 1);
    assert_eq!(text_tokens("✓✓"), 2);
}

#[test]
fn a_message_adds_its_framing() {
    assert_eq!(message_tokens(&user("")), MESSAGE_FRAMING_TOKENS);
    assert_eq!(message_tokens(&user("abcde")), 2 + MESSAGE_FRAMING_TOKENS);
}

#[test]
fn an_assistant_message_counts_text_reasoning_and_calls() {
    let bytes = "looking".len()
        + "thinking hard".len()
        + "call_1".len()
        + "read".len()
        + "{\"path\":\"a\"}".len();
    assert_eq!(
        message_tokens(&assistant_call()),
        (bytes as u64).div_ceil(4) + MESSAGE_FRAMING_TOKENS
    );
}

#[test]
fn a_tool_message_counts_its_id_and_output() {
    let m = ChatMessage::Tool {
        tool_call_id: "abcd".into(),
        content: "efgh".into(),
    };
    assert_eq!(message_tokens(&m), 2 + MESSAGE_FRAMING_TOKENS);
}

#[test]
fn categories_sum_exactly_to_the_total() {
    let history = vec![user("hello there"), assistant_call()];
    let b = estimate(
        Some("role"),
        Some("rules text"),
        Some("skills text"),
        &history,
        &tool_defs(),
    );
    assert!(b.system > 0 && b.rules > 0 && b.skills_catalog > 0 && b.tools > 0 && b.messages > 0);
    assert_eq!(
        b.total(),
        b.system + b.rules + b.skills_catalog + b.tools + b.messages
    );
    assert_eq!(
        b.messages,
        history.iter().map(message_tokens).sum::<u64>()
    );
}

#[test]
fn absent_components_weigh_nothing_and_the_contract_always_counts() {
    let b = estimate(None, None, None, &[], &[]);
    assert_eq!(
        b,
        Breakdown {
            system: text_tokens(SYSTEM_CONTRACT) + MESSAGE_FRAMING_TOKENS,
            ..Default::default()
        }
    );
    let with_role = estimate(Some("role"), None, None, &[], &[]);
    assert_eq!(
        with_role.system,
        b.system + text_tokens("role") + MESSAGE_FRAMING_TOKENS
    );
}

#[test]
fn the_estimate_matches_the_assembled_request() {
    let history = vec![user("hello"), assistant_call()];
    let tools = tool_defs();
    let b = estimate(Some("role"), Some("rules"), Some("skills"), &history, &tools);
    let req = build_request(
        "m",
        Some("role"),
        Some("rules"),
        Some("skills"),
        history,
        tools,
        None,
    );
    let from_request: u64 = req.messages.iter().map(message_tokens).sum::<u64>()
        + tools_tokens(&req.tools);
    assert_eq!(b.total(), from_request);
}

#[test]
fn budget_follows_the_formula() {
    // W = 1_000_000: R = min(8192, 250_000) = 8192, M = 50_000.
    let b = Budget::compute(1_000_000, None, None, None).unwrap();
    assert_eq!(b.output_reserve, 8192);
    assert_eq!(b.margin, 50_000);
    assert_eq!(b.input_budget, 1_000_000 - 8192 - 50_000);
}

#[test]
fn the_input_limit_caps_the_budget() {
    let b = Budget::compute(1_000_000, Some(200_000), None, None).unwrap();
    assert_eq!(b.input_budget, 200_000 - 50_000);
    // A limit above W - R changes nothing.
    let b = Budget::compute(100_000, Some(500_000), None, None).unwrap();
    assert_eq!(b.input_budget, 100_000 - 8192 - 5000);
    // A zero limit means "not set".
    let b = Budget::compute(100_000, Some(0), None, None).unwrap();
    assert_eq!(b.input_budget, 100_000 - 8192 - 5000);
}

#[test]
fn the_output_reserve_prefers_models_json_then_the_smaller_bound() {
    assert_eq!(output_reserve(200_000, Some(16_000), Some(4_000)), 16_000);
    assert_eq!(output_reserve(200_000, None, Some(4_000)), 4_000);
    assert_eq!(output_reserve(200_000, None, Some(64_000)), 8192);
    assert_eq!(output_reserve(20_000, None, None), 5_000);
    assert_eq!(output_reserve(20_000, Some(0), None), 5_000);
    assert_eq!(output_reserve(200_000, None, Some(0)), 8192);
}

#[test]
fn the_margin_is_five_percent_with_a_floor() {
    assert_eq!(margin(10_000), 4096);
    assert_eq!(margin(80_000), 4096);
    assert_eq!(margin(100_000), 5000);
    assert_eq!(margin(1_000_001), 50_001);
}

#[test]
fn an_impossible_budget_is_a_clear_error() {
    // W = 4096: R = 1024, M = 4096 leaves nothing.
    let err = Budget::compute(4096, None, None, None).unwrap_err();
    assert_eq!(err.window, 4096);
    let text = err.to_string();
    assert!(text.contains("4096") && text.contains("models.json"), "{text}");
    // An output limit as large as the window.
    assert!(Budget::compute(100_000, None, Some(100_000), None).is_err());
    assert!(Budget::compute(100_000, None, Some(500_000), None).is_err());
    // An input limit inside the margin.
    let err = Budget::compute(100_000, Some(5000), None, None).unwrap_err();
    assert!(err.to_string().contains("input limit of 5000"));
    // Exactly the margin is still zero room.
    assert!(Budget::compute(100_000, Some(5000), None, None).is_err());
    assert!(Budget::compute(100_000, Some(5001), None, None).is_ok());
}

#[test]
fn percent_rounds_to_the_nearest_whole_number() {
    assert_eq!(percent(0, 1000), 0);
    assert_eq!(percent(42_000, 1_000_000), 4);
    assert_eq!(percent(45_000, 1_000_000), 5);
    assert_eq!(percent(500, 1000), 50);
    assert_eq!(percent(2000, 1000), 200);
    assert_eq!(percent(5, 0), 0);
}
