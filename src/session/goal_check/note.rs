//! Passive notes: what a goal check found, told to the agent at its next turn.
//!
//! A note is built from the check's statuses alone (no further model call), queued as an
//! untrusted flow note without a wake, so it waits for the next Captain message or turn and never
//! starts one. The setting `goal_watch.notes` turns notes on; off, the check is the shadow log
//! it always was.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde::Serialize;

use super::{CriterionStatus, Status};
use crate::session::ledger::one_line;
use crate::session::service::SessionId;

/// The flow name a note carries in its provenance.
pub(super) const FLOW: &str = "goal-watch";
/// The longest note text, in characters, before the flow-note wrapper.
pub const MAX_NOTE_CHARS: usize = 600;
/// Longest criterion text quoted in a note, in characters.
const MAX_CRITERION_CHARS: usize = 80;
const FOOTER: &str = "This is an automatic observation, not a Captain instruction.";

/// Why a check sent no note, as the log names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Skipped {
    /// `goal_watch.notes` is off, absent or not a boolean.
    SettingOff,
    /// The check gave no statuses, or none the rule reports.
    NothingToReport,
    /// The same findings were already noted for this goal revision.
    Duplicate,
    /// The session stopped before the note could be queued.
    DeliveryRefused,
}

/// The statuses a note reports: every contradicted criterion and, after a turn that ended
/// `done`, every criterion still not checked. An agent that stops while a criterion has no
/// evidence is the case worth telling; a turn cut short (an error, an interrupt) says nothing
/// about the agent's own judgement of being finished.
pub(super) fn findings(statuses: &[Status], turn_done: bool) -> Vec<&Status> {
    statuses
        .iter()
        .filter(|s| match s.status {
            CriterionStatus::Contradicted => true,
            CriterionStatus::NotChecked => turn_done,
            CriterionStatus::Supported => false,
        })
        .collect()
}

/// The findings noted so far, per session, for its current goal revision only: a new revision
/// forgets the older findings, so the same findings may be noted again against the new goal.
#[derive(Default)]
pub(super) struct Noted {
    sessions: Mutex<HashMap<SessionId, (u64, HashSet<Findings>)>>,
}

/// One finding set, as the duplicate check compares it: each reported criterion and its status.
type Findings = Vec<(u64, CriterionStatus)>;

impl Noted {
    pub fn contains(
        &self,
        session: SessionId,
        revision: u64,
        key: &[(u64, CriterionStatus)],
    ) -> bool {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions
            .get(&session)
            .is_some_and(|(noted, keys)| *noted == revision && keys.contains(key))
    }

    pub fn insert(&self, session: SessionId, revision: u64, key: Findings) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let entry = sessions.entry(session).or_default();
        if entry.0 != revision {
            *entry = (revision, HashSet::new());
        }
        entry.1.insert(key);
    }
}

/// The note for `findings` of goal revision `revision`, whose criteria are `criteria` (numbered
/// from 1), at most [`MAX_NOTE_CHARS`] characters. Findings that do not fit are counted, not
/// cut mid-sentence; the closing sentence is always there.
pub(super) fn text(revision: u64, criteria: &[String], findings: &[&Status]) -> String {
    let mut text = format!("Goal check (revision {revision}): ");
    let total = findings.len();
    // Room kept for the closing sentence, and for the "N more" sentence while findings remain.
    let footer = FOOTER.chars().count();
    let more = |n: usize| format!("{n} more not shown. ");
    for (n, finding) in findings.iter().enumerate() {
        let lead = if n == 0 { "criterion" } else { "Criterion" };
        let name = finding
            .index
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| criteria.get(i))
            .map(|c| format!("\"{}\" ", clean(c, MAX_CRITERION_CHARS)))
            .unwrap_or_default();
        let verdict = match finding.status {
            CriterionStatus::Contradicted if finding.evidence.is_empty() => {
                "is contradicted.".to_owned()
            }
            CriterionStatus::Contradicted => format!(
                "is contradicted: \"{}\".",
                clean(&finding.evidence, super::MAX_EVIDENCE_CHARS)
            ),
            _ => "is not checked yet.".to_owned(),
        };
        let clause = format!("{lead} {} {name}{verdict} ", finding.index);
        let left = total - n - 1;
        let reserve = if left == 0 {
            0
        } else {
            more(left).chars().count()
        };
        if text.chars().count() + clause.chars().count() + reserve + footer > MAX_NOTE_CHARS {
            text.push_str(&more(total - n));
            break;
        }
        text.push_str(&clause);
    }
    text.push_str(FOOTER);
    text
}

