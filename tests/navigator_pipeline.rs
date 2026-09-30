//! Table-driven tests over fixture TOMLs for the pipeline loader and
//! evaluator (spec §Testing: the pipeline is a pure function over fixture
//! TOML, tuned constantly). Covers: valid, parse error, unknown identifier,
//! non-boolean, unknown template var, no_branch, first trigger wins,
//! pre-filters, and the flattening contract.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use rust_agent::navigator::pipeline::{Pipeline, PipelineSnapshot, SnapshotSource};
use rust_agent::navigator::ports::{Answer, DecisionModel, ModelError, Question};
use rust_agent::navigator::{TriggerInputs, TriggerKind};
use tokio_util::sync::CancellationToken;

const BASE: &str = r#"
[instinct]
model = "instinct"

[[trigger]]
on = ["save"]
state = "s.md"

[trigger.questions.worth_attention]
type = "noul"
instructions = "Does this change deserve a remark?"

[trigger.questions.route]
type = "choice"
instructions = "Who?"
criteria = { silent = "Nothing", metacognition = "Remark", cognition = "Discuss" }

[trigger.questions.severity]
type = "score"
instructions = "How serious?"
criteria = ["none", "minor", "moderate", "major"]

[trigger.questions.drift]
type = "noul"
instructions = "Drift?"

[[trigger.branch]]
when = "drift > 0.8"
action = "wake_cognition"

[[trigger.branch]]
when = "worth_attention > 0.7 && route == \"metacognition\""
action = "remark"
prompt = "r.md"

[[trigger.branch]]
when = "true"
action = "silent"
"#;

fn prompts() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("s.md".to_string(), "{{file}}\n{{diff}}".to_string()),
        ("r.md".to_string(), "remark about {{intent}}".to_string()),
    ])
}

fn snapshot_with(toml: &str) -> Result<PipelineSnapshot, rust_agent::navigator::PipelineError> {
    PipelineSnapshot::from_parts(toml, prompts(), SnapshotSource::Builtin)
}

fn full_answers() -> BTreeMap<String, Answer> {
    // Same shapes as tests/fixtures/navigator/systemone_multi.json.
    BTreeMap::from([
        ("worth_attention".to_string(), Answer::Noul { noul: 0.8 }),
        (
            "route".to_string(),
            Answer::Choice {
                choice: "cognition".to_string(),
                probabilities: BTreeMap::from([
                    ("silent".to_string(), 0.04),
                    ("metacognition".to_string(), 0.45),
                    ("cognition".to_string(), 0.51),
                ]),
                confidence: 0.27,
            },
        ),
        (
            "severity".to_string(),
            Answer::Score {
                score: 1.56,
                legend: BTreeMap::from([
                    ("0".to_string(), "none".to_string()),
                    ("1".to_string(), "minor".to_string()),
                    ("2".to_string(), "moderate".to_string()),
                    ("3".to_string(), "major".to_string()),
                ]),
                probabilities: BTreeMap::from([
                    ("0".to_string(), 0.14),
                    ("1".to_string(), 0.35),
                    ("2".to_string(), 0.31),
                    ("3".to_string(), 0.2),
                ]),
                confidence: 0.16,
            },
        ),
        ("drift".to_string(), Answer::Noul { noul: 0.54 }),
    ])
}

struct FakeModel {
    answers: BTreeMap<String, Answer>,
}

impl DecisionModel for FakeModel {
    fn decide<'a>(
        &'a self,
        _state: &'a str,
        _questions: &'a BTreeMap<String, Question>,
        _cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<BTreeMap<String, Answer>, ModelError>> + Send + 'a>>
    {
        let answers = self.answers.clone();
        Box::pin(async move { Ok(answers) })
    }
}

fn inputs(changed: i64) -> TriggerInputs {
    TriggerInputs {
        changed_lines: changed,
        since_last_remark_ms: 120_000,
        has_errors: false,
        has_intent: true,
    }
}

fn tvars() -> BTreeMap<String, String> {
    [
        "file",
        "language",
        "intent",
        "diff",
        "context",
        "diagnostics",
        "live_state",
    ]
    .into_iter()
    .map(|k| (k.to_string(), format!("value-{k}")))
    .collect()
}

#[test]
fn valid_pipeline_loads() {
    let snap = snapshot_with(BASE).unwrap();
    assert_eq!(snap.model_alias(), "instinct");
    assert_eq!(snap.hash().len(), 16);
}

#[test]
fn parse_error_is_rejected() {
    let err = snapshot_with("[instinct\nmodel = ").unwrap_err();
    assert!(
        matches!(err, rust_agent::navigator::PipelineError::Parse { .. }),
        "{err}"
    );
}

#[test]
fn unknown_identifier_is_rejected() {
    let bad = BASE.replace("drift > 0.8", "nope > 0.8");
    let err = snapshot_with(&bad).unwrap_err();
    assert!(
        err.to_string().contains("undeclared identifier `nope`"),
        "{err}"
    );
}

