use super::*;
use crate::provider::openai::{RequestFunction, RequestToolCall};
use futures::future::BoxFuture;
use std::sync::Mutex;

struct Fake {
    requests: Mutex<Vec<PruneBatch>>,
    useful: f64,
    full: f64,
    fail: bool,
}
impl DecisionModel for Fake {
    fn decide<'a>(
        &'a self,
        state: &'a str,
        questions: &'a Questions,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Answers, crate::provider::systemone::DecisionError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(PruneBatch {
                state: state.into(),
                questions: questions.clone(),
            });
            if self.fail {
                return Err(crate::provider::systemone::DecisionError::Unavailable);
            }
            Ok(questions
                .keys()
                .map(|key| {
                    (
                        key.clone(),
                        Answer::Noul {
                            noul: if key.starts_with("useful") {
                                self.useful
                            } else {
                                self.full
                            },
                        },
                    )
                })
                .collect())
        })
    }
}
fn fake(useful: f64, full: f64) -> Fake {
    Fake {
        requests: Mutex::new(vec![]),
        useful,
        full,
        fail: false,
    }
}
fn exchange(id: usize, name: &str, args: Value, text: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::Assistant {
            content: None,
            reasoning_content: None,
            tool_calls: vec![RequestToolCall {
                id: id.to_string(),
                kind: "function".into(),
                function: RequestFunction {
                    name: name.into(),
                    arguments: args.to_string(),
                },
            }],
        },
        ChatMessage::Tool {
            tool_call_id: id.to_string(),
            content: text.into(),
            answered_by: None,
        },
    ]
}
fn output(messages: &[ChatMessage], index: usize) -> &str {
    match &messages[index] {
        ChatMessage::Tool { content, .. } => content,
        _ => panic!("result"),
    }
}
#[tokio::test]
async fn batches_share_bounded_state_and_never_score_tail_or_unpaired_calls() {
    let mut transcript = vec![ChatMessage::User {
        provenance: Default::default(),
        content: "task".repeat(30000),
        images: Vec::new(),
    }];
    for id in 0..20 {
        transcript.extend(exchange(
            id,
            "read",
            json!({"path":format!("p{id}")}),
            &"x".repeat(40000),
        ));
    }
    let end = transcript.len();
    transcript.extend(exchange(21, "read", json!({"path":"tail"}), "tail stays"));
    transcript.push(exchange(22, "read", json!({}), "").remove(0));
    let judge = fake(0.1, 0.9);
    let projection = prune(&transcript, 0, end, None, &judge, &CancellationToken::new())
        .await
        .unwrap();
    let requests = judge.requests.lock().unwrap();
    assert!(requests.len() > 1);
    assert_eq!(
        requests.iter().map(|r| r.questions.len()).sum::<usize>(),
        40
    );
    for request in requests.iter() {
        assert!(request_bytes(request) <= REQUEST_BYTES);
        assert!(serde_json::to_vec(&request.state).unwrap().len() <= STATE_BYTES);
        assert_eq!(request.state, requests[0].state);
        for key in request
            .questions
            .keys()
            .filter(|key| key.starts_with("useful"))
        {
            assert!(
                request
                    .questions
                    .contains_key(&key.replace("useful", "full"))
            );
        }
    }
    assert_eq!(
        serde_json::to_value(&projection[end..]).unwrap(),
        serde_json::to_value(&transcript[end..]).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&projection[1]).unwrap(),
        serde_json::to_value(&transcript[1]).unwrap()
    );
}
#[tokio::test]
async fn pruning_thresholds_failure_and_insufficient_savings() {
    let transcript = exchange(0, "read", json!({"path":"a"}), &"é".repeat(30000));
    let cancel = CancellationToken::new();
    let dropped = prune(&transcript, 0, 2, None, &fake(0.49, 1.0), &cancel)
        .await
        .unwrap();
    assert!(output(&dropped, 1).starts_with("[output elided"));
    let stub = prune(&transcript, 0, 2, None, &fake(0.5, 0.49), &cancel)
        .await
        .unwrap();
    assert!(output(&stub, 1).contains("shortened"));
    assert!(output(&stub, 1).chars().count() < 5100);
    assert!(
        prune(&transcript, 0, 2, None, &fake(0.5, 0.5), &cancel)
            .await
            .is_none()
    );
    let mut failed = fake(0.0, 0.0);
    failed.fail = true;
    assert!(
        prune(&transcript, 0, 2, None, &failed, &cancel)
            .await
            .is_none()
    );
    assert!(
        prune(&transcript, 0, 2, None, &fake(f64::NAN, 0.0), &cancel)
            .await
            .is_none()
    );
}
#[test]
fn pins_resolve_errors_only_on_same_target_and_preserve_latest_references() {
    let mut transcript = exchange(
        0,
        "read",
        json!({"path":"a"}),
        r#"{"status":"error","output":"failed"}"#,
    );
    transcript.extend(exchange(1, "read", json!({"path":"b"}), "ok"));
    transcript.extend(exchange(2, "skill", json!({"name":"old"}), "skill"));
    transcript.extend(exchange(3, "skill", json!({"name":"new"}), "skill"));
    transcript.extend(exchange(
        4,
        "bash",
        json!({"command":"git diff && git commit -m x"}),
        "commit",
    ));
    transcript.extend(exchange(
        5,
        "write",
        json!({"path":"written","content":"body"}),
        "ok",
    ));
    transcript.push(ChatMessage::User {
        provenance: Default::default(),
        content: "keep b".into(),
        images: Vec::new(),
    });
    let pins = pinned(&transcript);
    assert!(pins.contains(&1) && pins.contains(&3));
    assert!(!pins.contains(&5));
    assert!(pins.contains(&7) && pins.contains(&9) && pins.contains(&11));
    transcript.extend(exchange(6, "read", json!({"path":"a"}), "ok"));
    assert!(!pinned(&transcript).contains(&1));
}

