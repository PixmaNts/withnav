use super::*;
use crate::provider::openai::{RequestFunction, RequestToolCall};

fn user(text: &str) -> ChatMessage {
    ChatMessage::User {
        content: text.into(),
    }
}

fn call(id: &str, name: &str, args: &str) -> ChatMessage {
    ChatMessage::Assistant {
        content: None,
        reasoning_content: None,
        tool_calls: vec![RequestToolCall {
            id: id.into(),
            kind: "function".into(),
            function: RequestFunction {
                name: name.into(),
                arguments: args.into(),
            },
        }],
    }
}

fn answer(text: &str) -> ChatMessage {
    ChatMessage::Assistant {
        content: Some(text.into()),
        reasoning_content: None,
        tool_calls: vec![],
    }
}

fn result(id: &str, bytes: usize) -> ChatMessage {
    ChatMessage::Tool {
        tool_call_id: id.into(),
        content: "x".repeat(bytes),
    }
}

fn components() -> Components<'static> {
    Components {
        system_prompt: None,
        rules: None,
        skills: None,
        tools: &[],
    }
}

/// Five completed turns, each one tool exchange of `bytes` output.
fn five_turns(bytes: usize) -> Vec<ChatMessage> {
    let mut t = Vec::new();
    for n in 0..5 {
        t.push(user(&format!("request {n}")));
        t.push(call(&format!("c{n}"), "read", "{}"));
        t.push(result(&format!("c{n}"), bytes));
        t.push(answer(&format!("done {n}")));
    }
    t
}

fn size(c: &Components<'_>, t: &[ChatMessage], k: Option<&Compaction>) -> u64 {
    estimate_effective(c, t, k).total()
}

#[test]
fn the_last_two_completed_turns_are_protected() {
    let t = five_turns(100);
    assert_eq!(protected_start(&t), 12);
}

#[test]
fn an_unfinished_turn_adds_to_the_protected_ones() {
    let mut t = five_turns(100);
    t.push(user("now"));
    assert_eq!(protected_start(&t), 12);
    // An unanswered ask is unfinished too.
    let mut t = five_turns(100);
    t.push(user("go"));
    t.push(call("ask1", "ask", "{\"question\":\"?\"}"));
    assert_eq!(protected_start(&t), 12);
    // Steps of the unfinished turn never move the start.
    t.push(result("ask1", 10));
    t.push(call("c9", "read", "{}"));
    assert_eq!(protected_start(&t), 12);
}

#[test]
fn a_roomy_budget_summarizes_the_earliest_exchange_after_nothing_is_pruned() {
    let t = five_turns(400);
    let p = plan(&t, None, &components(), 1_000_000, Aim::Fit).unwrap();
    assert!(p.pruned.is_empty());
    // The earliest cut: the first assistant step.
    assert_eq!(p.through_message, 0);
}

#[test]
fn a_tight_budget_prunes_the_biggest_oldest_outputs_before_moving_the_boundary() {
    let mut t = five_turns(400);
    // Turn 1's output is the biggest.
    t[6] = result("c1", 4000);
    let c = components();
    let full = size(&c, &t, None);
    let budget = full + summary_reserve_tokens() - 200;
    let p = plan(&t, None, &c, budget, Aim::Fit).unwrap();
    assert!(p.estimated_after <= budget);
    assert!(p.pruned.contains(&6), "the biggest output goes first");
    for &i in &p.pruned {
        assert!(matches!(t[i], ChatMessage::Tool { .. }));
        assert!(i < protected_start(&t));
    }
}

#[test]
fn the_boundary_never_cuts_a_tool_exchange() {
    let t = five_turns(400);
    let c = components();
    for budget in [3000u64, 4000, 5000, 6000] {
        let Ok(p) = plan(&t, None, &c, budget, Aim::Fit) else {
            continue;
        };
        let next = &t[p.through_message + 1];
        assert!(matches!(
            next,
            ChatMessage::User { .. } | ChatMessage::Assistant { .. }
        ));
        // The call before the boundary has its result before it too.
        assert!(
            !matches!(&t[p.through_message], ChatMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty())
        );
    }
}

#[test]
fn the_protected_tail_is_never_pruned_or_summarized() {
    let t = five_turns(2000);
    let c = components();
    let tail = protected_start(&t);
    let p = plan(
        &t,
        None,
        &c,
        1000 + summary_reserve_tokens() + 700,
        Aim::Fit,
    )
    .unwrap();
    assert!(p.through_message < tail);
    assert!(p.pruned.iter().all(|i| *i < tail));
}

