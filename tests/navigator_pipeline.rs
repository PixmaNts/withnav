//! Table-driven tests over fixture TOMLs for the pipeline loader and
//! evaluator (spec §Testing: the pipeline is a pure function over fixture
//! TOML, tuned constantly). Covers: valid, parse error, unknown identifier,
//! non-boolean, unknown template var, no_branch, first trigger wins,
//! pre-filters, and the flattening contract.

use std::collections::BTreeMap;

use rust_agent::navigator::pipeline::{Pipeline, PipelineSnapshot, SnapshotSource};
use rust_agent::navigator::ports::Answer;
use rust_agent::navigator::{TriggerInputs, TriggerKind};
use tokio_util::sync::CancellationToken;

mod common;
use common::navigator::{systemone_answers, ScriptedDecision};

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
    // The captured System One response, minus the conversation question the
    // test pipelines do not ask.
    let mut answers = systemone_answers();
    answers.remove("contradicts_conversation");
    answers
}

fn inputs(changed: i64) -> TriggerInputs {
    TriggerInputs {
        changed_lines: changed,
        since_last_remark_ms: 120_000,
        since_last_wake_ms: 120_000,
        called_for_help: false,
        has_errors: false,
        has_intent: true,
        has_conversation: false,
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
        "conversation",
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

/// Every way a pipeline is rejected at load time, with the error kind and a
/// fragment of the message: TOML snippet (plus the prompts it loads) in,
/// error out.
#[test]
fn a_bad_pipeline_is_rejected_at_load() {
    use rust_agent::navigator::PipelineError;

    // (case, toml, state prompt override, is a TOML parse error, message fragment)
    let cases: Vec<(&str, String, Option<&str>, bool, &str)> = vec![
        ("parse error", "[instinct\nmodel = ".into(), None, true, ""),
        (
            "undeclared identifier",
            BASE.replace("drift > 0.8", "nope > 0.8"),
            None,
            false,
            "undeclared identifier `nope`",
        ),
        (
            "non-boolean condition",
            BASE.replace("\"true\"", "\"42\""),
            None,
            false,
            "Expected a Value::Boolean",
        ),
        (
            "builtin function",
            BASE.replace("\"true\"", "\"max(1, 2) == 2\""),
            None,
            false,
            "functions",
        ),
        (
            "assignment",
            BASE.replace("\"true\"", "\"(drift = 1) > 0.5\""),
            None,
            false,
            "assign",
        ),
        (
            "unknown template variable",
            BASE.to_string(),
            Some("{{mystery}}"),
            false,
            "unknown variable `{{mystery}}`",
        ),
        (
            "score criteria out of range",
            BASE.replace(
                "criteria = [\"none\", \"minor\", \"moderate\", \"major\"]",
                "criteria = [\"only\"]",
            ),
            None,
            false,
            "level labels",
        ),
    ];
    for (case, toml, state, is_parse, fragment) in cases {
        let mut prompts = prompts();
        if let Some(state) = state {
            prompts.insert("s.md".to_string(), state.to_string());
        }
        let err =
            PipelineSnapshot::from_parts(&toml, prompts, SnapshotSource::Builtin).expect_err(case);
        let kind_ok = if is_parse {
            matches!(err, PipelineError::Parse { .. })
        } else {
            matches!(err, PipelineError::Invalid { .. })
        };
        assert!(kind_ok, "{case}: wrong error kind: {err:?}");
        assert!(err.to_string().contains(fragment), "{case}: {err}");
    }
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
    let model = ScriptedDecision::fixed(BTreeMap::from([(
        "worth_attention".to_string(),
        Answer::Noul { noul: 0.5 },
    )]));
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
    let model = ScriptedDecision::fixed(answers);
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
    let model = ScriptedDecision::fixed(BTreeMap::from([(
        "q".to_string(),
        Answer::Noul { noul: 1.0 },
    )]));

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
    let model = ScriptedDecision::fixed(full_answers());
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
    let model = ScriptedDecision::fixed(BTreeMap::from([(
        "drift".to_string(),
        Answer::Noul { noul: 0.95 },
    )]));
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

/// The built-in wake shape: a contradiction with the conversation wakes
/// the cognition, but only when a conversation digest exists.
#[tokio::test]
async fn contradiction_wakes_only_with_a_conversation() {
    let toml = r#"
[instinct]
model = "instinct"

[[trigger]]
on = ["save"]
state = "s.md"

[trigger.questions.drift]
type = "noul"
instructions = "Drift?"

[trigger.questions.contradicts_conversation]
type = "noul"
instructions = "Does this change contradict what the driver and the cognition just agreed in the conversation?"

[[trigger.branch]]
when = "(has_intent && drift > 0.8) || (has_conversation && contradicts_conversation > 0.8)"
action = "wake_cognition"

[[trigger.branch]]
when = "true"
action = "silent"
"#;
    let pipeline = Pipeline::new(snapshot_with(toml).unwrap());
    let model = ScriptedDecision::fixed(BTreeMap::from([
        ("drift".to_string(), Answer::Noul { noul: 0.1 }),
        (
            "contradicts_conversation".to_string(),
            Answer::Noul { noul: 0.95 },
        ),
    ]));
    // No intent, no conversation: the contradiction alone wakes nobody.
    let mut i = inputs(0);
    i.has_intent = false;
    i.has_conversation = false;
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

    // A conversation digest exists: the same answer wakes the cognition.
    let mut i = inputs(0);
    i.has_intent = false;
    i.has_conversation = true;
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
    assert_eq!(eval.branch_index, Some(0));
}

#[tokio::test]
async fn evaluation_carries_rendered_state_and_template_inputs() {
    let pipeline = Pipeline::new(snapshot_with(BASE).unwrap());
    let model = ScriptedDecision::fixed(full_answers());
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
    assert_eq!(triggers.len(), 2);
    assert_eq!(triggers[0].on.len(), 2);
    assert_eq!(triggers[0].prefilters.min_changed_lines, 3);
    assert_eq!(triggers[0].prefilters.cooldown_ms, 45_000);
    assert_eq!(triggers[0].questions.len(), 6);
    assert!(triggers[0].questions.contains_key("unfinished"));
    assert!(triggers[0].questions.contains_key("stuck"));
    assert!(triggers[0]
        .questions
        .contains_key("contradicts_conversation"));
    let specs = triggers[0].branch_specs();
    assert_eq!(specs.len(), 5);
    assert_eq!(specs[0].0.as_str(), "remark");
    assert_eq!(specs[0].1, Some("prompts/help_remark.md"));
    assert_eq!(specs[1].0.as_str(), "wake_cognition", "stuck");
    assert_eq!(
        specs[2].0.as_str(),
        "wake_cognition",
        "drift or contradiction"
    );
    assert_eq!(specs[3].0.as_str(), "remark");
    assert_eq!(specs[3].1, Some("prompts/remark.md"));
    assert_eq!(specs[4].0.as_str(), "silent");

    // The conversation trigger: one question, a remark or silence.
    assert_eq!(triggers[1].on, vec![TriggerKind::Conversation]);
    assert_eq!(triggers[1].prefilters.cooldown_ms, 30_000);
    assert_eq!(triggers[1].questions.len(), 1);
    assert!(triggers[1]
        .questions
        .contains_key("code_contradicts_conversation"));
    let specs = triggers[1].branch_specs();
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0].0.as_str(), "remark");
    assert_eq!(specs[0].1, Some("prompts/conversation_remark.md"));
    assert_eq!(specs[1].0.as_str(), "silent");
}

/// The built-in conversation trigger ignores `min_changed_lines` (it has
/// none, and an unchanged file must still be judged), keeps its cooldown and
/// remarks only with a conversation.
#[tokio::test]
async fn builtin_conversation_trigger_ignores_changed_lines_and_remarks_on_contradiction() {
    let pipeline = Pipeline::new(PipelineSnapshot::builtin().unwrap());
    let model = ScriptedDecision::fixed(BTreeMap::from([(
        "code_contradicts_conversation".to_string(),
        Answer::Noul { noul: 0.95 },
    )]));
    let mut conversing = inputs(0);
    conversing.has_conversation = true;
    let eval = pipeline
        .evaluate(
            TriggerKind::Conversation,
            conversing,
            &tvars(),
            &model,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(eval.prefilter.as_str(), "pass", "0 changed lines is fine");
    assert_eq!(eval.requested_action.as_str(), "remark");
    assert_eq!(eval.branch_index, Some(0));

    // No conversation digest: nothing to contradict.
    let mut alone = inputs(0);
    alone.has_conversation = false;
    let eval = pipeline
        .evaluate(
            TriggerKind::Conversation,
            alone,
            &tvars(),
            &model,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(eval.requested_action.as_str(), "silent");

    // Cooldown still applies.
    let mut cooling = conversing;
    cooling.since_last_remark_ms = 29_999;
    let eval = pipeline
        .evaluate(
            TriggerKind::Conversation,
            cooling,
            &tvars(),
            &model,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(eval.prefilter.skip_reason(), Some("cooldown"));
}

// ---------- built-in pipeline: wake guard, wake cooldown, routing ----------

fn builtin_answers(worth: f64, route: &str, drift: f64) -> BTreeMap<String, Answer> {
    BTreeMap::from([
        ("worth_attention".to_string(), Answer::Noul { noul: worth }),
        (
            "route".to_string(),
            Answer::Choice {
                choice: route.to_string(),
                probabilities: BTreeMap::from([(route.to_string(), 0.9)]),
                confidence: 0.9,
            },
        ),
        ("drift".to_string(), Answer::Noul { noul: drift }),
        (
            "contradicts_conversation".to_string(),
            Answer::Noul { noul: 0.1 },
        ),
        ("unfinished".to_string(), Answer::Noul { noul: 0.1 }),
        ("stuck".to_string(), Answer::Noul { noul: 0.1 }),
    ])
}

/// `answers` with the noul question `name` set to `value`.
fn with_noul(
    mut answers: BTreeMap<String, Answer>,
    name: &str,
    value: f64,
) -> BTreeMap<String, Answer> {
    answers.insert(name.to_string(), Answer::Noul { noul: value });
    answers
}

async fn builtin_action(answers: BTreeMap<String, Answer>, i: TriggerInputs) -> String {
    let pipeline = Pipeline::new(PipelineSnapshot::builtin().unwrap());
    let model = ScriptedDecision::fixed(answers);
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
    eval.requested_action.as_str().to_string()
}

/// Compile errors are no criterion: the instinct judges whether the edit is
/// unfinished (still being typed) or the driver stuck. With errors and a high
/// drift the navigator wakes unless the edit is unfinished.
#[tokio::test]
async fn builtin_judges_unfinished_edits_not_compile_errors() {
    let mut with_errors = inputs(5);
    with_errors.has_errors = true;
    let drifting = builtin_answers(0.5, "silent", 0.95);

    assert_eq!(
        builtin_action(drifting.clone(), with_errors).await,
        "wake_cognition",
        "errors present, drift high, edit finished: wake"
    );
    let unfinished = with_noul(drifting, "unfinished", 0.9);
    assert_eq!(
        builtin_action(unfinished, with_errors).await,
        "silent",
        "the edit is still being typed"
    );

    // The remark branch is gated by the same question.
    let worth_it = builtin_answers(0.95, "cognition", 0.3);
    assert_eq!(
        builtin_action(worth_it.clone(), with_errors).await,
        "remark"
    );
    assert_eq!(
        builtin_action(with_noul(worth_it, "unfinished", 0.5), with_errors).await,
        "silent",
        "unfinished at 0.5 is not below 0.5"
    );
}

/// A stuck driver gets the chat whether the code compiles or not, and even
/// when the edit looks unfinished; a score at the threshold does not.
#[tokio::test]
async fn builtin_wakes_a_stuck_driver_with_or_without_errors() {
    let stuck = with_noul(builtin_answers(0.1, "silent", 0.1), "stuck", 0.87);
    let mut with_errors = inputs(5);
    with_errors.has_errors = true;
    for i in [inputs(5), with_errors] {
        assert_eq!(builtin_action(stuck.clone(), i).await, "wake_cognition");
    }
    let stuck_and_typing = with_noul(stuck.clone(), "unfinished", 0.9);
    assert_eq!(
        builtin_action(stuck_and_typing, with_errors).await,
        "wake_cognition"
    );
    // Intent-less: stuck does not need an intent.
    let mut no_intent = inputs(5);
    no_intent.has_intent = false;
    assert_eq!(builtin_action(stuck, no_intent).await, "wake_cognition");

    let at_threshold = with_noul(builtin_answers(0.1, "silent", 0.1), "stuck", 0.8);
    assert_eq!(builtin_action(at_threshold, with_errors).await, "silent");

    // The wake has its own cooldown: inside it, stuck falls through.
    let mut just_woke = inputs(5);
    just_woke.since_last_wake_ms = 1_000;
    let stuck_and_worth = with_noul(builtin_answers(0.95, "cognition", 0.1), "stuck", 0.9);
    assert_eq!(builtin_action(stuck_and_worth, just_woke).await, "remark");
}

/// Route `cognition` under the wake thresholds is a remark: only `silent`
/// routing keeps the metacognition quiet.
#[tokio::test]
async fn builtin_remarks_on_cognition_route_below_the_wake_thresholds() {
    for (route, expected) in [
        ("cognition", "remark"),
        ("metacognition", "remark"),
        ("silent", "silent"),
    ] {
        assert_eq!(
            builtin_action(builtin_answers(0.9, route, 0.3), inputs(5)).await,
            expected,
            "route {route}"
        );
    }
    // Worth under the threshold stays silent whatever the route.
    assert_eq!(
        builtin_action(builtin_answers(0.8, "cognition", 0.3), inputs(5)).await,
        "silent"
    );
}

/// A wake inside its cooldown does not match: the trigger falls through to
/// the remark branch (or silence) instead of skipping the instinct, and the
/// remark cooldown plays no part in it.
#[tokio::test]
async fn builtin_wake_has_its_own_cooldown() {
    let drifting = builtin_answers(0.95, "cognition", 0.95);
    let mut just_woke = inputs(5);
    just_woke.since_last_wake_ms = 119_999;
    assert_eq!(
        builtin_action(drifting.clone(), just_woke).await,
        "remark",
        "second wake inside 120 s falls through"
    );
    let mut long_ago = inputs(5);
    long_ago.since_last_wake_ms = 120_000;
    assert_eq!(builtin_action(drifting, long_ago).await, "wake_cognition");

    let snap = PipelineSnapshot::builtin().unwrap();
    assert_eq!(snap.triggers()[0].prefilters.wake_cooldown_ms, 120_000);
    assert_eq!(snap.triggers()[1].prefilters.wake_cooldown_ms, 0);
}

// ---------- built-in pipeline: an explicit call for help ----------

/// A one-line comment that calls on the navigator passes the size limit and
/// the remark cooldown, and is answered with a remark (never a wake) whatever
/// the instinct's scores; the same line without the call is skipped.
#[tokio::test]
async fn builtin_a_call_for_help_bypasses_the_small_change_filter_and_remarks() {
    let pipeline = Pipeline::new(PipelineSnapshot::builtin().unwrap());
    let run = |i: TriggerInputs, answers| {
        let pipeline = pipeline.clone();
        async move {
            pipeline
                .evaluate(
                    TriggerKind::Idle,
                    i,
                    &tvars(),
                    &ScriptedDecision::fixed(answers),
                    &CancellationToken::new(),
                )
                .await
                .unwrap()
        }
    };

    let mut one_line = inputs(1);
    one_line.since_last_remark_ms = 1_000;
    let skipped = run(one_line, builtin_answers(0.1, "silent", 0.1)).await;
    assert_eq!(
        skipped.prefilter.skip_reason(),
        Some("too_few_changed_lines")
    );
    assert!(skipped.answers.is_none(), "the instinct is not paid for");

    one_line.called_for_help = true;
    // Scores that would be silent, and scores that would wake: both remark.
    for answers in [
        builtin_answers(0.1, "silent", 0.1),
        builtin_answers(0.95, "cognition", 0.95),
    ] {
        let eval = run(one_line, answers).await;
        assert_eq!(eval.prefilter.skip_reason(), None);
        assert_eq!(eval.requested_action.as_str(), "remark");
        assert_eq!(eval.branch_index, Some(0));
    }

    // The conversation trigger has no such bypass.
    let snap = PipelineSnapshot::builtin().unwrap();
    assert!(snap.triggers()[0].prefilters.bypass_on_help_call);
    assert!(!snap.triggers()[1].prefilters.bypass_on_help_call);
}