#[tokio::test]
async fn mutation_heads_and_pinned_errors_skills_and_references_bypass_scoring() {
    let mut transcript = exchange(0, "read", json!({"path":"noise"}), &"n".repeat(80000));
    transcript.extend(exchange(
        1,
        "read",
        json!({"path":"failed"}),
        r#"{"status":"error","output":"unresolved"}"#,
    ));
    transcript.extend(exchange(2, "skill", json!({"name":"skill"}), "whole skill"));
    transcript.extend(exchange(
        3,
        "read",
        json!({"path":"important"}),
        "whole reference",
    ));
    transcript.extend(exchange(
        4,
        "edit",
        json!({"path":"file","old_string":"o".repeat(8000),"new_string":"v".repeat(8000)}),
        &"m".repeat(8000),
    ));
    transcript.extend(exchange(
        5,
        "write",
        json!({"path":"new","content":"b".repeat(8000)}),
        "written",
    ));
    let end = transcript.len();
    transcript.push(ChatMessage::User {
        provenance: Default::default(),
        content: "check important".into(),
        images: Vec::new(),
    });
    let judge = fake(0.0, 0.0);
    let projection = prune(&transcript, 0, end, None, &judge, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(judge.requests.lock().unwrap()[0].questions.len(), 2);
    for index in [3, 5, 7] {
        assert_eq!(output(&projection, index), output(&transcript, index));
    }
    assert!(output(&projection, 9).chars().count() < 650);
    let ChatMessage::Assistant { tool_calls, .. } = &projection[8] else {
        panic!("call")
    };
    let args: Value = serde_json::from_str(&tool_calls[0].function.arguments).unwrap();
    assert_eq!(args["path"], "file");
    assert_eq!(args["old_string"].as_str().unwrap().len(), 300);
    let ChatMessage::Assistant { tool_calls, .. } = &projection[10] else {
        panic!("call")
    };
    assert!(
        tool_calls[0]
            .function
            .arguments
            .contains("write body: 8000 bytes")
    );
    assert!(
        serde_json::to_string(&transcript)
            .unwrap()
            .contains(&"b".repeat(8000))
    );
}

#[tokio::test]
async fn pruning_uses_the_shared_scripted_decision_model() {
    use crate::provider::systemone::ScriptedDecisionModel;
    let transcript = exchange(0, "read", json!({"path":"a.rs"}), &"x".repeat(12_000));
    let answer = [
        ("useful_t0".into(), Answer::Noul { noul: 0.0 }),
        ("full_t0".into(), Answer::Noul { noul: 0.0 }),
    ]
    .into_iter()
    .collect();
    let model = ScriptedDecisionModel::new([Ok(answer)]);
    let _ = prune(
        &transcript,
        0,
        transcript.len(),
        None,
        &model,
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}
