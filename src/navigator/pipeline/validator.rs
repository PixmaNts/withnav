//! Load-time validation: every expression parses, every identifier it reads
//! is declared by the trigger's questions or the always-available set, every
//! expression evaluates to a boolean against correctly typed dummy values,
//! builtin functions are disabled, no assignment, and templates only use
//! known variables.

use std::collections::BTreeMap;

use evalexpr::{build_operator_tree, HashMapContext, Value};

/// The default numeric types (i64 ints, f64 floats) used for all pipeline
/// expressions.
type CtxNumeric = evalexpr::DefaultNumericTypes;

use super::{BranchDef, PipelineError, TriggerDef, TriggerInputs};
use crate::navigator::pipeline::templates;

/// Compiles one `when` expression with builtin functions disabled and no
/// assignment. Returns the variable identifiers it reads.
pub(crate) fn compile_when(
    expr: &str,
    source: &'static str,
) -> Result<CompiledWhen, PipelineError> {
    let tree = build_operator_tree::<CtxNumeric>(expr).map_err(|e| {
        PipelineError::invalid(source, format!("condition `{expr}` does not parse: {e}"))
    })?;
    // Assignment: a write identifier anywhere means the expression assigns.
    let writes: Vec<String> = tree
        .iter_write_variable_identifiers()
        .map(String::from)
        .collect();
    if !writes.is_empty() {
        return Err(PipelineError::invalid(
            source,
            format!(
                "condition `{expr}` assigns to variables ({writes:?}); assignment is not allowed"
            ),
        ));
    }
    // Function calls: any identifier followed by a leftsided-value use that
    // is not a declared variable is a function call; evalexpr reports them
    // via iter_function_identifiers.
    let functions: Vec<String> = tree.iter_function_identifiers().map(String::from).collect();
    if !functions.is_empty() {
        return Err(PipelineError::invalid(
            source,
            format!("condition `{expr}` calls functions ({functions:?}); builtin functions are disabled"),
        ));
    }
    let reads: Vec<String> = tree
        .iter_read_variable_identifiers()
        .map(String::from)
        .collect();
    Ok(CompiledWhen {
        expr: expr.to_string(),
        tree,
        reads,
    })
}

/// A parsed `when` expression ready for repeated evaluation.
#[derive(Clone, Debug)]
pub(crate) struct CompiledWhen {
    pub expr: String,
    tree: evalexpr::Node<CtxNumeric>,
    reads: Vec<String>,
}

impl CompiledWhen {
    /// The variable identifiers this expression reads.
    pub fn reads(&self) -> &[String] {
        &self.reads
    }

    /// Evaluates to a boolean over the given variable values. Builtin
    /// functions are disabled on the context; a read of an undeclared
    /// variable fails here with `VariableIdentifierNotFound`.
    pub fn eval_boolean(&self, vars: &BTreeMap<String, Value>) -> Result<bool, String> {
        use evalexpr::{Context, ContextWithMutableVariables};
        let mut ctx = HashMapContext::<CtxNumeric>::new();
        if let Err(e) = ctx.set_builtin_functions_disabled(true) {
            return Err(format!("could not disable builtin functions: {e}"));
        }
        for (k, v) in vars {
            let _ = ctx.set_value(k.clone(), v.clone());
        }
        self.tree
            .eval_boolean_with_context(&ctx)
            .map_err(|e| format!("condition `{}` failed: {e}", self.expr))
    }
}

use crate::navigator::ports::Question;

/// A `score` question must declare 2 to 10 level labels (spec).
const SCORE_LEVELS_RANGE: std::ops::RangeInclusive<usize> = 2..=10;

/// One question's flattened variable names and their dummy value kinds,
/// exactly as the answers flatten them at evaluation time.
fn declared_names(questions: &BTreeMap<String, Question>) -> BTreeMap<String, Value> {
    let mut names = BTreeMap::new();
    for (name, q) in questions {
        match q {
            Question::Noul { .. } => {
                names.insert(name.clone(), Value::from_float(0.5));
            }
            Question::Choice { criteria, .. } => {
                names.insert(name.clone(), Value::from("option"));
                for option in criteria.keys() {
                    names.insert(format!("{name}_p_{option}"), Value::from_float(0.5));
                }
                names.insert(format!("{name}_confidence"), Value::from_float(0.5));
            }
            Question::Score { criteria, .. } => {
                names.insert(name.clone(), Value::from_float(1.5));
                for i in 0..criteria.len() {
                    names.insert(format!("{name}_p_{i}"), Value::from_float(0.5));
                }
                names.insert(format!("{name}_confidence"), Value::from_float(0.5));
            }
        }
    }
    names
}

