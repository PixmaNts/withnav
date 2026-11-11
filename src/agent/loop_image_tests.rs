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

// Given a loader that never answers
// Then the step stops waiting after its bound and sends the image as unavailable
#[tokio::test(start_paused = true)]
async fn a_loader_that_never_answers_times_out_into_placeholders() {
    // The clock is paused so the wait is skipped; it resumes when the timed-out load is dropped,
    // so the request to the mock server runs on real time and no other deadline is jumped over.
    struct ResumeOnDrop;
    impl Drop for ResumeOnDrop {
        fn drop(&mut self) {
            tokio::time::resume();
        }
    }
    struct NeverAnswers;
    impl AttachmentLoader for NeverAnswers {
        fn load(
            &self,
            _ids: Vec<String>,
        ) -> BoxFuture<'static, HashMap<String, Result<ImagePayload, String>>> {
            Box::pin(async {
                let _resume = ResumeOnDrop;
                std::future::pending().await
            })
        }
        fn store(&self, _: String, _: Vec<u8>) -> BoxFuture<'static, Result<ImageRef, String>> {
            panic!("no tool ran");
        }
    }
    let body = body_of_the_request(true, Some(Arc::new(NeverAnswers))).await;
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

// ---------------------------------------------------------------------------
// request_history: the projection a request carries, decided per call.
// These tests drive `AgentActor::request_history` and
// `AgentActor::context_breakdown_at` directly — no provider, no turn: the
// loop tests above prove what a whole request sent over HTTP carries; these
// prove the projection itself is request-scoped, reproducible and
// budget-aware.
// ---------------------------------------------------------------------------

/// A transcript of Captain messages with images, independent of any actor.
fn image_transcript() -> Vec<ChatMessage> {
    vec![
        user_message("first", vec![image("a1")]),
        user_message("second", vec![image("b2")]),
    ]
}

fn user_message(text: &str, images: Vec<ImageRef>) -> ChatMessage {
    ChatMessage::User {
        content: text.to_string(),
        provenance: UserProvenance::Captain,
        images,
    }
}
fn projected_ids(history: &[ChatMessage]) -> Vec<String> {
    history
        .iter()
        .filter_map(|m| match m {
            ChatMessage::User { images, .. } => Some(images),
            _ => None,
        })
        .flatten()
        .map(|i| i.attachment_id.clone())
        .collect()
}

async fn plain_actor() -> AgentActor {
    let server = spawn_mock(vec![]).await;
    let mut actor = make_actor(server.url, registry_all(), CancellationToken::new());
    actor.transcript = Vec::new();
    actor
}

// Given a transcript with images and a model that accepts them
// When the actor has no loader
// Then every image becomes an `unavailable` placeholder in the projection,
//      while the transcript keeps its references untouched
#[tokio::test]
async fn request_history_without_a_loader_projects_placeholders() {
    let mut actor = plain_actor().await;
    actor.images = Some(true);
    actor.attachment_loader = None;
    let transcript = image_transcript();

    let projected = actor.request_history(Vec::new());
    assert!(
        projected.is_empty(),
        "empty transcript in, empty projection out"
    );
    actor.transcript = transcript.clone();
    let projected = actor.request_history(actor.transcript.clone());
    assert_eq!(projected_ids(&projected), Vec::<String>::new());
    // Both messages survive with their text: the projection replaced the
    // images, it did not drop the messages that carried them.
    assert_eq!(projected.len(), 2, "both messages kept: {projected:?}");
    for (message, text) in projected.iter().zip(["first", "second"]) {
        let ChatMessage::User {
            content, images, ..
        } = message
        else {
            panic!("only user messages expected: {message:?}");
        };
        assert!(content.starts_with(text), "text preserved: {content}");
        assert!(images.is_empty(), "image references removed");
        assert!(content.contains("unavailable"), "placeholder in: {content}");
    }
    // The transcript itself is untouched: projection is request-scoped.
    assert_eq!(actor.transcript.len(), 2);
    assert_eq!(projected_ids(&actor.transcript), vec!["a1", "b2"]);

    // With a loader, the same transcript projects to the images themselves.
    actor.attachment_loader = Some(Arc::new(FakeLoader { fail: vec![] }));
    let projected = actor.request_history(actor.transcript.clone());
    assert_eq!(projected_ids(&projected), vec!["a1", "b2"]);
}

