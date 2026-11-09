//! Images through the agent loop: what a request carries for a Captain message with images,
//! under each condition the projection decides on.

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::tests::{make_actor, registry_all, spawn_mock, text_chunk};
use super::*;
use crate::agent::images::AttachmentLoader;
use crate::provider::openai::{ImagePayload, ImageRef, UserProvenance};
use crate::provider::test_support::{http_sse, sse_body};

fn image(id: &str) -> ImageRef {
    ImageRef {
        attachment_id: id.into(),
        media_type: "image/png".into(),
        name: format!("{id}.png"),
        size: 4,
        width: 2,
        height: 2,
    }
}

struct FakeLoader {
    fail: Vec<&'static str>,
}

impl AttachmentLoader for FakeLoader {
    fn load(
        &self,
        ids: Vec<String>,
    ) -> BoxFuture<'static, HashMap<String, Result<ImagePayload, String>>> {
        let fail = self.fail.clone();
        Box::pin(async move {
            ids.into_iter()
                .map(|id| {
                    let result = if fail.contains(&id.as_str()) {
                        Err("row missing".to_string())
                    } else {
                        ImagePayload::from_bytes("image/png", b"\x89PNG")
                    };
                    (id, result)
                })
                .collect()
        })
    }

    fn store(&self, name: String, bytes: Vec<u8>) -> BoxFuture<'static, Result<ImageRef, String>> {
        Box::pin(async move {
            if bytes.is_empty() {
                return Err("invalid image: empty".into());
            }
            Ok(ImageRef {
                attachment_id: format!("stored-{name}"),
                media_type: "image/png".into(),
                name,
                size: bytes.len() as u64,
                width: 2,
                height: 2,
            })
        })
    }
}

async fn body_of_the_request(images: bool, loader: Option<Arc<dyn AttachmentLoader>>) -> Value {
    let server = spawn_mock(vec![http_sse(&sse_body(&[&text_chunk("seen")]))]).await;
    let mut actor = make_actor(server.url.clone(), registry_all(), CancellationToken::new());
    actor.images = Some(images);
    actor.attachment_loader = loader;
    actor.push_user_with_images_in_current_turn(
        "what is this?",
        UserProvenance::Captain,
        vec![image("a1")],
    );
    let outcome = run_agent(&mut actor, &mut |_| {}).await.unwrap();
    assert!(matches!(outcome, RunOutcome::Done(_)));
    // The transcript keeps the reference whatever the request carried.
    assert!(
        matches!(&actor.transcript[0], ChatMessage::User { content, images, .. }
        if content == "what is this?" && images.len() == 1)
    );
    let requests = server.requests.lock().await;
    serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap()
}

fn user_content(body: &Value) -> &Value {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .map(|m| &m["content"])
        .unwrap()
}

#[tokio::test]
async fn a_captain_image_reaches_the_request_as_a_data_url_after_its_text() {
    let body = body_of_the_request(true, Some(Arc::new(FakeLoader { fail: vec![] }))).await;
    let content = user_content(&body);
    assert_eq!(
        content[0],
        serde_json::json!({"type": "text", "text": "what is this?"})
    );
    assert_eq!(content[1]["type"], "image_url");
    assert!(
        content[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
}

#[tokio::test]
async fn without_a_loader_the_image_is_an_unavailable_placeholder() {
    let body = body_of_the_request(true, None).await;
    assert_eq!(
        user_content(&body),
        "what is this?\n[image a1.png, image/png, 4 B unavailable]"
    );
}

#[tokio::test]
async fn an_image_the_loader_cannot_read_becomes_a_placeholder_and_the_turn_goes_on() {
    let body = body_of_the_request(true, Some(Arc::new(FakeLoader { fail: vec!["a1"] }))).await;
    assert_eq!(
        user_content(&body),
        "what is this?\n[image a1.png, image/png, 4 B unavailable]"
    );
}

#[tokio::test]
async fn a_model_without_images_gets_the_cannot_see_placeholder_and_the_loader_is_not_asked() {
    struct Panics;
    impl AttachmentLoader for Panics {
        fn load(
            &self,
            _ids: Vec<String>,
        ) -> BoxFuture<'static, HashMap<String, Result<ImagePayload, String>>> {
            panic!("a model that cannot see images needs no bytes");
        }
        fn store(&self, _: String, _: Vec<u8>) -> BoxFuture<'static, Result<ImageRef, String>> {
            panic!("no tool ran");
        }
    }
    let body = body_of_the_request(false, Some(Arc::new(Panics))).await;
    assert_eq!(
        user_content(&body),
        "what is this?\n[image a1.png, image/png, 4 B: this model cannot see images]"
    );
    assert!(!body.to_string().contains("base64"));
}

