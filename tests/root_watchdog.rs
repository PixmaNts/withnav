use futures::future::BoxFuture;
use navai::session::subagents::watchdog::{Clock, Evidence, Judge, JudgeError, Verdict};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
mod common;
use common::deadline::Deadline;
use common::server::{collect_until, send_text, TestServerConfig};
use common::sse::{spawn_streaming_provider, text_chunk, tool_call_chunk};

struct FakeClock {
    now: Mutex<Instant>,
    sleeps: mpsc::UnboundedSender<oneshot::Sender<()>>,
}
impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.now.lock().unwrap()
    }
    fn sleep(&self, _: Duration) -> BoxFuture<'static, ()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.sleeps.send(tx);
        Box::pin(async move {
            let _ = rx.await;
        })
    }
}
struct FakeJudge(Verdict);
impl Judge for FakeJudge {
    fn judge<'a>(
        &'a self,
        _: &'a Evidence,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Verdict, JudgeError>> {
        Box::pin(async { Ok(self.0) })
    }
}

#[tokio::test]
async fn root_doubt_parks_after_results_and_owner_continue_releases_the_same_turn() {
    pause_lifecycle(0).await;
}

#[tokio::test]
async fn two_restarts_keep_fresh_driver_gate_when_only_admission_was_persisted() {
    pause_lifecycle(2).await;
}

#[tokio::test]
async fn driver_message_supersedes_live_pause_but_navigator_does_not() {
    pause_lifecycle(1).await;
}

#[tokio::test]
async fn skill_driver_message_supersedes_watchdog_pause_but_refused_skill_does_not() {
    pause_lifecycle(3).await;
}

