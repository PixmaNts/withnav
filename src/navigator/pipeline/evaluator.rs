//! Per-trigger evaluation: pick the first trigger whose `on` lists the
//! event, run its pre-filters, render the state, call the instinct once,
//! flatten the answers into expression variables, try branches in order.

use std::collections::BTreeMap;

use evalexpr::Value;
use tokio_util::sync::CancellationToken;

use super::snapshot::PipelineSnapshot;
use super::validator::{flatten_inputs, CompiledBranch};
use super::{Action, Evaluation, PipelineError, PrefilterOutcome, TriggerInputs, TriggerKind};
use crate::navigator::pipeline::templates;
use crate::navigator::ports::{Answer, Answers, DecisionModel, ModelError, Question};

/// The evaluator over a validated snapshot. Clone it per reload; the
/// snapshot is immutable.
#[derive(Clone)]
pub struct Pipeline {
    snapshot: PipelineSnapshot,
}

impl Pipeline {
    pub fn new(snapshot: PipelineSnapshot) -> Self {
        Self { snapshot }
    }

    /// The underlying snapshot (hash, source, trigger table).
    pub fn snapshot(&self) -> &PipelineSnapshot {
        &self.snapshot
    }

    /// Everything the templates render from, per trigger. Built by the
    /// integrator from document state, diagnostics and live state.
    pub fn render_state(
        &self,
        kind: TriggerKind,
        vars: &BTreeMap<String, String>,
    ) -> Result<String, PipelineError> {
        let Some(trigger) = self.snapshot.trigger_for(kind) else {
            return Err(PipelineError::invalid(
                self.snapshot.source.as_str(),
                format!("no trigger handles event {}", kind.as_str()),
            ));
        };
        Ok(templates::render(&trigger.state_text, vars))
    }

    /// The questions of the trigger handling `kind`, for logging.
    pub fn questions(&self, kind: TriggerKind) -> Option<&BTreeMap<String, Question>> {
        self.snapshot.trigger_for(kind).map(|t| &t.questions)
    }

    /// The questions of the trigger handling `kind`, serialized for the
    /// trigger log record.
    pub fn questions_for(&self, kind: TriggerKind) -> Option<BTreeMap<String, Question>> {
        self.questions(kind).cloned()
    }

    /// The models.json alias this snapshot's instinct resolves through.
    pub fn model_alias(&self) -> &str {
        &self.snapshot.inner.model_alias
    }

    /// Renders the prompt attached to the selected branch. `branch_index`
    /// is the index returned by [`Pipeline::evaluate`].
    pub fn render_remark_branch(
        &self,
        kind: TriggerKind,
        branch_index: Option<usize>,
        vars: &BTreeMap<String, String>,
    ) -> String {
        let Some(index) = branch_index else {
            return String::new();
        };
        let Some(trigger) = self.snapshot.trigger_for(kind) else {
            return String::new();
        };
        let Some(prompt_path) = trigger
            .branches
            .get(index)
            .and_then(|b| b.prompt.as_deref())
        else {
            return String::new();
        };
        let Some(prompt) = self.snapshot.prompts.get(prompt_path) else {
            return String::new();
        };
        templates::render(prompt, vars)
    }

