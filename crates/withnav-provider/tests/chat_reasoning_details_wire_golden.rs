//! Wire-golden lock for OpenRouter's `reasoning_details` on Chat Completions:
//! recorded streams (2026-11-09, three reasoning models through OmniRoute,
//! `tests/fixtures/openrouter-reasoning/`) assemble into the expected opaque
//! items, the continuation body carries them back for the model that produced
//! them and only for it, and no opaque byte reaches an event or an error.

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use withnav_provider::ChatTurn;
use withnav_provider::StreamDelta;
use withnav_provider::client::ModelClient;
use withnav_provider::config::ProviderConfig;
use withnav_provider::error::ProviderError;
use withnav_provider::openai::{
    ChatMessage, ChatRequest, ClientCache, OpaqueReasoning, Producer, RequestFunction,
    RequestToolCall,
};

const ANTHROPIC: &str = include_str!("fixtures/openrouter-reasoning/anthropic-stream.sse");
const OPENAI: &str = include_str!("fixtures/openrouter-reasoning/openai-stream.sse");
const DEEPSEEK: &str = include_str!("fixtures/openrouter-reasoning/deepseek-stream.sse");

const ANTHROPIC_MODEL: &str = "anthropic/claude-haiku-4.5";

fn client(server: &MockServer) -> (ModelClient, String) {
    let base = format!("{}/v1", server.uri());
    let provider = ProviderConfig {
        base_url: base.clone(),
        api_key_env: None,
        headers: Default::default(),
        connect_timeout_ms: 5_000,
        request_timeout_ms: 30_000,
        catalog_format: None,
    };
    let client = ModelClient::new("p", &provider, "key".into(), &ClientCache::new()).unwrap();
    (client, base)
}

fn request(model: &str, messages: Vec<ChatMessage>) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages,
        tools: vec![],
        stream: true,
        stream_options: None,
        max_tokens: None,
        temperature: None,
        extra: Default::default(),
        images: Default::default(),
    }
}

fn user(content: &str) -> ChatMessage {
    ChatMessage::User {
        content: content.into(),
        provenance: Default::default(),
        images: vec![],
    }
}

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.to_string(), "text/event-stream")
}

/// Plays `stream` for one call and returns the turn with every delta shown.
async fn play(stream: &str, model: &str) -> (ChatTurn, Vec<StreamDelta>) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(stream))
        .mount(&server)
        .await;
    let (client, _) = client(&server);
    let mut deltas = Vec::new();
    let turn = client
        .chat_turn(
            request(model, vec![user("17*23, think first")]),
            None,
            &CancellationToken::new(),
            &mut |d| deltas.push(d),
        )
        .await
        .unwrap();
    (turn, deltas)
}

fn entries(turn: &ChatTurn) -> Vec<(String, Value)> {
    turn.opaque
        .iter()
        .map(|i| (i.kind.clone(), serde_json::from_str(&i.data).unwrap()))
        .collect()
}

/// The `reasoning_details` of every frame of a recorded stream, in order.
fn recorded_fragments(stream: &str) -> Vec<Value> {
    stream
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|v| {
            v.pointer("/choices/0/delta/reasoning_details")
                .filter(|d| d.is_array())
                .cloned()
        })
        .flat_map(|d| d.as_array().unwrap().clone())
        .collect()
}

fn recorded_field(stream: &str, key: &str) -> String {
    recorded_fragments(stream)
        .iter()
        .find_map(|f| f.get(key).and_then(Value::as_str).map(String::from))
        .unwrap()
}

#[tokio::test]
async fn an_anthropic_stream_assembles_one_signed_text_entry() {
    let (turn, _) = play(ANTHROPIC, ANTHROPIC_MODEL).await;
    let got = entries(&turn);
    assert_eq!(got.len(), 1);
    let (kind, entry) = &got[0];
    assert_eq!(kind, "reasoning_details");
    assert_eq!(entry["type"], "reasoning.text");
    assert_eq!(entry["format"], "anthropic-claude-v1");
    assert_eq!(entry["index"], 0);
    assert_eq!(entry["text"], turn.message.thinking);
    assert_eq!(entry["signature"], recorded_field(ANTHROPIC, "signature"));
    assert!(recorded_fragments(ANTHROPIC).len() > 5);
}

