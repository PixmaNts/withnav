//! Stable, credential-free host contract shared by the CLI and Studio.
use instinct_flow::*;
use serde_json::json;
use std::collections::BTreeMap;

pub fn catalogue() -> Catalogue {
    use ValueType::{Array, Boolean, Integer, Object, String as Text};
    let all = [
        "conversation",
        "diagnostics",
        "idle",
        "message.received",
        "save",
        "tool.pre_call",
        "turn.end",
    ];
    let editor = ["conversation", "diagnostics", "idle", "save"];
    let strings = |names: &[&str]| names.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let mut actions = vec![];
    for (name, param, priority, triggers) in [
        ("ask_driver", Some("note"), Some(1), &all[5..6]),
        ("deny", Some("note"), Some(2), &all[5..6]),
        ("inject_note", Some("text"), None, &all[..]),
        ("log", Some("text"), None, &all[..]),
        ("no_objection", None, Some(0), &all[5..6]),
        ("remark", Some("prompt"), None, &editor[..]),
        ("wake", Some("note"), None, &all[..]),
    ] {
        actions.push(ActionSpec {
            name: name.into(),
            params: param
                .into_iter()
                .map(|name| ParamSpec {
                    name: name.into(),
                    value_type: Text,
                    required: true,
                })
                .collect(),
            kind: if priority.is_some() {
                ActionKind::Verdict
            } else {
                ActionKind::Effect
            },
            priority,
            allowed_triggers: strings(triggers),
        });
    }
    let schemas = vec![
        (
            "conversation",
            vec![
                ("turn_id", Text, true),
                ("text", Text, true),
                ("uri", Text, false),
            ],
            json!({"turn_id":"sample","text":"Done","uri":"file:///repo/src/lib.rs"}),
        ),
        (
            "diagnostics",
            vec![
                ("uri", Text, true),
                ("language_id", Text, true),
                ("version", Integer, true),
                ("diagnostics", Array, true),
                ("diff", Text, true),
            ],
            json!({"uri":"file:///repo/src/lib.rs","language_id":"rust","version":1,"diagnostics":[],"diff":""}),
        ),
        (
            "idle",
            vec![
                ("uri", Text, true),
                ("language_id", Text, true),
                ("version", Integer, true),
                ("diff", Text, true),
                ("idle_ms", Integer, true),
            ],
            json!({"uri":"file:///repo/src/lib.rs","language_id":"rust","version":1,"diff":"","idle_ms":1000}),
        ),
        (
            "message.received",
            vec![
                ("command_id", Text, true),
                ("author", Text, true),
                ("text", Text, true),
                ("selected_skill", Object, false),
            ],
            json!({"command_id":"sample","author":"driver","text":"Please review"}),
        ),
        (
            "save",
            vec![
                ("uri", Text, true),
                ("language_id", Text, true),
                ("version", Integer, true),
                ("diff", Text, true),
            ],
            json!({"uri":"file:///repo/src/lib.rs","language_id":"rust","version":1,"diff":""}),
        ),
        (
            "tool.pre_call",
            vec![
                ("tool", Text, true),
                ("args", Object, true),
                ("call_id", Text, true),
            ],
            json!({"tool":"write","args":{"path":"src/lib.rs","content":""},"call_id":"sample"}),
        ),
        (
            "turn.end",
            vec![
                ("turn_id", Text, true),
                ("text", Text, true),
                ("status", Text, true),
            ],
            json!({"turn_id":"sample","text":"Done","status":"completed"}),
        ),
    ];
    let triggers = schemas
        .into_iter()
        .map(|(name, mut fields, sample_payload)| {
            if editor.contains(&name) {
                fields.extend(super::migrate::TEMPLATE_FIELDS.iter().map(|f| {
                    (
                        match *f {
                            "file" => "template_file",
                            "language" => "template_language",
                            "intent" => "template_intent",
                            "diff" => "template_diff",
                            "context" => "template_context",
                            "diagnostics" => "template_diagnostics",
                            "live_state" => "template_live_state",
                            "conversation" => "template_conversation",
                            _ => unreachable!("fixed template fields"),
                        },
                        Text,
                        false,
                    )
                }));
                fields.extend(
                    super::migrate::INTEGER_FIELDS
                        .iter()
                        .map(|f| (*f, Integer, false)),
                );
                fields.extend(
                    super::migrate::BOOLEAN_FIELDS
                        .iter()
                        .map(|f| (*f, Boolean, false)),
                );
            }
            TriggerSpec {
                name: name.into(),
                payload_fields: fields
                    .into_iter()
                    .map(|(name, value_type, required)| FieldSpec {
                        name: name.into(),
                        value_type,
                        required,
                    })
                    .collect(),
                can_block: matches!(name, "tool.pre_call" | "message.received"),
                allowed_actions: actions
                    .iter()
                    .filter(|a| a.allowed_triggers.iter().any(|t| t == name))
                    .map(|a| a.name.clone())
                    .collect(),
                sample_payload,
                default_fallback: (name == "tool.pre_call").then(|| FallbackAction {
                    action: "ask_driver".into(),
                    params: BTreeMap::from([(
                        "note".into(),
                        json!("Flow failed: {failure_message}"),
                    )]),
                    then: None,
                }),
            }
        })
        .collect();
    let mut inputs = vec![];
    for (name, value_type) in [
        ("session.id", Text),
        ("session.agent", Text),
        ("session.model", Text),
        ("session.workspace", Text),
        ("conversation.last_driver", Text),
        ("conversation.digest", Text),
        ("conversation.last_turns", Array),
        ("intent.record", Text),
        ("workspace.root", Text),
        ("workspace.branch", Text),
        ("workspace.dirty_files", Array),
        ("editor.open_files", Array),
        ("editor.cursor", Object),
        ("editor.selection", Text),
    ] {
        let source = name.split('.').next().unwrap_or_default();
        inputs.push(InputSpec {
            name: name.into(),
            input_type: InputType::Text,
            value_type,
            source: source.into(),
            triggers: strings(if source == "editor" { &editor } else { &all }),
        });
    }
    inputs.sort_by(|a, b| a.name.cmp(&b.name));
    let catalogue = Catalogue {
        version: 1,
        triggers,
        inputs,
        actions,
        limits: Limits::default(),
    };
    debug_assert!(catalogue.validate().is_ok());
    catalogue
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_schema_is_complete_and_symmetric() {
        let c = catalogue();
        c.validate().unwrap();
        assert_eq!(c.triggers.len(), 7);
        assert_eq!(c.inputs.len(), 14);
        assert_eq!(c.actions.len(), 7);
        assert_eq!(
            serde_json::to_value(&c).unwrap()["limits"]["flow_bytes"],
            262144
        );
        for t in &c.triggers {
            for f in &t.payload_fields {
                if f.required {
                    assert!(f.value_type.matches(&t.sample_payload[&f.name]));
                }
            }
        }
    }
}
