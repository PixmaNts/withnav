//! Frozen-trigger replay over a fixture decision log: the
//! same triggers re-run through a candidate pipeline, compared with the
//! actions the driver saw and the verdicts they gave them.
//!
//! The fixture log deliberately mixes the older prototype lines with the
//! pipeline-era ones: a replay must skip what it cannot use, count it, and
//! never fail on it.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use rust_agent::navigator::pipeline::Pipeline;
use rust_agent::navigator::ports::{Answer, DecisionModel, ModelError, Questions};
use rust_agent::navigator::replay::{load_candidate, ReplayLog, Replayer};
use tokio_util::sync::CancellationToken;

const LOG: &str = include_str!("fixtures/navigator/replay/mixed_log.jsonl");
const STRICTER: &str = "fixtures/navigator/replay/stricter";
const REWORDED: &str = "fixtures/navigator/replay/reworded";

fn fixture(relative: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(relative)
}

fn log() -> ReplayLog {
    ReplayLog::parse(LOG)
}

fn pipeline(relative: &str) -> Pipeline {
    Pipeline::new(load_candidate(Some(&fixture(relative))).expect("candidate pipeline"))
}

// ---------- the candidate the fixture log was produced with ----------

/// The pipeline the fixture log was recorded under: the built-in shape, so
/// replaying it must change nothing.
/// The pipeline the fixture log was recorded with (the first built-in
/// default), frozen here so tuning the built-in does not rewrite history.
fn recorded() -> Pipeline {
    Pipeline::new(load_candidate(Some(&fixture("fixtures/navigator/replay/recorded"))).expect("recorded pipeline"))
}

