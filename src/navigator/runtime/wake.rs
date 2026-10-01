//! The message a wake posts to the cognition: one parseable first line
//! saying what the instinct noticed and where, the change in a fenced diff
//! block, and (for drift only) the intent it drifts from. The cognition
//! already has the conversation, so none of it is repeated.

use std::path::Path;

use crate::navigator::ports::{Answer, Answers};

/// Lines of the diff kept in the message.
const DIFF_MAX_LINES: usize = 40;

/// The questions of the built-in wake branch, read to name the reason. The
/// pipeline's own branch conditions decide whether a wake happens at all.
const REASON_QUESTIONS: (&str, &str) = ("drift", "contradicts_conversation");

/// Why the instinct woke the cognition, with the score that fired it.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum WakeReason {
    /// The code drifts from the intent on record.
    Drift(f64),
    /// The code contradicts what was just agreed in the conversation.
    Contradiction(f64),
    /// A custom pipeline woke without asking either question.
    Other,
}

impl WakeReason {
    /// Picks the reason from the instinct's answers: of the `drift` question
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
        let drift = score(REASON_QUESTIONS.0).filter(|_| has_intent);
        let contradiction = score(REASON_QUESTIONS.1).filter(|_| has_conversation);
        match (drift, contradiction) {
            (Some(d), Some(c)) if c > d => Self::Contradiction(c),
            (Some(d), _) => Self::Drift(d),
            (None, Some(c)) => Self::Contradiction(c),
            (None, None) => Self::Other,
        }
    }

    fn sentence(&self) -> String {
        match self {
            Self::Drift(score) => format!("the code drifts from the intent ({score:.2})"),
            Self::Contradiction(score) => {
                format!("the code contradicts what was just agreed ({score:.2})")
            }
            Self::Other => "the change looks at odds with the intent or the conversation".into(),
        }
    }
}

/// `uri` as a path relative to `workspace`; the uri itself when it is not a
/// file under it.
pub(super) fn relative_path(uri: &str, workspace: Option<&Path>) -> String {
    crate::navigator::uri_to_path_str(uri)
        .and_then(|path| {
            let path = Path::new(&path);
            let rel = path.strip_prefix(workspace?).ok()?;
            Some(rel.display().to_string())
        })
        .unwrap_or_else(|| uri.to_string())
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
    let place = if line > 0 {
        format!("{file}:{line}")
    } else {
        file.to_string()
    };
    let mut out = format!("Navigator noticed: {} in {place}\n", reason.sentence());
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

/// The diff in a ```diff block, capped at [`DIFF_MAX_LINES`]. The fence is
/// longer than any backtick run inside, so the diff cannot close it.
fn fenced_diff(diff: &str) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    let shown = lines.len().min(DIFF_MAX_LINES);
    let longest_run = diff
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat(longest_run.max(2) + 1);
    let mut out = format!("{fence}diff\n{}\n", lines[..shown].join("\n"));
    if lines.len() > shown {
        out.push_str(&format!("… ({} more lines)\n", lines.len() - shown));
    }
    out.push_str(&fence);
    out.push('\n');
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
            "Navigator noticed: the code contradicts what was just agreed (0.81) in src/lib.rs"
        );
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
        let message = wake_message(&WakeReason::Contradiction(0.9), "a.md", 1, "+```rust\n", "");
        assert!(message.contains("````diff\n+```rust\n````\n"));
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
            WakeReason::Other
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
    }
}
