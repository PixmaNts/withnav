//! The replay report: per-action counts, changed and not-comparable triggers,
//! and its plain-text rendering.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use crate::navigator::pipeline::Action;
use crate::navigator::ports::{Answer, Answers};

/// Per-action counts, historical and candidate over the same comparable
/// triggers.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ActionCounts {
    pub silent: usize,
    pub remark: usize,
    pub wake_cognition: usize,
    /// An action this build does not know, or a record that requested
    /// nothing: counted apart from the silence it looks like.
    pub unknown: usize,
}

impl ActionCounts {
    pub(super) fn add(&mut self, action: Option<Action>) {
        match action {
            Some(Action::Silent) => self.silent += 1,
            Some(Action::Remark) => self.remark += 1,
            Some(Action::WakeCognition) => self.wake_cognition += 1,
            None => self.unknown += 1,
        }
    }

    /// The counts of every action bucket, compared and candidate alike.
    pub fn total(&self) -> usize {
        self.silent + self.remark + self.wake_cognition + self.unknown
    }

    /// Rows in report order: `(label, historical, candidate)`.
    fn rows(&self, other: &Self) -> [(&'static str, usize, usize); 4] {
        [
            ("silent", self.silent, other.silent),
            ("remark", self.remark, other.remark),
            ("wake_cognition", self.wake_cognition, other.wake_cognition),
            ("unknown", self.unknown, other.unknown),
        ]
    }
}

/// A trigger the candidate cannot be compared with, and why.
#[derive(Debug, Clone, Serialize)]
pub struct NotComparable {
    pub trigger_id: String,
    pub event: String,
    pub ts: Option<String>,
    pub uri: String,
    /// `no_trigger_for_event`, `new_question:<name>`,
    /// `changed_question:<name>`, `unanswered:<name>`,
    /// `no_logged_questions`, `no_template_inputs`, `reask_failed: ...`,
    /// `branch_failed: ...` (several may be joined by a comma).
    pub reason: String,
}

/// A trigger whose candidate action differs from the historical one.
#[derive(Debug, Clone, Serialize)]
pub struct ChangedDecision {
    pub trigger_id: String,
    pub event: String,
    pub ts: Option<String>,
    pub uri: String,
    /// `silent` | `remark` | `wake_cognition` | `unknown`.
    pub historical: &'static str,
    pub candidate: &'static str,
    /// The candidate branch that matched, if any.
    pub branch: Option<usize>,
    /// Whether the instinct had to be asked again for this trigger.
    pub reasked: bool,
    /// The driver's verdict on the historical remark, when it has one.
    pub verdict: Option<String>,
    /// The answers the fired branch reads: the scores that decided it.
    pub fired_answers: Answers,
}

/// One compared trigger with the answers the candidate used for it.
#[derive(Debug, Clone, Serialize)]
pub struct ComparedTrigger {
    pub trigger_id: String,
    pub ts: Option<String>,
    pub uri: String,
    pub historical: &'static str,
    pub candidate: &'static str,
    pub branch: Option<usize>,
    /// Whether the answers come from a fresh model call (else the log).
    pub reasked: bool,
    pub answers: Answers,
}

/// The whole replay: what was read, what was compared, and what it changes.
#[derive(Debug, Clone, Serialize)]
pub struct ReplayReport {
    pub log_path: String,
    /// Where the candidate pipeline came from.
    pub pipeline: String,
    pub pipeline_hash: String,
    /// Whether a changed question re-asks the instinct.
    pub reask: bool,
    pub log_lines: usize,
    pub triggers: usize,
    pub verdicts: usize,
    pub skipped_records: usize,
    pub skipped_by_reason: BTreeMap<String, usize>,
    /// Triggers whose candidate action could be computed.
    pub compared: usize,
    pub not_comparable: Vec<NotComparable>,
    /// Of those, how many had to be asked again.
    pub reasked: usize,
    pub historical: ActionCounts,
    pub candidate: ActionCounts,
    pub changed: Vec<ChangedDecision>,
    /// Every compared trigger with its answers, changed or not.
    pub compared_triggers: Vec<ComparedTrigger>,
    /// Historical remarks rated `noise` that the candidate would not have
    /// asked for.
    pub noise_avoided: usize,
    /// Historical remarks rated `useful` that the candidate would not have
    /// asked for.
    pub useful_lost: usize,
    /// Rated `useful` remarks the candidate still asks for.
    pub useful_kept: usize,
    /// Rated `noise` remarks the candidate still asks for.
    pub noise_kept: usize,
    /// Of the useful ones (lost + kept), how many were `followed`: cleared
    /// because the driver edited right next to them, not rated by hand.
    pub followed: usize,
    /// Compared triggers whose historical remark has no `useful`/`noise`
    /// verdict (none, `dismissed`, or no remark at all).
    pub unknown_verdicts: usize,
}

impl ReplayReport {
    /// The header line's log path.
    pub fn with_log_path(mut self, path: &Path) -> Self {
        self.log_path = path.display().to_string();
        self
    }

