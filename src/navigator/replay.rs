//! Frozen-trigger replay: run a candidate pipeline over the triggers the
//! log recorded, exactly as they happened, and compare the actions it would
//! have chosen with the actions the driver saw and rated.
//!
//! The replay is frozen on purpose, in three ways:
//!
//! - **One evaluation per recorded trigger, in log order.** Nothing is
//!   re-simulated: a history-dependent pre-filter such as the cooldown reads
//!   the timestamps the log recorded, so a candidate that speaks more often is
//!   not silently granted the cooldown of a history it did not have.
//! - **A threshold-only candidate never pays for a model call.** The logged
//!   answers are replayed through the candidate's branches. Only a question
//!   whose definition changed forces the instinct to be asked again
//!   ([`Replayer::with_reask`]), and even then the state is re-rendered from
//!   the stored template inputs, never from the editor.
//! - **What the log cannot answer for is reported, never guessed.** A trigger
//!   whose questions were never asked is `not comparable`, a remark without a
//!   `useful`/`noise` verdict is `unknown`, and a line that is not a usable
//!   trigger record is `skipped` (an older log format mixed into a newer one
//!   is normal, and must never make the replay fail).
//!
//! Two inputs are reconstructed rather than logged: `has_intent` and
//! `has_errors` are not pre-filter inputs, so they are derived from the
//! stored template inputs (a non-empty `intent`, a `[error]` line in
//! `diagnostics`) exactly as the server derived them from the intent record
//! and the latest diagnostics.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::navigator::pipeline::evaluator::{flatten_answers_into, SnapshotTrigger};
use crate::navigator::pipeline::validator::flatten_inputs;
use crate::navigator::pipeline::{
    Action, Pipeline, PipelineError, PipelineSnapshot, PrefilterOutcome, Prefilters,
    SnapshotSource, TriggerInputs, TriggerKind, NO_REMARK_MS,
};
use crate::navigator::ports::{Answers, DecisionModel, Questions};

/// How many changed and not-comparable triggers a text report lists before
/// truncating; the JSON report always carries all of them.
pub const DEFAULT_LIST_LIMIT: usize = 20;

/// One v2 `trigger` record, as the replay needs it.
#[derive(Debug, Clone)]
pub struct LoggedTrigger {
    pub trigger_id: String,
    pub event: TriggerKind,
    pub uri: String,
    pub version: i64,
    pub ts: Option<String>,
    /// The pre-filter inputs the log recorded, with `has_intent` and
    /// `has_errors` derived from the stored template inputs.
    pub inputs: TriggerInputs,
    /// The questions the logged pipeline asked, when it got that far.
    pub questions: Option<Questions>,
    /// The logged instinct answers (`None` when the pre-filters skipped).
    pub answers: Option<Answers>,
    /// The structured template variable map, for a re-ask.
    pub template_inputs: BTreeMap<String, String>,
    /// Whether the record stored the template inputs at all.
    pub has_template_inputs: bool,
    /// The action the logged pipeline requested. `Some(Silent)` also covers a
    /// record that never reached a branch: a pre-filter skip requested
    /// nothing, which is silence. `None` is an action this build does not
    /// know, and counts as unknown.
    pub historical_action: Option<Action>,
    /// What was delivered: `remark`, `silent`, `none`, `cancelled`, ...
    pub delivered: String,
    /// The delivered remark, the only thing a verdict can label.
    pub remark_id: Option<String>,
}

/// A parsed decision log: the usable trigger records, the verdicts joined by
/// remark id, and the lines that were skipped.
#[derive(Debug, Clone, Default)]
pub struct ReplayLog {
    /// Where the log was read from, for the report header.
    pub path: Option<PathBuf>,
    pub triggers: Vec<LoggedTrigger>,
    /// Remark id to the driver's verdict, latest wins.
    pub verdicts: BTreeMap<String, String>,
    /// Non-empty lines read.
    pub lines: usize,
    /// Lines that are not usable v2 trigger records, by reason.
    pub skipped_by_reason: BTreeMap<String, usize>,
}

