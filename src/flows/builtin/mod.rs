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
        .chain(std::iter::once((
            Definition::parse(include_str!("permission-default.toml"))
                .expect("embedded permission flow parses"),
            BTreeMap::from([
                (
                    "state.md".into(),
                    include_str!("permission-default-state.md").into(),
                ),
                (
                    "protected-state.md".into(),
                    include_str!("permission-default-protected-state.md").into(),
                ),
            ]),
        )))
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
}