#[tokio::test]
async fn an_openai_stream_assembles_a_summary_and_an_encrypted_entry_held_back_from_replay() {
    let (turn, _) = play(OPENAI, "openai/gpt-5-nano").await;
    let got = entries(&turn);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].0, "reasoning_details.openai-responses-v1");
    assert_eq!(got[0].1["type"], "reasoning.summary");
    assert_eq!(got[0].1["index"], 0);
    let summary: String = recorded_fragments(OPENAI)
        .iter()
        .filter(|f| f["type"] == "reasoning.summary")
        .filter_map(|f| f["summary"].as_str())
        .collect();
    assert_eq!(got[0].1["summary"], summary);
    assert_eq!(got[1].0, "reasoning_details.openai-responses-v1");
    assert_eq!(got[1].1["type"], "reasoning.encrypted");
    assert_eq!(got[1].1["index"], 1);
    assert_eq!(got[1].1["data"], recorded_field(OPENAI, "data"));
    assert_eq!(got[1].1["id"], recorded_field(OPENAI, "id"));
}

#[tokio::test]
async fn a_deepseek_stream_assembles_unsigned_text() {
    let (turn, _) = play(DEEPSEEK, "deepseek/deepseek-r1-0528").await;
    let got = entries(&turn);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, "reasoning_details");
    assert_eq!(got[0].1["text"], turn.message.thinking);
    assert!(got[0].1.get("signature").is_none());
}

#[tokio::test]
async fn no_opaque_byte_reaches_the_message_or_a_delta() {
    for (stream, model, key) in [
        (ANTHROPIC, ANTHROPIC_MODEL, "signature"),
        (OPENAI, "openai/gpt-5-nano", "data"),
    ] {
        let secret = recorded_field(stream, key);
        let (turn, deltas) = play(stream, model).await;
        assert!(!format!("{:?}", turn.message).contains(&secret));
        assert!(!format!("{deltas:?}").contains(&secret));
    }
}

fn assistant_with(turn: &ChatTurn, base: &str, model: &str) -> ChatMessage {
    ChatMessage::Assistant {
        content: None,
        reasoning_content: Some(turn.message.thinking.clone()),
        tool_calls: vec![RequestToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: RequestFunction {
                name: "bash".into(),
                arguments: "{}".into(),
            },
        }],
        opaque: Some(OpaqueReasoning {
            producer: Producer::chat_completions(base, model),
            items: turn.opaque.clone(),
        }),
    }
}

/// The continuation request's body, as the server received it.
async fn continuation_body(
    turn: &ChatTurn,
    produced_by: &str,
    sent_to: &str,
    base_override: Option<&str>,
) -> String {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse("data: [DONE]\n\n"))
        .mount(&server)
        .await;
    let (client, base) = client(&server);
    let history = vec![
        user("go"),
        assistant_with(turn, base_override.unwrap_or(&base), produced_by),
        ChatMessage::Tool {
            tool_call_id: "call_1".into(),
            content: "ok".into(),
            answered_by: None,
            images: vec![],
        },
    ];
    client
        .chat_turn(
            request(sent_to, history),
            None,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    String::from_utf8(requests[0].body.clone()).unwrap()
}

/// Locks `body` byte for byte against `tests/fixtures/openrouter-reasoning/<name>`;
/// run with `WITHNAV_BLESS=1` to rewrite the file after an intended change.
fn assert_golden(name: &str, body: &str) {
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/openrouter-reasoning")
        .join(name);
    if std::env::var_os("WITHNAV_BLESS").is_some() {
        std::fs::write(&file, body).unwrap();
    }
    let expected = std::fs::read_to_string(&file).unwrap();
    assert!(body == expected, "{name} differs from the request sent");
}