/// Why one line is not a usable trigger record.
#[derive(Debug, Clone, Copy)]
enum Skip {
    /// The prototype-era `trigger` line: no `trigger_id`, no pre-filter
    /// record, nothing a frozen replay can evaluate.
    Legacy,
    /// A pipeline-era line whose fields do not parse.
    Unusable,
}

impl Skip {
    fn reason(self) -> &'static str {
        match self {
            Self::Legacy => "legacy_trigger",
            Self::Unusable => "unusable_trigger",
        }
    }
}

impl ReplayLog {
    /// Parses a whole JSONL log. Unknown kinds, the older prototype format
    /// and unparsable lines are counted, never fatal.
    pub fn parse(text: &str) -> Self {
        let mut log = Self::default();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            log.lines += 1;
            let value: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => {
                    log.skip("not_json");
                    continue;
                }
            };
            match value.get("kind").and_then(Value::as_str) {
                Some("trigger") => match LoggedTrigger::from_value(&value) {
                    Ok(trigger) => log.triggers.push(trigger),
                    Err(skip) => log.skip(skip.reason()),
                },
                // Verdicts label the remark they name, nothing else.
                Some("feedback") => match value.get("id").and_then(Value::as_str) {
                    Some(id) => {
                        let verdict = value
                            .get("verdict")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        log.verdicts.insert(id.to_string(), verdict.to_string());
                    }
                    None => log.skip("unusable_feedback"),
                },
                Some(other) => log.skip(&format!("kind:{other}")),
                None => log.skip("no_kind"),
            }
        }
        log
    }

    /// Reads and parses a log file. A missing or unreadable file is an error;
    /// an unparsable line inside it is not.
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let mut log = Self::parse(&std::fs::read_to_string(path)?);
        log.path = Some(path.to_path_buf());
        Ok(log)
    }

    /// Lines that are not usable v2 trigger records.
    pub fn skipped(&self) -> usize {
        self.skipped_by_reason.values().sum()
    }

    fn skip(&mut self, reason: &str) {
        *self
            .skipped_by_reason
            .entry(reason.to_string())
            .or_insert(0) += 1;
    }
}