// Given one transcript and a model whose image support flips between
// requests (true → false → unknown → true)
// When the projection is recomputed from the original transcript each time
// Then it follows the current support, and the transcript is restored to its
//      original state after every call
#[tokio::test]
async fn request_history_reprojects_the_same_transcript_when_support_changes() {
    use std::matches;

    let mut actor = plain_actor().await;
    actor.attachment_loader = Some(Arc::new(FakeLoader { fail: vec![] }));
    actor.transcript = image_transcript();

    actor.images = Some(true);
    assert_eq!(
        projected_ids(&actor.request_history(actor.transcript.clone())),
        vec!["a1", "b2"]
    );

    actor.images = Some(false);
    let projected = actor.request_history(actor.transcript.clone());
    assert_eq!(projected_ids(&projected), Vec::<String>::new());
    // Both messages survive with their text and the cannot-see placeholder;
    // a projection that dropped the messages outright would fail here.
    assert_eq!(projected.len(), 2, "both messages kept: {projected:?}");
    for (message, text) in projected.iter().zip(["first", "second"]) {
        let ChatMessage::User {
            content, images, ..
        } = message
        else {
            panic!("only user messages expected: {message:?}");
        };
        assert!(content.starts_with(text), "text preserved: {content}");
        assert!(images.is_empty(), "image references removed");
        assert!(
            content.contains("this model cannot see images"),
            "cannot-see placeholder in: {content}"
        );
    }

    actor.images = None;
    let projected = actor.request_history(actor.transcript.clone());
    // Same survival check for the unknown-support placeholder.
    assert_eq!(projected.len(), 2, "both messages kept: {projected:?}");
    for (message, text) in projected.iter().zip(["first", "second"]) {
        let ChatMessage::User {
            content, images, ..
        } = message
        else {
            panic!("only user messages expected: {message:?}");
        };
        assert!(content.starts_with(text), "text preserved: {content}");
        assert!(images.is_empty(), "image references removed");
        assert!(
            content.contains("image support is unknown"),
            "unknown-support placeholder in: {content}"
        );
    }

    // Back to supported: the same original transcript sends both again.
    actor.images = Some(true);
    let projected = actor.request_history(actor.transcript.clone());
    assert_eq!(projected_ids(&projected), vec!["a1", "b2"]);

    assert!(
        matches!(actor.transcript.as_slice(), [m1, m2]
            if projected_ids(std::slice::from_ref(m1)) == ["a1".to_string()]
                && projected_ids(std::slice::from_ref(m2)) == ["b2".to_string()]),
        "the transcript itself was never mutated: {:?}",
        actor.transcript
    );
}

// Given 21 image occurrences in one transcript (one attachment repeated —
// occurrences count, not unique ids)
// When the model accepts images and a loader exists
// Then the newest 20 are sent and the oldest becomes a size-limit placeholder;
//      nothing older is backfilled
#[tokio::test]
async fn request_history_applies_the_count_budget() {
    let mut actor = plain_actor().await;
    actor.images = Some(true);
    actor.attachment_loader = Some(Arc::new(FakeLoader { fail: vec![] }));
    let mut transcript: Vec<ChatMessage> = (0..20)
        .map(|n| user_message("", vec![image(&format!("i{n}"))]))
        .collect();
    transcript.insert(0, user_message("oldest", vec![image("dup"), image("dup")]));
    actor.transcript = transcript;

    let projected = actor.request_history(actor.transcript.clone());
    let sent = projected_ids(&projected);
    assert_eq!(sent.len(), 20, "exactly MAX_REQUEST_IMAGES sent");

    // The two `dup` occurrences are the oldest and are cut; the full ordered
    // set is i0..i19 — a budget that counted unique ids instead of
    // occurrences would keep a different set.
    assert_eq!(
        sent,
        (0..20).map(|n| format!("i{n}")).collect::<Vec<_>>(),
        "the newest 20 occurrences, in order"
    );
    assert!(matches!(&projected[0], ChatMessage::User { content, .. }
        if content.contains("not included in this request (size limit)")));

    // The token estimate runs on the projection, not the raw transcript:
    // 20 sent images are charged, the two cut occurrences are not.
    let breakdown = actor.context_breakdown();
    let expected: u64 = projected
        .iter()
        .map(crate::agent::context::message_tokens)
        .sum();
    assert_eq!(breakdown.messages, expected);
}

