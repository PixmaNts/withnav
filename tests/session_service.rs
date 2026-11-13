//! The session `Service` driven directly: no router, no HTTP.

use std::time::Duration;

use withnav::bus::ResumeOutcome;
use withnav::session::Service;

mod common;
use common::server::TestServerConfig;
use common::sse::{SseEnd, read_sse};

// Given a Service over a provider nothing listens on
// When a session is created without any router
// Then no turn runs (no provider call), and a subscriber sees exactly
//      `session.start` at seq 0 and nothing after it
#[tokio::test]
async fn service_create_without_http_emits_start_zero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");
    let service = Service::new(TestServerConfig::unreachable(tmp.path()).config().split().0)
        .expect("service");

    let id = service
        .create_session(&ws, None, false)
        .await
        .expect("create without http");

    let mut reply = service.subscribe(id, None).await.expect("subscribe");
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
    use withnav::bus::command::{Command, ContentBlock, DeliveryMode};
    use withnav::error::ServerError;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");
    let service = Service::new(TestServerConfig::unreachable(tmp.path()).config().split().0)
        .expect("service");
    let id = service
        .create_session(&ws, None, false)
        .await
        .expect("create");

    let command = |target: &str, content: Vec<ContentBlock>| Command {
        command_id: ulid::Ulid::generate(),
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

    let reply = service.subscribe(id, None).await.expect("subscribe");
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

/// The ordered envelopes `Service::subscribe(id, None)` replays.
async fn service_replay(
    server: &common::server::TestServer,
    id: ulid::Ulid,
) -> Vec<withnav::bus::event::EventEnvelope> {
    let sub = server.state.subscribe(id, None).await.expect("subscribe");
    match sub.replay {
        ResumeOutcome::Replay(events) => events,
        other => panic!("expected a replay: {other:?}"),
    }
}

/// Reads `GET /events` until `count` envelopes arrived or the server ended
/// the stream.
async fn http_replay(
    base: &str,
    id: ulid::Ulid,
    count: usize,
) -> Vec<withnav::bus::event::EventEnvelope> {
    let read = read_sse(base, &id.to_string(), None, |frames| frames.len() >= count).await;
    assert_ne!(read.end, SseEnd::Timeout, "{:?}", read.frames);
    read.frames
        .iter()
        .map(|f| serde_json::from_str(&f.data).expect("envelope"))
        .collect()
}

// Given a session that ran one scripted turn
// When `Service::subscribe(id, None)` and the HTTP SSE stream replay it,
//      live and then after a stop
// Then both yield the same ordered envelopes, and a stopped session's
//      subscription has no live receiver
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_subscription_matches_http_replay() {
    use common::server::{TestServer, create_session, post_message_ok};
    use common::sse::{http_sse, sse_body};

    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = TestServer::start(vec![http_sse(&sse_body(&[done]))]).await;
    let id: ulid::Ulid = create_session(&server.base, &server.ws)
        .await
        .parse()
        .expect("ulid");
    post_message_ok(&server.base, &id.to_string(), "go").await;

    // Wait for the turn to finish.
    let mut expected = service_replay(&server, id).await;
    for _ in 0..100 {
        if expected.iter().any(|e| e.kind == "turn.end") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        expected = service_replay(&server, id).await;
    }
    assert!(
        expected.iter().any(|e| e.kind == "turn.end"),
        "{expected:?}"
    );

    // Live session.
    let live = server.state.subscribe(id, None).await.expect("subscribe");
    assert!(live.live.is_some(), "a live session has a live receiver");
    assert_eq!(
        expected,
        http_replay(&server.base, id, expected.len()).await
    );

    // Stopped session: the store replays through `session.end`, then ends.
    let stop = reqwest::Client::new()
        .post(format!("{}/v1/sessions/{id}/stop", server.base))
        .send()
        .await
        .expect("stop");
    assert_eq!(stop.status(), 200);
    let stopped = server.state.subscribe(id, None).await.expect("subscribe");
    assert!(
        stopped.live.is_none(),
        "a stopped session has no live receiver"
    );
    let ResumeOutcome::Replay(expected) = stopped.replay else {
        panic!("a stopped session replays");
    };
    assert_eq!(
        expected.last().map(|e| e.kind.as_str()),
        Some("session.end")
    );
    assert_eq!(expected, http_replay(&server.base, id, usize::MAX).await);

    // An unknown id is not found.
    assert!(matches!(
        server.state.subscribe(ulid::Ulid::generate(), None).await,
        Err(withnav::error::ServerError::SessionNotFound(_))
    ));
}

// Given a session that ran a turn, was stopped, was resumed to run another
//      turn, and was stopped again
// When its history is replayed, live and then stored
// Then the live replay shows both turns with no `session.end` at all, and
//      the stored replay shows the full history with exactly one
//      `session.end`, in last position. A resume continues the same
//      sequence space, so the first stop's end sits mid-history in the
//      store; a client that ends reading on `session.end` must not have it
//      truncate the replay there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_replay_of_a_resumed_session_hides_intermediate_session_end() {
    use common::server::{TestServer, create_session, post_message_ok};
    use common::sse::{http_sse, sse_body};

    async fn stop(server: &common::server::TestServer, id: ulid::Ulid) {
        let stop = reqwest::Client::new()
            .post(format!("{}/v1/sessions/{id}/stop", server.base))
            .send()
            .await
            .expect("stop");
        assert_eq!(stop.status(), 200);
    }

    let done = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
    let server = TestServer::start(vec![
        http_sse(&sse_body(&[done])),
        http_sse(&sse_body(&[done])),
    ])
    .await;
    let id: ulid::Ulid = create_session(&server.base, &server.ws)
        .await
        .parse()
        .expect("ulid");

    // First turn.
    post_message_ok(&server.base, &id.to_string(), "go").await;
    let mut replay = service_replay(&server, id).await;
    for _ in 0..100 {
        if replay.iter().any(|e| e.kind == "turn.end") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        replay = service_replay(&server, id).await;
    }
    assert!(replay.iter().any(|e| e.kind == "turn.end"), "{replay:?}");

    // First stop: the stored replay ends with the stop's `session.end`.
    stop(&server, id).await;
    let stopped_sub = server.state.subscribe(id, None).await.expect("subscribe");
    assert!(
        stopped_sub.live.is_none(),
        "a stopped session has no live receiver"
    );
    let first_stop = service_replay(&server, id).await;
    assert_eq!(
        first_stop.last().map(|e| e.kind.as_str()),
        Some("session.end"),
        "a stopped session's replay ends with session.end: {first_stop:?}"
    );

    // Resume the same id: same sequence space, new runtime.
    let (resumed, _) = server
        .state
        .create_or_resume_session(&server.ws, None, false, Some(id))
        .await
        .expect("resume");

    // Second turn.
    post_message_ok(&server.base, &resumed.to_string(), "go again").await;
    let mut live_replay = service_replay(&server, id).await;
    for _ in 0..100 {
        if live_replay.iter().filter(|e| e.kind == "turn.end").count() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        live_replay = service_replay(&server, id).await;
    }
    assert_eq!(
        live_replay.iter().filter(|e| e.kind == "turn.end").count(),
        2,
        "both turns replay while live: {live_replay:?}"
    );

    // While live, no `session.end` at all — not even the stale one the
    // first stop left mid-history.
    assert!(
        !live_replay.iter().any(|e| e.kind == "session.end"),
        "a live resumed session's replay carries no session.end: {live_replay:?}"
    );

    // Stop again: the stored replay holds the full history, exactly one
    // `session.end` in last position, with strictly increasing seqs (the
    // gap marks the hidden stale end).
    stop(&server, id).await;
    let final_replay = service_replay(&server, id).await;
    let ends: Vec<usize> = final_replay
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == "session.end")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        ends,
        vec![final_replay.len() - 1],
        "exactly one trailing session.end: {final_replay:?}"
    );
    assert_eq!(
        final_replay.iter().filter(|e| e.kind == "turn.end").count(),
        2,
        "the full history survives the intermediate session.end: {final_replay:?}"
    );
    assert!(
        final_replay.windows(2).all(|w| w[1].seq > w[0].seq),
        "seqs increase, with a gap where the stale end was hidden: {final_replay:?}"
    );
}

