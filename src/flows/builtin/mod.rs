//! Embedded editor defaults with the legacy state and question bytes preserved.
use instinct_flow::Definition;
use std::collections::BTreeMap;

pub fn definitions() -> Vec<(Definition, BTreeMap<String, String>)> {
    [
        (
            include_str!("navigator-default-save.toml"),
            include_str!("navigator-default-save-state.md"),
        ),
        (
            include_str!("navigator-default-idle.toml"),
            include_str!("navigator-default-idle-state.md"),
        ),
        (
            include_str!("navigator-default-conversation.toml"),
            include_str!("navigator-default-conversation-state.md"),
        ),
    ]
    .into_iter()
    .map(|(text, state)| {
        (
            Definition::parse(text).expect("embedded flow parses"),
            BTreeMap::from([("state.md".into(), state.into())]),
        )
    })
    .collect()
}

/// All host defaults; editor-only callers retain their historical definition set.
pub fn all_definitions() -> Vec<(Definition, BTreeMap<String, String>)> {
    definitions()
        .into_iter()
        .chain([(
            Definition::parse(include_str!("permission-default.toml"))
                .expect("embedded permission flow parses"),
            BTreeMap::from([
                (
                    "call-state.md".into(),
                    include_str!("permission-default-call-state.md").into(),
                ),
                (
                    "state.md".into(),
                    include_str!("permission-default-state.md").into(),
                ),
                (
                    "protected-state.md".into(),
                    include_str!("permission-default-protected-state.md").into(),
                ),
                (
                    "requested-state.md".into(),
                    include_str!("permission-default-requested-state.md").into(),
                ),
            ]),
        )])
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn editor_state_templates_preserve_historical_bytes() {
        let editor = include_str!("../../../tests/fixtures/navigator/legacy/prompts/state.md");
        let conversation =
            include_str!("../../../tests/fixtures/navigator/legacy/prompts/conversation_state.md");
        for (definition, templates) in super::definitions() {
            assert_eq!(
                templates["state.md"],
                if definition.flow.on == "conversation" {
                    conversation
                } else {
                    editor
                }
            );
        }
    }

    #[test]
    fn embedded_flows_validate() {
        for (definition, templates) in super::all_definitions() {
            definition
                .validate(
                    &crate::flows::catalogue::catalogue(),
                    &instinct_flow::Capabilities { images: false },
                    &templates,
                )
                .unwrap();
        }
    }

    #[test]
    fn embedded_questions_avoid_neutral_cuts_and_compound_applicability() {
        use instinct_flow::{NodeKind, Threshold};
        let catalogue = crate::flows::catalogue::catalogue();
        for (definition, templates) in super::all_definitions() {
            for template in templates.values() {
                assert!(template.len() <= catalogue.limits.template_bytes);
            }
            for node in &definition.node {
                let instructions = match &node.kind {
                    NodeKind::Noul {
                        instructions,
                        threshold,
                        ..
                    } => {
                        let cut = match threshold {
                            Threshold::Number(value) => *value,
                            Threshold::Reference(path) => definition.constants
                                [path.strip_prefix("constant.").unwrap()]
                            .as_f64()
                            .unwrap(),
                        };
                        assert!(
                            (cut - 0.5).abs() > f64::EPSILON,
                            "{}: {}",
                            definition.flow.name,
                            node.id
                        );
                        instructions
                    }
                    NodeKind::Choice { instructions, .. }
                    | NodeKind::Score { instructions, .. } => instructions,
                    _ => continue,
                };
                assert!(
                    !instructions
                        .to_ascii_lowercase()
                        .contains(" or if applicable")
                );
            }
        }
    }

    #[test]
    fn permission_flow_uses_the_whole_blocking_budget() {
        let (definition, _) = super::all_definitions()
            .into_iter()
            .find(|(d, _)| d.flow.name == "permission-default")
            .expect("embedded permission definition");
        assert_eq!(
            definition.flow.budget_ms,
            Some(instinct_flow::BLOCKING_BUDGET_MS)
        );
    }
}
