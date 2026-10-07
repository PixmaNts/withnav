//! Lossless editor pipeline conversion; the legacy source remains available for rollback.
use crate::navigator::pipeline::{ActionDef, PipelineFile};
use instinct_flow::*;
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
            let gate = format!(
                "changed_lines >= {} && since_last_remark_ms >= {}",
                pf.min_changed_lines, pf.cooldown_ms
            );
            let gate = if pf.bypass_on_help_call {
                format!("called_for_help || ({gate})")
            } else {
                gate
            };
            let mut nodes = Vec::new();
            let question_ids: Vec<_> = trigger.questions.keys().cloned().collect();
            for (id, question) in &trigger.questions {
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
                    protocol::Question::Score { .. } => {
                        anyhow::bail!("legacy score questions are unsupported")
                    }
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
                let mut aliases = prefilter_inputs.clone();
                aliases.extend(
                    question_ids
                        .iter()
                        .map(|id| (id.clone(), format!("answer.{id}"))),
                );
                let condition = if branch.action == ActionDef::WakeCognition {
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

/// Converts an existing legacy directory without overwriting driver-authored flows.
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
    #[test]
    fn conversion_is_once_and_keeps_original() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("navigator");
        let target = root.path().join("flows");
        std::fs::create_dir_all(source.join("prompts")).unwrap();
        let text = include_str!("../navigator/default_pipeline/pipeline.toml");
        std::fs::write(source.join("pipeline.toml"), text).unwrap();
        for (name, body) in [
            (
                "state.md",
                include_str!("../navigator/default_pipeline/prompts/state.md"),
            ),
            (
                "conversation_state.md",
                include_str!("../navigator/default_pipeline/prompts/conversation_state.md"),
            ),
            (
                "remark.md",
                include_str!("../navigator/default_pipeline/prompts/remark.md"),
            ),
            (
                "help_remark.md",
                include_str!("../navigator/default_pipeline/prompts/help_remark.md"),
            ),
            (
                "conversation_remark.md",
                include_str!("../navigator/default_pipeline/prompts/conversation_remark.md"),
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
