//! The session `Service` driven directly: no router, no HTTP.

use std::time::Duration;

use rust_agent::bus::ResumeOutcome;
use rust_agent::session::Service;

mod common;
use common::server::TestServerConfig;

// Given a Service over a provider nothing listens on
// When a session is created without any router
// Then no turn runs (no provider call), and a subscriber sees exactly
//      `session.start` at seq 0 and nothing after it
#[tokio::test]
async fn service_create_without_http_emits_start_zero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");
    let service = Service::new(TestServerConfig::unreachable(tmp.path()).config());

    let id = service
        .create_session(&ws, None, None)
        .await
        .expect("create without http");

    let handle = service.handle(id).expect("session is live");
    let mut reply = handle
        .sequencer
        .subscribe_since(None)
        .await
        .expect("subscribe");
    let ResumeOutcome::Replay(events) = &reply.replay else {
        panic!("a fresh subscription replays: {:?}", reply.replay);
    };
    assert_eq!(events.len(), 1, "only session.start exists: {events:?}");
    assert_eq!(events[0].seq, 0);
    assert_eq!(events[0].kind.as_str(), "session.start");

    // A provider call would start a turn and emit more events.
    let mut live = reply.live.take().expect("live receiver");
    let late = tokio::time::timeout(Duration::from_millis(300), live.recv()).await;
    assert!(late.is_err(), "nothing follows session.start: {late:?}");
}

// Given a live session on a Service
// When a command with non-text content, then one aimed at an unknown actor,
//      is sent
// Then each is rejected with a typed error and no `message.received` is
//      sequenced; a valid command afterwards is accepted
#[tokio::test]
async fn service_send_rejects_invalid_content_without_event() {
    use rust_agent::bus::command::{Command, ContentBlock, DeliveryMode};
    use rust_agent::error::ServerError;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");
    let service = Service::new(TestServerConfig::unreachable(tmp.path()).config());
    let id = service
        .create_session(&ws, None, None)
        .await
        .expect("create");

    let command = |target: &str, content: Vec<ContentBlock>| Command {
        command_id: ulid::Ulid::new(),
        session_id: id,
        source_actor_id: "user:test".to_string(),
        target_actor_id: target.to_string(),
        mode: DeliveryMode::NextTurn,
        content,
        created_at: std::time::SystemTime::now(),
    };
    let text = |text: &str| ContentBlock::Text {
        text: text.to_string(),
    };

    let non_text = command(
        "agent:main",
        vec![ContentBlock::Thinking {
            text: "not allowed".to_string(),
        }],
    );
    assert!(matches!(
        service.send(non_text).await,
        Err(ServerError::NonTextContent)
    ));
    let foreign = command("agent:other", vec![text("hello")]);
    assert!(matches!(
        service.send(foreign).await,
        Err(ServerError::MalformedBody(_))
    ));

    let handle = service.handle(id).expect("live");
    let reply = handle
        .sequencer
        .subscribe_since(None)
        .await
        .expect("subscribe");
    let ResumeOutcome::Replay(events) = &reply.replay else {
        panic!("a fresh subscription replays");
    };
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["session.start"],
        "a rejected command sequences nothing"
    );
}
