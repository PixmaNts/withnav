use serde_json::Value;

use crate::provider::{AssistantMessage, ToolCall};

pub const ASK_TOOL: &str = "ask";

#[derive(Debug, Clone, PartialEq)]
pub struct HumanRequest {
    pub tool_call_id: String,
    pub question: String,
    pub kind: Option<String>,
    pub details: Option<Value>,
    pub choices: Option<crate::tools::control::AskChoices>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StepOutcome {
    Continue { tool_calls: Vec<ToolCall> },
    Done { response: AssistantMessage },
    Yield { request: HumanRequest },
}

pub fn classify(msg: AssistantMessage) -> StepOutcome {
    classify_with_ask(msg, true)
}

pub fn classify_with_ask(msg: AssistantMessage, allow_ask: bool) -> StepOutcome {
    classify_with(msg, |name| name == ASK_TOOL && allow_ask)
}

/// Whether a call pauses the turn for the Captain: an `ask`, or a valid `request_secret`.
/// An invalid secret request is not a question: it is settled as an error the model reads.
pub fn is_human_call(call: &ToolCall) -> bool {
    call.name == ASK_TOOL
        || (call.name == crate::tools::secret::REQUEST_SECRET_TOOL
            && crate::tools::secret::validated_request(&call.args).is_ok())
}

/// [`classify_with_ask`] for every human call `allowed` lets through.
pub fn classify_with(msg: AssistantMessage, allowed: impl Fn(&str) -> bool) -> StepOutcome {
    if let Some(ask) = msg
        .tool_calls
        .iter()
        .find(|tc| is_human_call(tc) && allowed(&tc.name))
    {
        return StepOutcome::Yield {
            request: human_request_from(ask),
        };
    }
    if !msg.tool_calls.is_empty() {
        return StepOutcome::Continue {
            tool_calls: msg.tool_calls,
        };
    }
    StepOutcome::Done { response: msg }
}

pub(crate) fn human_request_from(ask: &ToolCall) -> HumanRequest {
    if ask.name == crate::tools::secret::REQUEST_SECRET_TOOL
        && let Ok(details) = crate::tools::secret::validated_request(&ask.args)
    {
        return HumanRequest {
            tool_call_id: ask.id.clone(),
            question: crate::tools::secret::question_text(&details),
            kind: Some(crate::secrets::SECRET_KIND.to_owned()),
            details: serde_json::to_value(&details).ok(),
            choices: None,
        };
    }
    let question = ask
        .args
        .get("question")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Only `request_secret` raises a secret card: an `ask` cannot claim to be one.
    let kind = ask
        .args
        .get("kind")
        .and_then(|v| v.as_str())
        .filter(|kind| *kind != crate::secrets::SECRET_KIND)
        .map(String::from);
    let details = ask.args.get("details").cloned();
    HumanRequest {
        tool_call_id: ask.id.clone(),
        question,
        kind,
        details,
        choices: crate::tools::control::validated_choices(&ask.args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tc(id: &str, name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            args,
            args_valid: true,
        }
    }

    fn msg(tool_calls: Vec<ToolCall>, text: &str) -> AssistantMessage {
        AssistantMessage {
            text: text.into(),
            tool_calls,
            ..Default::default()
        }
    }

    #[test]
    fn ask_toolcall_yields() {
        let m = msg(
            vec![tc("call_1", "ask", json!({"question": "insert SD card"}))],
            "",
        );
        match classify(m) {
            StepOutcome::Yield { request } => {
                assert_eq!(request.question, "insert SD card");
                assert_eq!(request.tool_call_id, "call_1");
            }
            other => panic!("expected Yield, got {other:?}"),
        }
    }

    #[test]
    fn ask_yield_preserves_valid_choices_and_drops_malformed_choices() {
        for (args, has_choices) in [
            (
                json!({"question":"Where?", "options":[{"label":"A"},{"label":"B"}]}),
                true,
            ),
            (
                json!({"question":"Where?", "options":[{"label":"A"}]}),
                false,
            ),
        ] {
            let StepOutcome::Yield { request } = classify(msg(vec![tc("ask_1", "ask", args)], ""))
            else {
                panic!("ask must yield");
            };
            assert_eq!(request.question, "Where?");
            assert_eq!(request.choices.is_some(), has_choices);
        }
    }

    #[test]
    fn bash_toolcall_continues() {
        let m = msg(vec![tc("c", "bash", json!({"command": "ls"}))], "");
        assert!(matches!(classify(m), StepOutcome::Continue { .. }));
    }

    #[test]
    fn text_only_is_done() {
        let m = msg(vec![], "all finished");
        match classify(m) {
            StepOutcome::Done { response } => assert_eq!(response.text, "all finished"),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn ask_wins_over_sibling_toolcall() {
        let m = msg(
            vec![
                tc("c1", "bash", json!({"command": "ls"})),
                tc("c2", "ask", json!({"question": "continue?"})),
            ],
            "",
        );
        match classify(m) {
            StepOutcome::Yield { request } => assert_eq!(request.question, "continue?"),
            other => panic!("expected Yield (ask wins), got {other:?}"),
        }
    }

    #[test]
    fn a_valid_secret_request_yields_a_secret_question_and_an_invalid_one_runs() {
        let args = json!({"name":"API_KEY","label":"Key","reason":"the tests"});
        let StepOutcome::Yield { request } =
            classify_with(msg(vec![tc("s1", "request_secret", args)], ""), |_| true)
        else {
            panic!("a secret request must yield");
        };
        assert_eq!(request.kind.as_deref(), Some("secret"));
        assert_eq!(request.details.unwrap()["name"], "API_KEY");
        let invalid = msg(
            vec![tc(
                "s2",
                "request_secret",
                json!({"name":"path","label":"x","reason":"y"}),
            )],
            "",
        );
        assert!(matches!(
            classify_with(invalid, |_| true),
            StepOutcome::Continue { .. }
        ));
        let disallowed = msg(
            vec![tc(
                "s3",
                "request_secret",
                json!({"name":"API_KEY","label":"x","reason":"y"}),
            )],
            "",
        );
        assert!(matches!(
            classify_with(disallowed, |name| name == "ask"),
            StepOutcome::Continue { .. }
        ));
    }

    #[test]
    fn an_ask_cannot_claim_the_secret_kind() {
        let StepOutcome::Yield { request } = classify(msg(
            vec![tc("a", "ask", json!({"question":"key?","kind":"secret"}))],
            "",
        )) else {
            panic!("ask must yield");
        };
        assert_eq!(request.kind, None);
    }
}
