//! The message a wake posts to the cognition: one parseable first line
//! saying what the instinct noticed and where, the change in a fenced diff
//! block, and (for drift only) the intent it drifts from. The cognition
//! already has the conversation, so none of it is repeated.
//!
//! The shape is a contract with the panel, which styles the message:
//! - line 1 is exactly `Navigator noticed: <reason> (<score, 2 decimals>) in
//!   <path>:<line>`, on one line, the path without whitespace and the line
//!   always present (1-based, at least 1);
//! - the diff is the FIRST fenced block, opened with exactly ```` ```diff ````
//!   and closed with ```` ``` ````;
//! - the intent section, drift only (never for `stuck`), opens with `Intent on record:` alone
//!   on its line, after the diff block.

use std::path::Path;

use crate::navigator::ports::{Answer, Answers};

/// Lines of the diff kept in the message.
const DIFF_MAX_LINES: usize = 40;

/// The questions of the built-in wake branch, read to name the reason. The
/// pipeline's own branch conditions decide whether a wake happens at all.
const REASON_QUESTIONS: (&str, &str) = ("drift", "contradicts_conversation");

/// The built-in `stuck` question and the score above which its branch wakes
/// (first wake branch, so it names the reason whatever drift says).
const STUCK_QUESTION: &str = "stuck";
const STUCK_WAKE_THRESHOLD: f64 = 0.8;

/// Why the instinct woke the cognition, with the score that fired it.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum WakeReason {
    /// The driver looks stuck: the same errors persist, edits are undone and
    /// retried, or a comment shows frustration.
    Stuck(f64),
    /// The code drifts from the intent on record.
    Drift(f64),
    /// The code contradicts what was just agreed in the conversation.
    Contradiction(f64),
    /// A custom pipeline woke without asking either question: the highest
    /// `noul` score it did ask (0 when none), so the first line keeps its
    /// shape.
    Other(f64),
}

impl WakeReason {
    /// Picks the reason from the instinct's answers: a `stuck` score over
    /// the wake threshold first (its branch comes first); else of the `drift` question
    /// (when an intent exists) and the `contradicts_conversation` question
    /// (when a conversation exists), the higher score fired the branch.
    pub(super) fn from_answers(
        answers: Option<&Answers>,
        has_intent: bool,
        has_conversation: bool,
    ) -> Self {
        let score = |name: &str| match answers?.get(name)? {
            Answer::Noul { noul } => Some(*noul),
            _ => None,
        };
        if let Some(stuck) = score(STUCK_QUESTION).filter(|s| *s > STUCK_WAKE_THRESHOLD) {
            return Self::Stuck(stuck);
        }
        let drift = score(REASON_QUESTIONS.0).filter(|_| has_intent);
        let contradiction = score(REASON_QUESTIONS.1).filter(|_| has_conversation);
        match (drift, contradiction) {
            (Some(d), Some(c)) if c > d => Self::Contradiction(c),
            (Some(d), _) => Self::Drift(d),
            (None, Some(c)) => Self::Contradiction(c),
            (None, None) => Self::Other(
                answers
                    .into_iter()
                    .flat_map(|a| a.values())
                    .filter_map(|a| match a {
                        Answer::Noul { noul } => Some(*noul),
                        _ => None,
                    })
                    .fold(0.0, f64::max),
            ),
        }
    }

    fn sentence(&self) -> String {
        match self {
            Self::Stuck(score) => format!("you look stuck ({score:.2})"),
            Self::Drift(score) => format!("the code drifts from the intent ({score:.2})"),
            Self::Contradiction(score) => {
                format!("the code contradicts what was just agreed ({score:.2})")
            }
            Self::Other(score) => {
                format!("the change looks at odds with the intent or the conversation ({score:.2})")
            }
        }
    }
}

