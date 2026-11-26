#![cfg(feature = "test-support")]
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use withnav::session::subagents::root_watchdog::{
    GoalSource, ProgressReviewer, REVIEW_INPUT_BYTES, Review, ReviewOutcome, ReviewRequest,
};
use withnav::session::subagents::watchdog::Verdict;
mod common;
use common::deadline::Deadline;
use common::server::{TestServerConfig, collect_until, send_text};
use common::sse::{spawn_streaming_provider, text_chunk, tool_call_chunk};
use common::watchdog::{ConstantJudge, FakeClock};

#[tokio::test]
async fn root_doubt_parks_after_results_and_owner_continue_releases_the_same_turn() {
    pause_lifecycle(0).await;
}

#[tokio::test]
async fn two_restarts_keep_fresh_captain_gate_when_only_admission_was_persisted() {
    pause_lifecycle(2).await;
}

#[tokio::test]
async fn captain_message_supersedes_live_pause_but_navigator_does_not() {
    pause_lifecycle(1).await;
}

#[tokio::test]
async fn skill_captain_message_supersedes_watchdog_pause_but_refused_skill_does_not() {
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
        let skills = withnav::agent::skills::Skills::load(
            &withnav::agent::instructions::RulesRoots { home: None },
            &ws,
            &withnav::settings::Settings::default(),
        );
        let snapshot = skills.snapshot("review").unwrap();
        withnav::permission::store::PermissionStore::open(tmp.path().join("data"))
            .unwrap()
            .validate(
                withnav::permission::validation::ValidationKind::Skill,
                &snapshot.name,
                &snapshot.location,
                &snapshot.hash,
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
    service.install_root_watchdog_test(
        clock.clone(),
        Arc::new(ConstantJudge(Verdict::Doubt)),
        None,
    );
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
    let typed: withnav::bus::event::AgentYield =
        serde_json::from_value(pause.payload.clone()).unwrap();
    assert!(typed.question.contains("alt+c"));
    let token = serde_json::from_value(typed.details.unwrap()["pause_token"].clone()).unwrap();
    let loaded = service.history().load(id).await.unwrap().unwrap();
    assert!(loaded.transcript.iter().any(|m| matches!(m, withnav::provider::openai::ChatMessage::Tool { tool_call_id, .. } if tool_call_id == "glob")));
    assert!(provider.requests.try_recv().is_err());
    if mode == 2 {
        let child = service
            .create_child_session(id, "vigie", "finish child now")
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
        let saved = service.history().load(id).await.unwrap().unwrap();
        let admitted = withnav::bus::event::EventEnvelope::new(
            id,
            saved.meta.last_seq + 1,
            withnav::bus::event::EventKind::MessageReceived,
            serde_json::json!({
                "source":"user:http","target":"agent:main","command_id":ulid::Ulid::generate(),"mode":"next_turn","content":"lost admitted correction"
            }),
        );
        service.close_session(id).await.unwrap();
        // Install the admission last, as if the process died before its command was consumed.
        let saved = service.history().load(id).await.unwrap().unwrap();
        let mut admitted = admitted;
        admitted.seq = saved.meta.last_seq + 1;
        service
            .history()
            .append_event_acked(id, admitted)
            .await
            .unwrap();
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
                .send(input_command(id, source, "not captain input"))
                .await
                .unwrap();
        }
        assert!(
            provider.requests.try_recv().is_err(),
            "recovery must await a real captain"
        );
        assert!(
            service
                .continue_child_with_token(id, id, Some(token))
                .await
                .is_err()
        );
        service
            .send(input_command(id, "user:cli", "replacement instruction"))
            .await
            .unwrap();
    } else if mode == 3 {
        use withnav::bus::command::ContentBlock;
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
        let command = withnav::bus::command::Command {
            command_id: ulid::Ulid::generate(),
            session_id: id,
            source_actor_id: "navigator".into(),
            target_actor_id: "agent:main".into(),
            mode: withnav::bus::command::DeliveryMode::NextTurn,
            content: vec![withnav::bus::command::ContentBlock::Text {
                text: "autonomous wake".into(),
            }],
            created_at: std::time::SystemTime::now(),
        };
        service.send(command).await.unwrap();
        for source in ["user:subagent", "parent:agent"] {
            service
                .send(input_command(id, source, "not captain input"))
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
        assert!(
            service
                .continue_child_with_token(id, id, Some(token))
                .await
                .is_err()
        );
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
    // Every turn carries a task id, never the nil one. Within one actor the
    // released turn keeps the parked turn's id; mode 2 closes and recreates
    // the session, so its second turn starts a fresh task on a fresh actor.
    let started: Vec<Option<ulid::Ulid>> = events
        .iter()
        .filter(|e| e.kind == "turn.start")
        .map(|e| e.correlation_id)
        .collect();
    assert_eq!(started.len(), 2, "exactly two turn starts");
    for task_id in &started {
        assert_ne!(*task_id, Some(ulid::Ulid::nil()));
    }
    if mode != 2 {
        assert_eq!(
            started[0], started[1],
            "the released turn keeps the parked turn's task id"
        );
    }
    assert_eq!(
        events.iter().filter(|e| e.kind == "turn.end").count(),
        2,
        "each turn ends once"
    );
    assert!(
        events
            .iter()
            .any(|e| e.payload["data"]["resolution"].is_string())
    );
    if mode == 2 {
        // The turn-end transcript snapshot is saved just after the turn's
        // terminal events are shown, so wait for it rather than read once.
        common::deadline::eventually("the replacement in the stored transcript", || async {
            let saved = service.history().load(id).await.unwrap().unwrap();
            saved.transcript.iter().any(|message| matches!(message,withnav::provider::openai::ChatMessage::User {content,..} if content == "replacement instruction")).then_some(())
        })
        .await;
        assert!(
            events
                .iter()
                .any(|event| event.payload["data"]["captain_committed"] == true)
        );
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
    id: withnav::session::SessionId,
    source: &str,
    text: &str,
) -> withnav::bus::command::Command {
    withnav::bus::command::Command {
        command_id: ulid::Ulid::generate(),
        session_id: id,
        source_actor_id: source.into(),
        target_actor_id: "agent:main".into(),
        mode: withnav::bus::command::DeliveryMode::NextTurn,
        content: vec![withnav::bus::command::ContentBlock::Text { text: text.into() }],
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
        .allow_everything()
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
        Arc::new(ConstantJudge(Verdict::Active)),
        None,
    );
    let id = service.create_session(&ws, None, false).await.unwrap();
    send_text(&service, id, "keep looking").await;
    let sub = service.subscribe(id, None).await.unwrap();
    let first = common::server::collect_until_within(
        sub,
        |e| e.kind == "turn.end",
        Duration::from_secs(120),
    )
    .await;
    assert_eq!(
        first.iter().filter(|e| e.kind == "llm.response").count(),
        500
    );
    assert!(sleeps.try_recv().is_err());
    let child = service
        .create_child_session(id, "vigie", "finish child now")
        .await
        .unwrap();
    let sub = service.subscribe(child, None).await.unwrap();
    collect_until(sub, |e| e.kind == "turn.end").await;
    let mut command = withnav::bus::command::Command {
        command_id: ulid::Ulid::generate(),
        session_id: id,
        source_actor_id: "navigator".into(),
        target_actor_id: "agent:main".into(),
        mode: withnav::bus::command::DeliveryMode::NextTurn,
        content: vec![withnav::bus::command::ContentBlock::Text {
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
    assert_eq!(provider.bodies().len(), 501);
    command.command_id = ulid::Ulid::generate();
    command.source_actor_id = "user:http".into();
    command.content = vec![withnav::bus::command::ContentBlock::Text {
        text: "fresh captain window".into(),
    }];
    service.send(command).await.unwrap();
    let sub = service.subscribe(id, None).await.unwrap();
    let second = common::server::collect_until_within(
        sub,
        |e| e.kind == "turn.end" && e.seq > first.last().unwrap().seq,
        Duration::from_secs(120),
    )
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
        .allow_everything()
        .state();
    let id = service.create_session(&ws, None, false).await.unwrap();
    let sub = service.subscribe(id, None).await.unwrap();
    send_text(&service, id, "keep looking").await;
    let events = common::server::collect_until_within(
        sub,
        |e| e.kind == "turn.end",
        Duration::from_secs(120),
    )
    .await;
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
    service.install_root_watchdog_test(
        clock.clone(),
        Arc::new(ConstantJudge(Verdict::Stuck)),
        None,
    );
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
    assert!(events.iter().any(|e| {
        e.payload["data"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("Stopped by watchdog"))
    }));
    service.close_all().await;
}

#[tokio::test]
async fn a_watchdog_judgement_is_recorded_with_its_latency() {
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
    service.install_root_watchdog_test(
        clock.clone(),
        Arc::new(ConstantJudge(Verdict::Active)),
        None,
    );
    let id = service.create_session(&ws, None, false).await.unwrap();
    send_text(&service, id, "find the parser files").await;
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let tick = sleeps.recv().bounded(file!(), line!()).await.unwrap();
    *clock.now.lock().unwrap() += Duration::from_secs(300);
    tick.send(()).unwrap();

    let log = withnav::paths::instinct_call_log(&tmp.path().join("data"));
    let line: serde_json::Value = common::deadline::eventually("the watchdog record", || {
        let log = log.clone();
        async move {
            let text = std::fs::read_to_string(&log).ok()?;
            serde_json::from_str(text.lines().next()?).ok()
        }
    })
    .await;

    assert_eq!(line["caller"], "root_watchdog");
    assert_eq!(line["session_id"], id.to_string());
    assert_eq!(line["questions"], serde_json::json!(["subagent_progress"]));
    assert_eq!(line["answers"]["subagent_progress"]["choice"], "active");
    assert_eq!(line["error"], serde_json::Value::Null);
    assert!(line["latency_ms"].is_u64());
    assert!(line["state_bytes"].as_u64().is_some_and(|bytes| bytes > 0));
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        !text.contains("find the parser files"),
        "the goal is part of the state: {text}"
    );
    stream.send(text_chunk("done")).unwrap();
    drop(stream);
    service.close_all().await;
}

/// Records each review request and answers with one fixed outcome.
struct CapturingReviewer {
    seen: mpsc::UnboundedSender<ReviewRequest>,
    outcome: ReviewOutcome,
}

impl ProgressReviewer for CapturingReviewer {
    fn review<'a>(
        &'a self,
        request: &'a ReviewRequest,
        _: &'a tokio_util::sync::CancellationToken,
    ) -> futures::future::BoxFuture<'a, ReviewOutcome> {
        Box::pin(async move {
            self.seen.send(request.clone()).unwrap();
            self.outcome.clone()
        })
    }
}

/// Starts a root session whose instinct always doubts, optionally sets a goal, sends
/// `find files`, and fires one watchdog check while the provider still streams. Returns the
/// request the reviewer read and the events up to the first watchdog notice when `outcome` stops.
async fn one_review(
    goal: Option<(&str, &str)>,
    outcome: ReviewOutcome,
) -> (ReviewRequest, Vec<withnav::bus::event::EventEnvelope>) {
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
    let (seen, mut requests) = mpsc::unbounded_channel();
    let stops = outcome.decision == Review::Stop;
    service.install_root_watchdog_test(
        clock.clone(),
        Arc::new(ConstantJudge(Verdict::Doubt)),
        Some(Arc::new(CapturingReviewer { seen, outcome })),
    );
    let id = service.create_session(&ws, None, false).await.unwrap();
    if let Some((objective, criterion)) = goal {
        for arguments in [objective.to_owned(), format!("criterion {criterion}")] {
            assert!(
                service
                    .run_command(id, "goal", &arguments)
                    .await
                    .unwrap()
                    .ok
            );
        }
    }
    let sub = service.subscribe(id, None).await.unwrap();
    send_text(&service, id, "find files").await;
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    // The reviewer folds the stored events, which the store writes after they are streamed:
    // wait until the turn's start is stored so the ledger it reads is fixed, not a race.
    common::deadline::eventually("the stored turn.start", || async {
        let events = service.history().replay(id, None, u64::MAX).await.unwrap();
        events.iter().any(|e| e.kind == "turn.start").then_some(())
    })
    .await;
    let tick = sleeps.recv().bounded(file!(), line!()).await.unwrap();
    *clock.now.lock().unwrap() += Duration::from_secs(300);
    tick.send(()).unwrap();
    let request = requests.recv().bounded(file!(), line!()).await.unwrap();
    let events = if stops {
        collect_until(sub, |e| e.payload["name"] == "watchdog").await
    } else {
        Vec::new()
    };
    stream.send(text_chunk("done")).unwrap();
    drop(stream);
    service.close_all().await;
    (request, events)
}

#[tokio::test]
async fn the_root_reviewer_reads_the_active_goal_and_the_session_ledger() {
    let (request, _) = one_review(
        Some(("ship the parser", "tests pass")),
        Review::Continue.into(),
    )
    .await;
    assert_eq!(request.goal_source, GoalSource::SessionGoal);
    assert_eq!(
        request.evidence.goal,
        "Objective: ship the parser\n1. tests pass"
    );
    let ledger = request.ledger.as_ref().expect("the ledger was read");
    assert_eq!(
        ledger.turns.count, 1,
        "the running turn is folded: its start was stored before the check"
    );
    assert_eq!(ledger.goal, None, "the goal field already carries it");
    let content = request.user_content();
    assert!(content.len() <= REVIEW_INPUT_BYTES);
    assert!(content.contains("session ledger: facts folded from the event log"));
}

#[tokio::test]
async fn without_a_goal_the_reviewer_reads_the_last_request_and_its_reason_reaches_the_stop() {
    let (request, events) = one_review(
        None,
        ReviewOutcome {
            decision: Review::Stop,
            reason: Some("the same search keeps failing".into()),
        },
    )
    .await;
    assert_eq!(request.goal_source, GoalSource::LastCaptainMessage);
    assert_eq!(request.evidence.goal, "find files");
    assert!(request.ledger.is_some());
    let notice = events
        .iter()
        .find(|e| e.payload["name"] == "watchdog")
        .unwrap();
    assert_eq!(
        notice.payload["data"]["reason"],
        "the same search keeps failing"
    );
    assert!(
        notice.payload["data"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Stopped by watchdog")
    );
}
