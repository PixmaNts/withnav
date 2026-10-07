//! Offline legacy capture adapter: original question and state provenance are mandatory.
use instinct_flow::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub fn capture(value: &Value, definition: &CompiledDefinition) -> anyhow::Result<RunRecord> {
    let questions: protocol::Questions = serde_json::from_value(
        value
            .get("questions")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing question provenance"))?,
    )?;
    let answers: protocol::Answers = serde_json::from_value(
        value
            .get("answers")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing answers"))?,
    )?;
    let state = value
        .get("rendered_state")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing original rendered state"))?;
    let templates = value
        .get("template_inputs")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("missing template capture"))?;
    let prefilters = value
        .pointer("/prefilter/inputs")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("missing prefilter capture"))?;
    let mut payload = serde_json::Map::new();
    let event = value
        .get("event")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing trigger"))?;
    if event == "conversation" {
        payload.insert("turn_id".into(), json!(value["trigger_id"]));
        payload.insert(
            "text".into(),
            templates.get("conversation").cloned().unwrap_or(json!("")),
        );
        payload.insert("uri".into(), value["uri"].clone());
    } else {
        for key in ["uri", "version"] {
            payload.insert(key.into(), value[key].clone());
        }
        payload.insert(
            "language_id".into(),
            templates.get("language").cloned().unwrap_or(json!("")),
        );
        payload.insert(
            "diff".into(),
            templates.get("diff").cloned().unwrap_or(json!("")),
        );
        if event == "idle" {
            payload.insert("idle_ms".into(), json!(0));
        }
        if event == "diagnostics" {
            payload.insert("diagnostics".into(), json!([]));
        }
    }
    for field in crate::flows::migrate::TEMPLATE_FIELDS {
        let original = templates
            .get(*field)
            .ok_or_else(|| anyhow::anyhow!("missing template field {field}"))?;
        payload.insert(format!("template_{field}"), original.clone());
    }
    for field in crate::flows::migrate::INTEGER_FIELDS
        .iter()
        .chain(crate::flows::migrate::BOOLEAN_FIELDS)
    {
        let original = prefilters
            .get(*field)
            .ok_or_else(|| anyhow::anyhow!("missing prefilter field {field}"))?;
        payload.insert(field.to_string(), original.clone());
    }
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing model identity"))?;
    let mut nodes = Vec::new();
    for node in &definition.definition().node {
        if node.kind.question().is_none() {
            continue;
        }
        let selected_inputs = node
            .inputs
            .iter()
            .map(|(alias, reference)| {
                let captured = InputValue {
                    value: payload
                        .get(reference.strip_prefix("event.").unwrap_or(reference))
                        .cloned(),
                    cut: false,
                    redacted: false,
                };
                (
                    alias.clone(),
                    SelectedInput {
                        reference: reference.clone(),
                        captured,
                    },
                )
            })
            .collect();
        nodes.push(NodeRecord {
            id: node.id.clone(),
            status: "completed".into(),
            port: None,
            duration_ms: 0,
            answer: answers.get(&node.id).cloned(),
            result: None,
            variables: BTreeMap::new(),
            batch_id: Some("legacy".into()),
            error: None,
            selected_inputs: Some(selected_inputs),
        });
    }
    Ok(RunRecord {
        version: 1,
        run_id: value["trigger_id"].as_str().unwrap_or_default().into(),
        flow_name: definition.definition().flow.name.clone(),
        definition_digest: "legacy".into(),
        mode: "live".into(),
        started_at: value["ts"].as_str().unwrap_or_default().into(),
        duration_ms: 0,
        budget_ms: 5000,
        event: Event {
            trigger: event.into(),
            payload: payload.into(),
        },
        identity: ClientIdentity {
            model: model.into(),
            variant: value
                .get("variant")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        inputs: BTreeMap::new(),
        // The legacy decision log never redacted event fields, so no event path is unavailable.
        redacted_event_paths: Vec::new(),
        nodes,
        batches: vec![BatchRecord {
            id: "legacy".into(),
            node_ids: questions.keys().cloned().collect(),
            state: state.into(),
            redacted: false,
            questions,
            duration_ms: 0,
            usage: None,
            cost_usd: None,
        }],
        status: "success".into(),
        failure: None,
        actions: vec![],
        fallback_actions: vec![],
        usage: None,
        cost_usd: None,
        application: ApplyReport::default(),
    })
}