    /// Overrides the pipeline origin with the path the driver pointed at
    /// (the snapshot only knows `project` / `user` / `builtin`).
    pub fn with_pipeline_origin(mut self, origin: &str) -> Self {
        self.pipeline = origin.to_string();
        self
    }

    /// Triggers whose historical remark was rated, one way or the other.
    pub fn rated(&self) -> usize {
        self.noise_avoided + self.useful_lost + self.useful_kept + self.noise_kept
    }

    /// The plain-text report: a per-action table, the verdict join, and the
    /// changed and not-comparable triggers (truncated at `limit`; the JSON
    /// report carries all of them).
    pub fn render_text(&self, limit: usize) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "frozen-trigger replay");
        let _ = writeln!(
            out,
            "  log       {}",
            if self.log_path.is_empty() {
                "(in memory)"
            } else {
                &self.log_path
            }
        );
        let _ = writeln!(
            out,
            "  pipeline  {} ({})",
            self.pipeline, self.pipeline_hash
        );
        let _ = writeln!(out, "  reask     {}", if self.reask { "yes" } else { "no" });
        let _ = writeln!(
            out,
            "  records   {} lines, {} triggers, {} verdicts, {} skipped{}",
            self.log_lines,
            self.triggers,
            self.verdicts,
            self.skipped_records,
            skipped_summary(&self.skipped_by_reason)
        );
        out.push('\n');

        let _ = writeln!(
            out,
            "{:<16}{:>11}{:>11}{:>8}",
            "action", "historical", "candidate", "delta"
        );
        for (label, historical, candidate) in self.historical.rows(&self.candidate) {
            let delta = candidate as i64 - historical as i64;
            let _ = writeln!(
                out,
                "{:<16}{:>11}{:>11}{:>8}",
                label,
                historical,
                candidate,
                format_delta(delta)
            );
        }
        let _ = writeln!(
            out,
            "{:<16}{:>11}{:>11}",
            "total",
            self.historical.total(),
            self.candidate.total()
        );
        out.push('\n');

        let _ = writeln!(
            out,
            "verdicts      rated {}, noise avoided {}, useful lost {}, useful kept {}, noise kept {}, unknown {} (followed {} counted as useful)",
            self.rated(),
            self.noise_avoided,
            self.useful_lost,
            self.useful_kept,
            self.noise_kept,
            self.unknown_verdicts,
            self.followed
        );
        let _ = writeln!(
            out,
            "comparability {} compared, {} not comparable, {} re-asked",
            self.compared,
            self.not_comparable.len(),
            self.reasked
        );
        out.push('\n');

        let _ = writeln!(out, "changed decisions ({})", self.changed.len());
        for change in self.changed.iter().take(limit) {
            let branch = change
                .branch
                .map(|index| format!("branch {index}"))
                .unwrap_or_else(|| "no branch".to_string());
            let verdict = change
                .verdict
                .as_deref()
                .map(|verdict| format!("  verdict {verdict}"))
                .unwrap_or_default();
            let reasked = if change.reasked { "  re-asked" } else { "" };
            let _ = writeln!(
                out,
                "  {}  {}  {} -> {}  {}{}{}{}  {}",
                change.trigger_id,
                change.event,
                change.historical,
                change.candidate,
                branch,
                verdict,
                reasked,
                scores_label(&change.fired_answers),
                file_label(&change.uri)
            );
        }
        push_more(&mut out, self.changed.len(), limit);

        let _ = writeln!(out, "not comparable ({})", self.not_comparable.len());
        for skipped in self.not_comparable.iter().take(limit) {
            let _ = writeln!(
                out,
                "  {}  {}  {}  {}",
                skipped.trigger_id,
                skipped.event,
                skipped.reason,
                file_label(&skipped.uri)
            );
        }
        push_more(&mut out, self.not_comparable.len(), limit);

        out
    }
}

/// `  [stuck 0.91, route cognition]`: the answers the fired branch read.
fn scores_label(answers: &Answers) -> String {
    if answers.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = answers
        .iter()
        .map(|(name, answer)| match answer {
            Answer::Noul { noul } => format!("{name} {noul:.2}"),
            Answer::Choice { choice, .. } => format!("{name} {choice}"),
            Answer::Score { score, .. } => format!("{name} {score:.2}"),
        })
        .collect();
    format!("  [{}]", parts.join(", "))
}

fn push_more(out: &mut String, total: usize, limit: usize) {
    if total > limit {
        let _ = writeln!(
            out,
            "  ... {} more (the JSON report lists all)",
            total - limit
        );
    }
}

fn format_delta(delta: i64) -> String {
    if delta > 0 {
        format!("+{delta}")
    } else {
        delta.to_string()
    }
}

fn skipped_summary(by_reason: &BTreeMap<String, usize>) -> String {
    if by_reason.is_empty() {
        return String::new();
    }
    let details: Vec<String> = by_reason
        .iter()
        .map(|(reason, count)| format!("{reason} {count}"))
        .collect();
    format!(" ({})", details.join(", "))
}

fn file_label(uri: &str) -> &str {
    if uri.is_empty() {
        return "-";
    }
    uri.rsplit('/').next().unwrap_or(uri)
}