/// Dummy typed values for the always-available variables.
fn always_available_dummies() -> BTreeMap<String, Value> {
    let inputs = TriggerInputs {
        changed_lines: 3,
        since_last_remark_ms: 60_000,
        has_errors: true,
        has_intent: true,
        has_conversation: true,
    };
    flatten_inputs(inputs)
}

/// Flattens the always-available inputs into expression values.
pub(crate) fn flatten_inputs(inputs: TriggerInputs) -> BTreeMap<String, Value> {
    let mut vars = BTreeMap::new();
    vars.insert(
        "changed_lines".to_string(),
        Value::from_int(inputs.changed_lines),
    );
    vars.insert(
        "since_last_remark_ms".to_string(),
        Value::from_int(inputs.since_last_remark_ms),
    );
    vars.insert("has_errors".to_string(), Value::from(inputs.has_errors));
    vars.insert("has_intent".to_string(), Value::from(inputs.has_intent));
    vars.insert(
        "has_conversation".to_string(),
        Value::from(inputs.has_conversation),
    );
    vars
}

/// Validates one trigger completely. Returns its compiled branches.
pub(crate) fn validate_trigger(
    trigger: &TriggerDef,
    source: &'static str,
) -> Result<Vec<CompiledBranch>, PipelineError> {
    if trigger.on.is_empty() {
        return Err(PipelineError::invalid(
            source,
            "a [[trigger]] must list at least one event in `on`",
        ));
    }
    for ev in &trigger.on {
        if !matches!(
            ev.as_str(),
            "save" | "idle" | "diagnostics" | "conversation"
        ) {
            return Err(PipelineError::invalid(
                source,
                format!("unknown event `{ev}` in `on` (save | idle | diagnostics | conversation)"),
            ));
        }
    }
    if trigger.questions.is_empty() {
        return Err(PipelineError::invalid(
            source,
            "a [[trigger]] must declare at least one question",
        ));
    }
    for (name, q) in &trigger.questions {
        if let crate::navigator::ports::Question::Score { criteria, .. } = q {
            if !SCORE_LEVELS_RANGE.contains(&criteria.len()) {
                return Err(PipelineError::invalid(
                    source,
                    format!(
                        "score question `{name}` must declare 2 to 10 level labels (got {})",
                        criteria.len()
                    ),
                ));
            }
        }
    }

    // The template must only use known variables before anything else.
    templates::validate_template(&trigger.state, source)?;

    let declared = declared_names(&trigger.questions);
    let dummies: BTreeMap<String, Value> = always_available_dummies();

    let mut compiled_branches = Vec::with_capacity(trigger.branch.len());
    for (i, branch) in trigger.branch.iter().enumerate() {
        validate_branch(branch, &declared, &dummies, source, i, trigger)?;
        let compiled = compile_when(&branch.when, source)?;
        compiled_branches.push(CompiledBranch {
            when: compiled,
            action: branch.action.into(),
            prompt: branch.prompt.clone(),
        });
    }
    Ok(compiled_branches)
}

fn validate_branch(
    branch: &BranchDef,
    declared: &BTreeMap<String, Value>,
    dummies: &BTreeMap<String, Value>,
    source: &'static str,
    index: usize,
    trigger: &TriggerDef,
) -> Result<(), PipelineError> {
    let compiled = compile_when(&branch.when, source)?;

    // Every identifier read must be declared by the questions or the
    // always-available set.
    for read in compiled.reads() {
        if !declared.contains_key(read) && !dummies.contains_key(read) {
            return Err(PipelineError::invalid(
                source,
                format!(
                    "branch {index} condition `{}` reads undeclared identifier `{read}`",
                    branch.when
                ),
            ));
        }
    }

    // Boolean result against correctly typed dummy values.
    let mut vars = dummies.clone();
    vars.extend(declared.iter().map(|(k, v)| (k.clone(), v.clone())));
    match compiled.eval_boolean(&vars) {
        Ok(_) => {}
        Err(e) => {
            return Err(PipelineError::invalid(
                source,
                format!("branch {index} of trigger on {:?}: {e}", trigger.on),
            ));
        }
    }

    // A remark needs a prompt.
    if matches!(branch.action, super::ActionDef::Remark) && branch.prompt.is_none() {
        return Err(PipelineError::invalid(
            source,
            format!("branch {index} requests `remark` without a `prompt`"),
        ));
    }
    Ok(())
}