/// Replays only captured data; never creates a client, collects context or applies effects.
pub fn report(text: &str) -> Value {
    let definitions: Vec<_> = crate::flows::builtin::definitions()
        .into_iter()
        .map(|(d, t)| {
            d.validate(
                &crate::flows::catalogue::catalogue(),
                &Capabilities { images: false },
                &t,
            )
            .expect("builtin validates")
        })
        .collect();
    let mut compared = 0;
    let mut changed = Vec::new();
    let mut noncomparable = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value["kind"] != "trigger" {
            continue;
        }
        let id = value["trigger_id"].clone();
        let Some(definition) = definitions
            .iter()
            .find(|d| value["event"] == d.definition().flow.on)
        else {
            noncomparable.push(json!({"trigger_id":id,"reason":"no builtin trigger"}));
            continue;
        };
        let record = match capture(&value, definition) {
            Ok(r) => r,
            Err(e) => {
                noncomparable.push(json!({"trigger_id":id,"reason":e.to_string()}));
                continue;
            }
        };
        let result = instinct_flow::replay(definition, &record, &record.identity);
        let Some(outcome) = result.outcome.filter(|_| result.comparable) else {
            noncomparable.push(json!({"trigger_id":id,"nodes":result.nodes}));
            continue;
        };
        let action = outcome
            .trace
            .actions
            .first()
            .map(|a| {
                if a.action == "wake" {
                    "wake_cognition"
                } else {
                    a.action.as_str()
                }
            })
            .unwrap_or("silent");
        compared += 1;
        if value["requested_action"] != action {
            changed.push(
                json!({"trigger_id":id,"historical":value["requested_action"],"candidate":action}),
            );
        }
    }
    json!({"compared":compared,"changed":changed,"not_comparable":noncomparable})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_question_and_state_captures_choose_identical_actions() {
        let pipeline: crate::navigator::pipeline::PipelineFile =
            toml::from_str(include_str!("../default_pipeline/pipeline.toml")).unwrap();
        let trigger = &pipeline.triggers[0];
        let fields: BTreeMap<String, String> = crate::flows::migrate::TEMPLATE_FIELDS
            .iter()
            .map(|f| (f.to_string(), String::new()))
            .collect();
        let rendered = crate::navigator::pipeline::templates::render(
            include_str!("../default_pipeline/prompts/state.md"),
            &fields,
        );
        let mut records = Vec::new();
        for (help, stuck, worth, expected) in [
            (false, 0.0, 0.1, "silent"),
            (true, 0.0, 0.1, "remark"),
            (false, 0.9, 0.1, "wake_cognition"),
            (false, 0.0, 0.9, "remark"),
        ] {
            let answers: BTreeMap<_, _> = trigger.questions.keys().map(|id| {
                let answer = if id == "route" { json!({"type":"choice","choice":"metacognition","confidence":1.0,"probabilities":{"metacognition":1.0,"silent":0.0,"cognition":0.0}}) } else { json!({"type":"noul","noul":match id.as_str() { "stuck" => stuck, "worth_attention" => worth, _ => 0.0 }}) };
                (id.clone(), answer)
            }).collect();
            records.push(json!({"kind":"trigger","event":"save","trigger_id":expected,"uri":"file:///repo/main.rs","version":1,"model":"fake/instinct","questions":trigger.questions,"answers":answers,"rendered_state":rendered,"template_inputs":fields,"requested_action":expected,"prefilter":{"inputs":{"changed_lines":1,"since_last_remark_ms":100000,"since_last_wake_ms":200000,"called_for_help":help,"has_errors":false,"has_intent":false,"has_conversation":false}}}).to_string());
        }
        let result = report(&records.join("\n"));
        let snapshot = crate::navigator::pipeline::PipelineSnapshot::builtin().unwrap();
        for record in &records {
            let value: Value = serde_json::from_str(record).unwrap();
            let answers = serde_json::from_value(value["answers"].clone()).unwrap();
            let inputs = crate::navigator::TriggerInputs {
                changed_lines: 1,
                since_last_remark_ms: 100000,
                since_last_wake_ms: 200000,
                called_for_help: value["prefilter"]["inputs"]["called_for_help"]
                    .as_bool()
                    .unwrap(),
                has_errors: false,
                has_intent: false,
                has_conversation: false,
            };
            let (action, _) = snapshot.triggers()[0].select(inputs, &answers).unwrap();
            assert_eq!(value["requested_action"], action.as_str());
        }
        assert_eq!(result["not_comparable"], json!([]), "{result}");
        assert_eq!(result["changed"], json!([]), "{result}");
        assert_eq!(result["compared"], 4);
        let result = report(&records[0].replace("rendered_state", "missing_state"));
        assert_eq!(result["compared"], 0);
    }
}