async fn pause_lifecycle(mode: u8) {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    if mode == 3 {
        let skill = ws.join(".agents/skills/review");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: review\ndescription: Review work\n---\nReview selected work carefully.",
        )
        .unwrap();
    }
    let mut provider = spawn_streaming_provider().await;
    let service = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let (tx, mut sleeps) = mpsc::unbounded_channel();
    let clock = Arc::new(FakeClock {
        now: Mutex::new(Instant::now()),
        sleeps: tx,
    });
    service.install_root_watchdog_test(clock.clone(), Arc::new(FakeJudge(Verdict::Doubt)), None);
    let id = service.create_session(&ws, None, false).await.unwrap();
    send_text(&service, id, "find files").await;
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let tick = sleeps.recv().bounded(file!(), line!()).await.unwrap();
    *clock.now.lock().unwrap() += Duration::from_secs(300);
    tick.send(()).unwrap();
    let _next_tick = sleeps.recv().bounded(file!(), line!()).await.unwrap();
    stream
        .send(tool_call_chunk(
            "glob",
            "glob",
            &serde_json::json!({"pattern":"*"}),
        ))
        .unwrap();
    drop(stream);
    let sub = service.subscribe(id, None).await.unwrap();
    let events = collect_until(sub, |event| event.kind == "turn.end").await;
    let pause = events
        .iter()
        .find(|event| event.kind == "agent.yield" && event.payload["kind"] == "watchdog")
        .unwrap();
    let typed: navai::bus::event::AgentYield =
        serde_json::from_value(pause.payload.clone()).unwrap();
    assert!(typed.question.contains("alt+c"));
    let token = serde_json::from_value(typed.details.unwrap()["pause_token"].clone()).unwrap();
    service.history().flush().await.unwrap();
    let loaded = service.history().load(id).await.unwrap().unwrap();
    assert!(loaded.transcript.iter().any(|m| matches!(m, navai::provider::openai::ChatMessage::Tool { tool_call_id, .. } if tool_call_id == "glob")));
    assert!(provider.requests.try_recv().is_err());
    if mode == 2 {
        let child = service
            .create_child_session(id, "explore", "finish child now")
            .await
            .unwrap();
        let (_, child_stream) = provider
            .requests
            .recv()
            .bounded(file!(), line!())
            .await
            .unwrap();
        child_stream.send(text_chunk("child finished")).unwrap();
        drop(child_stream);
        collect_until(service.subscribe(child, None).await.unwrap(), |e| {
            e.kind == "turn.end"
        })
        .await;
        service.close_session(id).await.unwrap();
        // Simulate a crash after the settled checkpoint but before a resolution was stored.
        let db = rusqlite::Connection::open(tmp.path().join("history.db")).unwrap();
        db.execute("DELETE FROM events WHERE session_id=?1 AND kind='command.result' AND json_extract(envelope,'$.payload.data.resolution')='expired'", [id.to_string()]).unwrap();
        drop(db);
        service
            .create_or_resume_session(&ws, None, false, Some(id))
            .await
            .unwrap();
        // Admission is durable, but the crash precedes consumption into the transcript.
        service.history().flush().await.unwrap();
        let saved = service.history().load(id).await.unwrap().unwrap();
        let admitted = navai::bus::event::EventEnvelope::new(
            id,
            saved.meta.last_seq + 1,
            navai::bus::event::EventKind::MessageReceived,
            serde_json::json!({
                "source":"user:http","target":"agent:main","command_id":ulid::Ulid::new(),"mode":"next_turn","content":"lost admitted correction"
            }),
        );
        service.close_session(id).await.unwrap();
        // Install the admission last, as if the process died before its command was consumed.
        let saved = service.history().load(id).await.unwrap().unwrap();
        let mut admitted = admitted;
        admitted.seq = saved.meta.last_seq + 1;
        service.history().append_event(id, admitted);
        service.history().flush().await.unwrap();
        service
            .create_or_resume_session(&ws, None, false, Some(id))
            .await
            .unwrap();
        assert!(
            !service
                .history()
                .unconsumed_completions(id)
                .await
                .unwrap()
                .is_empty(),
            "completion remains outstanding across recovery"
        );
        for source in ["navigator", "user:subagent", "parent:agent"] {
            service
                .send(input_command(id, source, "not driver input"))
                .await
                .unwrap();
        }
        service.history().flush().await.unwrap();
        assert!(
            provider.requests.try_recv().is_err(),
            "recovery must await a real driver"
        );
        assert!(service
            .continue_child_with_token(id, id, Some(token))
            .await
            .is_err());
        service
            .send(input_command(id, "user:cli", "replacement instruction"))
            .await
            .unwrap();
    } else if mode == 3 {
        use navai::bus::command::ContentBlock;
        let mut refused = input_command(id, "user:http", "refused correction");
        refused.content.push(ContentBlock::Skill {
            name: "missing-skill".into(),
        });
        assert!(service.send(refused).await.is_err());
        assert!(service.continue_child(id, id).await.is_err());
        assert!(provider.requests.try_recv().is_err());
        let mut selected = input_command(id, "user:http", "replacement instruction");
        selected.content.push(ContentBlock::Skill {
            name: "review".into(),
        });
        service.send(selected).await.unwrap();
    } else if mode == 1 {
        let command = navai::bus::command::Command {
            command_id: ulid::Ulid::new(),
            session_id: id,
            source_actor_id: "navigator".into(),
            target_actor_id: "agent:main".into(),
            mode: navai::bus::command::DeliveryMode::NextTurn,
            content: vec![navai::bus::command::ContentBlock::Text {
                text: "autonomous wake".into(),
            }],
            created_at: std::time::SystemTime::now(),
        };
        service.send(command).await.unwrap();
        for source in ["user:subagent", "parent:agent"] {
            service
                .send(input_command(id, source, "not driver input"))
                .await
                .unwrap();
        }
        assert!(provider.requests.try_recv().is_err());
        send_text(&service, id, "replacement instruction").await;
    } else {
        assert!(service.continue_child(id, id).await.is_err());
        service
            .continue_child_with_token(id, id, Some(token))
            .await
            .unwrap();
        assert!(service
            .continue_child_with_token(id, id, Some(token))
            .await
            .is_err());
    }
    let (request, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(request.contains("\"role\":\"tool\""));
    if mode > 0 {
        assert!(request.contains("replacement instruction"));
    }
    if mode == 3 {
        assert!(request.contains("Review selected work carefully."));
        assert!(!request.contains("refused correction"));
    }
    if mode == 2 {
        assert!(!request.contains("lost admitted correction"));
    }
    stream.send(text_chunk("finished")).unwrap();
    drop(stream);
    let sub = service.subscribe(id, None).await.unwrap();
    let events = collect_until(sub, |event| {
        event.kind == "turn.end" && event.payload["outcome"] == "done"
    })
    .await;
    assert_eq!(events.iter().filter(|e| e.kind == "turn.start").count(), 2);
    assert_eq!(
        events.iter().filter(|e| e.kind == "turn.end").count(),
        2,
        "each turn ends once"
    );
    assert!(events
        .iter()
        .any(|e| e.payload["data"]["resolution"].is_string()));
    if mode == 2 {
        service.history().flush().await.unwrap();
        let saved = service.history().load(id).await.unwrap().unwrap();
        assert!(saved.transcript.iter().any(|message| matches!(message,navai::provider::openai::ChatMessage::User {content,..} if content == "replacement instruction")));
        assert!(events
            .iter()
            .any(|event| event.payload["data"]["driver_committed"] == true));
        service.close_session(id).await.unwrap();
        service
            .create_or_resume_session(&ws, None, false, Some(id))
            .await
            .unwrap();
        service
            .send(input_command(
                id,
                "navigator",
                "healthy autonomous wake after committed correction",
            ))
            .await
            .unwrap();
        let (request, stream) = provider
            .requests
            .recv()
            .bounded(file!(), line!())
            .await
            .unwrap();
        assert!(request.contains("replacement instruction"));
        assert!(!request.contains("lost admitted correction"));
        stream.send(text_chunk("healthy recovery")).unwrap();
        drop(stream);
        collect_until(service.subscribe(id, None).await.unwrap(), |e| {
            e.kind == "turn.end" && e.payload["outcome"] == "done"
        })
        .await;
    }
    service.close_all().await;
    assert!(
        _next_tick.is_closed(),
        "supervisor sleep is cancelled and joined on runtime exit"
    );
}