#[tokio::test]
async fn an_answer_to_an_ask_stays_text_and_the_images_follow_in_a_captain_message() {
    let mut actor = make_actor(
        "http://127.0.0.1:1/v1".into(),
        registry_all(),
        CancellationToken::new(),
    );
    actor.pending = Some(PendingInteraction::Ask {
        tool_call_id: "ask_1".into(),
    });
    actor.push_user_with_images_in_current_turn(
        "this one",
        UserProvenance::Captain,
        vec![image("a1")],
    );
    assert!(
        matches!(&actor.transcript[0], ChatMessage::Tool { tool_call_id, content, answered_by: Some(UserProvenance::Captain), .. }
        if tool_call_id == "ask_1" && content == "this one")
    );
    assert!(
        matches!(&actor.transcript[1], ChatMessage::User { content, provenance: UserProvenance::Captain, images }
        if content.is_empty() && images.len() == 1)
    );
    assert_eq!(actor.transcript.len(), 2);
}

#[test]
fn a_sent_image_counts_a_fixed_cost_and_a_placeholder_counts_its_text() {
    let with_image = ChatMessage::User {
        content: String::new(),
        provenance: UserProvenance::Captain,
        images: vec![image("a1"), image("a2")],
    };
    let plain = ChatMessage::User {
        content: String::new(),
        provenance: UserProvenance::Captain,
        images: vec![],
    };
    assert_eq!(
        crate::agent::context::message_tokens(&with_image)
            - crate::agent::context::message_tokens(&plain),
        2 * crate::agent::context::IMAGE_TOKENS
    );

    let mut actor = make_actor(
        "http://127.0.0.1:1/v1".into(),
        registry_all(),
        CancellationToken::new(),
    );
    actor.transcript.push(with_image);
    // Model cannot see images: the request has text placeholders, never the 1,600 tokens.
    let blind = actor.context_breakdown().messages;
    assert!(blind < crate::agent::context::IMAGE_TOKENS);
    // Model sees images, no loader: unavailable placeholders, still text.
    actor.images = Some(true);
    assert!(actor.context_breakdown().messages < crate::agent::context::IMAGE_TOKENS);
    // Model sees images and a loader can supply them: the fixed cost applies.
    actor.attachment_loader = Some(Arc::new(FakeLoader { fail: vec![] }));
    assert!(actor.context_breakdown().messages >= 2 * crate::agent::context::IMAGE_TOKENS);
}

// Given a vision model that calls `read` on a PNG in the workspace
// Then the image is stored through the session's loader, the transcript's tool message keeps only
//      its reference, and the next request carries the picture in a user message that follows the
//      tool message and says it is tool output
#[tokio::test]
async fn a_tool_image_is_stored_and_reaches_the_next_request_after_the_tool_message() {
    let call = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"r1","function":{"name":"read","arguments":"{\"path\":\"shot.png\"}"}}]},"finish_reason":"tool_calls"}]}"#;
    let server = spawn_mock(vec![
        http_sse(&sse_body(&[call])),
        http_sse(&sse_body(&[&text_chunk("a red square")])),
    ])
    .await;
    let mut actor = make_actor(server.url.clone(), registry_all(), CancellationToken::new());
    std::fs::write(
        actor.tool_context.workspace.join("shot.png"),
        crate::session::attachments::fixtures::png(2, 2),
    )
    .unwrap();
    actor.images = Some(true);
    actor.attachment_loader = Some(Arc::new(FakeLoader { fail: vec![] }));
    actor.push_user_with_provenance("look at shot.png", UserProvenance::Captain);
    let mut posted = Vec::new();
    let outcome = run_agent(&mut actor, &mut |event| {
        if let AgentEvent::PostCall { result, .. } = event {
            posted.push(serde_json::to_string(result).unwrap());
        }
    })
    .await
    .unwrap();
    assert!(matches!(outcome, RunOutcome::Done(_)));
    let tool = actor
        .transcript
        .iter()
        .find_map(|m| match m {
            ChatMessage::Tool { images, .. } => Some(images.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(tool.len(), 1);
    assert_eq!(tool[0].attachment_id, "stored-shot.png");
    // The published result names the reference, never the bytes.
    assert!(posted[0].contains("stored-shot.png"));
    assert!(!posted[0].contains("base64"));

    let requests = server.requests.lock().await;
    let body: Value = serde_json::from_str(requests[1].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let at = messages.iter().position(|m| m["role"] == "tool").unwrap();
    assert!(
        messages[at]["content"]
            .as_str()
            .unwrap()
            .contains("attached in the next message")
    );
    let follow = &messages[at + 1];
    assert_eq!(follow["role"], "user");
    assert_eq!(
        follow["content"][0]["text"],
        crate::provider::openai::TOOL_IMAGES_PREFACE
    );
    assert!(
        follow["content"][2]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    // The follow-up exists only on the wire: the Captain's evidence is unchanged.
    assert_eq!(
        actor
            .transcript
            .iter()
            .filter(|m| m.is_captain_message())
            .count(),
        1
    );
}

// Given the same tool image and a model without image support
// Then the tool message carries the placeholder and no follow-up user message is sent
#[tokio::test]
async fn a_tool_image_for_a_text_only_model_is_a_placeholder_without_follow_up() {
    let call = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"r1","function":{"name":"read","arguments":"{\"path\":\"shot.png\"}"}}]},"finish_reason":"tool_calls"}]}"#;
    let server = spawn_mock(vec![
        http_sse(&sse_body(&[call])),
        http_sse(&sse_body(&[&text_chunk("ok")])),
    ])
    .await;
    let mut actor = make_actor(server.url.clone(), registry_all(), CancellationToken::new());
    std::fs::write(
        actor.tool_context.workspace.join("shot.png"),
        crate::session::attachments::fixtures::png(2, 2),
    )
    .unwrap();
    actor.images = Some(false);
    actor.attachment_loader = Some(Arc::new(FakeLoader { fail: vec![] }));
    actor.push_user_with_provenance("look", UserProvenance::Captain);
    run_agent(&mut actor, &mut |_| {}).await.unwrap();
    let requests = server.requests.lock().await;
    let body: Value = serde_json::from_str(requests[1].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let at = messages.iter().position(|m| m["role"] == "tool").unwrap();
    assert!(
        messages[at]["content"]
            .as_str()
            .unwrap()
            .contains("this model cannot see images")
    );
    assert_eq!(messages.len(), at + 1);
}

