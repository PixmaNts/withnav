//! Lossless editor pipeline conversion; the legacy source remains available for rollback.
mod legacy;
use instinct_flow::*;
use legacy::{ActionDef, PipelineFile};
use serde_json::json;
use std::{collections::BTreeMap, path::Path};

pub const TEMPLATE_FIELDS: &[&str] = &[
    "file",
    "language",
    "intent",
    "diff",
    "context",
    "diagnostics",
    "live_state",
    "conversation",
];
pub const INTEGER_FIELDS: &[&str] = &[
    "changed_lines",
    "since_last_remark_ms",
    "since_last_wake_ms",
];
pub const BOOLEAN_FIELDS: &[&str] = &[
    "called_for_help",
    "has_errors",
    "has_intent",
    "has_conversation",
];

/// Converts first-matching legacy triggers to independent single-trigger flows.
pub fn convert(
    text: &str,
    prompts: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<(Definition, BTreeMap<String, String>)>> {
    let pipeline: PipelineFile = toml::from_str(text)?;
    let mut converted = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for trigger in pipeline.triggers {
        for event in &trigger.on {
            if !seen.insert(event.clone()) {
                continue;
            }
            let state = prompts
                .get(&trigger.state)
                .ok_or_else(|| anyhow::anyhow!("missing state template {}", trigger.state))?;
            let inputs: BTreeMap<_, _> = TEMPLATE_FIELDS
                .iter()
                .map(|f| (f.to_string(), format!("event.template_{f}")))
                .collect();
            let prefilter_inputs: BTreeMap<_, _> = INTEGER_FIELDS
                .iter()
                .chain(BOOLEAN_FIELDS)
                .map(|f| (f.to_string(), format!("event.{f}")))
                .collect();
            let pf = &trigger.prefilter;
            let gate = if event == "conversation" {
                format!("since_last_remark_ms >= {}", pf.cooldown_ms)
            } else {
                format!(
                    "changed_lines >= {} && since_last_remark_ms >= {}",
                    pf.min_changed_lines, pf.cooldown_ms
                )
            };
            let gate = if pf.bypass_on_help_call {
                format!("called_for_help || ({gate})")
            } else {
                gate
            };
            let mut nodes = Vec::new();
            let question_ids: Vec<_> = trigger.questions.keys().cloned().collect();
            let mut answer_aliases = prefilter_inputs.clone();
            for (id, question) in &trigger.questions {
                answer_aliases.insert(id.clone(), format!("answer.{id}"));
                let options: Vec<String> = match question {
                    protocol::Question::Noul { .. } => vec![],
                    protocol::Question::Choice { criteria, .. } => {
                        criteria.keys().cloned().collect()
                    }
                    protocol::Question::Score { criteria, .. } => {
                        (0..criteria.len()).map(|index| index.to_string()).collect()
                    }
                };
                if !matches!(question, protocol::Question::Noul { .. }) {
                    answer_aliases.insert(
                        format!("{id}_confidence"),
                        format!("answer.{id}.confidence"),
                    );
                    for option in options {
                        answer_aliases.insert(
                            format!("{id}_p_{option}"),
                            format!("answer.{id}.probabilities.{option}"),
                        );
                    }
                }
                let question: protocol::Question =
                    serde_json::from_value(serde_json::to_value(question)?)?;
                let kind = match question {
                    protocol::Question::Noul { instructions } => NodeKind::Noul {
                        instructions,
                        threshold: Threshold::Number(0.5),
                        state_template: Some("state.md".into()),
                    },
                    protocol::Question::Choice {
                        instructions,
                        criteria,
                    } => NodeKind::Choice {
                        instructions,
                        criteria,
                        state_template: Some("state.md".into()),
                    },
                    protocol::Question::Score {
                        instructions,
                        criteria,
                    } => NodeKind::Score {
                        instructions,
                        at_least: criteria.first().cloned().ok_or_else(|| {
                            anyhow::anyhow!("score question {id} has no criteria")
                        })?,
                        levels: criteria,
                        state_template: Some("state.md".into()),
                    },
                };
                let next = kind
                    .ports()
                    .into_iter()
                    .map(|port| (port, vec!["branch_0".into()]))
                    .collect();
                nodes.push(Node {
                    id: id.clone(),
                    label: None,
                    kind,
                    inputs: inputs.clone(),
                    next,
                });
            }
            for (index, branch) in trigger.branch.iter().enumerate() {
                let tree =
                    evalexpr::build_operator_tree::<evalexpr::DefaultNumericTypes>(&branch.when)
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "branch {index} condition cannot be translated: {error}"
                            )
                        })?;
                let unknown: Vec<_> = tree
                    .iter_read_variable_identifiers()
                    .filter(|name| !answer_aliases.contains_key(*name))
                    .collect();
                let functions: Vec<_> = tree.iter_function_identifiers().collect();
                let writes: Vec<_> = tree.iter_write_variable_identifiers().collect();
                anyhow::ensure!(
                    unknown.is_empty() && functions.is_empty() && writes.is_empty(),
                    "branch {index} condition cannot be translated: unknown variables {unknown:?}; functions {functions:?}; assignments {writes:?}"
                );
                // Collect only referenced answers: every declared alias would otherwise
                // multiply the input budget and require unused upstream evidence.
                let mut aliases: BTreeMap<_, _> = tree
                    .iter_read_variable_identifiers()
                    .map(|name| (name.to_string(), answer_aliases[name].clone()))
                    .collect();
                let condition = if branch.action == ActionDef::WakeCognition {
                    aliases.insert(
                        "since_last_wake_ms".into(),
                        "event.since_last_wake_ms".into(),
                    );
                    format!(
                        "({}) && since_last_wake_ms >= {}",
                        branch.when, pf.wake_cooldown_ms
                    )
                } else {
                    branch.when.clone()
                };
                let action_id = format!("action_{index}");
                let mut next = BTreeMap::new();
                if branch.action != ActionDef::Silent {
                    next.insert("true".into(), vec![action_id.clone()]);
                }
                if index + 1 < trigger.branch.len() {
                    next.insert("false".into(), vec![format!("branch_{}", index + 1)]);
                }
                nodes.push(Node {
                    id: format!("branch_{index}"),
                    label: None,
                    kind: NodeKind::Expression {
                        condition,
                        assign: BTreeMap::new(),
                    },
                    inputs: aliases,
                    next,
                });
                let (action, params) = match branch.action {
                    ActionDef::Silent => continue,
                    ActionDef::Remark => {
                        let path = branch
                            .prompt
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("remark has no prompt"))?;
                        let prompt = prompts
                            .get(path)
                            .ok_or_else(|| anyhow::anyhow!("missing remark template {path}"))?;
                        (
                            "remark",
                            BTreeMap::from([(
                                "prompt".into(),
                                json!(prompt.replace("{{", "{").replace("}}", "}")),
                            )]),
                        )
                    }
                    ActionDef::WakeCognition => (
                        "wake",
                        BTreeMap::from([(
                            "note".into(),
                            json!("Navigator noticed an obstacle in {file}.\n{diff}"),
                        )]),
                    ),
                };
                nodes.push(Node {
                    id: action_id,
                    label: None,
                    kind: NodeKind::Action {
                        action: action.into(),
                        params,
                    },
                    inputs: inputs.clone(),
                    next: BTreeMap::new(),
                });
            }
            if nodes.is_empty() {
                anyhow::bail!("empty legacy trigger");
            }
            let definition = Definition {
                flow: Flow {
                    version: 1,
                    name: format!("navigator-default-{event}"),
                    on: event.clone(),
                    enabled: true,
                    blocking: false,
                    budget_ms: None,
                    inputs: prefilter_inputs,
                    when: Some(gate),
                    start: if question_ids.is_empty() {
                        vec!["branch_0".into()]
                    } else {
                        question_ids.clone()
                    },
                },
                constants: BTreeMap::new(),
                node: nodes,
                fallback: None,
                layout: BTreeMap::new(),
            };
            converted.push((
                definition,
                BTreeMap::from([("state.md".into(), state.clone())]),
            ));
        }
    }
    Ok(converted)
}

