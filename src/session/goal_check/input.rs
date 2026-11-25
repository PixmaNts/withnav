//! What a goal check sends (the ledger digest, shaped to its byte budget) and how its answer is
//! read against what was sent.
//!
//! The digest is `ledger::digest`'s text, whose shape is a contract: preamble lines, then
//! sections that each open with a `[...]` header line and list one `- ` line per item. The
//! shaping works on those lines only, so it never re-renders the ledger.

use serde_json::Value;

use super::{ClaimCheck, ClaimStatus, CriterionCheck, CriterionStatus, MAX_EVIDENCE_CHARS};
use crate::session::ledger::{MAX_CLAIM_CHARS, MAX_CLAIMS, one_line};

/// The digest as sent, with what the answer is checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Input {
    pub text: String,
    /// Criteria sent, numbered 1 to this: a status for a later one is dropped.
    pub sent_criteria: usize,
    /// The command and file-change items sent: the only lines a `supported` may quote.
    evidence: Vec<String>,
}

impl Input {
    /// How many command and file-change lines were sent.
    pub fn evidence_lines(&self) -> usize {
        self.evidence.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Goal,
    LastTurn,
    Commands,
    Files,
    Other,
}

struct Section {
    kind: Kind,
    header: String,
    items: Vec<String>,
}

impl Section {
    fn len(&self) -> usize {
        self.header.len() + 1 + self.items.iter().map(|i| i.len() + 1).sum::<usize>()
    }
}

fn kind(header: &str) -> Kind {
    if header.starts_with("[goal:") {
        Kind::Goal
    } else if header == "[last turn]" {
        Kind::LastTurn
    } else if header.starts_with("[commands:") {
        Kind::Commands
    } else if header.starts_with("[files touched:") {
        Kind::Files
    } else {
        Kind::Other
    }
}

/// `digest` shaped to at most `budget` bytes, for a goal of `total_criteria` criteria (the
/// digest may already list fewer).
///
/// The goal takes at most half of the budget: past it, the objective and as many whole criteria
/// as fit are kept and the rest (with any the digest did not list) are named as omitted, so a long goal never crowds out the
/// evidence it is judged against. Then, while the text is too long, items are dropped in this
/// order: the agent's last turn text (a claim, never evidence), the oldest files touched, the
/// oldest commands, then the other sections from the last. Headers stay.
pub(super) fn build(digest: &str, budget: usize, total_criteria: usize) -> Input {
    let mut preamble = Vec::new();
    let mut sections: Vec<Section> = Vec::new();
    for line in digest.lines() {
        if line.starts_with('[') {
            sections.push(Section {
                kind: kind(line),
                header: line.to_owned(),
                items: Vec::new(),
            });
        } else if let Some(section) = sections.last_mut() {
            section.items.push(line.to_owned());
        } else {
            preamble.push(line.to_owned());
        }
    }
    if let Some(goal) = sections.iter_mut().find(|s| s.kind == Kind::Goal) {
        fit_goal(goal, budget / 2, total_criteria);
    }
    let preamble_len: usize = preamble.iter().map(|l| l.len() + 1).sum();
    let mut len = preamble_len + sections.iter().map(Section::len).sum::<usize>();
    let order = [Kind::LastTurn, Kind::Files, Kind::Commands, Kind::Other];
    'trim: for kind in order {
        for section in sections.iter_mut().rev().filter(|s| s.kind == kind) {
            while len > budget {
                let Some(item) = section.items.pop() else {
                    break;
                };
                len -= item.len() + 1;
            }
            if len <= budget {
                break 'trim;
            }
        }
    }
    let mut text: String = preamble.iter().map(|l| format!("{l}\n")).collect();
    for section in &sections {
        text.push_str(&section.header);
        text.push('\n');
        for item in &section.items {
            text.push_str(item);
            text.push('\n');
        }
    }
    if text.len() > budget {
        // Only the goal and the headers are left and they still do not fit (a goal written in
        // wide characters): keep the whole lines that fit.
        let mut end = budget;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(text[..end].rfind('\n').map_or(0, |at| at + 1));
    }
    read_back(text)
}