/// Scripted instinct: answers per call, in order, recording the states it
/// was asked about. No network, and the last answer repeats once the script
/// runs out.
struct ScriptedModel {
    answers: Vec<BTreeMap<String, Answer>>,
    states: Arc<std::sync::Mutex<Vec<String>>>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl ScriptedModel {
    fn new(answers: Vec<BTreeMap<String, Answer>>) -> Self {
        Self {
            answers,
            states: Arc::new(std::sync::Mutex::new(Vec::new())),
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn states(&self) -> Vec<String> {
        self.states.lock().unwrap().clone()
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl DecisionModel for ScriptedModel {
    fn decide<'a>(
        &'a self,
        state: &'a str,
        _questions: &'a Questions,
        _cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<BTreeMap<String, Answer>, ModelError>> + Send + 'a>>
    {
        let index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let answers = self
            .answers
            .get(index)
            .or_else(|| self.answers.last())
            .cloned()
            .unwrap_or_default();
        self.states.lock().unwrap().push(state.to_string());
        Box::pin(async move { Ok(answers) })
    }
}

fn noul(name: &str, value: f64) -> (String, Answer) {
    (name.to_string(), Answer::Noul { noul: value })
}

fn route(name: &str, choice: &str) -> (String, Answer) {
    (
        name.to_string(),
        Answer::Choice {
            choice: choice.to_string(),
            probabilities: BTreeMap::from([
                ("silent".to_string(), 0.05),
                ("metacognition".to_string(), 0.8),
                ("cognition".to_string(), 0.15),
            ]),
            confidence: 0.8,
        },
    )
}

/// The three logged questions, answered the way the instinct answered them.
fn all_three(worth: f64, route_to: &str, drift: f64) -> BTreeMap<String, Answer> {
    BTreeMap::from([
        noul("worth_attention", worth),
        route("route", route_to),
        noul("drift", drift),
    ])
}

// ---------- reading the log ----------

#[test]
fn the_log_yields_its_triggers_and_counts_what_it_skips() {
    let log = log();
    // 11 lines: 6 pipeline-era triggers, 3 verdicts, one prototype trigger,
    // one call record.
    assert_eq!(log.lines, 11);
    assert_eq!(log.triggers.len(), 6);
    assert_eq!(log.verdicts.len(), 3);
    assert_eq!(log.skipped(), 2);
    assert_eq!(log.skipped_by_reason["legacy_trigger"], 1);
    assert_eq!(log.skipped_by_reason["kind:call"], 1);
    // The joins the replay depends on are all present.
    assert_eq!(
        log.verdicts
            .get("01m3qa1remarkuseful00001")
            .map(String::as_str),
        Some("useful")
    );
    assert_eq!(
        log.verdicts
            .get("01m3qa1remarknoise000001")
            .map(String::as_str),
        Some("noise")
    );
}

#[test]
fn the_derived_inputs_match_what_the_server_derived() {
    let log = log();
    let remark = &log.triggers[0];
    assert_eq!(remark.inputs.changed_lines, 13);
    assert!(remark.inputs.has_intent, "the log stored an intent");
    assert!(!remark.inputs.has_errors, "no error diagnostics");
    // The cooldown of the save that came 4.2 s after a remark.
    assert_eq!(log.triggers[3].inputs.since_last_remark_ms, 4_200);
    assert_eq!(log.triggers[3].inputs.changed_lines, 7);
    // The questions are logged even when the pre-filters skipped: the
    // pipeline was loaded, the instinct simply was not asked.
    assert!(log.triggers[3].questions.is_some());
    assert!(log.triggers[3].answers.is_none());
}

// ---------- the offline replay ----------

#[tokio::test]
async fn the_pipeline_that_was_recorded_changes_nothing() {
    let report = Replayer::new(&recorded()).run(&log()).await;
    assert_eq!(report.compared, 5, "the wake trigger and the silent one");
    assert!(report.changed.is_empty(), "{:?}", report.changed);
    assert_eq!(report.historical.remark, 2);
    assert_eq!(report.historical.silent, 2);
    assert_eq!(report.historical.wake_cognition, 1);
    assert_eq!(report.historical.total(), 5);
    assert_eq!(report.useful_kept, 1);
    assert_eq!(report.noise_kept, 1);
    assert_eq!(report.unknown_verdicts, 3, "three remarks were never rated");
    assert_eq!(report.noise_avoided, 0);
    assert_eq!(report.useful_lost, 0);
    assert_eq!(report.rated(), 2);
}

#[tokio::test]
async fn a_threshold_only_candidate_replays_offline_and_counts_its_verdicts() {
    // `worth_attention > 0.9` instead of `0.7`: the two rated remarks fall
    // below the new threshold, the silent and the wake trigger do not move.
    let report = Replayer::new(&pipeline(STRICTER)).run(&log()).await;
    assert_eq!(report.compared, 5);
    assert_eq!(report.historical.remark, 2);
    assert_eq!(report.candidate.silent, 4);
    assert_eq!(report.candidate.remark, 0);
    // Both rated remarks are lost: the useful one and the noise one.
    assert_eq!(report.changed.len(), 2);
    assert_eq!(report.useful_lost, 1);
    assert_eq!(report.noise_avoided, 1);
    assert_eq!(report.useful_kept, 0);
    assert_eq!(report.noise_kept, 0);
    assert_eq!(report.unknown_verdicts, 3);
    for change in &report.changed {
        assert_eq!(change.historical, "remark");
        assert_eq!(change.candidate, "silent");
    }
    // The offline replay asked nothing.
    assert_eq!(report.reasked, 0);
}

#[tokio::test]
async fn the_cooldown_is_decided_from_the_logged_timestamp_not_re_simulated() {
    // The save 4.2 s after a remark was skipped by the logged pre-filters.
    // A candidate with a shorter cooldown lets it through, and since the log
    // holds no answers for it the trigger is not comparable: the replay does
    // not re-simulate the history that never happened.
    let impatient = Pipeline::new(
        load_candidate(Some(&fixture("fixtures/navigator/replay/impatient")))
            .expect("candidate pipeline"),
    );
    let report = Replayer::new(&impatient).run(&log()).await;
    let save = report
        .not_comparable
        .iter()
        .find(|t| t.trigger_id == "01m3qa4ereab6phh1g99p6rkm5w")
        .expect("the trigger the shorter cooldown no longer skips");
    assert!(
        save.reason.contains("unanswered:"),
        "no logged answers to replay: {}",
        save.reason
    );
    // With the recorded pre-filters the same trigger is a comparable silence.
    let report = Replayer::new(&recorded()).run(&log()).await;
    assert!(report
        .not_comparable
        .iter()
        .all(|t| t.trigger_id != "01m3qa4ereab6phh1g99p6rkm5w"));
}

#[tokio::test]
async fn a_record_the_pipeline_never_ran_is_not_comparable() {
    let report = Replayer::new(&recorded()).run(&log()).await;
    let without = report
        .not_comparable
        .iter()
        .find(|t| t.trigger_id == "01m3qa6ereab6phh1g99p6rkm5y")
        .expect("the instinct_unavailable trigger");
    assert_eq!(without.reason, "no_logged_questions");
    assert_eq!(without.event, "idle");
    assert_eq!(without.uri, "file:///ws/src/main.rs");
}

// ---------- a changed question ----------

#[tokio::test]
async fn a_changed_question_is_not_comparable_without_a_reask() {
    let report = Replayer::new(&pipeline(REWORDED)).run(&log()).await;
    // The four triggers that reached a branch hold answers to another
    // question; the cooldown skip is a silence either way.
    assert_eq!(report.compared, 1);
    assert_eq!(report.not_comparable.len(), 5);
    let reasons: Vec<&str> = report
        .not_comparable
        .iter()
        .map(|t| t.reason.as_str())
        .collect();
    assert_eq!(
        reasons
            .iter()
            .filter(|r| r.contains("changed_question:worth_attention"))
            .count(),
        4
    );
    assert!(reasons.iter().any(|r| r.contains("no_logged_questions")));
    // The reported counts only speak about what was compared.
    assert_eq!(report.historical.total(), 1);
    assert_eq!(report.candidate.total(), 1);
}

#[tokio::test]
async fn a_reask_rerenders_the_state_from_the_stored_template_inputs() {
    let model = ScriptedModel::new(vec![
        all_three(0.95, "metacognition", 0.1),
        all_three(0.2, "silent", 0.1),
        all_three(0.95, "metacognition", 0.95),
        all_three(0.2, "silent", 0.1),
    ]);
    let report = Replayer::with_reask(&pipeline(REWORDED), &model)
        .run(&log())
        .await;
    // The four changed triggers were asked again, the fifth (no logged
    // questions) still was not: there is nothing to re-render.
    assert_eq!(report.reasked, 4);
    assert_eq!(model.calls(), 4);
    assert_eq!(report.compared, 5);
    assert_eq!(report.not_comparable.len(), 1);

    // The states are the candidate's own template over the stored inputs,
    // never the logged rendering: `{{diff}}` of the first trigger.
    let states = model.states();
    assert_eq!(states.len(), 4);
    assert!(
        states[0].contains("File: file:///ws/src/main.rs (rust)"),
        "{states:?}"
    );
    assert!(
        states[0].contains("@@ -1 +1,12 @@\n+use std::io;"),
        "{states:?}"
    );
    assert!(
        states[0].contains("tighten the config parsing"),
        "{states:?}"
    );

    // The re-asked answers decide: the first trigger would still be a remark
    // (useful kept), the second a silence (noise avoided), the third a wake.
    assert_eq!(report.candidate.remark, 1);
    assert_eq!(report.candidate.silent, 3);
    assert_eq!(report.candidate.wake_cognition, 1);
    assert_eq!(report.useful_kept, 1);
    assert_eq!(report.noise_avoided, 1);
    assert_eq!(report.useful_lost, 0);
}

#[tokio::test]
async fn a_failing_reask_leaves_the_trigger_not_comparable() {
    struct Failing;
    impl DecisionModel for Failing {
        fn decide<'a>(
            &'a self,
            _state: &'a str,
            _questions: &'a Questions,
            _cancel: &'a CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<BTreeMap<String, Answer>, ModelError>> + Send + 'a>>
        {
            Box::pin(async { Err(ModelError::Malformed("no answers".to_string())) })
        }
    }
    let report = Replayer::with_reask(&pipeline(REWORDED), &Failing)
        .run(&log())
        .await;
    assert_eq!(report.reasked, 0, "a failed re-ask is not a re-ask");
    assert_eq!(report.not_comparable.len(), 5);
    assert!(
        report
            .not_comparable
            .iter()
            .any(|t| t.reason.starts_with("reask_failed:")),
        "{:?}",
        report.not_comparable
    );
}

// ---------- the report ----------

#[tokio::test]
async fn the_json_report_carries_the_counts_and_the_changed_triggers() {
    let report = Replayer::new(&pipeline(STRICTER)).run(&log()).await;
    let json: serde_json::Value = serde_json::to_value(&report).unwrap();
    assert_eq!(json["compared"], 5);
    assert_eq!(json["not_comparable"].as_array().unwrap().len(), 1);
    assert_eq!(json["useful_lost"], 1);
    assert_eq!(json["noise_avoided"], 1);
    assert_eq!(json["skipped_records"], 2);
    assert_eq!(json["skipped_by_reason"]["legacy_trigger"], 1);
    assert_eq!(json["changed"].as_array().unwrap().len(), 2);
    let first = &json["changed"][0];
    assert_eq!(first["historical"], "remark");
    assert_eq!(first["candidate"], "silent");
    assert_eq!(first["branch"], 2, "the silent fallback branch");
    assert_eq!(first["event"], "idle");
    assert!(
        first["verdict"].is_string(),
        "the verdict joins by remark id"
    );
}

#[tokio::test]
async fn the_text_report_counts_per_action_and_lists_what_changed() {
    let report = Replayer::new(&pipeline(STRICTER)).run(&log()).await;
    let text = report
        .clone()
        .with_log_path(&fixture("fixtures/navigator/replay/mixed_log.jsonl"))
        .with_pipeline_origin(STRICTER)
        .render_text(20);
    assert!(text.contains("frozen-trigger replay"), "{text}");
    assert!(text.contains(STRICTER), "{text}");
    assert!(
        text.contains("11 lines, 6 triggers, 3 verdicts, 2 skipped"),
        "{text}"
    );
    assert!(text.contains("legacy_trigger 1"), "{text}");
    assert!(text.contains("noise avoided 1"), "{text}");
    assert!(text.contains("useful lost 1"), "{text}");
    assert!(text.contains("remark -> silent"), "{text}");
    assert!(text.contains("no_logged_questions"), "{text}");
    // The changed list is truncated at the limit, the count is not.
    let text = report.render_text(0);
    assert!(text.contains("changed decisions (2)"), "{text}");
    assert!(text.contains("... 2 more"), "{text}");
}