/// Converts an existing legacy directory without overwriting captain-authored flows.
pub fn migrate_directory(source: &Path, target: &Path) -> anyhow::Result<bool> {
    let path = source.join("pipeline.toml");
    let directory = match super::fs::Directory::open(source, false) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let text = match directory.read("pipeline.toml", 262144, false) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let pipeline: PipelineFile = toml::from_str(&text)?;
    let mut prompts = BTreeMap::new();
    for trigger in &pipeline.triggers {
        for name in std::iter::once(&trigger.state)
            .chain(trigger.branch.iter().filter_map(|b| b.prompt.as_ref()))
        {
            let relative = Path::new(name);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                anyhow::bail!("invalid legacy prompt path");
            }
            let parent = relative.parent().unwrap_or_else(|| Path::new(""));
            let filename = relative
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| anyhow::anyhow!("invalid legacy prompt filename"))?;
            let prompt_dir = super::fs::Directory::open(&source.join(parent), false)?;
            prompts.insert(name.clone(), prompt_dir.read(filename, 65536, false)?);
        }
    }
    let flows = convert(&text, &prompts)?;
    let target_directory = super::fs::Directory::open(target, true)?;
    let names = target_directory.names()?;
    if flows.iter().any(|(d, _)| {
        names.contains(&format!("{}.toml", d.flow.name))
            || names.contains(&format!("{}-state.md", d.flow.name))
    }) {
        return Ok(false);
    }
    let mut installs = Vec::new();
    for (mut definition, templates) in flows {
        let state_name = format!("{}-state.md", definition.flow.name);
        for node in &mut definition.node {
            match &mut node.kind {
                NodeKind::Noul { state_template, .. }
                | NodeKind::Choice { state_template, .. }
                | NodeKind::Score { state_template, .. } => {
                    *state_template = Some(state_name.clone())
                }
                _ => {}
            }
        }
        let templates = BTreeMap::from([(state_name.clone(), templates["state.md"].clone())]);
        definition
            .validate(
                &super::catalogue::catalogue(),
                &Capabilities { images: false },
                &templates,
            )
            .map_err(|e| anyhow::anyhow!("converted flow invalid: {e:?}"))?;
        installs.push((
            state_name,
            templates.values().next().expect("one template").clone(),
        ));
        installs.push((
            format!("{}.toml", definition.flow.name),
            definition.to_toml().map_err(|e| anyhow::anyhow!("{e:?}"))?,
        ));
    }
    // Validate every conversion before installing any file; a competing editor wins.
    for (name, bytes) in installs {
        target_directory.write_new(&name, bytes.as_bytes())?;
    }
    tracing::warn!(source = %path.display(), "legacy navigator pipeline converted to flows; original retained");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCORED: &str = r#"