// Given a persisted session created through the Service
// When it is deleted through the Service
// Then it is gone from `list(workspace)`, `send` answers not-found, and
//      `subscribe` answers not-found
#[tokio::test]
async fn service_delete_removes_listing_and_routing() {
    use withnav::bus::command::{Command, ContentBlock, DeliveryMode};
    use withnav::error::ServerError;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");
    let config = TestServerConfig::unreachable(tmp.path())
        .history_db(tmp.path().join("history.db"))
        .config()
        .split()
        .0;
    let service = Service::new(config).expect("service");
    let ws_str = ws.display().to_string();

    let id = service
        .create_session(&ws, None, false)
        .await
        .expect("create");
    let listed = service.list(&ws_str).await.expect("list");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].id, id);
    assert!(listed[0].live);

    service.delete(id).await.expect("delete");

    assert!(service.list(&ws_str).await.expect("list").is_empty());
    let command = Command {
        command_id: ulid::Ulid::generate(),
        session_id: id,
        source_actor_id: "user:test".to_string(),
        target_actor_id: "agent:main".to_string(),
        mode: DeliveryMode::NextTurn,
        content: vec![ContentBlock::Text {
            text: "hi".to_string(),
        }],
        created_at: std::time::SystemTime::now(),
    };
    assert!(matches!(
        service.send(command).await,
        Err(ServerError::SessionNotFound(_))
    ));
    assert!(matches!(
        service.subscribe(id, None).await,
        Err(ServerError::SessionNotFound(_))
    ));
    // Deleting again: neither in memory nor on disk.
    assert!(matches!(
        service.delete(id).await,
        Err(ServerError::SessionNotFound(_))
    ));
}

