use super::*;
use crate::provider::openai::{RequestFunction, RequestToolCall};
use crate::provider::test_support::{http_sse, sse_body};
use crate::provider::ToolCall;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

fn user(text: &str) -> ChatMessage {
    ChatMessage::User {
        content: text.into(),
    }
}

fn call(id: &str, name: &str, args: &str) -> ChatMessage {
    ChatMessage::Assistant {
        content: None,
        reasoning_content: None,
        tool_calls: vec![RequestToolCall {
            id: id.into(),
            kind: "function".into(),
            function: RequestFunction {
                name: name.into(),
                arguments: args.into(),
            },
        }],
    }
}

fn answer(text: &str) -> ChatMessage {
    ChatMessage::Assistant {
        content: Some(text.into()),
        reasoning_content: None,
        tool_calls: vec![],
    }
}

fn result(id: &str, bytes: usize) -> ChatMessage {
    ChatMessage::Tool {
        tool_call_id: id.into(),
        content: "x".repeat(bytes),
    }
}

fn five_turns(bytes: usize) -> Vec<ChatMessage> {
    let mut t = Vec::new();
    for n in 0..5 {
        t.push(user(&format!("request {n}")));
        t.push(call(&format!("c{n}"), "read", "{\"path\":\"a\"}"));
        t.push(result(&format!("c{n}"), bytes));
        t.push(answer(&format!("done {n}")));
    }
    t
}

async fn serve(responses: Vec<Vec<u8>>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        for response in responses {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                let n = sock.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                let s = String::from_utf8_lossy(&buf).into_owned();
                if let Some(idx) = s.find("\r\n\r\n") {
                    let len = s[..idx]
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if buf.len() >= idx + 4 + len {
                        break;
                    }
                }
            }
            log.lock()
                .await
                .push(String::from_utf8_lossy(&buf).into_owned());
            let _ = sock.write_all(&response).await;
            let _ = sock.shutdown().await;
        }
    });
    (url, seen)
}

fn provider(base_url: String) -> ProviderConfig {
    ProviderConfig {
        base_url,
        api_key_env: None,
        headers: BTreeMap::new(),
        connect_timeout_ms: 2000,
        request_timeout_ms: 600_000,
        catalog_format: None,
    }
}

fn text_answer(text: &str) -> Vec<u8> {
    let frame = serde_json::json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}]})
        .to_string();
    http_sse(&sse_body(&[&frame]))
}

fn input<'a>(
    provider: &'a ProviderConfig,
    transcript: &'a [ChatMessage],
    variant: Option<&'a Variant>,
) -> PrepareInput<'a> {
    PrepareInput {
        provider,
        api_key: "k".into(),
        model: "m",
        variant,
        max_tokens: Some(512),
        transcript,
        current: None,
        components: Components {
            system_prompt: None,
            rules: None,
            skills: None,
            tools: &[],
        },
        destination_budget: 100_000,
        aim: Aim::Fit,
        source_budget: 100_000,
        focus: None,
    }
}