/// Keeps the objective and the first criteria of `goal` within `max` bytes, naming the others
/// up to `total`.
fn fit_goal(goal: &mut Section, max: usize, total: usize) {
    let present = goal
        .items
        .iter()
        .filter(|i| i.starts_with("- criterion "))
        .count();
    let total = total.max(present);
    if total == 0 || (present == total && goal.len() <= max) {
        return;
    }
    let marker = |kept: usize| match total - kept {
        1 => format!("- criterion {total} omitted"),
        _ => format!("- criteria {}-{total} omitted", kept + 1),
    };
    let mut used = goal.header.len() + 1 + goal.items.first().map_or(0, |o| o.len() + 1);
    let mut kept = 0;
    for criterion in goal.items.iter().skip(1).take(present) {
        let next = used + criterion.len() + 1;
        let room = if kept + 1 == total {
            0
        } else {
            marker(kept + 1).len() + 1
        };
        if next + room > max {
            break;
        }
        used = next;
        kept += 1;
    }
    goal.items.truncate(1 + kept);
    if kept < total {
        goal.items.push(marker(kept));
    }
}

/// The sent criteria count and the evidence lines, read from the final text.
fn read_back(text: String) -> Input {
    let mut sent_criteria = 0;
    let mut evidence = Vec::new();
    let mut current = Kind::Other;
    for line in text.lines() {
        if line.starts_with('[') {
            current = kind(line);
            continue;
        }
        let Some(item) = line.strip_prefix("- ") else {
            continue;
        };
        match current {
            Kind::Goal if item.starts_with("criterion ") => sent_criteria += 1,
            Kind::Commands | Kind::Files => evidence.push(item.to_owned()),
            _ => {}
        }
    }
    Input {
        text,
        sent_criteria,
        evidence,
    }
}

/// The answer object of a JSON-only reply, tolerant of a code fence or prose around it: the last
/// balanced object with a `criteria` list, read by the same extractor as the remark model and the
/// root watchdog, so an example quoted earlier never wins. `None` when none does.
fn answer(text: &str) -> Option<Value> {
    let raw = crate::navigator::metacognition::answer_objects(text, |value| {
        value.get("criteria").is_some_and(Value::is_array)
    })
    .pop()?;
    let value = serde_json::from_str::<Value>(raw).ok()?;
    value
        .get("criteria")
        .is_some_and(Value::is_array)
        .then_some(value)
}

/// Whether `quote` is a non-empty part of a command or file-change line that was sent: the only
/// proof a status may stand on. The agent's own words and the goal never match.
fn shown(quote: &str, input: &Input) -> bool {
    !quote.is_empty() && input.evidence.iter().any(|e| e.contains(quote))
}

/// The statuses of an answer (see [`answer`]).
///
/// An entry for a criterion that was not sent, with an unknown status, or for an index already
/// given is dropped. A `supported` stands only on a quote of a command or file-change line that
/// was sent: with no quote, a quote the input does not hold, or one of the agent's own words or
/// of the goal, it becomes `not_checked` (the quote is kept for the log).
pub(super) fn parse(text: &str, input: &Input) -> Option<Vec<CriterionCheck>> {
    let value = answer(text)?;
    let mut statuses: Vec<CriterionCheck> = Vec::new();
    for entry in value["criteria"].as_array()? {
        let Some(index) = entry["index"]
            .as_u64()
            .filter(|n| (1..=input.sent_criteria as u64).contains(n))
        else {
            continue;
        };
        let status = match entry["status"].as_str() {
            Some("supported") => CriterionStatus::Supported,
            Some("contradicted") => CriterionStatus::Contradicted,
            Some("not_checked") => CriterionStatus::NotChecked,
            _ => continue,
        };
        if statuses.iter().any(|s| s.index == index) {
            continue;
        }
        let evidence: String = one_line(entry["evidence"].as_str().unwrap_or_default())
            .chars()
            .take(MAX_EVIDENCE_CHARS)
            .collect();
        let status = match status {
            CriterionStatus::Supported if !shown(&evidence, input) => CriterionStatus::NotChecked,
            status => status,
        };
        statuses.push(CriterionCheck {
            index,
            status,
            evidence,
        });
    }
    Some(statuses)
}