#[tokio::test]
async fn the_continuation_carries_reasoning_details_for_the_same_model() {
    let (turn, _) = play(ANTHROPIC, ANTHROPIC_MODEL).await;
    let body = continuation_body(&turn, ANTHROPIC_MODEL, ANTHROPIC_MODEL, None).await;
    assert_golden("continuation-same-model.json", &body);
    let sent: Value = serde_json::from_str(&body).unwrap();
    let expected: Vec<Value> = entries(&turn).into_iter().map(|(_, v)| v).collect();
    assert_eq!(sent["messages"][1]["reasoning_details"], json!(expected));
    assert_eq!(
        sent["messages"][1]["reasoning_details"][0]["signature"],
        recorded_field(ANTHROPIC, "signature")
    );
}

#[tokio::test]
async fn the_continuation_for_another_model_or_endpoint_has_no_reasoning_details() {
    let (turn, _) = play(ANTHROPIC, ANTHROPIC_MODEL).await;
    let other_model =
        continuation_body(&turn, ANTHROPIC_MODEL, "anthropic/claude-sonnet-4.6", None).await;
    assert_golden("continuation-other-model.json", &other_model);
    let other_endpoint = continuation_body(
        &turn,
        ANTHROPIC_MODEL,
        ANTHROPIC_MODEL,
        Some("https://elsewhere.example/v1"),
    )
    .await;
    assert_golden("continuation-other-endpoint.json", &other_endpoint);
    for body in [&other_model, &other_endpoint] {
        let sent: Value = serde_json::from_str(body).unwrap();
        assert!(sent["messages"][1].get("reasoning_details").is_none());
        assert!(!body.contains(&recorded_field(ANTHROPIC, "signature")));
    }
}

#[tokio::test]
async fn an_openai_responses_entry_is_captured_but_never_sent() {
    let (turn, _) = play(OPENAI, "openai/gpt-5-nano").await;
    let body = continuation_body(&turn, "openai/gpt-5-nano", "openai/gpt-5-nano", None).await;
    assert_golden("continuation-openai-responses.json", &body);
    assert!(!body.contains("reasoning_details"));
    assert!(!body.contains(&recorded_field(OPENAI, "data")));
}

#[tokio::test]
async fn an_upstream_error_that_quotes_the_request_does_not_leak_the_signature() {
    let (turn, _) = play(ANTHROPIC, ANTHROPIC_MODEL).await;
    let signature = recorded_field(ANTHROPIC, "signature");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": format!("bad reasoning_details: {signature}")}
        })))
        .mount(&server)
        .await;
    let (client, base) = client(&server);
    let history = vec![user("go"), assistant_with(&turn, &base, ANTHROPIC_MODEL)];
    let err = client
        .chat_turn(
            request(ANTHROPIC_MODEL, history),
            None,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .unwrap_err();
    let ProviderError::Api { message, .. } = err else {
        panic!("expected an API error");
    };
    assert!(message.starts_with("bad reasoning_details:"), "{message}");
    assert!(!message.contains(&signature));
    assert!(!message.contains(&signature[..60]));
}

#[tokio::test]
async fn an_upstream_error_that_quotes_part_of_the_signature_does_not_leak_it() {
    let (turn, _) = play(ANTHROPIC, ANTHROPIC_MODEL).await;
    let signature = recorded_field(ANTHROPIC, "signature");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(502).set_body_json(json!({
            "error": {"message": format!("cannot parse {{\"signature\":\"{}", &signature[..200])}
        })))
        .mount(&server)
        .await;
    let (client, base) = client(&server);
    let history = vec![user("go"), assistant_with(&turn, &base, ANTHROPIC_MODEL)];
    let err = client
        .chat_turn(
            request(ANTHROPIC_MODEL, history),
            None,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .unwrap_err();
    let ProviderError::Api { message, .. } = err else {
        panic!("expected an API error");
    };
    assert!(message.starts_with("cannot parse"), "{message}");
    assert!(!message.contains(&signature[..60]), "{message}");
}