#[tokio::test]
async fn prepare_asks_the_model_once_without_tools_and_returns_a_checkpoint() {
    let (url, seen) = serve(vec![text_answer("Goal: ship it.\nNext: run tests.")]).await;
    let provider = provider(url);
    let t = five_turns(200);
    let mut i = input(&provider, &t, None);
    // Room for the summary and about the last three turns.
    i.destination_budget = crate::agent::compaction::summary_reserve_tokens() + 400;
    let prepared = prepare(i, &CancellationToken::new()).await.unwrap();
    let k = &prepared.compaction.checkpoint;
    assert_eq!(k.summary, "Goal: ship it.\nNext: run tests.");
    assert_eq!(k.generation, 1);
    assert!(k.through_message > 0 && k.through_message < 12);
    assert!(prepared.estimated_after < prepared.estimated_before);

    let seen = seen.lock().await;
    assert_eq!(seen.len(), 1);
    let body = seen[0].split("\r\n\r\n").nth(1).unwrap();
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(v["model"], "m");
    assert!(v.get("tools").is_none());
    assert_eq!(v["messages"][0]["role"], "system");
    assert!(v["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("Skills to reload"));
    assert_eq!(v["messages"][1]["role"], "user");
    assert!(v["messages"][1]["content"]
        .as_str()
        .unwrap()
        .contains("request 0"));
}

#[tokio::test]
async fn the_summary_request_carries_the_sessions_variant() {
    let (url, seen) = serve(vec![text_answer("s")]).await;
    let provider = provider(url);
    let variant: Variant =
        serde_json::from_value(serde_json::json!({"reasoning_effort": "high"})).unwrap();
    let t = five_turns(200);
    prepare(
        input(&provider, &t, Some(&variant)),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(seen.lock().await[0].contains("\"reasoning_effort\":\"high\""));
}

#[tokio::test]
async fn an_oversized_source_cuts_the_biggest_outputs_of_the_part_to_summarize() {
    let t = {
        let mut t = five_turns(100);
        t[2] = result("c0", 40_000);
        t
    };
    let provider = provider("http://127.0.0.1:1/v1".into());
    let mut i = input(&provider, &t, None);
    i.destination_budget = crate::agent::compaction::summary_reserve_tokens() + 330;
    let planned = plan(&t, None, &i.components, i.destination_budget, Aim::Fit).unwrap();
    assert!(planned.through_message > 2);
    i.source_budget = 2_000;
    let req = build_summary_request(&i, &planned).unwrap();
    let ChatMessage::User { content } = &req.messages[1] else {
        panic!("user message expected");
    };
    assert!(content.contains("[output elided to save context: 40000 bytes]"));
    assert!(req.tools.is_empty());
    i.source_budget = 10;
    assert!(matches!(
        build_summary_request(&i, &planned),
        Err(CompactionError::SummaryInputTooLarge { .. })
    ));
}

#[tokio::test]
async fn a_second_summary_carries_the_first_and_counts_generations() {
    let (url, seen) = serve(vec![text_answer("second")]).await;
    let provider = provider(url);
    let t = five_turns(200);
    let first = Compaction {
        checkpoint: Checkpoint {
            through_message: 3,
            summary: "first summary".into(),
            generation: 1,
        },
        pruned_message_indices: vec![],
    };
    let mut i = input(&provider, &t, None);
    i.current = Some(&first);
    let prepared = prepare(i, &CancellationToken::new()).await.unwrap();
    assert_eq!(prepared.compaction.checkpoint.generation, 2);
    assert!(prepared.compaction.checkpoint.through_message > 3);
    let seen = seen.lock().await;
    assert!(seen[0].contains("first summary"));
    assert!(!seen[0].contains("request 0"));
}

#[tokio::test]
async fn an_invalid_summary_is_refused() {
    let t = five_turns(200);
    let provider_cfg = |url| provider(url);
    let (url, _) = serve(vec![text_answer("   ")]).await;
    let cfg = provider_cfg(url);
    let err = prepare(input(&cfg, &t, None), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, CompactionError::InvalidSummary(_)));
}

#[test]
fn validation_rules() {
    let msg = |text: &str| AssistantMessage {
        text: text.into(),
        finish_reason: Some("stop".into()),
        ..Default::default()
    };
    assert_eq!(validate_summary(&msg("  ok \n")).unwrap(), "ok");
    assert!(validate_summary(&msg("")).is_err());
    assert!(validate_summary(&msg(&"a".repeat(SUMMARY_MAX_BYTES))).is_ok());
    assert!(validate_summary(&msg(&"a".repeat(SUMMARY_MAX_BYTES + 1))).is_err());
    let mut with_call = msg("text");
    with_call.tool_calls.push(ToolCall {
        id: "c".into(),
        name: "read".into(),
        args: serde_json::json!({}),
        args_valid: true,
    });
    assert!(validate_summary(&with_call).is_err());
    let mut cut = msg("partial");
    cut.finish_reason = Some("length".into());
    assert!(validate_summary(&cut).is_err());
}

#[test]
fn a_focus_is_added_to_the_summary_prompt() {
    let t = five_turns(100);
    let provider = provider("http://127.0.0.1:1/v1".into());
    let mut i = input(&provider, &t, None);
    let planned = plan(&t, None, &i.components, i.destination_budget, Aim::Fit).unwrap();
    let text = |i: &PrepareInput<'_>| match &build_summary_request(i, &planned).unwrap().messages[0]
    {
        ChatMessage::System { content } => content.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(text(&i), SUMMARY_PROMPT);
    i.focus = Some("  the migration  ");
    assert!(text(&i).ends_with("pay particular attention to: the migration"));
}