// Given a long conversation (several hundred messages) where the Captain already sent two large
//       screenshots, and a vision model that calls `read` on a third, larger picture
// Then the very next request, the one right after the tool result, carries the tool's image
//      bytes together with the two earlier ones: nothing waits for another Captain message
#[tokio::test]
async fn in_a_long_history_with_earlier_images_a_tool_image_reaches_the_very_next_request() {
    let call = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"r1","function":{"name":"read","arguments":"{\"path\":\"board.png\"}"}}]},"finish_reason":"tool_calls"}]}"#;
    let server = spawn_mock(vec![
        http_sse(&sse_body(&[call])),
        http_sse(&sse_body(&[&text_chunk("the board")])),
    ])
    .await;
    let mut actor = make_actor(server.url.clone(), registry_all(), CancellationToken::new());
    std::fs::write(
        actor.tool_context.workspace.join("board.png"),
        crate::session::attachments::fixtures::png(2, 2),
    )
    .unwrap();
    actor.images = Some(true);
    actor.attachment_loader = Some(Arc::new(FakeLoader { fail: vec![] }));
    for turn in 0..300 {
        actor.push_user_with_provenance(format!("question {turn}"), UserProvenance::Captain);
        actor.transcript.push(ChatMessage::Assistant {
            content: Some(format!("answer {turn}")),
            reasoning_content: None,
            tool_calls: vec![],
            opaque: None,
        });
    }
    let screenshot = |id: &str| ImageRef {
        size: 340_000,
        ..image(id)
    };
    actor.push_user_with_images_in_current_turn(
        "here is the first",
        UserProvenance::Captain,
        vec![screenshot("cap-1")],
    );
    actor.transcript.push(ChatMessage::Assistant {
        content: Some("seen".into()),
        reasoning_content: None,
        tool_calls: vec![],
        opaque: None,
    });
    actor.push_user_with_images_in_current_turn(
        "and the second; now read board.png",
        UserProvenance::Captain,
        vec![screenshot("cap-2")],
    );
    run_agent(&mut actor, &mut |_| {}).await.unwrap();

    let requests = server.requests.lock().await;
    assert_eq!(requests.len(), 2, "one tool step, one answer step");
    let body: Value = serde_json::from_str(requests[1].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let urls: Vec<&Value> = messages
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|part| part["type"] == "image_url")
        .collect();
    assert_eq!(urls.len(), 3, "both Captain screenshots and the tool image");
    let tool_at = messages.iter().rposition(|m| m["role"] == "tool").unwrap();
    let follow = &messages[tool_at + 1];
    assert_eq!(follow["role"], "user");
    assert_eq!(follow["content"][2]["type"], "image_url");
    assert_eq!(
        tool_at + 2,
        messages.len(),
        "the tool image is the last thing the model reads"
    );
}
