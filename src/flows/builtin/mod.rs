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

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_flows_validate() {
        for (definition, templates) in super::definitions() {
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