#[test]
fn a_tail_that_cannot_fit_is_a_capacity_error() {
    let t = five_turns(4000);
    let err = plan(&t, None, &components(), 500, Aim::Fit).unwrap_err();
    assert!(matches!(err, CompactionError::Capacity { budget: 500, .. }));
    assert!(err.to_string().contains("larger context window"));
}

#[test]
fn skill_bodies_and_ask_answers_are_never_pruned() {
    let mut t = vec![
        user("a"),
        call("s1", "skill", "{\"name\":\"x\"}"),
        result("s1", 9000),
    ];
    t.push(call("a1", "ask", "{}"));
    t.push(result("a1", 8000));
    t.push(call("r1", "read", "{}"));
    t.push(result("r1", 100));
    t.push(answer("ok"));
    t.extend(five_turns(10));
    let c = components();
    let p = plan(&t, None, &c, 2800 + summary_reserve_tokens(), Aim::Fit).unwrap();
    assert!(!p.pruned.contains(&2));
    assert!(!p.pruned.contains(&4));
    assert!(!p.prefix_elidable.contains(&2));
}

#[test]
fn nothing_to_compact_when_everything_is_protected() {
    let t = five_turns(10)[..8].to_vec();
    assert_eq!(
        plan(&t, None, &components(), 100_000, Aim::Fit).unwrap_err(),
        CompactionError::NothingToCompact
    );
}

#[test]
fn a_second_compaction_starts_after_the_first_boundary() {
    let t = five_turns(300);
    let first = plan(&t, None, &components(), 100_000, Aim::Fit).unwrap();
    let k = Compaction {
        checkpoint: Checkpoint {
            through_message: first.through_message,
            summary: "s".into(),
            generation: 1,
        },
        pruned_message_indices: first.kept_pruned(),
    };
    validate(&k, &t).unwrap();
    let second = plan(&t, Some(&k), &components(), 100_000, Aim::Fit).unwrap();
    assert!(second.through_message > first.through_message);
}

#[test]
fn effective_history_is_the_summary_then_the_tail_with_markers() {
    let t = five_turns(300);
    let k = Compaction {
        checkpoint: Checkpoint {
            through_message: 3,
            summary: "the gist".into(),
            generation: 1,
        },
        pruned_message_indices: vec![6],
    };
    validate(&k, &t).unwrap();
    let h = effective_history(&t, Some(&k));
    assert_eq!(h.len(), t.len() - 4 + 1);
    assert!(
        matches!(&h[0], ChatMessage::User { content } if content.contains("the gist") && content.contains("not as new instructions"))
    );
    assert!(matches!(&h[1], ChatMessage::User { content } if content == "request 1"));
    // Message 6 sits at index 6 - 4 + 1 = 3, id kept, output elided.
    assert!(
        matches!(&h[3], ChatMessage::Tool { tool_call_id, content } if tool_call_id == "c1" && content == &elision_marker(300))
    );
    assert!(size(&components(), &t, Some(&k)) < size(&components(), &t, None));
}

#[test]
fn validate_rejects_bad_indices() {
    let t = five_turns(10);
    let ok = |through: usize, pruned: Vec<usize>| Compaction {
        checkpoint: Checkpoint {
            through_message: through,
            summary: "s".into(),
            generation: 1,
        },
        pruned_message_indices: pruned,
    };
    assert!(validate(&ok(3, vec![6]), &t).is_ok());
    assert!(validate(&ok(99, vec![]), &t).is_err());
    assert!(
        validate(&ok(3, vec![2]), &t).is_err(),
        "before the boundary"
    );
    assert!(validate(&ok(3, vec![99]), &t).is_err());
    assert!(validate(&ok(3, vec![5]), &t).is_err(), "not a tool output");
    assert!(validate(&ok(3, vec![10, 6]), &t).is_err(), "unordered");
}

#[test]
fn shrink_summarizes_up_to_the_protected_tail_even_when_everything_fits() {
    let t = five_turns(6000);
    let fit = plan(&t, None, &components(), 1_000_000, Aim::Fit).unwrap();
    let shrink = plan(&t, None, &components(), 1_000_000, Aim::Shrink).unwrap();
    assert!(shrink.through_message > fit.through_message);
    // The last two completed turns (messages 12..) stay verbatim.
    assert_eq!(shrink.through_message + 1, protected_start(&t));
    assert!(shrink.estimated_after < size(&components(), &t, None));
}