/// `uri` as a path relative to `workspace`; the uri itself when it is not a
/// file under it. Whitespace is percent-encoded so the path stays one token
/// of the wake's first line.
pub(super) fn relative_path(uri: &str, workspace: Option<&Path>) -> String {
    let path = crate::navigator::uri_to_path_str(uri)
        .and_then(|path| {
            let path = Path::new(&path);
            let rel = path.strip_prefix(workspace?).ok()?;
            Some(rel.display().to_string())
        })
        .unwrap_or_else(|| uri.to_string());
    path.chars()
        .map(|c| {
            if c.is_whitespace() {
                format!("%{:02X}", c as u32 & 0xFF)
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// The wake message. The first line is `Navigator noticed: <reason> in
/// <file>:<line>` and stays stable: the panel styles messages that start
/// with `Navigator noticed:`.
pub(super) fn wake_message(
    reason: &WakeReason,
    file: &str,
    line: usize,
    diff: &str,
    intent: &str,
) -> String {
    // A pure deletion has no inserted line to point at: line 1 stands in, so
    // the first line always ends in `:<line>`.
    let line = line.max(1);
    let mut out = format!(
        "Navigator noticed: {} in {file}:{line}\n",
        reason.sentence()
    );
    let diff = diff.trim_end();
    if !diff.is_empty() {
        out.push('\n');
        out.push_str(&fenced_diff(diff));
    }
    if matches!(reason, WakeReason::Drift(_)) && !intent.trim().is_empty() {
        out.push_str("\nIntent on record:\n");
        out.push_str(intent.trim());
        out.push('\n');
    }
    out
}

/// The diff in a ```` ```diff ```` block, capped at [`DIFF_MAX_LINES`]. The
/// fence is always exactly three backticks (the panel looks for that), so a
/// run of three backticks inside the diff is broken with a zero-width space:
/// a context line like ```` ```` ```` would otherwise close the block early.
fn fenced_diff(diff: &str) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    let shown = lines.len().min(DIFF_MAX_LINES);
    let body = lines[..shown].join("\n").replace("```", "``\u{200b}`");
    let mut out = format!("```diff\n{body}\n");
    if lines.len() > shown {
        out.push_str(&format!("… ({} more lines)\n", lines.len() - shown));
    }
    out.push_str("```\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_message_has_a_stable_first_line_a_fenced_diff_and_the_intent() {
        let message = wake_message(
            &WakeReason::Drift(0.84),
            "src/main.rs",
            12,
            "@@ -1 +1 @@\n-a_b\n+c_d\n",
            "add an auth remove command",
        );
        assert_eq!(
            message.lines().next().unwrap(),
            "Navigator noticed: the code drifts from the intent (0.84) in src/main.rs:12"
        );
        assert!(message.contains("```diff\n@@ -1 +1 @@\n-a_b\n+c_d\n```\n"));
        assert!(message.ends_with("Intent on record:\nadd an auth remove command\n"));
        assert!(!message.contains("Recent conversation"));
    }

    #[test]
    fn contradiction_message_omits_the_intent() {
        let message = wake_message(
            &WakeReason::Contradiction(0.81),
            "src/lib.rs",
            0,
            "+x\n",
            "some intent",
        );
        assert_eq!(
            message.lines().next().unwrap(),
            "Navigator noticed: the code contradicts what was just agreed (0.81) in src/lib.rs:1"
        );
        assert!(!message.contains("some intent"));
    }

    #[test]
    fn stuck_message_says_so_and_omits_the_intent() {
        let message = wake_message(
            &WakeReason::Stuck(0.87),
            "src/lib.rs",
            4,
            "+x\n",
            "some intent",
        );
        assert_eq!(
            message.lines().next().unwrap(),
            "Navigator noticed: you look stuck (0.87) in src/lib.rs:4"
        );
        assert!(!message.contains("Intent on record"));
        assert!(!message.contains("some intent"));
    }

    #[test]
    fn a_long_diff_is_capped_and_the_rest_counted() {
        let diff: String = (0..100).map(|i| format!("+line {i}\n")).collect();
        let message = wake_message(&WakeReason::Contradiction(0.9), "a.rs", 1, &diff, "");
        assert!(message.contains("+line 39\n"));
        assert!(!message.contains("+line 40\n"));
        assert!(message.contains("… (60 more lines)\n```\n"));
    }

    #[test]
    fn a_fence_inside_the_diff_cannot_close_the_block() {
        let message = wake_message(
            &WakeReason::Contradiction(0.9),
            "a.md",
            1,
            "@@ -1 +1 @@\n ```\n+```rust\n",
            "",
        );
        // Exactly one opening and one closing fence of three backticks.
        let fences = message.lines().filter(|l| l.starts_with("```")).count();
        assert_eq!(fences, 2, "{message}");
        assert!(message.contains("```diff\n@@ -1 +1 @@\n ``\u{200b}`\n+``\u{200b}`rust\n```\n"));
    }

    /// The contract with the panel: what it parses, pinned in one place.
    #[test]
    fn the_message_matches_the_panel_contract() {
        for (reason, intent_expected) in [
            (WakeReason::Drift(0.84), true),
            (WakeReason::Contradiction(0.81), false),
            (WakeReason::Other(0.0), false),
            (WakeReason::Stuck(0.87), false),
        ] {
            // A file with a space in its name, with and without a line.
            let file = relative_path("file:///ws/my%20dir/a.rs", Some(Path::new("/ws")));
            for line in [0usize, 7] {
                let message = wake_message(
                    &reason,
                    &file,
                    line,
                    "@@ -1,2 +1 @@\n a\n-b\n",
                    "ship the thing",
                );
                // Line 1: `Navigator noticed: <reason> (d.dd) in <path>:<line>`.
                let first = message.lines().next().unwrap();
                let rest = first.strip_prefix("Navigator noticed: ").expect(first);
                let (head, place) = rest.rsplit_once(" in ").expect(first);
                let (path, n) = place.rsplit_once(':').expect(first);
                assert_eq!(path, "my%20dir/a.rs", "{first}");
                assert!(!path.contains(char::is_whitespace));
                assert_eq!(n.parse::<usize>().unwrap(), line.max(1), "{first}");
                let score = head.rsplit_once(" (").expect(first).1;
                let score = score.strip_suffix(')').expect(first);
                assert_eq!(score.len(), 4, "two decimals: {first}");
                assert!(score.parse::<f64>().is_ok(), "{first}");

                // The diff is the first fenced block, opened with ```diff.
                let first_fence = message.lines().position(|l| l.starts_with("```")).unwrap();
                assert_eq!(message.lines().nth(first_fence).unwrap(), "```diff");
                assert_eq!(first_fence, 2, "blank line, then the block: {message}");

                // The intent header is alone on its line, after the diff.
                let header = message.lines().position(|l| l == "Intent on record:");
                assert_eq!(header.is_some(), intent_expected, "{message}");
                if let Some(header) = header {
                    assert!(header > first_fence);
                    assert_eq!(message.lines().nth(header + 1), Some("ship the thing"));
                }
            }
        }
    }

    #[test]
    fn the_reason_is_the_higher_gated_score() {
        let answers: Answers = [
            ("drift".to_string(), Answer::Noul { noul: 0.84 }),
            (
                "contradicts_conversation".to_string(),
                Answer::Noul { noul: 0.9 },
            ),
        ]
        .into();
        assert_eq!(
            WakeReason::from_answers(Some(&answers), true, true),
            WakeReason::Contradiction(0.9)
        );
        // Without a conversation only drift can have fired.
        assert_eq!(
            WakeReason::from_answers(Some(&answers), true, false),
            WakeReason::Drift(0.84)
        );
        assert_eq!(
            WakeReason::from_answers(None, true, true),
            WakeReason::Other(0.0)
        );
    }

    #[test]
    fn stuck_over_the_threshold_is_the_reason_whatever_drift_says() {
        let answers: Answers = [
            ("drift".to_string(), Answer::Noul { noul: 0.95 }),
            ("stuck".to_string(), Answer::Noul { noul: 0.87 }),
        ]
        .into();
        assert_eq!(
            WakeReason::from_answers(Some(&answers), true, false),
            WakeReason::Stuck(0.87)
        );
        let calm: Answers = [
            ("drift".to_string(), Answer::Noul { noul: 0.85 }),
            ("stuck".to_string(), Answer::Noul { noul: 0.8 }),
        ]
        .into();
        assert_eq!(
            WakeReason::from_answers(Some(&calm), true, false),
            WakeReason::Drift(0.85)
        );
    }

    #[test]
    fn paths_are_relative_to_the_workspace() {
        let ws = Path::new("/ws");
        assert_eq!(
            relative_path("file:///ws/src/main.rs", Some(ws)),
            "src/main.rs"
        );
        assert_eq!(
            relative_path("file:///other/a.rs", Some(ws)),
            "file:///other/a.rs"
        );
        assert_eq!(relative_path("untitled:1", None), "untitled:1");
        assert_eq!(
            relative_path("file:///ws/my%20dir/a.rs", Some(ws)),
            "my%20dir/a.rs",
            "whitespace is encoded: the path is one token"
        );
        assert_eq!(
            relative_path("file:///ws/my%20dir/a.rs", Some(ws)),
            "my%20dir/a.rs",
            "whitespace is encoded: the path is one token"
        );
    }
}
