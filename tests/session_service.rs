//! The session `Service` driven directly: no router, no HTTP.

use std::time::Duration;

use rust_agent::bus::ResumeOutcome;
use rust_agent::session::Service;

mod common;
use common::server::TestServerConfig;
use common::sse::{read_sse, SseEnd};

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
        .create_session(&ws, None, None)
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
    use rust_agent::bus::command::{Command, ContentBlock, DeliveryMode};
    use rust_agent::error::ServerError;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");
    let service = Service::new(TestServerConfig::unreachable(tmp.path()).config().split().0)
        .expect("service");
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
) -> Vec<rust_agent::bus::event::EventEnvelope> {
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
) -> Vec<rust_agent::bus::event::EventEnvelope> {
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
    use common::server::{create_session, post_message_ok, TestServer};
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
        server.state.subscribe(ulid::Ulid::new(), None).await,
        Err(rust_agent::error::ServerError::SessionNotFound(_))
    ));
}

// Given a persisted session created through the Service
// When it is deleted through the Service
// Then it is gone from `list(workspace)`, `send` answers not-found, and
//      `subscribe` answers not-found
#[tokio::test]
async fn service_delete_removes_listing_and_routing() {
    use rust_agent::bus::command::{Command, ContentBlock, DeliveryMode};
    use rust_agent::error::ServerError;

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
        .create_session(&ws, None, None)
        .await
        .expect("create");
    let listed = service.list(&ws_str).await.expect("list");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].id, id);
    assert!(listed[0].live);

    service.delete(id).await.expect("delete");

    assert!(service.list(&ws_str).await.expect("list").is_empty());
    let command = Command {
        command_id: ulid::Ulid::new(),
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
    use rust_agent::error::ServerError;

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

impl rust_agent::session::SessionHooks for WatchHooks {
    fn default_model(&self, _profile: Option<&str>) -> Option<&str> {
        None
    }

    fn configure_actor(
        &self,
        _profile: Option<&str>,
        _actor: &mut rust_agent::agent::actor::AgentActor,
    ) {
    }

    fn started(
        &self,
        info: rust_agent::session::StartedSession,
        _sender: rust_agent::session::SessionSender,
        _subscription: rust_agent::session::Subscription,
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
    use rust_agent::error::ServerError;

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
            .create_session(&ws, None, None)
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
                        .create_or_resume_session(&ws, None, None, Some(id))
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