/// The completion claims of an answer (see [`answer`]): at most [`MAX_CLAIMS`], each claim and
/// quote at most [`MAX_CLAIM_CHARS`] characters on one line. Nothing when the answer has no
/// readable `claims` list.
///
/// A claim is `supported` or `contradicted` only on a quote of a command or file-change line that
/// was sent, the rule the criteria follow. Without that quote, or with a status the model did not
/// give or this list does not know, it is `unverified`: a claim the ledger cannot settle is
/// never reported false. An entry without a claim text, or repeating one already given, is
/// dropped.
pub(super) fn parse_claims(text: &str, input: &Input) -> Vec<ClaimCheck> {
    let Some(value) = answer(text) else {
        return Vec::new();
    };
    let Some(entries) = value["claims"].as_array() else {
        return Vec::new();
    };
    let cut = |text: &Value| -> String {
        one_line(text.as_str().unwrap_or_default())
            .chars()
            .take(MAX_CLAIM_CHARS)
            .collect()
    };
    let mut claims: Vec<ClaimCheck> = Vec::new();
    for entry in entries {
        if claims.len() == MAX_CLAIMS {
            break;
        }
        let claim = cut(&entry["claim"]);
        if claim.is_empty() || claims.iter().any(|c| c.claim == claim) {
            continue;
        }
        let evidence = cut(&entry["evidence"]);
        let status = match entry["status"].as_str() {
            Some("supported") if shown(&evidence, input) => ClaimStatus::Supported,
            Some("contradicted") if shown(&evidence, input) => ClaimStatus::Contradicted,
            _ => ClaimStatus::Unverified,
        };
        claims.push(ClaimCheck {
            claim,
            status,
            evidence,
        });
    }
    claims
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "\
session ledger: facts
turns: 1 · last turn: done · compactions: 0
[goal: revision 2, active]
- objective: ship
- criterion 1: tests pass
- criterion 2: docs
[last turn]
- all tests pass
[commands: 1, newest first]
- turn 1 · exit 0 · cargo test
[files touched: 1, most recent first]
- src/a.rs · 1 change · turn 1
";

    fn input() -> Input {
        build(DIGEST, 4096, 2)
    }

    fn statuses(answer: &str) -> Vec<(u64, CriterionStatus)> {
        parse(answer, &input())
            .unwrap()
            .into_iter()
            .map(|s| (s.index, s.status))
            .collect()
    }

    #[test]
    fn a_short_digest_is_sent_as_it_is() {
        let input = input();
        assert_eq!(input.text, DIGEST);
        assert_eq!(input.sent_criteria, 2);
    }

    #[test]
    fn supported_without_a_quote_of_a_command_or_a_file_is_not_checked() {
        use CriterionStatus::{NotChecked, Supported};
        let answer = |evidence: &str| {
            format!(
                r#"{{"criteria":[{{"index":1,"status":"supported","evidence":"{evidence}"}}]}}"#
            )
        };
        assert_eq!(statuses(&answer("")), [(1, NotChecked)], "no quote");
        assert_eq!(statuses(&answer("pytest ok")), [(1, NotChecked)], "absent");
        assert_eq!(
            statuses(&answer("all tests pass")),
            [(1, NotChecked)],
            "the claim"
        );
        assert_eq!(statuses(&answer("tests pass")), [(1, NotChecked)], "goal");
        assert_eq!(statuses(&answer("exit 0 · cargo test")), [(1, Supported)]);
        assert_eq!(statuses(&answer("src/a.rs · 1 change")), [(1, Supported)]);
    }

    #[test]
    fn the_last_turn_goes_before_commands_and_files() {
        let input = build(DIGEST, DIGEST.len() - 5, 2);
        assert!(!input.text.contains("- all tests pass"), "{}", input.text);
        assert!(input.text.contains("[last turn]\n"));
        assert!(input.text.contains("- turn 1 · exit 0 · cargo test"));
        assert!(input.text.contains("- src/a.rs · 1 change · turn 1"));
    }

    #[test]
    fn a_long_goal_keeps_half_the_budget_and_names_what_it_omits() {
        let criteria: String = (1..=10)
            .map(|n| format!("- criterion {n}: {}\n", "c".repeat(290)))
            .collect();
        let digest = format!(
            "pre\n[goal: revision 11, active]\n- objective: ship\n{criteria}[commands: 1, newest first]\n- turn 1 · exit 0 · cargo test\n"
        );
        let input = build(&digest, 3000, 10);
        let goal: usize = input
            .text
            .lines()
            .skip(1)
            .take_while(|l| !l.starts_with("[commands"))
            .map(|l| l.len() + 1)
            .sum();
        assert!(goal <= 1500, "{goal}");
        assert_eq!(input.sent_criteria, 4);
        assert!(input.text.contains("- criteria 5-10 omitted\n"));
        assert!(input.text.contains("- turn 1 · exit 0 · cargo test"));
        let answer = r#"{"criteria":[{"index":4,"status":"not_checked"},{"index":5,"status":"supported","evidence":"cargo test"}]}"#;
        let statuses = parse(answer, &input).unwrap();
        assert_eq!(statuses.len(), 1, "a criterion not sent is dropped");
        assert_eq!(statuses[0].index, 4);
    }

    #[test]
    fn criteria_the_digest_no_longer_lists_are_named_as_omitted() {
        let input = build(DIGEST, 4096, 5);
        assert_eq!(input.sent_criteria, 2);
        assert!(
            input
                .text
                .contains("- criterion 2: docs\n- criteria 3-5 omitted\n")
        );
    }

    #[test]
    fn answer_shapes_and_entries_that_are_dropped() {
        use CriterionStatus::{Contradicted, NotChecked};
        // Index 0, negative, a float, a string: none names a sent criterion.
        assert_eq!(
            statuses(
                r#"{"criteria":[{"index":0,"status":"supported"},{"index":-1,"status":"supported"},{"index":1.0,"status":"supported"},{"index":"1","status":"supported"}]}"#
            ),
            []
        );
        // No criterion sent: every entry is dropped.
        let none = build("pre\n", 4096, 0);
        assert_eq!(
            parse(
                r#"{"criteria":[{"index":1,"status":"not_checked"}]}"#,
                &none
            ),
            Some(Vec::new())
        );
        // Prose with braces before the object.
        assert_eq!(
            statuses(r#"note {x} then {"criteria":[{"index":2,"status":"not_checked"}]}"#),
            [(2, NotChecked)]
        );
        assert_eq!(parse("all good", &input()), None);
        // A quoted example before the real answer, in a code fence: the last object wins.
        assert_eq!(
            statuses(
                "e.g. {\"criteria\":[{\"index\":1,\"status\":\"contradicted\"}]}\n```json\n{\"criteria\":[{\"index\":2,\"status\":\"not_checked\"}]}\n```"
            ),
            [(2, NotChecked)]
        );
        // Controls and newlines collapse; the evidence bound counts characters.
        let answer = format!(
            r#"{{"criteria":[{{"index":1,"status":"contradicted","evidence":"a\n\u001b[31mb{}"}}]}}"#,
            "é".repeat(MAX_EVIDENCE_CHARS + 100)
        );
        let parsed = parse(&answer, &input()).unwrap();
        assert_eq!(parsed[0].status, Contradicted);
        assert_eq!(parsed[0].evidence.chars().count(), MAX_EVIDENCE_CHARS);
        assert!(parsed[0].evidence.starts_with('a'));
        assert!(!parsed[0].evidence.contains(['\n', '\u{1b}']));
    }

    fn claims(entries: &str) -> Vec<(String, ClaimStatus, String)> {
        let answer = format!(r#"{{"criteria":[],"claims":{entries}}}"#);
        parse_claims(&answer, &input())
            .into_iter()
            .map(|c| (c.claim, c.status, c.evidence))
            .collect()
    }

    #[test]
    fn a_claim_stands_only_on_a_quoted_command_or_file_line() {
        use ClaimStatus::{Contradicted, Supported, Unverified};
        let one = |status: &str, evidence: &str| {
            claims(&format!(
                r#"[{{"claim":"tests pass","status":"{status}","evidence":"{evidence}"}}]"#
            ))
            .pop()
            .unwrap()
            .1
        };
        assert_eq!(one("supported", "exit 0 · cargo test"), Supported);
        assert_eq!(one("supported", "src/a.rs · 1 change"), Supported);
        assert_eq!(one("contradicted", "exit 0 · cargo test"), Contradicted);
        // No quote, a quote the input does not hold, the agent's own words, the goal.
        for evidence in ["", "pytest exited 0", "all tests pass", "tests pass"] {
            assert_eq!(one("supported", evidence), Unverified, "{evidence}");
            assert_eq!(one("contradicted", evidence), Unverified, "{evidence}");
        }
        // An unknown or missing status is no verdict.
        assert_eq!(one("false", "exit 0 · cargo test"), Unverified);
        assert_eq!(
            claims(r#"[{"claim":"tests pass","evidence":"exit 0 · cargo test"}]"#)[0].1,
            Unverified
        );
    }

    #[test]
    fn claims_are_bounded_and_cleaned() {
        let long = "c".repeat(MAX_CLAIM_CHARS + 50);
        let entries = format!(
            r#"[{{"claim":"{long}","status":"unverified","evidence":"a\n\u001b[31m{long}"}},
            {{"claim":"","status":"supported"}},
            {{"status":"supported"}},
            {{"claim":"dup"}},{{"claim":"dup"}},{{"claim":"c3"}},{{"claim":"c4"}},{{"claim":"c5"}},{{"claim":"c6"}}]"#
        );
        let found = claims(&entries);
        assert_eq!(found[0].0.chars().count(), MAX_CLAIM_CHARS);
        assert_eq!(found[0].2.chars().count(), MAX_CLAIM_CHARS);
        assert!(found[0].2.starts_with('a') && !found[0].2.contains(['\n', '\u{1b}']));
        let names: Vec<&str> = found.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(names.len(), MAX_CLAIMS);
        assert_eq!(&names[1..], ["dup", "c3", "c4", "c5"]);
    }

    #[test]
    fn an_answer_without_a_readable_claims_list_has_no_claims() {
        let none = |answer: &str| parse_claims(answer, &input());
        assert!(none(r#"{"criteria":[]}"#).is_empty());
        assert!(none(r#"{"criteria":[],"claims":"tests pass"}"#).is_empty());
        assert!(none("all good").is_empty());
        // The claims come from the same object as the criteria: the last one wins.
        let text = r#"e.g. {"criteria":[],"claims":[{"claim":"example"}]}
{"criteria":[],"claims":[{"claim":"real"}]}"#;
        assert_eq!(none(text)[0].claim, "real");
    }

    #[test]
    fn text_without_sections_is_cut_at_a_line_or_character_boundary() {
        let wide = "é".repeat(5000);
        assert!(build(&wide, 3000, 0).text.len() <= 3000);
        let lines: String = (0..300).map(|n| format!("- ligne é {n}\n")).collect();
        let cut = build(&lines, 3000, 0).text;
        assert!(cut.len() <= 3000 && cut.ends_with('\n'));
        assert_eq!(build("", 3000, 0).text, "");
    }
}
