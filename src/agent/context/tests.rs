use super::*;
use crate::agent::prompt::build_request;
use crate::provider::openai::{RequestFunction, RequestToolCall};
use serde_json::json;

fn user(text: &str) -> ChatMessage {
    ChatMessage::User {
        provenance: Default::default(),
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
        answered_by: None,
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
    assert_eq!(b.messages, history.iter().map(message_tokens).sum::<u64>());
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
    for parameters in [
        json!({"type":"object"}),
        json!({"description":"quotes \" slash \\ newline\n é ✓", "nested":[null, true, 42, {"x":"\u{0000}"}]}),
    ] {
        let tools = vec![ToolDef::function("read", "read a file", parameters)];
        let bytes: usize = tools
            .iter()
            .map(|tool| serde_json::to_string(tool).unwrap().len())
            .sum();
        assert_eq!(tools_tokens(&tools), bytes_tokens(bytes));
    }
    assert_eq!(tools_tokens(&[]), 0);
    let history = vec![user("hello"), assistant_call()];
    let tools = tool_defs();
    let b = estimate(
        Some("role"),
        Some("rules"),
        Some("skills"),
        &history,
        &tools,
    );
    let req = build_request(
        "m",
        Some("role"),
        Some("rules"),
        Some("skills"),
        history,
        tools,
        None,
    );
    let from_request: u64 =
        req.messages.iter().map(message_tokens).sum::<u64>() + tools_tokens(&req.tools);
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
    for (case, window, input_limit, expected) in [
        ("input cap", 1_000_000, 200_000, 200_000 - 50_000),
        (
            "above window minus reserve",
            100_000,
            500_000,
            100_000 - 8192 - 5000,
        ),
        ("zero means unset", 100_000, 0, 100_000 - 8192 - 5000),
    ] {
        let b = Budget::compute(window, Some(input_limit), None, None)
            .unwrap_or_else(|err| panic!("{case}: {err}"));
        assert_eq!(b.input_budget, expected, "{case}");
    }
}

#[test]
fn the_output_reserve_prefers_models_json_then_the_smaller_bound() {
    for (case, window, configured, provider, expected) in [
        (
            "configured wins",
            200_000,
            Some(16_000),
            Some(4_000),
            16_000,
        ),
        ("provider below default", 200_000, None, Some(4_000), 4_000),
        ("provider above default", 200_000, None, Some(64_000), 8192),
        ("quarter-window bound", 20_000, None, None, 5_000),
        ("zero configured means unset", 20_000, Some(0), None, 5_000),
        ("zero provider means unset", 200_000, None, Some(0), 8192),
    ] {
        assert_eq!(
            output_reserve(window, configured, provider),
            expected,
            "{case}"
        );
    }
}

#[test]
fn the_margin_is_five_percent_with_a_floor() {
    assert_eq!(margin(32_768), 4096);
    assert_eq!(margin(80_000), 4096);
    assert_eq!(margin(100_000), 5000);
    assert_eq!(margin(1_000_001), 50_001);
}

#[test]
fn a_small_window_keeps_a_proportional_margin() {
    // An eighth of the window below 32k, so an 8k model with a 4k output
    // limit still has room for input.
    assert_eq!(margin(8192), 1024);
    assert_eq!(margin(16_384), 2048);
    let b = Budget::compute(8192, None, Some(4096), None).unwrap();
    assert_eq!(b.input_budget, 8192 - 4096 - 1024);
    let b = Budget::compute(8192, None, None, None).unwrap();
    assert_eq!(b.input_budget, 8192 - 2048 - 1024);
}

#[test]
fn an_impossible_budget_is_a_clear_error() {
    for (case, window, input_limit, model_limit, diagnostics) in [
        (
            "small window exhausted",
            4096,
            None,
            Some(4096),
            &["4096", "models.json"][..],
        ),
        (
            "output equals window",
            100_000,
            None,
            Some(100_000),
            &[][..],
        ),
        (
            "output exceeds window",
            100_000,
            None,
            Some(500_000),
            &[][..],
        ),
        (
            "input leaves no room",
            100_000,
            Some(5000),
            None,
            &["input limit of 5000"][..],
        ),
        ("input equals margin", 100_000, Some(5000), None, &[][..]),
    ] {
        let err = Budget::compute(window, input_limit, model_limit, None).expect_err(case);
        assert_eq!(err.window, window, "{case}");
        let text = err.to_string();
        for diagnostic in diagnostics {
            assert!(text.contains(diagnostic), "{case}: {text}");
        }
    }
    assert!(
        Budget::compute(100_000, Some(5001), None, None).is_ok(),
        "input one token above margin"
    );
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

fn limits() -> ContextLimits {
    ContextLimits {
        alias: "default".into(),
        gateway: "omniroute".into(),
        upstream_id: "combo/model".into(),
        variant: None,
        window_source: "models.json".into(),
        budget: Budget::compute(1_000_000, None, None, None).unwrap(),
        skills: SkillCounts::default(),
        rule_files: 0,
    }
}

#[test]
fn a_snapshot_never_mixes_the_estimate_with_the_provider_count() {
    let b = estimate(None, None, None, &[user("hi")], &[]);
    let mut tracker = ContextTracker::new(limits(), b);
    let first = tracker.snapshot().clone();
    assert_eq!(first.revision, 0);
    assert_eq!(first.last_usage, None);
    assert_eq!(first.estimate_total, b.total());
    assert_eq!(first.window, 1_000_000);
    assert_eq!(first.input_budget, 1_000_000 - 8192 - 50_000);
    assert_eq!(first.model, "omniroute/combo/model");
    assert_eq!(first.gateway, "omniroute");
    assert_eq!(first.upstream_id, "combo/model");

    let step = ulid::Ulid::generate();
    tracker.record_usage(
        step,
        crate::provider::Usage {
            prompt_tokens: 900,
            completion_tokens: 30,
            total_tokens: 930,
        },
    );
    let next = estimate(None, None, None, &[user("hi"), user("more")], &[]);
    let snap = tracker.update(next).expect("changed").clone();
    assert_eq!(snap.revision, 1);
    assert_eq!(snap.estimate_total, next.total());
    let usage = snap.last_usage.expect("usage");
    assert_eq!(
        (usage.prompt_tokens, usage.completion_tokens, usage.step),
        (900, 30, step)
    );
    // Same estimate, same usage: nothing to publish.
    assert!(tracker.update(next).is_none());
    assert_eq!(tracker.snapshot().revision, 1);
}

#[test]
fn a_new_usage_alone_makes_a_new_snapshot() {
    let b = estimate(None, None, None, &[], &[]);
    let mut tracker = ContextTracker::new(limits(), b);
    tracker.record_usage(ulid::Ulid::generate(), crate::provider::Usage::default());
    assert_eq!(tracker.update(b).map(|s| s.revision), Some(1));
}

#[test]
fn tokens_are_shown_in_thousands_with_separators() {
    assert_eq!(tokens_k(0), "0");
    assert_eq!(tokens_k(999), "999");
    assert_eq!(tokens_k(1_000), "1k");
    assert_eq!(tokens_k(41_600), "42k");
    assert_eq!(tokens_k(1_000_000), "1,000k");
    assert_eq!(tokens_k(12_345_678), "12,346k");
}

#[test]
fn the_text_view_shows_the_gauge_and_every_category() {
    let history = vec![user(&"x".repeat(168_000))];
    let b = estimate(Some("role"), Some("rules"), None, &history, &tool_defs());
    let text = ContextTracker::new(limits(), b).snapshot().render_text();
    let first = text.lines().next().unwrap();
    assert!(
        first.starts_with("~42k / 1,000k · 4% · input budget 942k"),
        "{first}"
    );
    for name in ["system", "rules", "skills catalog", "tools", "messages"] {
        assert!(text.contains(name), "{text}");
    }
    assert!(text.contains("window from models.json"));
    assert!(!text.contains("last request"));
}

#[test]
fn each_tracker_is_its_own_incarnation_and_revisions_restart() {
    let b = estimate(None, None, None, &[], &[]);
    let mut first = ContextTracker::new(limits(), b);
    let bigger = estimate(None, None, None, &[user("hi")], &[]);
    first.update(bigger);
    let old = first.snapshot().clone();
    let resumed = ContextTracker::new(limits(), bigger);
    let new = resumed.snapshot().clone();
    assert_eq!((old.revision, new.revision), (1, 0));
    assert_ne!(old.incarnation, new.incarnation);
}

#[test]
fn a_snapshot_is_marked_estimated_and_names_the_sources_behind_the_estimate() {
    let mut l = limits();
    l.skills = SkillCounts {
        listed: 7,
        omitted: 2,
    };
    l.rule_files = 3;
    let b = estimate(None, None, None, &[], &[]);
    let mut tracker = ContextTracker::new(l, b);
    let snap = tracker.snapshot().clone();
    assert!(snap.estimated);
    assert_eq!((snap.skills.listed, snap.skills.omitted), (7, 2));
    assert_eq!(snap.rule_files, 3);
    tracker.record_usage(ulid::Ulid::generate(), crate::provider::Usage::default());
    let snap = tracker.update(b).expect("usage changed").clone();
    assert_eq!(
        snap.last_usage.expect("usage").model,
        "omniroute/combo/model"
    );
    assert_eq!(snap.skills.listed, 7);
}

#[test]
fn a_snapshot_without_the_newer_fields_still_reads() {
    let wire = serde_json::json!({
        "model": "p/a", "gateway": "p", "upstream_id": "a", "window": 100000, "window_source": "models.json",
        "estimate_total": 10, "breakdown": {"system": 10, "rules": 0, "skills_catalog": 0, "tools": 0, "messages": 0},
        "output_reserve": 1, "margin": 1, "input_budget": 98, "percent": 0,
        "incarnation": ulid::Ulid::generate().to_string(), "revision": 1,
    });
    let snap: ContextSnapshot = serde_json::from_value(wire).unwrap();
    assert!(snap.estimated);
    assert_eq!(snap.skills, SkillCounts::default());
    assert_eq!(snap.rule_files, 0);
}