fn input_command(
    id: navai::session::SessionId,
    source: &str,
    text: &str,
) -> navai::bus::command::Command {
    navai::bus::command::Command {
        command_id: ulid::Ulid::new(),
        session_id: id,
        source_actor_id: source.into(),
        target_actor_id: "agent:main".into(),
        mode: navai::bus::command::DeliveryMode::NextTurn,
        content: vec![navai::bus::command::ContentBlock::Text { text: text.into() }],
        created_at: std::time::SystemTime::now(),
    }
}

#[tokio::test]
async fn watchdog_off_uses_fallback_and_autonomous_results_cannot_renew_it() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let provider = common::sse::spawn_responding_provider(1001, |_, request| {
        if request.contains("finish child now") {
            common::sse::http_sse(&common::sse::sse_body(&[&text_chunk("child finished")]))
        } else {
            common::sse::http_sse(&common::sse::sse_body(&[&tool_call_chunk(
                "glob",
                "glob",
                &serde_json::json!({"pattern":"*"}),
            )]))
        }
    })
    .await;
    let service = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(1_000_000)
        .state();
    std::fs::write(
        tmp.path().join("cfg/config.json"),
        r#"{"agents":{"build":{"model":"mock/mock-model"}},"agent":{"watchdog_interval_secs":0}}"#,
    )
    .unwrap();
    let (tx, mut sleeps) = mpsc::unbounded_channel();
    service.install_root_watchdog_test(
        Arc::new(FakeClock {
            now: Mutex::new(Instant::now()),
            sleeps: tx,
        }),
        Arc::new(FakeJudge(Verdict::Active)),
        None,
    );
    let id = service.create_session(&ws, None, false).await.unwrap();
    send_text(&service, id, "keep looking").await;
    let sub = service.subscribe(id, None).await.unwrap();
    let first = collect_until(sub, |e| e.kind == "turn.end").await;
    assert_eq!(
        first.iter().filter(|e| e.kind == "llm.response").count(),
        500
    );
    assert!(sleeps.try_recv().is_err());
    let child = service
        .create_child_session(id, "explore", "finish child now")
        .await
        .unwrap();
    let sub = service.subscribe(child, None).await.unwrap();
    collect_until(sub, |e| e.kind == "turn.end").await;
    let mut command = navai::bus::command::Command {
        command_id: ulid::Ulid::new(),
        session_id: id,
        source_actor_id: "navigator".into(),
        target_actor_id: "agent:main".into(),
        mode: navai::bus::command::DeliveryMode::NextTurn,
        content: vec![navai::bus::command::ContentBlock::Text {
            text: "autonomous wake".into(),
        }],
        created_at: std::time::SystemTime::now(),
    };
    service.send(command.clone()).await.unwrap();
    for source in ["user:subagent", "parent:agent"] {
        service
            .send(input_command(id, source, "do not renew"))
            .await
            .unwrap();
    }
    service.history().flush().await.unwrap();
    assert_eq!(provider.bodies().len(), 501);
    command.command_id = ulid::Ulid::new();
    command.source_actor_id = "user:http".into();
    command.content = vec![navai::bus::command::ContentBlock::Text {
        text: "fresh driver window".into(),
    }];
    service.send(command).await.unwrap();
    let sub = service.subscribe(id, None).await.unwrap();
    let second = collect_until(sub, |e| {
        e.kind == "turn.end" && e.seq > first.last().unwrap().seq
    })
    .await;
    assert_eq!(
        second.iter().filter(|e| e.kind == "llm.response").count(),
        1000
    );
    assert_eq!(provider.bodies().len(), 1001);
    service.close_all().await;
}

