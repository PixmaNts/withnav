//! One host policy for persisted captures and private HTTP responses.
use instinct_flow::{InputValue, RunRecord};
use serde_json::Value;

pub(crate) fn contains_secret(value: &Value) -> bool {
    match value {
        Value::String(text) => text
            .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ':' | '='))
            .any(|word| {
                [
                    "sk-",
                    "ghp_",
                    "github_pat_",
                    "gho_",
                    "xoxb-",
                    "xoxp-",
                    "AKIA",
                    "eyJ",
                ]
                .iter()
                .any(|prefix| word.starts_with(prefix) && word.len() >= 20)
            }),
        Value::Array(values) => values.iter().any(contains_secret),
        Value::Object(values) => values.iter().any(|(key, value)| {
            contains_secret(&Value::String(key.clone())) || contains_secret(value)
        }),
        _ => false,
    }
}
fn input(value: &mut InputValue) {
    if value.redacted || value.value.as_ref().is_some_and(contains_secret) {
        value.value = None;
        value.cut = false;
        value.redacted = true;
    }
}
pub(crate) fn response(value: &mut Value) {
    match value {
        Value::String(_) if contains_secret(value) => *value = "<redacted>".into(),
        Value::Array(values) => values.iter_mut().for_each(response),
        Value::Object(values) => {
            let keys: Vec<_> = values
                .keys()
                .filter(|key| contains_secret(&Value::String((*key).clone())))
                .cloned()
                .collect();
            let mut index = 0;
            for key in keys {
                let replacement = loop {
                    index += 1;
                    let candidate = format!("<redacted-key-{index}>");
                    if !values.contains_key(&candidate) {
                        break candidate;
                    }
                };
                if let Some(value) = values.remove(&key) {
                    values.insert(replacement, value);
                }
            }
            values.values_mut().for_each(response);
        }
        _ => {}
    }
}
/// Mutate only the capture, never the live outcome's proposed actions.
pub(crate) fn record(record: &mut RunRecord) {
    if let Value::Object(fields) = &record.event.payload {
        for (name, value) in fields {
            let path = if contains_secret(&Value::String(name.clone())) {
                Some("event".to_owned())
            } else if contains_secret(value) {
                Some(format!("event.{name}"))
            } else {
                None
            };
            if let Some(path) = path {
                if !record.redacted_event_paths.contains(&path) {
                    record.redacted_event_paths.push(path);
                }
            }
        }
    } else if contains_secret(&record.event.payload) {
        record.redacted_event_paths.push("event".into());
    }
    for value in record.inputs.values_mut() {
        input(value);
    }
    for node in &mut record.nodes {
        let mut redacted = false;
        if let Some(inputs) = &mut node.selected_inputs {
            for selected in inputs.values_mut() {
                input(&mut selected.captured);
                redacted |= selected.captured.redacted;
            }
        }
        if redacted {
            for batch in &mut record.batches {
                if batch.node_ids.contains(&node.id) {
                    batch.redacted = true;
                    batch.state = "<state omitted: redacted>".into();
                }
            }
        }
    }
    for batch in &mut record.batches {
        if contains_secret(&Value::String(batch.state.clone())) {
            batch.redacted = true;
            batch.state = "<state omitted: redacted>".into();
        }
    }
    // Also cover payloads, question text, outputs, failure messages and applied notes.
    if let Ok(mut value) = serde_json::to_value(&*record) {
        response(&mut value);
        if let Ok(safe) = serde_json::from_value(value) {
            *record = safe;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use instinct_flow::*;
    use std::{
        collections::BTreeMap,
        time::{Duration, Instant},
    };
    use tokio_util::sync::CancellationToken;
    struct Client;
    impl DecisionClient for Client {
        fn capabilities(&self) -> Capabilities {
            Capabilities { images: false }
        }
        fn identity(&self) -> ClientIdentity {
            ClientIdentity {
                model: "test".into(),
                variant: None,
            }
        }
        fn decide<'a>(
            &'a self,
            _: protocol::DecisionRequest<'a>,
            _: &'a CancellationToken,
            _: Instant,
        ) -> BoxFuture<'a, Result<protocol::DecisionResponse, DecisionError>> {
            Box::pin(async { Err(DecisionError::ProviderError) })
        }
    }
    struct Inputs;
    impl InputSource for Inputs {
        fn collect<'a>(
            &'a self,
            _: &'a [String],
            _: &'a CancellationToken,
            _: Instant,
        ) -> BoxFuture<'a, Result<BTreeMap<String, InputValue>, InputError>> {
            Box::pin(async { Ok(BTreeMap::new()) })
        }
    }
    #[test]
    fn response_redacts_nested_keys_without_overwriting_existing_placeholders() {
        let secret = "ghp_123456789012345678901234";
        let mut value = serde_json::json!({"args":[{
            secret: {"nested": secret},
            "<redacted-key-1>": "keep"
        }]});
        assert!(contains_secret(&value));
        response(&mut value);
        assert!(!serde_json::to_string(&value).unwrap().contains(secret));
        assert_eq!(value["args"][0]["<redacted-key-1>"], "keep");
        assert_eq!(value["args"][0]["<redacted-key-2>"]["nested"], "<redacted>");
        let once = value.clone();
        response(&mut value);
        assert_eq!(value, once);
    }

    #[tokio::test]
    async fn replay_rejects_redacted_event_evidence_for_every_consumer() {
        for (gate, body, reference, missing) in [
            ("", "type='expression'\ncondition='x == \"<redacted>\"'\n[node.inputs]\nx='event.text'\n", "event.text", true),
            ("when='x == \"<redacted>\"'\n[flow.inputs]\nx='event.text'\n", "type='expression'\ncondition='true'\n", "event.text", true),
            ("", "type='action'\naction='log'\n[node.params]\ntext='{x}'\n[node.inputs]\nx='event.text'\n", "event.text", true),
            ("", "type='noul'\ninstructions='test'\nthreshold=0.5\n[node.inputs]\nx='event.text'\n", "event.text", true),
            ("", "type='expression'\ncondition='x == \"driver\"'\n[node.inputs]\nx='event.author'\n", "event.author", false),
            ("when='x == \"driver\"'\n[flow.inputs]\nx='event.author'\n", "type='expression'\ncondition='true'\n", "event.author", false),
        ] {
            let text = format!("[flow]\nversion=1\nname='secret'\non='message.received'\nblocking=true\nbudget_ms=100\nstart=['a']\n{gate}[[node]]\nid='a'\n{body}");
            let compiled = Definition::parse(&text).unwrap().validate(
                &crate::flows::catalogue::catalogue(),
                &Capabilities { images: false },
                &BTreeMap::new(),
            ).unwrap();
            let event = Event {
                trigger: "message.received".into(),
                payload: serde_json::json!({"command_id":"test","author":"driver","text":"sk-123456789012345678901234"}),
            };
            let mut outcome = run(&compiled, &event, &Inputs, &Client, RunOptions {
                deadline: Instant::now() + Duration::from_secs(1),
                run_id: "test".into(),
            }, &CancellationToken::new()).await;
            let live = serde_json::to_value(&outcome.trace).unwrap();
            assert!(live.get("redacted_event_paths").is_none());
            let old: RunRecord = serde_json::from_value(live).unwrap();
            assert!(old.redacted_event_paths.is_empty());
            record(&mut outcome.trace);
            assert_eq!(outcome.trace.redacted_event_paths, ["event.text"]);
            let replayed = replay(&compiled, &outcome.trace, &Client.identity());
            assert_eq!(replayed.comparable, !missing, "{reference}: {gate}{body}");
            assert_eq!(replayed.outcome.is_none(), missing);
            if missing {
                assert_eq!(replayed.nodes[0].status, "missing_capture");
            }
            assert_eq!(event.payload["text"], "sk-123456789012345678901234");
        }
    }
    #[tokio::test]
    async fn event_evidence_and_states_are_redacted_without_changing_live_event() {
        let definition = Definition::parse("[flow]\nversion=1\nname='secret'\non='tool.pre_call'\nblocking=true\nbudget_ms=100\nstart=['a']\n[[node]]\nid='a'\ntype='noul'\ninstructions='test'\nthreshold=0.5\n[node.inputs]\nargs='event.args'\n").unwrap();
        let compiled = definition
            .validate(
                &crate::flows::catalogue::catalogue(),
                &Capabilities { images: false },
                &BTreeMap::new(),
            )
            .unwrap();
        let secret = "sk-123456789012345678901234";
        let event = Event {
            trigger: "tool.pre_call".into(),
            payload: serde_json::json!({"tool":"bash","args":{secret:"safe","key":secret},"call_id":"test"}),
        };
        let mut outcome = run(
            &compiled,
            &event,
            &Inputs,
            &Client,
            RunOptions {
                deadline: Instant::now() + Duration::from_secs(1),
                run_id: "test".into(),
            },
            &CancellationToken::new(),
        )
        .await;
        record(&mut outcome.trace);
        assert!(!serde_json::to_string(&outcome.trace)
            .unwrap()
            .contains(secret));
        assert!(
            outcome.trace.nodes[0].selected_inputs.as_ref().unwrap()["args"]
                .captured
                .redacted
        );
        assert!(outcome.trace.batches[0].redacted);
        assert_eq!(outcome.trace.redacted_event_paths, ["event.args"]);
        assert_eq!(
            replay(&compiled, &outcome.trace, &Client.identity()).nodes[0].status,
            "missing_capture"
        );
        assert_eq!(event.payload["args"][secret], "safe");
        assert_eq!(event.payload["args"]["key"], secret);
    }
}
