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
//! Three inputs are reconstructed rather than logged: `has_intent`,
//! `has_errors` and `has_conversation` are not pre-filter inputs, so they
//! are derived from the stored template inputs (a non-empty `intent`, a
//! `[error]` line in `diagnostics`, a non-empty `conversation`) exactly as
//! the server derived them from the intent record, the conversation feed
//! and the latest diagnostics.

mod log;
mod report;

use std::ffi::OsStr;
use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::navigator::pipeline::evaluator::{run_prefilters, SnapshotTrigger};
use crate::navigator::pipeline::{
    Action, Pipeline, PipelineError, PipelineSnapshot, PrefilterOutcome, SnapshotSource,
    TriggerKind,
};
use crate::navigator::ports::{Answers, DecisionModel};

pub use log::{LoggedTrigger, ReplayLog};
pub use report::{ActionCounts, ChangedDecision, NotComparable, ReplayReport};

/// How many changed and not-comparable triggers a text report lists before
/// truncating; the JSON report always carries all of them.
pub const DEFAULT_LIST_LIMIT: usize = 20;

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
            followed: 0,
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
                    // `followed` (the driver edited next to the remark) is
                    // implicit evidence it was acted on: counted as useful.
                    let verdict = trigger
                        .remark_id
                        .as_ref()
                        .and_then(|id| log.verdicts.get(id))
                        .cloned();
                    let would_remark = action == Action::Remark;
                    if verdict.as_deref() == Some("followed") {
                        report.followed += 1;
                    }
                    match verdict.as_deref() {
                        Some("useful" | "followed") if !would_remark => report.useful_lost += 1,
                        Some("useful" | "followed") => report.useful_kept += 1,
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
        if let PrefilterOutcome::Skip(_) =
            run_prefilters(&candidate.prefilters, trigger.event, trigger.inputs)
        {
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
        match candidate.select(trigger.inputs, &answers) {
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
        self.pipeline.snapshot().trigger_for(event)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigator::pipeline::NO_REMARK_MS;
    use crate::navigator::ports::Questions;
    use std::collections::BTreeMap;

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
        // A record from before wakes had their own cooldown replays as never woken.
        assert_eq!(trigger.inputs.since_last_wake_ms, NO_REMARK_MS);
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
    fn logged_flags_win_over_the_prose_derivation() {
        // The diagnostics text mentions `[error]` and the intent is filled,
        // but the server logged that neither flag held: replay believes it.
        let line = TRIGGER
            .replace(
                r#""intent":"","diagnostics":"""#,
                r#""intent":"ship the replay","diagnostics":"line 3: [error] quoted in a comment""#,
            )
            .replace(
                r#""since_last_remark_ms":4611686018427387903}"#,
                r#""since_last_remark_ms":4611686018427387903,"has_errors":false,"has_intent":false,"has_conversation":true}"#,
            );
        let trigger = &ReplayLog::parse(&line).triggers[0];
        assert!(!trigger.inputs.has_errors);
        assert!(!trigger.inputs.has_intent);
        assert!(trigger.inputs.has_conversation, "even with no digest text");
    }

    #[test]
    fn has_conversation_is_derived_from_a_non_empty_digest() {
        // The frozen TRIGGER line has no `conversation` key at all: none.
        let trigger = &ReplayLog::parse(TRIGGER).triggers[0];
        assert!(!trigger.inputs.has_conversation);
        let line = TRIGGER.replace(
            r#""template_inputs":{"file":"a.rs""#,
            r#""template_inputs":{"conversation":"","file":"a.rs""#,
        );
        let trigger = &ReplayLog::parse(&line).triggers[0];
        assert!(
            !trigger.inputs.has_conversation,
            "an empty digest is no conversation"
        );
        let line = TRIGGER.replace(
            r#""template_inputs":{"file":"a.rs""#,
            r#""template_inputs":{"conversation":"driver: add a retry","file":"a.rs""#,
        );
        let trigger = &ReplayLog::parse(&line).triggers[0];
        assert!(trigger.inputs.has_conversation);
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
    async fn a_followed_remark_counts_as_useful_and_is_reported_apart() {
        let pipeline = pipeline();
        let text = format!(
            "{TRIGGER}\n{}\n",
            r#"{"kind":"feedback","id":"r1","verdict":"followed"}"#
        );
        let report = Replayer::new(&pipeline).run(&ReplayLog::parse(&text)).await;
        assert_eq!(report.useful_kept, 1, "followed is evidence of usefulness");
        assert_eq!(report.followed, 1);
        assert_eq!(report.unknown_verdicts, 0);
        assert!(report
            .render_text(DEFAULT_LIST_LIMIT)
            .contains("followed 1 counted as useful"));
    }

    /// A decision model that always returns the same answers.
    struct FixedAnswers(Answers);

    impl DecisionModel for FixedAnswers {
        fn decide<'a>(
            &'a self,
            _state: &'a str,
            _questions: &'a Questions,
            _cancel: &'a CancellationToken,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Answers, crate::navigator::ports::ModelError>,
                    > + Send
                    + 'a,
            >,
        > {
            let answers = self.0.clone();
            Box::pin(async move { Ok(answers) })
        }
    }

    /// No catch-all branch, so a low score leaves every branch false.
    const PARITY: &str = r#"
[instinct]
model = "instinct"

[[trigger]]
on = ["idle"]
state = "prompts/state.md"
prefilter = { min_changed_lines = 0, cooldown_ms = 0 }

[trigger.questions.worth_attention]
type = "noul"
instructions = "Worth a remark?"

[[trigger.branch]]
when = "worth_attention > 0.8"
action = "remark"
prompt = "prompts/remark.md"

[[trigger.branch]]
when = "worth_attention > 0.4"
action = "silent"
"#;

    #[tokio::test]
    async fn live_evaluation_and_replay_pick_the_same_action_and_branch() {
        let snapshot = PipelineSnapshot::from_parts(
            PARITY,
            BTreeMap::from([
                ("prompts/state.md".to_string(), "{{diff}}".to_string()),
                ("prompts/remark.md".to_string(), "say {{diff}}".to_string()),
            ]),
            SnapshotSource::Builtin,
        )
        .unwrap();
        let pipeline = Pipeline::new(snapshot);
        let template_inputs = BTreeMap::from([("diff".to_string(), "+ x".to_string())]);
        // (noul answer, expected branch): first branch, second branch, none.
        for (noul, expected) in [(0.9, Some(0)), (0.5, Some(1)), (0.1, None)] {
            let line = TRIGGER.replace(r#""noul":0.8"#, &format!(r#""noul":{noul}"#));
            let log = ReplayLog::parse(&line);
            let logged = &log.triggers[0];
            let answers = logged.answers.clone().unwrap();

            let live = pipeline
                .evaluate(
                    logged.event,
                    logged.inputs,
                    &template_inputs,
                    &FixedAnswers(answers),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            let Decision::Comparable { action, branch, .. } =
                Replayer::new(&pipeline).decide(logged).await
            else {
                panic!("the replay of answer {noul} is not comparable");
            };

            assert_eq!(live.branch_index, expected, "live, answer {noul}");
            assert_eq!(branch, live.branch_index, "branch, answer {noul}");
            assert_eq!(action, live.requested_action, "action, answer {noul}");
        }
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
}