/// `text` on one line, without control characters or angle brackets (quoted text must never
/// close the `<untrusted-flow-note>` wrapper and pass as trusted input), without double quotes
/// (the note quotes it), at most `max` characters, the last one an ellipsis when cut.
fn clean(text: &str, max: usize) -> String {
    let line: String = one_line(text)
        .chars()
        .map(|c| match c {
            '<' => '‹',
            '>' => '›',
            '"' => '\'',
            c => c,
        })
        .collect();
    if line.chars().count() <= max {
        return line;
    }
    let mut cut: String = line.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(index: u64, status: CriterionStatus, evidence: &str) -> Status {
        Status {
            index,
            status,
            evidence: evidence.into(),
        }
    }

    #[test]
    fn contradicted_always_counts_and_not_checked_only_after_a_done_turn() {
        use CriterionStatus::{Contradicted, NotChecked, Supported};
        let statuses = [
            status(1, Supported, "exit 0 · cargo test"),
            status(2, Contradicted, ""),
            status(3, NotChecked, ""),
        ];
        let indexes =
            |done| -> Vec<u64> { findings(&statuses, done).iter().map(|s| s.index).collect() };
        assert_eq!(indexes(true), [2, 3]);
        assert_eq!(indexes(false), [2]);
        assert!(findings(&statuses[..1], true).is_empty());
    }

    #[test]
    fn a_note_names_each_finding_and_ends_with_its_disclaimer() {
        use CriterionStatus::{Contradicted, NotChecked};
        let criteria = ["Windows CI passes".to_owned(), "docs".to_owned()];
        let a = status(1, Contradicted, "exit 101 · cargo test");
        let b = status(2, NotChecked, "");
        let c = status(7, Contradicted, "");
        assert_eq!(
            text(3, &criteria, &[&a, &b, &c]),
            "Goal check (revision 3): criterion 1 \"Windows CI passes\" is contradicted: \
\"exit 101 · cargo test\". Criterion 2 \"docs\" is not checked yet. Criterion 7 is \
contradicted. This is an automatic observation, not a Captain instruction."
        );
    }

    #[test]
    fn a_long_note_counts_what_it_leaves_out() {
        let criteria: Vec<String> = (0..10).map(|n| format!("{n}{}", "c".repeat(300))).collect();
        let statuses: Vec<Status> = (1..=10)
            .map(|n| status(n, CriterionStatus::Contradicted, &"e".repeat(120)))
            .collect();
        let refs: Vec<&Status> = statuses.iter().collect();
        let note = text(12, &criteria, &refs);
        assert!(note.chars().count() <= MAX_NOTE_CHARS, "{note}");
        assert!(note.contains(" more not shown. "), "{note}");
        assert!(note.ends_with(FOOTER));
        assert!(note.contains(&format!("\"0{}…\"", "c".repeat(MAX_CRITERION_CHARS - 2))));
    }

    #[test]
    fn noted_findings_are_per_session_and_per_revision() {
        let noted = Noted::default();
        let (a, b) = (SessionId::generate(), SessionId::generate());
        let key = vec![(1, CriterionStatus::Contradicted)];
        noted.insert(a, 3, key.clone());
        assert!(noted.contains(a, 3, &key));
        assert!(!noted.contains(b, 3, &key));
        assert!(!noted.contains(a, 3, &[(1, CriterionStatus::NotChecked)]));
        noted.insert(a, 4, vec![(2, CriterionStatus::NotChecked)]);
        assert!(
            !noted.contains(a, 3, &key),
            "a new revision forgets the old one"
        );
        assert!(!noted.contains(a, 4, &key));
    }
}