// Given a history database path that cannot be opened (its parent is a file)
// When the service is built
// Then construction fails with a history error instead of running without
//      history
#[tokio::test]
async fn service_construction_fails_on_an_unopenable_history_db() {
    use withnav::error::ServerError;

    let tmp = tempfile::tempdir().expect("tempdir");
    let blocker = tmp.path().join("not-a-dir");
    std::fs::write(&blocker, "file").expect("blocker file");
    let config = TestServerConfig::unreachable(tmp.path())
        .history_db(blocker.join("history.db"))
        .config()
        .split()
        .0;

    let result = Service::new(config);
    assert!(matches!(result, Err(ServerError::History(_))));
}

/// Counts incarnations per session id: every new one must find all earlier
/// ones already cancelled, or two runtimes would exist for one id.
#[derive(Debug, Default)]
struct IncarnationWatch {
    tokens: std::sync::Mutex<
        std::collections::HashMap<ulid::Ulid, Vec<tokio_util::sync::CancellationToken>>,
    >,
    violations: std::sync::Mutex<Vec<String>>,
}

#[derive(Debug, Clone, Default)]
struct WatchHooks(std::sync::Arc<IncarnationWatch>);

impl withnav::session::SessionHooks for WatchHooks {
    fn configure_actor(&self, _editor: bool, _actor: &mut withnav::agent::actor::AgentActor) {}

    fn started(
        &self,
        info: withnav::session::StartedSession,
        _sender: withnav::session::SessionSender,
        _subscription: withnav::session::Subscription,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        let mut tokens = self.0.tokens.lock().expect("tokens");
        let earlier = tokens.entry(info.id).or_default();
        if earlier.iter().any(|token| !token.is_cancelled()) {
            self.0
                .violations
                .lock()
                .expect("violations")
                .push(format!("{}: a second runtime started", info.id));
        }
        earlier.push(cancel);
    }
}

// Given one session id and many interleaved resume, stop and delete calls
// When they all run concurrently, round after round
// Then a new incarnation never starts while an earlier one is alive, a
//      resume never fails except on a deleted id, and the final state is
//      whole: live or stopped with a stored row, or gone with none
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_resume_stop_delete_never_creates_two_runtimes() {
    use withnav::error::ServerError;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");
    let hooks = WatchHooks::default();
    let service = Service::with_hooks(
        TestServerConfig::unreachable(tmp.path()).config().split().0,
        std::sync::Arc::new(hooks.clone()),
    )
    .expect("service");

    for round in 0..16 {
        let with_delete = round % 2 == 1;
        let id = service
            .create_session(&ws, None, false)
            .await
            .expect("create");
        let mut calls = Vec::new();
        for step in 0..12 {
            let service = service.clone();
            let ws = ws.clone();
            let delete = with_delete && step == 7;
            calls.push(tokio::spawn(async move {
                if delete {
                    service.delete(id).await
                } else if step % 2 == 0 {
                    service
                        .create_or_resume_session(&ws, None, false, Some(id))
                        .await
                        .map(|_| ())
                } else {
                    service.close_session(id).await
                }
            }));
        }
        for call in calls {
            match call.await.expect("call task") {
                Ok(()) => {}
                Err(ServerError::SessionNotFound(_)) if with_delete => {}
                Err(err) => panic!("round {round}: unexpected error: {err}"),
            }
        }

        let stored = service.history().meta(id).await.expect("meta");
        match service.session_summary(id).await {
            Ok(summary) => {
                assert!(stored.is_some(), "round {round}: {summary:?} has no row");
                assert!(
                    summary.actor_state == "idle" || summary.actor_state == "stopped",
                    "round {round}: half state {summary:?}"
                );
            }
            Err(ServerError::SessionNotFound(_)) => {
                assert!(with_delete, "round {round}: lost without a delete");
                assert!(stored.is_none(), "round {round}: gone but still stored");
            }
            Err(err) => panic!("round {round}: {err}"),
        }
    }
    service.close_all().await;
    let violations = hooks.0.violations.lock().expect("violations");
    assert!(violations.is_empty(), "{violations:?}");
}