impl LoggedTrigger {
    fn from_value(value: &Value) -> Result<Self, Skip> {
        // `trigger_id` only exists in the pipeline-era record: its absence is
        // the older prototype format, which carries no questions, no answers
        // and no pre-filter record.
        let trigger_id = value
            .get("trigger_id")
            .and_then(Value::as_str)
            .ok_or(Skip::Legacy)?
            .to_string();
        let event = value
            .get("event")
            .and_then(Value::as_str)
            .and_then(trigger_kind)
            .ok_or(Skip::Unusable)?;
        let inputs = value
            .get("prefilter")
            .and_then(|p| p.get("inputs"))
            .ok_or(Skip::Unusable)?;
        let changed_lines = inputs
            .get("changed_lines")
            .and_then(Value::as_i64)
            .ok_or(Skip::Unusable)?;
        // A trigger that never remarked logged no cooldown: that is the
        // "never" value the server itself uses.
        let since_last_remark_ms = inputs
            .get("since_last_remark_ms")
            .and_then(Value::as_i64)
            .unwrap_or(NO_REMARK_MS);

        let template_inputs: BTreeMap<String, String> = value
            .get("template_inputs")
            .and_then(Value::as_object)
            .map(|vars| {
                vars.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let has_template_inputs = value.get("template_inputs").is_some();
        // The server derived these two from the intent record and the latest
        // diagnostics, both of which are in the stored template inputs.
        let has_intent = template_inputs
            .get("intent")
            .is_some_and(|intent| !intent.trim().is_empty());
        let has_errors = template_inputs
            .get("diagnostics")
            .is_some_and(|diagnostics| diagnostics.contains("[error]"));

        Ok(Self {
            trigger_id,
            event,
            uri: value
                .get("uri")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            version: value
                .get("version")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
            ts: value.get("ts").and_then(Value::as_str).map(str::to_string),
            inputs: TriggerInputs {
                changed_lines,
                since_last_remark_ms,
                has_errors,
                has_intent,
            },
            questions: optional_from_value(value.get("questions"))?,
            answers: optional_from_value(value.get("answers"))?,
            has_template_inputs,
            template_inputs,
            historical_action: historical_action(value.get("requested_action"))?,
            delivered: value
                .get("delivered")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            remark_id: value
                .get("remark_id")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
}

fn optional_from_value<T: serde::de::DeserializeOwned>(
    value: Option<&Value>,
) -> Result<Option<T>, Skip> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|_| Skip::Unusable),
    }
}

/// The logged `event`, as the pipeline's `on` lists them.
fn trigger_kind(event: &str) -> Option<TriggerKind> {
    match event {
        "save" => Some(TriggerKind::Save),
        "idle" => Some(TriggerKind::Idle),
        "diagnostics" => Some(TriggerKind::Diagnostics),
        "conversation" => Some(TriggerKind::Conversation),
        _ => None,
    }
}

/// A record with no `requested_action` never reached a branch: a pre-filter
/// skip, a pipeline that could not run, nothing requested. That is silence.
fn historical_action(value: Option<&Value>) -> Result<Option<Action>, Skip> {
    match value {
        None | Some(Value::Null) => Ok(Some(Action::Silent)),
        Some(Value::String(action)) => Ok(match action.as_str() {
            "silent" => Some(Action::Silent),
            "remark" => Some(Action::Remark),
            "wake_cognition" => Some(Action::WakeCognition),
            _ => None,
        }),
        Some(_) => Err(Skip::Unusable),
    }
}

/// The candidate pipeline of a replay: a `.navigator` directory, the
/// `pipeline.toml` inside one (its prompts resolve next to it), or the
/// built-in default.
pub fn load_candidate(path: Option<&Path>) -> Result<PipelineSnapshot, PipelineError> {
    let Some(path) = path else {
        return PipelineSnapshot::builtin();
    };
    if path.is_dir() {
        return PipelineSnapshot::load_dir(path, SnapshotSource::Project);
    }
    if path.file_name() == Some(OsStr::new("pipeline.toml")) {
        let base = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        return PipelineSnapshot::load_dir(base, SnapshotSource::Project);
    }
    Err(PipelineError::invalid(
        SnapshotSource::Project.as_str(),
        format!(
            "{} is not a pipeline.toml: point the replay at a .navigator directory \
             or at its pipeline.toml",
            path.display()
        ),
    ))
}

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
    fn add(&mut self, action: Option<Action>) {
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
            "verdicts      rated {}, noise avoided {}, useful lost {}, useful kept {}, noise kept {}, unknown {}",
            self.rated(),
            self.noise_avoided,
            self.useful_lost,
            self.useful_kept,
            self.noise_kept,
            self.unknown_verdicts
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
                "  {}  {}  {} -> {}  {}{}{}  {}",
                change.trigger_id,
                change.event,
                change.historical,
                change.candidate,
                branch,
                verdict,
                reasked,
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

/// The candidate's answer for one trigger, and where it came from.
struct AnswersSource {
    answers: Answers,
    reasked: bool,
}

/// What the candidate would have done with one logged trigger.
enum Decision {
    Comparable {
        action: Action,
        branch: Option<usize>,
        reasked: bool,
    },
    NotComparable(String),
}

/// Runs a candidate pipeline over a parsed log.
pub struct Replayer<'a> {
    pipeline: &'a Pipeline,
    /// The instinct used to re-ask a changed question. `None` makes such a
    /// trigger not comparable instead, which is the cheap default.
    reask: Option<&'a dyn DecisionModel>,
}

impl<'a> Replayer<'a> {
    /// A replay that never calls a model: only logged answers are used, so a
    /// candidate whose questions match the logged ones replays offline.
    pub fn new(pipeline: &'a Pipeline) -> Self {
        Self {
            pipeline,
            reask: None,
        }
    }

    /// A replay that re-asks the instinct for the triggers whose questions
    /// changed, re-rendering the state from the stored template inputs.
    pub fn with_reask(pipeline: &'a Pipeline, model: &'a dyn DecisionModel) -> Self {
        Self {
            pipeline,
            reask: Some(model),
        }
    }

    /// Replays every usable trigger record of the log, in order.
    pub async fn run(&self, log: &ReplayLog) -> ReplayReport {
        let snapshot = self.pipeline.snapshot();
        let mut report = ReplayReport {
            log_path: String::new(),
            pipeline: snapshot.source().as_str().to_string(),
            pipeline_hash: snapshot.hash().to_string(),
            reask: self.reask.is_some(),
            log_lines: log.lines,
            triggers: log.triggers.len(),
            verdicts: log.verdicts.len(),
            skipped_records: log.skipped(),
            skipped_by_reason: log.skipped_by_reason.clone(),
            compared: 0,
            not_comparable: Vec::new(),
            reasked: 0,
            historical: ActionCounts::default(),
            candidate: ActionCounts::default(),
            changed: Vec::new(),
            noise_avoided: 0,
            useful_lost: 0,
            useful_kept: 0,
            noise_kept: 0,
            unknown_verdicts: 0,
        };
        if let Some(path) = &log.path {
            report = report.with_log_path(path);
        }

        for trigger in &log.triggers {
            match self.decide(trigger).await {
                Decision::NotComparable(reason) => report.not_comparable.push(NotComparable {
                    trigger_id: trigger.trigger_id.clone(),
                    event: trigger.event.as_str().to_string(),
                    ts: trigger.ts.clone(),
                    uri: trigger.uri.clone(),
                    reason,
                }),
                Decision::Comparable {
                    action,
                    branch,
                    reasked,
                } => {
                    let historical = trigger.historical_action;
                    report.historical.add(historical);
                    report.candidate.add(Some(action));
                    report.compared += 1;
                    if reasked {
                        report.reasked += 1;
                    }
                    // A verdict labels one delivered remark: join it by
                    // remark id, and count only a `useful` or `noise` one.
                    let verdict = trigger
                        .remark_id
                        .as_ref()
                        .and_then(|id| log.verdicts.get(id))
                        .cloned();
                    let would_remark = action == Action::Remark;
                    match verdict.as_deref() {
                        Some("useful") if !would_remark => report.useful_lost += 1,
                        Some("useful") => report.useful_kept += 1,
                        Some("noise") if !would_remark => report.noise_avoided += 1,
                        Some("noise") => report.noise_kept += 1,
                        _ => report.unknown_verdicts += 1,
                    }
                    if historical != Some(action) {
                        report.changed.push(ChangedDecision {
                            trigger_id: trigger.trigger_id.clone(),
                            event: trigger.event.as_str().to_string(),
                            ts: trigger.ts.clone(),
                            uri: trigger.uri.clone(),
                            historical: historical.map_or("unknown", Action::as_str),
                            candidate: action.as_str(),
                            branch,
                            reasked,
                            verdict,
                        });
                    }
                }
            }
        }
        report
    }

    /// The candidate's decision for one logged trigger: pre-filters over the
    /// logged inputs, then its branches over the answers.
    async fn decide(&self, trigger: &LoggedTrigger) -> Decision {
        let Some(candidate) = self.pipeline_for(trigger.event) else {
            return Decision::NotComparable("no_trigger_for_event".to_string());
        };
        // A candidate that skips never needs answers, so it stays comparable
        // whatever the logged questions were.
        if let PrefilterOutcome::Skip(_) = run_prefilters(&candidate.prefilters, trigger.inputs) {
            return Decision::Comparable {
                action: Action::Silent,
                branch: None,
                reasked: false,
            };
        }
        let AnswersSource { answers, reasked } = match self.answers_for(trigger, candidate).await {
            Ok(answers) => answers,
            Err(reason) => return Decision::NotComparable(reason),
        };
        match branch_action(candidate, trigger.inputs, &answers) {
            Ok((action, branch)) => Decision::Comparable {
                action,
                branch,
                reasked,
            },
            Err(e) => Decision::NotComparable(format!("branch_failed: {e}")),
        }
    }

    /// The first trigger of the candidate pipeline that handles the event.
    fn pipeline_for(&self, event: TriggerKind) -> Option<&SnapshotTrigger> {
        self.pipeline
            .snapshot()
            .triggers()
            .iter()
            .find(|trigger| trigger.on.contains(&event))
    }

    /// The answers to replay, or why the trigger cannot be compared.
    ///
    /// A trigger is comparable when the candidate asks exactly the questions
    /// the logged run asked, with the same definitions, and the log holds
    /// their answers. Otherwise the instinct has to be asked again, which
    /// needs the stored template inputs to re-render the state from.
    async fn answers_for(
        &self,
        trigger: &LoggedTrigger,
        candidate: &SnapshotTrigger,
    ) -> Result<AnswersSource, String> {
        let questions = candidate.questions.clone();
        let mut blockers: Vec<String> = Vec::new();
        for (name, question) in &questions {
            match trigger
                .questions
                .as_ref()
                .and_then(|logged| logged.get(name))
            {
                Some(logged) if logged == question => {}
                Some(_) => blockers.push(format!("changed_question:{name}")),
                None => match &trigger.questions {
                    // The logged run never reached a pipeline: nothing was
                    // asked, so no answer can be replayed.
                    None => return Err("no_logged_questions".to_string()),
                    Some(_) => blockers.push(format!("new_question:{name}")),
                },
            }
            if trigger
                .answers
                .as_ref()
                .is_none_or(|answers| !answers.contains_key(name))
                && !blockers.iter().any(|blocker| blocker.ends_with(name))
            {
                // Same question, no answer: the logged pre-filters skipped,
                // or the instinct answered partially. Replaying another
                // trigger's answer here would invent history.
                blockers.push(format!("unanswered:{name}"));
            }
        }
        if blockers.is_empty() {
            return Ok(AnswersSource {
                answers: trigger.answers.clone().unwrap_or_default(),
                reasked: false,
            });
        }

        let Some(model) = self.reask else {
            return Err(blockers.join(", "));
        };
        if !trigger.has_template_inputs {
            return Err(format!("no_template_inputs ({})", blockers.join(", ")));
        }
        // The re-ask renders the candidate's own state template over the
        // stored template inputs: the replay never goes back to the editor.
        let state = self
            .pipeline
            .render_state(trigger.event, &trigger.template_inputs)
            .map_err(|e| format!("reask_failed: {e}"))?;
        let cancel = CancellationToken::new();
        match model.decide(&state, &questions, &cancel).await {
            Ok(answers) => Ok(AnswersSource {
                answers,
                reasked: true,
            }),
            Err(e) => Err(format!("reask_failed: {e}")),
        }
    }
}

/// The candidate's pre-filters over the logged inputs. Same two knobs as the
/// evaluator, and the same reasons: a threshold-only candidate changes what
/// passes without any model call.
fn run_prefilters(prefilters: &Prefilters, inputs: TriggerInputs) -> PrefilterOutcome {
    if inputs.changed_lines < prefilters.min_changed_lines as i64 {
        return PrefilterOutcome::Skip("too_few_changed_lines");
    }
    if inputs.since_last_remark_ms < prefilters.cooldown_ms as i64 {
        return PrefilterOutcome::Skip("cooldown");
    }
    PrefilterOutcome::Pass
}

/// The candidate's branches over the answers, in order; no branch holding is
/// silence, as in the evaluator.
fn branch_action(
    trigger: &SnapshotTrigger,
    inputs: TriggerInputs,
    answers: &Answers,
) -> Result<(Action, Option<usize>), String> {
    let mut vars = flatten_inputs(inputs);
    flatten_answers_into(answers, &mut vars);
    for (index, branch) in trigger.branches.iter().enumerate() {
        if branch
            .when
            .eval_boolean(&vars)
            .map_err(|e| format!("branch {index}: {e}"))?
        {
            return Ok((branch.action, Some(index)));
        }
    }
    Ok((Action::Silent, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const CANDIDATE: &str = r#"
[instinct]
model = "instinct"

[[trigger]]
on = ["save", "idle"]
state = "prompts/state.md"
prefilter = { min_changed_lines = 3, cooldown_ms = 20000 }

[trigger.questions.worth_attention]
type = "noul"
instructions = "Worth a remark?"

[[trigger.branch]]
when = "worth_attention > 0.5"
action = "remark"
prompt = "prompts/remark.md"

[[trigger.branch]]
when = "true"
action = "silent"
"#;

    fn candidate_dir() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("prompts")).unwrap();
        std::fs::write(tmp.path().join("pipeline.toml"), CANDIDATE).unwrap();
        std::fs::write(tmp.path().join("prompts/state.md"), "{{file}} {{diff}}").unwrap();
        std::fs::write(tmp.path().join("prompts/remark.md"), "{{diff}}").unwrap();
        tmp
    }

    #[test]
    fn candidate_loads_from_a_dir_or_its_pipeline_toml() {
        let tmp = candidate_dir();
        let from_dir = load_candidate(Some(tmp.path())).unwrap();
        let from_file = load_candidate(Some(&tmp.path().join("pipeline.toml"))).unwrap();
        assert_eq!(from_dir.hash(), from_file.hash());
        assert_eq!(from_dir.triggers()[0].prefilters.min_changed_lines, 3);
    }

    #[test]
    fn candidate_without_a_pipeline_argument_is_the_builtin_default() {
        let snapshot = load_candidate(None).unwrap();
        assert_eq!(snapshot.source(), SnapshotSource::Builtin);
    }

    #[test]
    fn a_file_that_is_not_a_pipeline_toml_is_refused() {
        let tmp = candidate_dir();
        let path = tmp.path().join("elsewhere.toml");
        std::fs::write(&path, CANDIDATE).unwrap();
        let err = load_candidate(Some(&path)).unwrap_err();
        assert!(err.to_string().contains("pipeline.toml"), "{err}");
    }

    #[test]
    fn a_missing_prompt_is_reported_as_an_io_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pipeline.toml"), CANDIDATE).unwrap();
        let err = load_candidate(Some(tmp.path())).unwrap_err();
        assert!(matches!(err, PipelineError::Io { .. }), "{err}");
    }

    fn pipeline() -> Pipeline {
        let snapshot = PipelineSnapshot::from_parts(
            CANDIDATE,
            BTreeMap::from([
                ("prompts/state.md".to_string(), "{{diff}}".to_string()),
                ("prompts/remark.md".to_string(), "say {{diff}}".to_string()),
            ]),
            SnapshotSource::Builtin,
        )
        .unwrap();
        Pipeline::new(snapshot)
    }

    /// One v2 trigger record, on a single line as the log writes it.
    const TRIGGER: &str = concat!(
        r#"{"kind":"trigger","trigger_id":"t1","run_id":"r1","uri":"file:///ws/a.rs","#,
        r#""version":4,"event":"idle","ts":"2026-09-29T16:55:07.255Z","#,
        r#""prefilter":{"inputs":{"changed_lines":13,"since_last_remark_ms":4611686018427387903},"outcome":"pass"},"#,
        r#""pipeline_hash":"h","template_inputs":{"file":"a.rs","diff":"+ x","intent":"","diagnostics":""},"#,
        r#""questions":{"worth_attention":{"type":"noul","instructions":"Worth a remark?"}},"#,
        r#""model":"instinct","answers":{"worth_attention":{"type":"noul","noul":0.8}},"#,
        r#""branch":0,"requested_action":"remark","delivered":"remark","remark_id":"r1"}"#
    );

    /// A trigger whose pre-filters skipped: no questions, no answers.
    const SKIPPED: &str = concat!(
        r#"{"kind":"trigger","trigger_id":"t2","run_id":"r1","uri":"file:///ws/a.rs","#,
        r#""version":4,"event":"save","#,
        r#""prefilter":{"inputs":{"changed_lines":1,"since_last_remark_ms":100},"outcome":"skip","reason":"cooldown"},"#,
        r#""delivered":"none","reason":"cooldown"}"#
    );

    #[test]
    fn parses_a_v2_trigger_and_derives_the_two_missing_inputs() {
        let log = ReplayLog::parse(TRIGGER);
        assert_eq!(log.triggers.len(), 1);
        let trigger = &log.triggers[0];
        assert_eq!(trigger.inputs.changed_lines, 13);
        assert_eq!(trigger.inputs.since_last_remark_ms, NO_REMARK_MS);
        assert!(!trigger.inputs.has_intent, "empty intent means none");
        assert!(!trigger.inputs.has_errors, "no diagnostics, no errors");
        assert_eq!(trigger.historical_action, Some(Action::Remark));
        assert_eq!(trigger.remark_id.as_deref(), Some("r1"));
    }

    #[test]
    fn a_record_that_never_reached_a_branch_requested_nothing() {
        let log = ReplayLog::parse(SKIPPED);
        assert_eq!(log.triggers[0].historical_action, Some(Action::Silent));
        assert_eq!(log.triggers[0].inputs.since_last_remark_ms, 100);
        assert!(log.triggers[0].questions.is_none());
    }

    #[test]
    fn intent_and_error_diagnostics_are_derived_from_the_stored_inputs() {
        let line = TRIGGER.replace(
            r#""intent":"","diagnostics":"""#,
            r#""intent":"ship the replay","diagnostics":"line 3: [error] cannot find value `x`""#,
        );
        let trigger = &ReplayLog::parse(&line).triggers[0];
        assert!(trigger.inputs.has_intent);
        assert!(trigger.inputs.has_errors);
    }

    #[test]
    fn old_and_unusable_lines_are_skipped_by_reason() {
        let text = format!(
            "{TRIGGER}\n{}\n{}\n{}\n{}\n{}\n",
            r#"{"kind":"trigger","changed_lines":16,"event":"idle","outcome":"fire","ts":"2026-09-29T09:45:06.056Z"}"#,
            r#"{"kind":"call","layer":"instinct","model":"jev","latency_ms":340}"#,
            r#"{"kind":"trigger","trigger_id":"t3","run_id":"r1","event":"teleport","delivered":"none"}"#,
            r#"{"kind":"trigger","trigger_id":"t4","run_id":"r1","event":"save","delivered":"none"}"#,
            r#"{"kind":"call","layer":"instinct""#,
        );
        let log = ReplayLog::parse(&text);
        assert_eq!(log.triggers.len(), 1, "only the v2 record replays");
        assert_eq!(log.lines, 6);
        assert_eq!(log.skipped(), 5);
        assert_eq!(log.skipped_by_reason["legacy_trigger"], 1);
        assert_eq!(log.skipped_by_reason["kind:call"], 1);
        assert_eq!(log.skipped_by_reason["unusable_trigger"], 2);
        assert_eq!(log.skipped_by_reason["not_json"], 1);
    }

    #[test]
    fn a_verdict_labels_only_the_remark_it_names() {
        let text = format!(
            "{TRIGGER}\n{}\n{}\n",
            r#"{"kind":"feedback","id":"r1","verdict":"useful"}"#,
            r#"{"kind":"feedback","id":"r2","verdict":"noise"}"#,
        );
        let log = ReplayLog::parse(&text);
        assert_eq!(log.verdicts.get("r1").map(String::as_str), Some("useful"));
        assert_eq!(log.verdicts.get("r2").map(String::as_str), Some("noise"));
        assert_eq!(log.triggers[0].remark_id.as_deref(), Some("r1"));
        // r2's verdict labels nothing: it never joins a trigger.
        assert_eq!(log.triggers[0].delivered, "remark");
    }

    #[tokio::test]
    async fn a_pre_filter_skip_is_comparable_without_any_answers() {
        let pipeline = pipeline();
        let report = Replayer::new(&pipeline)
            .run(&ReplayLog::parse(SKIPPED))
            .await;
        assert_eq!(report.compared, 1);
        assert!(report.changed.is_empty());
        assert_eq!(report.historical.silent, 1);
        assert_eq!(report.candidate.silent, 1);
        assert_eq!(report.unknown_verdicts, 1, "no remark, no verdict");
        assert!(report.not_comparable.is_empty());
    }

    #[tokio::test]
    async fn the_same_pipeline_over_the_same_answers_changes_nothing() {
        let pipeline = pipeline();
        let report = Replayer::new(&pipeline)
            .run(&ReplayLog::parse(TRIGGER))
            .await;
        assert_eq!(report.compared, 1);
        assert!(report.changed.is_empty());
        assert_eq!(report.historical.remark, 1);
        assert_eq!(report.candidate.remark, 1);
        assert_eq!(report.unknown_verdicts, 1, "no verdict for the remark");
    }

    #[tokio::test]
    async fn an_event_no_candidate_trigger_handles_is_not_comparable() {
        let pipeline = pipeline();
        let line = TRIGGER.replace(r#""event":"idle""#, r#""event":"conversation""#);
        let report = Replayer::new(&pipeline).run(&ReplayLog::parse(&line)).await;
        assert_eq!(report.compared, 0);
        assert_eq!(report.not_comparable[0].reason, "no_trigger_for_event");
    }

    #[tokio::test]
    async fn a_record_without_logged_questions_is_not_comparable() {
        let pipeline = pipeline();
        let line = concat!(
            r#"{"kind":"trigger","trigger_id":"t1","run_id":"r1","uri":"file:///ws/a.rs","#,
            r#""version":4,"event":"idle","#,
            r#""prefilter":{"inputs":{"changed_lines":13},"outcome":"pass","reason":"instinct_unavailable"},"#,
            r#""delivered":"none","reason":"instinct_unavailable"}"#
        );
        let report = Replayer::new(&pipeline).run(&ReplayLog::parse(line)).await;
        assert_eq!(report.not_comparable[0].reason, "no_logged_questions");
    }

    #[test]
    fn the_text_report_carries_the_counts_and_the_not_comparable_reasons() {
        let report = ReplayReport {
            log_path: PathBuf::from("/tmp/nav.jsonl").display().to_string(),
            pipeline: "builtin".to_string(),
            pipeline_hash: "abc123".to_string(),
            reask: false,
            log_lines: 3,
            triggers: 1,
            verdicts: 1,
            skipped_records: 2,
            skipped_by_reason: BTreeMap::from([("kind:call".to_string(), 2)]),
            compared: 1,
            not_comparable: vec![NotComparable {
                trigger_id: "t2".to_string(),
                event: "idle".to_string(),
                ts: None,
                uri: "file:///ws/b.rs".to_string(),
                reason: "changed_question:route".to_string(),
            }],
            reasked: 0,
            historical: ActionCounts {
                remark: 1,
                ..ActionCounts::default()
            },
            candidate: ActionCounts {
                silent: 1,
                ..ActionCounts::default()
            },
            changed: vec![ChangedDecision {
                trigger_id: "t1".to_string(),
                event: "save".to_string(),
                ts: Some("2026-09-29T16:55:07.255Z".to_string()),
                uri: "file:///ws/a.rs".to_string(),
                historical: "remark",
                candidate: "silent",
                branch: Some(2),
                reasked: false,
                verdict: Some("useful".to_string()),
            }],
            noise_avoided: 0,
            useful_lost: 1,
            useful_kept: 0,
            noise_kept: 0,
            unknown_verdicts: 0,
        };
        let text = report.render_text(DEFAULT_LIST_LIMIT);
        assert!(text.contains("useful lost 1"), "{text}");
        assert!(text.contains("remark -> silent"), "{text}");
        assert!(text.contains("changed_question:route"), "{text}");
        assert!(text.contains("kind:call 2"), "{text}");
        // The table rows: the count per action, historical and candidate.
        let row = text
            .lines()
            .find(|line| line.starts_with("remark"))
            .expect("a remark row");
        assert_eq!(
            row.split_whitespace().collect::<Vec<_>>(),
            ["remark", "1", "0", "-1"]
        );
    }
}