/// A validated branch, ready to evaluate.
#[derive(Clone, Debug)]
pub(crate) struct CompiledBranch {
    pub when: CompiledWhen,
    pub action: super::Action,
    pub prompt: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigator::ports::Question;

    fn questions() -> BTreeMap<String, Question> {
        let mut qs = BTreeMap::new();
        qs.insert(
            "worth_attention".to_string(),
            Question::Noul {
                instructions: "worth it?".to_string(),
            },
        );
        let mut criteria = BTreeMap::new();
        criteria.insert("silent".to_string(), "nothing".to_string());
        criteria.insert("cognition".to_string(), "deep".to_string());
        qs.insert(
            "route".to_string(),
            Question::Choice {
                instructions: "who?".to_string(),
                criteria,
            },
        );
        qs
    }

    fn trigger_def(when: &str) -> TriggerDef {
        TriggerDef {
            on: vec!["save".to_string()],
            prefilter: Default::default(),
            state: "x".to_string(),
            questions: questions(),
            branch: vec![BranchDef {
                when: when.to_string(),
                action: super::super::ActionDef::Silent,
                prompt: None,
            }],
        }
    }

    #[test]
    fn valid_condition_passes() {
        let t = trigger_def("worth_attention > 0.7 && route == \"cognition\"");
        assert!(validate_trigger(&t, "test").is_ok());
    }

    #[test]
    fn undeclared_identifier_is_rejected() {
        let t = trigger_def("mystery > 0.5");
        let err = validate_trigger(&t, "test").unwrap_err();
        assert!(
            err.to_string().contains("undeclared identifier `mystery`"),
            "{err}"
        );
    }

    #[test]
    fn non_boolean_result_is_rejected() {
        let t = trigger_def("worth_attention + 1");
        let err = validate_trigger(&t, "test").unwrap_err();
        assert!(
            err.to_string().contains("Expected a Value::Boolean")
                || err.to_string().contains("boolean"),
            "{err}"
        );
    }

    #[test]
    fn builtin_function_call_is_rejected() {
        let t = trigger_def("min(worth_attention, 0.5) > 0.2");
        let err = validate_trigger(&t, "test").unwrap_err();
        assert!(err.to_string().contains("functions"), "{err}");
    }

    #[test]
    fn assignment_is_rejected() {
        let t = trigger_def("(worth_attention = 1) > 0.5");
        let err = validate_trigger(&t, "test").unwrap_err();
        assert!(err.to_string().contains("assign"), "{err}");
    }

    #[test]
    fn parse_error_is_reported() {
        let t = trigger_def("worth_attention >");
        let err = validate_trigger(&t, "test").unwrap_err();
        assert!(matches!(err, PipelineError::Invalid { .. }), "{err}");
    }

    #[test]
    fn always_available_variables_are_declared() {
        let t = trigger_def(
            "changed_lines >= 3 && has_errors && has_intent && since_last_remark_ms > 1000 \
             && has_conversation",
        );
        assert!(validate_trigger(&t, "test").is_ok());
    }

    #[test]
    fn choice_options_declare_probability_variables() {
        let t = trigger_def("route_p_cognition > 0.6 || route_confidence < 0.3");
        assert!(validate_trigger(&t, "test").is_ok());
    }

    #[test]
    fn remark_without_prompt_is_rejected() {
        let mut t = trigger_def("true");
        t.branch[0].action = super::super::ActionDef::Remark;
        let err = validate_trigger(&t, "test").unwrap_err();
        assert!(err.to_string().contains("without a `prompt`"), "{err}");
    }

    #[test]
    fn unknown_event_is_rejected() {
        let mut t = trigger_def("true");
        t.on = vec!["on_save".to_string()];
        let err = validate_trigger(&t, "test").unwrap_err();
        assert!(err.to_string().contains("unknown event"), "{err}");
    }

    #[test]
    fn choice_dummy_string_and_probability_types_are_correct() {
        let declared = declared_names(&questions());
        assert!(matches!(declared.get("route"), Some(Value::String(_))));
        assert!(matches!(
            declared.get("route_p_cognition"),
            Some(Value::Float(_))
        ));
        assert!(matches!(
            declared.get("route_confidence"),
            Some(Value::Float(_))
        ));
        assert!(matches!(
            declared.get("worth_attention"),
            Some(Value::Float(_))
        ));
    }
}