/// A real 1×1 RGBA PNG: `AttachmentStore::insert` parses the header, and raster
/// decoding is on the roadmap for the store, so a header-only fake would break.
/// The IDAT decompresses to a full scanline (filter byte + RGBA), verified
/// with Python's zlib on 2026-11-09 — an earlier fixture truncated it.
fn one_pixel_png() -> Vec<u8> {
    [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x10,
        0x54, 0x32, 0xfe, 0x0f, 0x00, 0x02, 0x14, 0x01, 0x66, 0x04, 0xe0, 0xde, 0x7f, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ]
    .to_vec()
}

fn image_command(
    session: withnav::session::SessionId,
    text: &str,
    attachment_id: &str,
) -> withnav::bus::command::Command {
    use withnav::bus::command::{Command, ContentBlock, DeliveryMode};

    Command {
        command_id: ulid::Ulid::generate(),
        session_id: session,
        source_actor_id: "user:test".to_string(),
        target_actor_id: "agent:main".to_string(),
        mode: DeliveryMode::NextTurn,
        content: vec![
            ContentBlock::Text {
                text: text.to_string(),
            },
            ContentBlock::Image(withnav::provider::openai::ImageRef {
                attachment_id: attachment_id.to_string(),
                media_type: String::new(),
                name: String::new(),
                size: 0,
                width: 0,
                height: 0,
            }),
        ],
        created_at: std::time::SystemTime::now(),
    }
}

async fn replay_kinds(service: &Service, id: withnav::session::SessionId) -> Vec<String> {
    let reply = service.subscribe(id, None).await.expect("subscribe");
    let ResumeOutcome::Replay(events) = reply.replay else {
        panic!("a fresh subscription replays: {:?}", reply.replay);
    };
    events.iter().map(|e| e.kind.as_str().to_string()).collect()
}

// Given two sessions sharing one attachment store
// When a command to session A references an image stored by session B
// Then the whole command is refused before admission: the error names the
//      attachment, and A's replay still holds only `session.start` — no
//      `message.received` was sequenced. A command with an image A itself
//      holds is accepted right after, so the refusal is per-message.
#[tokio::test]
async fn service_send_refuses_a_foreign_session_image_before_sequencing() {
    use withnav::error::ServerError;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");
    let service = Service::new(TestServerConfig::unreachable(tmp.path()).config().split().0)
        .expect("service");

    let a = service.create_session(&ws, None, false).await.expect("A");
    let b = service.create_session(&ws, None, false).await.expect("B");
    let store = service.attachments().expect("attachment store");
    let foreign = store
        .insert(&b.to_string(), Some("b.png"), &one_pixel_png())
        .expect("store into B");

    let refused = service
        .send(image_command(a, "look at this", &foreign.attachment_id))
        .await;
    match &refused {
        Err(ServerError::MalformedBody(message)) => {
            assert!(message.contains("unknown attachment"), "{message}")
        }
        other => panic!("foreign image refused: {other:?}"),
    }
    assert_eq!(
        replay_kinds(&service, a).await,
        vec!["session.start".to_string()],
        "nothing was sequenced for the refused message"
    );

    // A's own image is accepted, proving the refusal was per-message.
    let own = store
        .insert(&a.to_string(), Some("a.png"), &one_pixel_png())
        .expect("store into A");
    let command_id = {
        let command = image_command(a, "now this one", &own.attachment_id);
        let id = command.command_id;
        service.send(command).await.expect("A's own image");
        id
    };
    let kinds = replay_kinds(&service, a).await;
    assert!(kinds.contains(&"message.received".to_string()), "{kinds:?}");
    let reply = service.subscribe(a, None).await.expect("subscribe");
    let ResumeOutcome::Replay(events) = reply.replay else {
        panic!("replay: {:?}", reply.replay);
    };
    let received = events
        .iter()
        .find(|e| e.kind.as_str() == "message.received")
        .expect("message.received sequenced");
    assert_eq!(received.payload["command_id"], command_id.to_string());
    service.close_all().await;
}

// Given a session with an attachment store
// When a command references an attachment id no session ever stored
// Then the command is refused before admission and the replay stays at
//      `session.start`
#[tokio::test]
async fn service_send_refuses_an_unknown_attachment_id_before_sequencing() {
    use withnav::error::ServerError;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");
    let service = Service::new(TestServerConfig::unreachable(tmp.path()).config().split().0)
        .expect("service");

    let a = service.create_session(&ws, None, false).await.expect("A");

    let refused = service
        .send(image_command(a, "look at this", "01NOPE"))
        .await;
    match &refused {
        Err(ServerError::MalformedBody(message)) => {
            assert!(message.contains("unknown attachment"), "{message}")
        }
        other => panic!("unknown image refused: {other:?}"),
    }
    assert_eq!(
        replay_kinds(&service, a).await,
        vec!["session.start".to_string()],
        "nothing was sequenced for the refused message"
    );
    service.close_all().await;
}