// Given 21 occurrences of ONE attachment id (the same image sent 21 times)
// When the model accepts images and a loader exists
// Then the newest 20 occurrences are sent and the oldest becomes a size-limit
//      placeholder — proving the budget counts occurrences, not unique ids
//      (a unique-id budget would admit or cut all 21 at once)
#[tokio::test]
async fn request_history_counts_occurrences_not_unique_ids() {
    let mut actor = plain_actor().await;
    actor.images = Some(true);
    actor.attachment_loader = Some(Arc::new(FakeLoader { fail: vec![] }));
    actor.transcript = (0..21)
        .map(|n| user_message(&format!("m{n}"), vec![image("same")]))
        .collect();

    let projected = actor.request_history(actor.transcript.clone());
    let sent = projected_ids(&projected);
    assert_eq!(
        sent.len(),
        20,
        "twenty occurrences of the one id are sent: {sent:?}"
    );
    // The cut occurrence is the OLDEST message (m0); m1..m20 all send.
    assert!(matches!(&projected[0], ChatMessage::User { content, .. }
        if content.starts_with("m0")
            && content.contains("not included in this request (size limit)")));
    assert_eq!(
        projected
            .iter()
            .filter(|m| matches!(m, ChatMessage::User { images, .. } if images.len() == 1))
            .count(),
        20,
        "the other twenty messages still carry their image"
    );
}

// Given three 7.5 MiB images (10 MiB base64 each: two fit in the 20 MiB
// request budget, a third does not) and a tiny image older than all of them
// When the model accepts images and a loader exists
// Then only the two newest big images are sent; the tiny one before the cut
//      stays cut — the sent set only moves at its oldest end
#[tokio::test]
async fn request_history_applies_the_byte_budget() {
    let big = 7 * 1024 * 1024 + 512 * 1024;
    let big_image = |id: &str| ImageRef {
        attachment_id: id.into(),
        media_type: "image/png".into(),
        name: format!("{id}.png"),
        size: big,
        width: 10,
        height: 10,
    };
    let mut actor = plain_actor().await;
    actor.images = Some(true);
    actor.attachment_loader = Some(Arc::new(FakeLoader { fail: vec![] }));
    actor.transcript = vec![
        user_message("old tiny", vec![image("tiny-old")]),
        user_message("", vec![big_image("big1")]),
        user_message("", vec![big_image("big2")]),
        user_message("", vec![big_image("big3")]),
    ];

    let projected = actor.request_history(actor.transcript.clone());
    assert_eq!(projected_ids(&projected), vec!["big2", "big3"]);

    // Estimates run on the projection: two images charged, not four.
    let breakdown = actor.context_breakdown();
    let expected: u64 = projected
        .iter()
        .map(crate::agent::context::message_tokens)
        .sum();
    assert_eq!(breakdown.messages, expected);
    // And the estimate differs from the unprojected transcript's.
    let unprojected: u64 = actor
        .transcript
        .iter()
        .map(crate::agent::context::message_tokens)
        .sum();
    assert!(breakdown.messages < unprojected);
}

// Given three 6 MiB images (8 MiB base64 each: the two newest fill 16 of the
// 20 MiB request budget, leaving 4 MiB spare) with a tiny image older than
// all of them
// When the model accepts images and a loader exists
// Then the tiny image still stays cut: the budget scans from the newest end
//      and never backfills older occurrences into leftover capacity (a
//      backfilling budget would admit the ~4 B tiny image into the 4 MiB
//      spare — this test pins that it does not)
#[tokio::test]
async fn request_history_does_not_backfill_spare_byte_capacity() {
    let six = 6 * 1024 * 1024;
    let big_image = |id: &str| ImageRef {
        attachment_id: id.into(),
        media_type: "image/png".into(),
        name: format!("{id}.png"),
        size: six,
        width: 10,
        height: 10,
    };
    let mut actor = plain_actor().await;
    actor.images = Some(true);
    actor.attachment_loader = Some(Arc::new(FakeLoader { fail: vec![] }));
    actor.transcript = vec![
        user_message("old tiny", vec![image("tiny-old")]),
        user_message("", vec![big_image("big1")]),
        user_message("", vec![big_image("big2")]),
        user_message("", vec![big_image("big3")]),
    ];

    let projected = actor.request_history(actor.transcript.clone());
    assert_eq!(
        projected_ids(&projected),
        vec!["big2", "big3"],
        "the two newest fill the budget; the tiny one is NOT backfilled"
    );
    assert!(matches!(&projected[0], ChatMessage::User { content, .. }
        if content.starts_with("old tiny")
            && content.contains("not included in this request (size limit)")));
}