    /// Public entry for the integration ticket: one trigger, end to end.
    /// Pre-filters run first (cheap, no model call); on pass the instinct is
    /// asked once and branches are tried in order; no matching branch means
    /// `silent` with reason `no_branch`.
    pub async fn evaluate(
        &self,
        event: TriggerKind,
        inputs: TriggerInputs,
        template_inputs: &BTreeMap<String, String>,
        model: &dyn DecisionModel,
        cancel: &CancellationToken,
    ) -> Result<Evaluation, ModelError> {
        // First matching trigger wins; others are ignored.
        let Some(trigger) = self.snapshot.trigger_for(event) else {
            // No trigger for this event at all: nothing requested, no
            // model call. A validated snapshot always handles something,
            // but the pipeline may simply not care about this event.
            return Ok(Evaluation {
                requested_action: Action::Silent,
                branch_index: None,
                answers: None,
                prefilter: PrefilterOutcome::Skip("no_trigger"),
                rendered_state: String::new(),
                template_inputs: serialize_inputs(template_inputs),
                hash: self.snapshot.hash.clone(),
            });
        };

        let rendered_state = templates::render(&trigger.state_text, template_inputs);
        let mut evaluation = Evaluation {
            requested_action: Action::Silent,
            branch_index: None,
            answers: None,
            prefilter: PrefilterOutcome::Pass,
            rendered_state,
            template_inputs: serialize_inputs(template_inputs),
            hash: self.snapshot.hash.clone(),
        };

        // Pre-filters before the instinct is paid for.
        let prefilter = run_prefilters(&trigger.prefilters, event, inputs);
        if let PrefilterOutcome::Skip(_) = &prefilter {
            evaluation.prefilter = prefilter;
            return Ok(evaluation);
        }
        evaluation.prefilter = prefilter;

        let answers = model
            .decide(&evaluation.rendered_state, &trigger.questions, cancel)
            .await?;
        evaluation.answers = Some(answers.clone());

        // No matching branch: silent, reason `no_branch`.
        let (action, branch_index) = trigger
            .select(inputs, &answers)
            .map_err(ModelError::Malformed)?;
        evaluation.requested_action = action;
        evaluation.branch_index = branch_index;
        Ok(evaluation)
    }
}

/// The pre-filters over the trigger's inputs. `min_changed_lines` does not
/// apply to the `conversation` event: the change is in the conversation, not
/// in the file, so the file's changed-line count says nothing. The cooldown
/// applies to every event.
pub(crate) fn run_prefilters(
    prefilters: &super::Prefilters,
    event: TriggerKind,
    inputs: TriggerInputs,
) -> PrefilterOutcome {
    if event != TriggerKind::Conversation
        && inputs.changed_lines < prefilters.min_changed_lines as i64
    {
        return PrefilterOutcome::Skip("too_few_changed_lines");
    }
    if inputs.since_last_remark_ms < prefilters.cooldown_ms as i64 {
        return PrefilterOutcome::Skip("cooldown");
    }
    PrefilterOutcome::Pass
}

fn serialize_inputs(inputs: &BTreeMap<String, String>) -> BTreeMap<String, serde_json::Value> {
    inputs
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect()
}

/// Flattens answers into expression variables: `noul` question `x` -> `x`
/// (float); `choice` -> `x` (string), `x_p_<option>` (float), `x_confidence`;
/// `score` -> `x` (float), `x_p_<index>` (float), `x_confidence`.
pub(crate) fn flatten_answers_into(
    answers: &BTreeMap<String, Answer>,
    vars: &mut BTreeMap<String, Value>,
) {
    for (name, answer) in answers {
        match answer {
            Answer::Noul { noul } => {
                vars.insert(name.clone(), Value::from_float(*noul));
            }
            Answer::Choice {
                choice,
                probabilities,
                confidence,
            } => {
                vars.insert(name.clone(), Value::from(choice.as_str()));
                for (option, p) in probabilities {
                    vars.insert(format!("{name}_p_{option}"), Value::from_float(*p));
                }
                vars.insert(format!("{name}_confidence"), Value::from_float(*confidence));
            }
            Answer::Score {
                score,
                probabilities,
                confidence,
                ..
            } => {
                vars.insert(name.clone(), Value::from_float(*score));
                for (index, p) in probabilities {
                    vars.insert(format!("{name}_p_{index}"), Value::from_float(*p));
                }
                vars.insert(format!("{name}_confidence"), Value::from_float(*confidence));
            }
        }
    }
}