[instinct]
model = "instinct"
[[trigger]]
on = ["diagnostics"]
state = "state.md"
[trigger.questions.severity]
type = "score"
instructions = "How severe?"
criteria = ["low", "high"]
[trigger.questions.route]
type = "choice"
instructions = "Who?"
criteria = { silent = "nothing", cognition = "discuss" }
[[trigger.branch]]
when = "severity > 0.7 && severity_p_1 > 0.7 && severity_confidence > 0.8 && route == \"cognition\" && route_p_cognition > 0.7 && route_confidence > 0.8 && has_errors && has_intent && !has_conversation && !called_for_help && changed_lines >= 1 && since_last_remark_ms >= 0 && since_last_wake_ms >= 0"
action = "wake_cognition"
[[trigger.branch]]
when = "true"
action = "remark"
prompt = "remark.md"
"#;

    struct Decisions;
    impl DecisionClient for Decisions {
        fn capabilities(&self) -> Capabilities {
            Capabilities { images: false }
        }
        fn identity(&self) -> ClientIdentity {
            ClientIdentity {
                model: "fake".into(),
                variant: None,
            }
        }
        fn decide<'a>(
            &'a self,
            request: protocol::DecisionRequest<'a>,
            _: &'a tokio_util::sync::CancellationToken,
            _: std::time::Instant,
        ) -> BoxFuture<'a, Result<protocol::DecisionResponse, DecisionError>> {
            Box::pin(async move {
                Ok(protocol::DecisionResponse {
                    answers: request
                        .questions
                        .iter()
                        .map(|(id, question)| {
                            let answer = match question {
                                protocol::Question::Score { criteria, .. } => {
                                    protocol::Answer::Score {
                                        score: 0.9,
                                        legend: criteria
                                            .iter()
                                            .enumerate()
                                            .map(|(i, label)| (i.to_string(), label.clone()))
                                            .collect(),
                                        probabilities: BTreeMap::from([
                                            ("0".into(), 0.1),
                                            ("1".into(), 0.9),
                                        ]),
                                        confidence: 0.9,
                                    }
                                }
                                protocol::Question::Choice { .. } => protocol::Answer::Choice {
                                    choice: "cognition".into(),
                                    probabilities: BTreeMap::from([
                                        ("silent".into(), 0.1),
                                        ("cognition".into(), 0.9),
                                    ]),
                                    confidence: 0.9,
                                },
                                protocol::Question::Noul { .. } => unreachable!(),
                            };
                            (id.clone(), answer)
                        })
                        .collect(),
                    usage: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn diagnostics_score_and_flattened_answers_preserve_first_matching_branch() {
        let prompts = BTreeMap::from([
            ("state.md".into(), "{{diagnostics}}".into()),
            ("remark.md".into(), "later branch".into()),
        ]);
        let (definition, templates) = convert(SCORED, &prompts).unwrap().remove(0);
        let compiled = definition
            .validate(
                &super::super::catalogue::catalogue(),
                &Capabilities { images: false },
                &templates,
            )
            .unwrap();
        let mut payload = serde_json::Map::new();
        payload.extend(serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(json!({"uri":"file:///repo/test.rs", "language_id":"rust", "version":1, "diagnostics":[], "diff":"edit"})).unwrap());
        for field in TEMPLATE_FIELDS {
            payload.insert(format!("template_{field}"), json!("evidence"));
        }
        for field in INTEGER_FIELDS {
            payload.insert((*field).into(), json!(1));
        }
        for field in BOOLEAN_FIELDS {
            payload.insert(
                (*field).into(),
                json!(matches!(*field, "has_errors" | "has_intent")),
            );
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        for (errors, expected) in [(true, "wake"), (false, "remark")] {
            payload.insert("has_errors".into(), json!(errors));
            let outcome = run(
                &compiled,
                &Event {
                    trigger: "diagnostics".into(),
                    payload: json!(payload),
                },
                &crate::flows::runtime::ContextSnapshot::default(),
                &Decisions,
                RunOptions {
                    deadline: std::time::Instant::now() + std::time::Duration::from_secs(5),
                    run_id: "conversion".into(),
                },
                &cancel,
            )
            .await;
            let RunResult::Success { actions } = outcome.result else {
                panic!("{:?}", outcome.result)
            };
            assert_eq!(actions.len(), 1);
            assert_eq!(actions[0].action, expected);
        }
    }

    #[test]
    fn unsupported_condition_lists_every_unknown_variable_and_keeps_source_visible() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let source = config.join("navigator");
        std::fs::create_dir_all(&source).unwrap();
        let text = SCORED.replace("severity > 0.7", "unknown_a > 0.7 && unknown_b > 0.7");
        std::fs::write(source.join("pipeline.toml"), &text).unwrap();
        std::fs::write(source.join("state.md"), "{{diagnostics}}").unwrap();
        std::fs::write(source.join("remark.md"), "remark").unwrap();
        let store = crate::flows::store::FlowStore::new(root.path().into(), config);
        let errors = store.migration_diagnostics();
        assert_eq!(errors.len(), 1);
        for fragment in [
            "pipeline.toml",
            "unknown_a",
            "unknown_b",
            "original retained",
        ] {
            assert!(
                errors[0].message.contains(fragment),
                "{}",
                errors[0].message
            );
        }
        assert!(
            store
                .reload_diagnostics()
                .iter()
                .any(|error| error.code == "migration")
        );
        assert!(
            store
                .list()
                .unwrap()
                .iter()
                .any(|row| row.name == "legacy-pipeline" && row.errors == errors)
        );
        store.reload().unwrap();
        assert_eq!(store.migration_diagnostics(), errors);
        assert_eq!(
            std::fs::read_to_string(source.join("pipeline.toml")).unwrap(),
            text
        );
        assert!(
            !store
                .snapshot()
                .contains_key("navigator-default-diagnostics")
        );
    }

    #[test]
    fn conversion_is_once_and_keeps_original() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("navigator");
        let target = root.path().join("flows");
        std::fs::create_dir_all(source.join("prompts")).unwrap();
        let text = include_str!("../../tests/fixtures/navigator/legacy/pipeline.toml");
        std::fs::write(source.join("pipeline.toml"), text).unwrap();
        for (name, body) in [
            (
                "state.md",
                include_str!("../../tests/fixtures/navigator/legacy/prompts/state.md"),
            ),
            (
                "conversation_state.md",
                include_str!("../../tests/fixtures/navigator/legacy/prompts/conversation_state.md"),
            ),
            (
                "remark.md",
                include_str!("../../tests/fixtures/navigator/legacy/prompts/remark.md"),
            ),
            (
                "help_remark.md",
                include_str!("../../tests/fixtures/navigator/legacy/prompts/help_remark.md"),
            ),
            (
                "conversation_remark.md",
                include_str!(
                    "../../tests/fixtures/navigator/legacy/prompts/conversation_remark.md"
                ),
            ),
        ] {
            std::fs::write(source.join("prompts").join(name), body).unwrap();
        }
        assert!(super::migrate_directory(&source, &target).unwrap());
        assert!(!super::migrate_directory(&source, &target).unwrap());
        assert_eq!(
            std::fs::read_to_string(source.join("pipeline.toml")).unwrap(),
            text
        );
    }
}
