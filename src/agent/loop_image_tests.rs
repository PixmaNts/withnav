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
                        Ok(ImagePayload::from_bytes("image/png", b"\x89PNG"))
                    };
                    (id, result)
                })
                .collect()
        })
    }
}

async fn body_of_the_request(images: bool, loader: Option<Arc<dyn AttachmentLoader>>) -> Value {
    let server = spawn_mock(vec![http_sse(&sse_body(&[&text_chunk("seen")]))]).await;
    let mut actor = make_actor(server.url.clone(), registry_all(), CancellationToken::new());
    actor.images = images;
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
        matches!(&actor.transcript[0], ChatMessage::Tool { tool_call_id, content, answered_by: Some(UserProvenance::Captain) }
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
    actor.images = true;
    assert!(actor.context_breakdown().messages < crate::agent::context::IMAGE_TOKENS);
    // Model sees images and a loader can supply them: the fixed cost applies.
    actor.attachment_loader = Some(Arc::new(FakeLoader { fail: vec![] }));
    assert!(actor.context_breakdown().messages >= 2 * crate::agent::context::IMAGE_TOKENS);
}