#[tokio::test]
async fn absent_instinct_caps_at_five_hundred_and_announces_once() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let provider = common::sse::spawn_responding_provider(501, |_, _| {
        common::sse::http_sse(&common::sse::sse_body(&[&tool_call_chunk(
            "glob",
            "glob",
            &serde_json::json!({"pattern":"*"}),
        )]))
    })
    .await;
    let service = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(1_000_000)
        .state();
    let id = service.create_session(&ws, None, false).await.unwrap();
    let sub = service.subscribe(id, None).await.unwrap();
    send_text(&service, id, "keep looking").await;
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    assert_eq!(
        events.iter().filter(|e| e.kind == "llm.response").count(),
        500
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.payload["name"] == "watchdog")
            .count(),
        1
    );
    assert!(events.iter().any(|e| e.payload["kind"] == "budget"));
    service.continue_child(id, id).await.unwrap();
    service.close_all().await;
}

#[tokio::test]
async fn stuck_root_settles_tool_results_without_requesting_another_step() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let mut provider = spawn_streaming_provider().await;
    let service = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let (tx, mut sleeps) = mpsc::unbounded_channel();
    let clock = Arc::new(FakeClock {
        now: Mutex::new(Instant::now()),
        sleeps: tx,
    });
    service.install_root_watchdog_test(clock.clone(), Arc::new(FakeJudge(Verdict::Stuck)), None);
    let id = service.create_session(&ws, None, false).await.unwrap();
    send_text(&service, id, "find files").await;
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let tick = sleeps.recv().bounded(file!(), line!()).await.unwrap();
    *clock.now.lock().unwrap() += Duration::from_secs(300);
    tick.send(()).unwrap();
    let sub = service.subscribe(id, None).await.unwrap();
    collect_until(sub, |e| e.payload["name"] == "watchdog").await;
    stream
        .send(tool_call_chunk(
            "glob",
            "glob",
            &serde_json::json!({"pattern":"*"}),
        ))
        .unwrap();
    drop(stream);
    let sub = service.subscribe(id, None).await.unwrap();
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    let post = events
        .iter()
        .position(|e| e.kind == "tool.post_call")
        .unwrap();
    let end = events.iter().position(|e| e.kind == "turn.end").unwrap();
    assert!(post < end);
    assert!(provider.requests.try_recv().is_err());
    assert!(events.iter().any(|e| e.payload["data"]["message"]
        .as_str()
        .is_some_and(|m| m.contains("Stopped by watchdog"))));
    service.close_all().await;
}