#[test]
fn non_boolean_condition_is_rejected() {
    let bad = BASE.replace("\"true\"", "\"42\"");
    let err = snapshot_with(&bad).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("Expected a Value::Boolean")
            || msg.contains("boolean")
            || msg.contains("condition"),
        "{msg}"
    );
}

#[test]
fn builtin_function_is_rejected() {
    let bad = BASE.replace("\"true\"", "\"max(1, 2) == 2\"");
    let err = snapshot_with(&bad).unwrap_err();
    assert!(err.to_string().contains("functions"), "{err}");
}

#[test]
fn assignment_is_rejected() {
    let bad = BASE.replace("\"true\"", "\"(drift = 1) > 0.5\"");
    let err = snapshot_with(&bad).unwrap_err();
    assert!(err.to_string().contains("assign"), "{err}");
}

#[test]
fn unknown_template_variable_is_rejected() {
    let prompts = BTreeMap::from([("s.md".to_string(), "{{mystery}}".to_string())]);
    let err = PipelineSnapshot::from_parts(BASE, prompts, SnapshotSource::Builtin).unwrap_err();
    assert!(
        err.to_string().contains("unknown variable `{{mystery}}`"),
        "{err}"
    );
}

#[tokio::test]
async fn no_branch_yields_silent() {
    // No branch matches: worth_attention low, route not metacognition,
    // drift low; the final `true` branch removed.
    let toml = r#"
[instinct]
model = "instinct"

[[trigger]]
on = ["save"]
state = "s.md"

[trigger.questions.worth_attention]
type = "noul"
instructions = "?"

[[trigger.branch]]
when = "worth_attention > 0.99"
action = "remark"
prompt = "r.md"
"#;
    let pipeline = Pipeline::new(snapshot_with(toml).unwrap());
    let model = FakeModel {
        answers: BTreeMap::from([("worth_attention".to_string(), Answer::Noul { noul: 0.5 })]),
    };
    let eval = pipeline
        .evaluate(
            TriggerKind::Save,
            inputs(5),
            &tvars(),
            &model,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(eval.requested_action.as_str(), "silent");
    assert_eq!(eval.branch_index, None);
}

#[tokio::test]
async fn first_matching_trigger_wins() {
    let two = format!(
        "{BASE}\n\n[[trigger]]\non = [\"save\", \"idle\"]\nstate = \"s.md\"\n\n[trigger.questions.extra]\ntype = \"noul\"\ninstructions = \"?\"\n\n[[trigger.branch]]\nwhen = \"true\"\naction = \"silent\"\n"
    );
    let pipeline = Pipeline::new(snapshot_with(&two).unwrap());
    let mut answers = full_answers();
    answers.insert("extra".to_string(), Answer::Noul { noul: 0.9 });
    let model = FakeModel { answers };
    // save: the FIRST trigger wins (drift 0.54 < 0.8, worth 0.8 > 0.7 but
    // route == cognition, so the silent fallback branch index 2 matches).
    let eval = pipeline
        .evaluate(
            TriggerKind::Save,
            inputs(5),
            &tvars(),
            &model,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(eval.requested_action.as_str(), "silent");
    assert_eq!(eval.branch_index, Some(2));
    // idle: only the second trigger lists it.
    let eval = pipeline
        .evaluate(
            TriggerKind::Idle,
            inputs(5),
            &tvars(),
            &model,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(eval.branch_index, Some(0));
}

#[tokio::test]
async fn prefilter_min_changed_lines_and_cooldown() {
    let toml = r#"
[instinct]
model = "instinct"

[[trigger]]
on = ["save"]
state = "s.md"
prefilter = { min_changed_lines = 3, cooldown_ms = 20000 }

[trigger.questions.q]
type = "noul"
instructions = "?"

[[trigger.branch]]
when = "true"
action = "silent"
"#;
    let pipeline = Pipeline::new(snapshot_with(toml).unwrap());
    let model = FakeModel {
        answers: BTreeMap::from([("q".to_string(), Answer::Noul { noul: 1.0 })]),
    };

    // Too few changed lines.
    let eval = pipeline
        .evaluate(
            TriggerKind::Save,
            inputs(2),
            &tvars(),
            &model,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(eval.prefilter.skip_reason(), Some("too_few_changed_lines"));

    // Cooldown.
    let mut i = inputs(5);
    i.since_last_remark_ms = 19_999;
    let eval = pipeline
        .evaluate(
            TriggerKind::Save,
            i,
            &tvars(),
            &model,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(eval.prefilter.skip_reason(), Some("cooldown"));

    // Both pass.
    let eval = pipeline
        .evaluate(
            TriggerKind::Save,
            inputs(3),
            &tvars(),
            &model,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(eval.prefilter.as_str(), "pass");
}

#[tokio::test]
async fn flattening_exposes_expected_variables_with_types() {
    // Acceptance: route_p_cognition, severity_p_2, route_confidence present
    // with the right types — exercised through conditions that would fail to
    // evaluate (type errors) if the flattening were wrong.
    let toml = r#"
[instinct]
model = "instinct"

[[trigger]]
on = ["save"]
state = "s.md"

[trigger.questions.route]
type = "choice"
instructions = "Who?"
criteria = { silent = "Nothing", metacognition = "Remark", cognition = "Discuss" }

[trigger.questions.severity]
type = "score"
instructions = "How serious?"
criteria = ["none", "minor", "moderate", "major"]

[[trigger.branch]]
when = "route_p_cognition > 0.5 && severity_p_2 > 0.3 && route_confidence < 1.0 && severity > 1.0 && route == \"cognition\""
action = "wake_cognition"

[[trigger.branch]]
when = "true"
action = "silent"
"#;
    let pipeline = Pipeline::new(snapshot_with(toml).unwrap());
    let model = FakeModel {
        answers: full_answers(),
    };
    let eval = pipeline
        .evaluate(
            TriggerKind::Save,
            inputs(0),
            &tvars(),
            &model,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    // route_p_cognition = 0.51 > 0.5, severity_p_2 = 0.31 > 0.3,
    // route_confidence = 0.27 < 1.0, severity = 1.56 > 1.0,
    // route == "cognition": the first branch matches.
    assert_eq!(eval.requested_action.as_str(), "wake_cognition");
    assert_eq!(eval.branch_index, Some(0));
}

#[tokio::test]
async fn wake_cognition_gate_respects_has_intent() {
    let toml = r#"
[instinct]
model = "instinct"

[[trigger]]
on = ["save"]
state = "s.md"

[trigger.questions.drift]
type = "noul"
instructions = "Drift?"

[[trigger.branch]]
when = "has_intent && drift > 0.8"
action = "wake_cognition"

[[trigger.branch]]
when = "true"
action = "silent"
"#;
    let pipeline = Pipeline::new(snapshot_with(toml).unwrap());
    let model = FakeModel {
        answers: BTreeMap::from([("drift".to_string(), Answer::Noul { noul: 0.95 })]),
    };
    let mut i = inputs(0);
    i.has_intent = false;
    let eval = pipeline
        .evaluate(
            TriggerKind::Save,
            i,
            &tvars(),
            &model,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(eval.requested_action.as_str(), "silent");

    let mut i = inputs(0);
    i.has_intent = true;
    let eval = pipeline
        .evaluate(
            TriggerKind::Save,
            i,
            &tvars(),
            &model,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(eval.requested_action.as_str(), "wake_cognition");
}

#[tokio::test]
async fn evaluation_carries_rendered_state_and_template_inputs() {
    let pipeline = Pipeline::new(snapshot_with(BASE).unwrap());
    let model = FakeModel {
        answers: full_answers(),
    };
    let eval = pipeline
        .evaluate(
            TriggerKind::Save,
            inputs(5),
            &tvars(),
            &model,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(eval.rendered_state, "value-file\nvalue-diff");
    assert_eq!(eval.template_inputs["intent"], "value-intent");
    assert!(!eval.hash.is_empty());
    assert!(eval.answers.is_some());
}

#[test]
fn builtin_default_pipeline_loads_and_is_the_spec_shape() {
    let snap = PipelineSnapshot::builtin().unwrap();
    assert_eq!(snap.source(), SnapshotSource::Builtin);
    assert_eq!(snap.model_alias(), "instinct");
    let triggers = snap.triggers();
    assert_eq!(triggers.len(), 1);
    assert_eq!(triggers[0].on.len(), 2);
    assert_eq!(triggers[0].prefilters.min_changed_lines, 3);
    assert_eq!(triggers[0].prefilters.cooldown_ms, 45_000);
    let specs = triggers[0].branch_specs();
    assert_eq!(specs.len(), 3);
    assert_eq!(specs[0].0.as_str(), "wake_cognition");
    assert_eq!(specs[1].0.as_str(), "remark");
    assert_eq!(specs[1].1, Some("prompts/remark.md"));
    assert_eq!(specs[2].0.as_str(), "silent");
}

#[test]
fn score_question_rejects_out_of_range_criteria() {
    let toml = r#"
[instinct]
model = "instinct"

[[trigger]]
on = ["save"]
state = "s.md"

[trigger.questions.s]
type = "score"
instructions = "?"
criteria = ["only"]

[[trigger.branch]]
when = "true"
action = "silent"
"#;
    let err = snapshot_with(toml).unwrap_err();
    assert!(
        err.to_string().contains("score") || err.to_string().contains("criteria"),
        "{err}"
    );
}