/// A compiled snapshot: validated triggers with parsed conditions.
#[derive(Clone)]
pub(crate) struct CompiledSnapshot {
    pub model_alias: String,
    pub triggers: Vec<SnapshotTrigger>,
}

/// One validated `[[trigger]]`, ready to evaluate.
#[derive(Clone)]
pub struct SnapshotTrigger {
    pub on: Vec<TriggerKind>,
    pub prefilters: super::Prefilters,
    /// The prompt path as referenced.
    pub state: String,
    /// The rendered prompt content (template text).
    pub state_text: String,
    pub questions: BTreeMap<String, Question>,
    pub(crate) branches: Vec<CompiledBranch>,
}

impl SnapshotTrigger {
    pub(crate) fn handles(&self, kind: TriggerKind) -> bool {
        self.on.contains(&kind)
    }

    /// The first branch whose condition holds over the inputs and answers,
    /// as its action and index; no branch holding is `silent` with no index.
    /// The one selection the evaluator and the replay share.
    pub(crate) fn select(
        &self,
        inputs: TriggerInputs,
        answers: &Answers,
    ) -> Result<(Action, Option<usize>), String> {
        let mut vars = flatten_inputs(inputs);
        flatten_answers_into(answers, &mut vars);
        for (index, branch) in self.branches.iter().enumerate() {
            // A wake inside its own cooldown does not match: the trigger
            // falls through to the next branch.
            if branch.action == Action::WakeCognition
                && inputs.since_last_wake_ms < self.prefilters.wake_cooldown_ms as i64
            {
                continue;
            }
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

    /// Branch count and per-branch (action, prompt) for logging and replay.
    pub fn branch_specs(&self) -> Vec<(Action, Option<&str>)> {
        self.branches
            .iter()
            .map(|b| (b.action, b.prompt.as_deref()))
            .collect()
    }
}

/// Compiles a parsed pipeline file (with prompt contents) into a
/// [`CompiledSnapshot`], validating every trigger.
pub(crate) fn compile(
    file: &super::PipelineFile,
    prompts: &BTreeMap<String, String>,
    source: &'static str,
) -> Result<CompiledSnapshot, PipelineError> {
    let mut triggers = Vec::with_capacity(file.triggers.len());
    for (ti, t) in file.triggers.iter().enumerate() {
        let state_text = prompts.get(&t.state).ok_or_else(|| {
            PipelineError::invalid(
                source,
                format!("trigger {ti}: state prompt `{}` was not loaded", t.state),
            )
        })?;
        templates::validate_template(state_text, source)?;
        let branches = super::validator::validate_trigger(t, source)?;
        let on =
            t.on.iter()
                .map(|s| match s.as_str() {
                    "save" => TriggerKind::Save,
                    "idle" => TriggerKind::Idle,
                    "diagnostics" => TriggerKind::Diagnostics,
                    _ => TriggerKind::Conversation,
                })
                .collect();
        triggers.push(SnapshotTrigger {
            on,
            prefilters: t.prefilter.clone(),
            state: t.state.clone(),
            state_text: state_text.clone(),
            questions: t.questions.clone(),
            branches,
        });
    }
    Ok(CompiledSnapshot {
        model_alias: file.instinct.model.clone(),
        triggers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigator::ports::Question;
    use std::future::Future;
    use std::pin::Pin;

    const TOML: &str = r#"
[instinct]
model = "instinct"

[[trigger]]
on = ["save"]
state = "prompts/state.md"
prefilter = { min_changed_lines = 3, cooldown_ms = 20000 }

[trigger.questions.worth_attention]
type = "noul"
instructions = "Worth a remark?"

[trigger.questions.route]
type = "choice"
instructions = "Who?"
criteria = { silent = "nothing", metacognition = "remark", cognition = "discuss" }

[[trigger.branch]]
when = "worth_attention > 0.7 && route == \"metacognition\""
action = "remark"
prompt = "prompts/remark.md"

[[trigger.branch]]
when = "true"
action = "silent"
"#;

    fn snapshot() -> PipelineSnapshot {
        let prompts = BTreeMap::from([
            (
                "prompts/state.md".to_string(),
                "{{file}} {{diff}}".to_string(),
            ),
            (
                "prompts/remark.md".to_string(),
                "say {{intent}}".to_string(),
            ),
        ]);
        PipelineSnapshot::from_parts(
            TOML,
            prompts,
            super::super::snapshot::SnapshotSource::Builtin,
        )
        .unwrap()
    }

    /// Scripted fake DecisionModel returning fixed answers.
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

    fn answers_remark() -> BTreeMap<String, Answer> {
        BTreeMap::from([
            ("worth_attention".to_string(), Answer::Noul { noul: 0.8 }),
            (
                "route".to_string(),
                Answer::Choice {
                    choice: "metacognition".to_string(),
                    probabilities: BTreeMap::from([
                        ("silent".to_string(), 0.1),
                        ("metacognition".to_string(), 0.7),
                        ("cognition".to_string(), 0.2),
                    ]),
                    confidence: 0.9,
                },
            ),
        ])
    }

    fn answers_silent() -> BTreeMap<String, Answer> {
        BTreeMap::from([
            ("worth_attention".to_string(), Answer::Noul { noul: 0.1 }),
            (
                "route".to_string(),
                Answer::Choice {
                    choice: "silent".to_string(),
                    probabilities: BTreeMap::from([
                        ("silent".to_string(), 0.9),
                        ("metacognition".to_string(), 0.05),
                        ("cognition".to_string(), 0.05),
                    ]),
                    confidence: 0.9,
                },
            ),
        ])
    }

    fn inputs(changed: i64) -> TriggerInputs {
        TriggerInputs {
            changed_lines: changed,
            since_last_remark_ms: 60_000,
            since_last_wake_ms: 60_000,
            has_errors: false,
            has_intent: true,
            has_conversation: false,
        }
    }

    fn template_inputs() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("file".to_string(), "a.rs".to_string()),
            ("language".to_string(), "rust".to_string()),
            ("intent".to_string(), "retries".to_string()),
            ("diff".to_string(), "+ x".to_string()),
            ("context".to_string(), String::new()),
            ("diagnostics".to_string(), String::new()),
            ("live_state".to_string(), String::new()),
            ("conversation".to_string(), String::new()),
        ])
    }

    #[tokio::test]
    async fn remark_branch_matches() {
        let pipeline = Pipeline::new(snapshot());
        let model = FakeModel {
            answers: answers_remark(),
        };
        let cancel = CancellationToken::new();
        let eval = pipeline
            .evaluate(
                TriggerKind::Save,
                inputs(5),
                &template_inputs(),
                &model,
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(eval.requested_action, Action::Remark);
        assert_eq!(eval.branch_index, Some(0));
        assert!(eval.answers.is_some());
        assert_eq!(eval.prefilter.as_str(), "pass");
        assert_eq!(eval.rendered_state, "a.rs + x");
        assert_eq!(eval.template_inputs["file"], "a.rs");
    }

    #[tokio::test]
    async fn no_branch_falls_back_to_silent() {
        let pipeline = Pipeline::new(snapshot());
        let model = FakeModel {
            answers: BTreeMap::from([
                ("worth_attention".to_string(), Answer::Noul { noul: 0.5 }),
                (
                    "route".to_string(),
                    Answer::Choice {
                        choice: "cognition".to_string(),
                        probabilities: BTreeMap::from([
                            ("silent".to_string(), 0.0),
                            ("metacognition".to_string(), 0.0),
                            ("cognition".to_string(), 1.0),
                        ]),
                        confidence: 0.5,
                    },
                ),
            ]),
        };
        let eval = pipeline
            .evaluate(
                TriggerKind::Save,
                inputs(5),
                &template_inputs(),
                &model,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        // The fallback `true` branch caught it (index 1): a no-match
        // pipeline (no `true` branch) is covered by the integration tests.
        assert_eq!(eval.requested_action, Action::Silent);
        assert_eq!(eval.branch_index, Some(1));
    }

    #[tokio::test]
    async fn prefilter_too_few_lines_skips_model() {
        struct Counting {
            calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }
        impl DecisionModel for Counting {
            fn decide<'a>(
                &'a self,
                _s: &'a str,
                _q: &'a BTreeMap<String, Question>,
                _c: &'a CancellationToken,
            ) -> Pin<
                Box<dyn Future<Output = Result<BTreeMap<String, Answer>, ModelError>> + Send + 'a>,
            > {
                let calls = self.calls.clone();
                Box::pin(async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(Default::default())
                })
            }
        }
        let pipeline = Pipeline::new(snapshot());
        let model = Counting {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let eval = pipeline
            .evaluate(
                TriggerKind::Save,
                inputs(2),
                &template_inputs(),
                &model,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(eval.prefilter.as_str(), "skip");
        assert_eq!(eval.prefilter.skip_reason(), Some("too_few_changed_lines"));
        assert!(eval.answers.is_none());
        assert_eq!(model.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn min_changed_lines_is_ignored_for_the_conversation_event() {
        let on_conversation = TOML.replace("on = [\"save\"]", "on = [\"save\", \"conversation\"]");
        let pipeline = Pipeline::new(
            PipelineSnapshot::from_parts(
                &on_conversation,
                BTreeMap::from([
                    ("prompts/state.md".to_string(), "{{file}}".to_string()),
                    ("prompts/remark.md".to_string(), "say".to_string()),
                ]),
                super::super::snapshot::SnapshotSource::Builtin,
            )
            .unwrap(),
        );
        let model = FakeModel {
            answers: answers_remark(),
        };
        // Zero changed lines: a save is skipped, a conversation is not.
        let save = pipeline
            .evaluate(
                TriggerKind::Save,
                inputs(0),
                &template_inputs(),
                &model,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(save.prefilter.skip_reason(), Some("too_few_changed_lines"));
        let conversation = pipeline
            .evaluate(
                TriggerKind::Conversation,
                inputs(0),
                &template_inputs(),
                &model,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(conversation.prefilter.as_str(), "pass");
        assert!(conversation.answers.is_some());

        // The cooldown still applies.
        let mut cooling = inputs(0);
        cooling.since_last_remark_ms = 1;
        let cooled = pipeline
            .evaluate(
                TriggerKind::Conversation,
                cooling,
                &template_inputs(),
                &model,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(cooled.prefilter.skip_reason(), Some("cooldown"));
    }

    #[tokio::test]
    async fn unhandled_event_skips_everything() {
        let pipeline = Pipeline::new(snapshot());
        let model = FakeModel {
            answers: answers_silent(),
        };
        let eval = pipeline
            .evaluate(
                TriggerKind::Conversation,
                inputs(5),
                &template_inputs(),
                &model,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(eval.prefilter.skip_reason(), Some("no_trigger"));
        assert!(eval.answers.is_none());
    }

    #[test]
    fn flattening_exposes_probability_and_confidence_variables() {
        let mut vars = BTreeMap::new();
        flatten_answers_into(&answers_remark(), &mut vars);
        assert!(matches!(vars.get("route"), Some(Value::String(s)) if s == "metacognition"));
        assert!(
            matches!(vars.get("route_p_metacognition"), Some(Value::Float(f)) if (*f - 0.7).abs() < 1e-9)
        );
        assert!(
            matches!(vars.get("route_confidence"), Some(Value::Float(f)) if (*f - 0.9).abs() < 1e-9)
        );
        assert!(
            matches!(vars.get("worth_attention"), Some(Value::Float(f)) if (*f - 0.8).abs() < 1e-9)
        );
    }
}
