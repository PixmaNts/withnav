use std::time::Duration;

use serde_json::{Value, json};
use withnav::provider::openai::ChatMessage;
use withnav::server::AppState;

mod common;
use common::deadline::{Deadline, WAIT};
use common::sse::request_matching;

async fn spawn_linked_child(
    state: &AppState,
    ws: &std::path::Path,
    agent: &str,
    prompt: &str,
) -> (ulid::Ulid, ulid::Ulid) {
    let parent = state
        .create_session(ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = state
        .create_child_session(parent, agent, prompt)
        .bounded(file!(), line!())
        .await
        .unwrap();
    (parent, child)
}

/// A flow of unrelated requests must not hold off the deadline: the search gives
/// up once `WAIT` has passed in total, however often other requests arrive.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "no provider request matched within")]
async fn request_matching_gives_up_despite_unrelated_requests() {
    let (tx, mut requests) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move {
        loop {
            let (stream, _) = tokio::sync::mpsc::unbounded_channel();
            if tx.send(("unrelated".to_string(), stream)).await.is_err() {
                return;
            }
            tokio::time::sleep(WAIT / 2).await;
        }
    });
    let mut pending = Vec::new();
    // Virtual time: this outer bound only fires if the search never gives up, and
    // its message differs from the one `should_panic` expects.
    if tokio::time::timeout(
        WAIT * 6,
        request_matching(&mut requests, &mut pending, |_| false),
    )
    .await
    .is_err()
    {
        panic!("request_matching never gave up");
    }
}

/// A parent request carrying a completed-child notice (`"outcome":"done"`).
fn is_completion_wake(body: &str) -> bool {
    let v: Value =
        serde_json::from_str(body.split_once("\r\n\r\n").unwrap().1).unwrap_or(Value::Null);
    v["messages"].as_array().is_some_and(|msgs| {
        msgs.iter().any(|m| {
            m["content"]
                .as_str()
                .and_then(|c| serde_json::from_str::<Value>(c).ok())
                .is_some_and(|n| n["outcome"] == "done")
        })
    })
}

/// A child's own turn: a request holding a message whose content is exactly
/// `goal`. A parent's wake notice also mentions the goal inside JSON, so a
/// substring match would take the parent's request for the child's.
fn is_child_turn(goal: &'static str) -> impl Fn(&str) -> bool {
    move |body: &str| {
        let v: Value =
            serde_json::from_str(body.split_once("\r\n\r\n").unwrap().1).unwrap_or(Value::Null);
        v["messages"]
            .as_array()
            .is_some_and(|msgs| msgs.iter().any(|m| m["content"].as_str() == Some(goal)))
    }
}

#[tokio::test]
async fn owner_continue_admits_one_budget_renewal_and_child_finishes() {
    use tower::ServiceExt;
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .allow_everything()
        .state();
    let parent = state
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let other = state
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = state
        .create_child_session(parent, "vigie", "inspect")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let mut pending = Vec::new();
    let (_, first) = request_matching(
        &mut provider.requests,
        &mut pending,
        is_child_turn("inspect"),
    )
    .await;
    assert!(matches!(
        state
            .continue_child(parent, child)
            .bounded(file!(), line!())
            .await,
        Err(withnav::error::ServerError::ChildNotParked)
    ));
    assert!(matches!(
        state
            .continue_child(other, child)
            .bounded(file!(), line!())
            .await,
        Err(withnav::error::ServerError::ChildReadOnly)
    ));
    let steps = withnav::agent::run_loop::max_steps();
    let mut stream = Some(first);
    for index in 0..steps {
        let tx = stream.take().unwrap();
        tx.send(tool_call_chunk(
            &format!("read-{index}"),
            "glob",
            &json!({"pattern":"*"}),
        ))
        .unwrap();
        drop(tx);
        if index + 1 < steps {
            stream = Some(
                request_matching(
                    &mut provider.requests,
                    &mut pending,
                    is_child_turn("inspect"),
                )
                .await
                .1,
            );
        }
    }
    tokio::time::timeout(WAIT, async {
        loop {
            if let withnav::bus::ResumeOutcome::Replay(events) = state
                .subscribe(child, None)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .replay
                && events
                    .iter()
                    .any(|e| e.kind == "agent.yield" && e.payload["kind"] == "budget")
            {
                break;
            }
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    // The budget park woke the parent: its turn carries the notice with the
    // counts and evidence. The parent answers it, so its turn ends.
    let (notice, parent_turn) = request_matching(&mut provider.requests, &mut pending, |body| {
        body.contains("used its 50 steps")
    })
    .await;
    assert!(notice.contains("top-ups used 0/3"), "{notice}");
    if let withnav::bus::ResumeOutcome::Replay(events) = state
        .subscribe(parent, None)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .replay
    {
        assert!(
            events
                .iter()
                .any(|event| event.kind == "subagent.watchdog"
                    && event.payload["verdict"] == "budget")
        );
        assert!(!events.iter().any(|event| event.kind == "subagent.done"));
    } else {
        panic!("expected parent event replay");
    }

    assert!(notice.contains("inspect"), "{notice}");
    parent_turn
        .send(text_chunk("parent noted the park"))
        .unwrap();
    drop(parent_turn);
    let app = withnav::server::app(state.clone());
    let request = || {
        axum::http::Request::post(format!("/v1/sessions/{parent}/children/{child}/continue"))
            .body(axum::body::Body::empty())
            .unwrap()
    };
    let (a, b) = tokio::join!(
        app.clone().oneshot(request()),
        app.clone().oneshot(request())
    );
    let mut statuses = vec![a.unwrap().status().as_u16(), b.unwrap().status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, vec![200, 409]);
    let (_, stream) = request_matching(
        &mut provider.requests,
        &mut pending,
        is_child_turn("inspect"),
    )
    .await;
    stream.send(text_chunk("child complete")).unwrap();
    drop(stream);
    // The parent's completion wake: a notice whose outcome is `done`. The earlier
    // budget notice is pending too and must not match.
    let (_, stream) =
        request_matching(&mut provider.requests, &mut pending, is_completion_wake).await;
    assert_eq!(
        state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "done"
    );
    assert_eq!(state.reserved_child_count(parent), 0);
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

/// Runs `child`'s steps to the budget boundary: `steps` tool calls on the
/// streaming provider, each answered by one request, then waits until its
/// budget yield is recorded.
async fn exhaust_child_budget(
    provider: &mut common::sse::StreamingProvider,
    pending: &mut Vec<(String, tokio::sync::mpsc::UnboundedSender<String>)>,
    state: &AppState,
    child: ulid::Ulid,
    first: tokio::sync::mpsc::UnboundedSender<String>,
    call_prefix: &str,
    steps: usize,
) {
    let mut stream = Some(first);
    for index in 0..steps {
        let tx = stream.take().unwrap();
        let mut chunk: Value = serde_json::from_str(&tool_call_chunk(
            &format!("{call_prefix}-{index}"),
            "glob",
            &json!({"pattern":"*"}),
        ))
        .unwrap();
        chunk["choices"][0]["delta"]["content"] = json!("Observable partial progress.");
        tx.send(chunk.to_string()).unwrap();
        drop(tx);
        if index + 1 < steps {
            stream = Some(
                request_matching(&mut provider.requests, pending, is_child_turn("inspect"))
                    .await
                    .1,
            );
        }
    }
    tokio::time::timeout(WAIT, async {
        loop {
            if let withnav::bus::ResumeOutcome::Replay(events) = state
                .subscribe(child, None)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .replay
                && events
                    .iter()
                    .any(|e| e.kind == "agent.yield" && e.payload["kind"] == "budget")
            {
                break;
            }
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
}

/// A budget notice whose first history write fails still reaches the owner
/// once, with no further child turn and nobody stepping in.
#[tokio::test]
async fn failed_budget_notice_write_is_retried_and_delivered_once() {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .allow_everything()
        .state();
    let (parent, child) = spawn_linked_child(&state, &ws, "vigie", "inspect").await;
    let mut pending = Vec::new();
    let (_, first) = request_matching(
        &mut provider.requests,
        &mut pending,
        is_child_turn("inspect"),
    )
    .await;
    state.fail_budget_notice_enqueue();
    let steps = withnav::agent::run_loop::max_steps();
    exhaust_child_budget(
        &mut provider,
        &mut pending,
        &state,
        child,
        first,
        "a",
        steps,
    )
    .await;
    let (notice, parent_turn) = request_matching(&mut provider.requests, &mut pending, |body| {
        body.contains("used its 50 steps")
    })
    .bounded(file!(), line!())
    .await;
    assert!(
        !state.budget_notice_failure_armed(),
        "the first enqueue must have failed"
    );
    assert!(notice.contains("top-ups used 0/3"), "{notice}");
    let withnav::bus::ResumeOutcome::Replay(events) = state
        .subscribe(parent, None)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .replay
    else {
        panic!("expected parent event replay");
    };
    let notices = events
        .iter()
        .filter(|e| e.kind == "subagent.watchdog" && e.payload["verdict"] == "budget")
        .count();
    assert_eq!(notices, 1);
    assert!(
        state
            .history()
            .pending_completions(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .is_empty()
    );
    // The child stays parked: it made no request after its budget ran out.
    while let Ok(request) = provider.requests.try_recv() {
        pending.push(request);
    }
    assert!(
        !pending
            .iter()
            .any(|(body, _)| is_child_turn("inspect")(body)),
        "the parked child must not run another turn"
    );
    parent_turn.send(text_chunk("noted")).unwrap();
    drop(parent_turn);
    state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn parent_tops_up_a_budget_parked_child_three_times_then_the_captain_continues() {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .allow_everything()
        .state();
    let (parent, child) = spawn_linked_child(&state, &ws, "vigie", "inspect").await;
    let mut pending = Vec::new();
    let (_, first) = request_matching(
        &mut provider.requests,
        &mut pending,
        is_child_turn("inspect"),
    )
    .await;
    let steps = withnav::agent::run_loop::max_steps();
    let service = state.service();
    let foreign = state
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(matches!(
        service
            .decide_subagent(foreign, child, false, None)
            .bounded(file!(), line!())
            .await,
        Err(withnav::error::ServerError::ChildReadOnly)
    ));

    exhaust_child_budget(
        &mut provider,
        &mut pending,
        &state,
        child,
        first,
        "a",
        steps,
    )
    .await;
    // The first park woke the parent with 0/3 used; it answers the notice.
    let (notice, parent_turn) = request_matching(&mut provider.requests, &mut pending, |body| {
        body.contains("used its 50 steps")
    })
    .await;
    assert!(notice.contains("top-ups used 0/3"), "{notice}");
    parent_turn.send(text_chunk("noted")).unwrap();
    drop(parent_turn);

    // Three parent top-ups: each renews the budget, the child runs to the next park.
    for top in 1..=3u32 {
        service
            .decide_subagent(
                parent,
                child,
                false,
                Some("Continue the remaining inspection"),
            )
            .bounded(file!(), line!())
            .await
            .unwrap_or_else(|e| panic!("top-up {top} refused: {e}"));
        let (_, stream) = request_matching(
            &mut provider.requests,
            &mut pending,
            is_child_turn("inspect"),
        )
        .await;
        exhaust_child_budget(
            &mut provider,
            &mut pending,
            &state,
            child,
            stream,
            &format!("t{top}"),
            steps,
        )
        .await;
        let (notice, parent_turn) =
            request_matching(&mut provider.requests, &mut pending, |body| {
                body.contains(&format!("top-ups used {top}/3"))
            })
            .await;
        assert!(notice.contains("steps"), "{notice}");
        parent_turn.send(text_chunk("noted")).unwrap();
        drop(parent_turn);
    }

    // The fourth is refused with the Captain's pointer; the child stays parked.
    let refused = service
        .decide_subagent(parent, child, false, None)
        .bounded(file!(), line!())
        .await;
    let message = refused.expect_err("fourth top-up refused").to_string();
    assert!(
        message.contains("top-up limit reached; the Captain can continue it"),
        "{message}"
    );
    // The Captain's continue still works after the cap and is not a top-up:
    // the child resumes and finishes.
    state
        .continue_child(parent, child)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = request_matching(
        &mut provider.requests,
        &mut pending,
        is_child_turn("inspect"),
    )
    .await;
    stream.send(text_chunk("child complete")).unwrap();
    drop(stream);
    let (_, wake) =
        request_matching(&mut provider.requests, &mut pending, is_completion_wake).await;
    drop(wake);
    assert_eq!(
        state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "done"
    );
    state.close_all().bounded(file!(), line!()).await;
}

/// A top-up that lands after the budget notice's first park check but before
/// its durable commit resumes the child: that notice is stale and must never
/// reach the parent. The next park's notice is delivered as usual.
#[tokio::test]
async fn topup_racing_the_budget_notice_never_delivers_a_stale_notice() {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .allow_everything()
        .state();
    let (parent, child) = spawn_linked_child(&state, &ws, "vigie", "inspect").await;
    let mut pending = Vec::new();
    let (_, first) = request_matching(
        &mut provider.requests,
        &mut pending,
        is_child_turn("inspect"),
    )
    .await;
    let steps = withnav::agent::run_loop::max_steps();
    let service = state.service();
    let barrier = withnav::session::ChildPreparationBarrier::default();
    service.pause_budget_notice(barrier.clone());

    exhaust_child_budget(
        &mut provider,
        &mut pending,
        &state,
        child,
        first,
        "a",
        steps,
    )
    .await;
    tokio::time::timeout(WAIT, barrier.reached.notified())
        .bounded(file!(), line!())
        .await
        .expect("budget notice reached its commit");
    service
        .decide_subagent(parent, child, false, None)
        .bounded(file!(), line!())
        .await
        .expect("top-up while the notice is in flight");
    barrier.release.notify_one();
    tokio::time::timeout(WAIT, barrier.reached.notified())
        .bounded(file!(), line!())
        .await
        .expect("the paused notice attempt finished");
    let outbox: i64 = rusqlite::Connection::open(tmp.path().join("history.db"))
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM completion_outbox WHERE parent_id=?1 AND outcome='watchdog'",
            [parent.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(outbox, 0, "the stale notice was committed for the parent");

    // The renewed child runs to its next park; that notice is the first the parent sees.
    let (_, stream) = request_matching(
        &mut provider.requests,
        &mut pending,
        is_child_turn("inspect"),
    )
    .await;
    exhaust_child_budget(
        &mut provider,
        &mut pending,
        &state,
        child,
        stream,
        "b",
        steps,
    )
    .await;
    let (notice, parent_turn) = request_matching(&mut provider.requests, &mut pending, |body| {
        body.contains("used its 50 steps")
    })
    .bounded(file!(), line!())
    .await;
    assert!(
        !notice.contains("top-ups used 0/3"),
        "stale notice delivered: {notice}"
    );
    assert!(notice.contains("top-ups used 1/3"), "{notice}");
    let withnav::bus::ResumeOutcome::Replay(events) = state
        .subscribe(parent, None)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .replay
    else {
        panic!("expected parent event replay");
    };
    let budget_notices: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "subagent.watchdog" && e.payload["verdict"] == "budget")
        .map(|e| e.payload["topups_used"].clone())
        .collect();
    assert_eq!(budget_notices, vec![json!(1)]);
    parent_turn.send(text_chunk("noted")).unwrap();
    drop(parent_turn);
    state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn owner_stop_on_a_budget_park_terminalizes_with_partial_output() {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .allow_everything()
        .state();
    let (parent, child) = spawn_linked_child(&state, &ws, "vigie", "inspect").await;
    let mut pending = Vec::new();
    let (_, first) = request_matching(
        &mut provider.requests,
        &mut pending,
        is_child_turn("inspect"),
    )
    .await;
    let steps = withnav::agent::run_loop::max_steps();
    exhaust_child_budget(
        &mut provider,
        &mut pending,
        &state,
        child,
        first,
        "s",
        steps,
    )
    .await;
    let (_, parent_turn) = request_matching(&mut provider.requests, &mut pending, |body| {
        body.contains("used its 50 steps")
    })
    .await;
    parent_turn.send(text_chunk("noted")).unwrap();
    drop(parent_turn);

    state
        .service()
        .decide_subagent(parent, child, true, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let meta = state
        .child_metadata(child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(meta.status, "stopped", "stop terminalizes the parked child");
    assert!(
        meta.result_text
            .as_deref()
            .unwrap()
            .contains("Observable partial progress.")
    );
    assert_eq!(
        state.reserved_child_count(parent),
        0,
        "capacity released once"
    );
    // The owner is told the child stopped at its budget, with the output so far.
    let (wake, parent_turn) = request_matching(&mut provider.requests, &mut pending, |body| {
        body.contains("stopped by owner at its step budget")
    })
    .await;
    // The request also carries this child's earlier budget-park notice: pick the
    // stop notice by its result, then check its structured fields.
    let notices = request_outputs(&wake);
    let notice = notices
        .iter()
        .find(|n| {
            n["child_session_id"] == child.to_string()
                && n["result"]
                    .as_str()
                    .is_some_and(|r| r.starts_with("stopped by owner at its step budget"))
        })
        .unwrap_or_else(|| panic!("no stop notice for the child: {notices:?}"));
    assert_eq!(notice["outcome"], "stopped", "{notice}");
    assert_eq!(notice["reason"], "budget", "{notice}");
    assert!(
        notice["result"]
            .as_str()
            .unwrap()
            .contains("Observable partial progress."),
        "the notice carries the partial output: {notice}"
    );
    drop(parent_turn);
    state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn queued_captain_correction_precedes_completion_only_wake() {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .send(common::server::captain_command(
            parent,
            "original instruction".into(),
        ))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, parent_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .create_child_session(parent, "vigie", "inspect")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, child_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    child_stream.send(text_chunk("child answer")).unwrap();
    drop(child_stream);
    notice_count(&state, parent, 1)
        .bounded(file!(), line!())
        .await;
    state
        .send(common::server::captain_command(
            parent,
            "captain correction".into(),
        ))
        .bounded(file!(), line!())
        .await
        .unwrap();
    parent_stream.send(text_chunk("old turn done")).unwrap();
    drop(parent_stream);
    let (request, stream) = tokio::time::timeout(WAIT, provider.requests.recv())
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let captain = messages
        .iter()
        .position(|m| m["content"] == "captain correction")
        .unwrap();
    let notice = messages
        .iter()
        .position(|m| {
            m["content"]
                .as_str()
                .is_some_and(|s| s.contains("untrusted_subagent_output"))
        })
        .unwrap();
    assert!(captain < notice);
    assert_eq!(request_outputs(&request).len(), 1);
    stream.send(text_chunk("corrected")).unwrap();
    drop(stream);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), provider.requests.recv())
            .bounded(file!(), line!())
            .await
            .is_err()
    );
    state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn mirrored_child_permission_is_resolved_after_parent_stop_and_resume() {
    let mut provider = common::sse::spawn_streaming_provider()
        .bounded(file!(), line!())
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir_all(ws.join(".withnav/agents")).unwrap();
    std::fs::write(
        ws.join(".withnav/agents/writer.md"),
        "---\nmode: subagent\naccess: full\n---\nWrite.",
    )
    .unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let (parent, child) = spawn_linked_child(&state, &ws, "writer", "write").await;
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream
        .send(tool_call_chunk(
            "pending",
            "bash",
            &json!({"command":"true"}),
        ))
        .unwrap();
    drop(stream);
    let request = tokio::time::timeout(WAIT, async {
        loop {
            if let withnav::bus::ResumeOutcome::Replay(events) = state
                .subscribe(parent, None)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .replay
                && let Some(e) = events
                    .into_iter()
                    .find(|e| e.kind == "permission.requested")
            {
                break e.payload["request_id"].clone();
            }
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    state
        .close_session(parent)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .create_or_resume_session(&ws, None, false, Some(parent))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let withnav::bus::ResumeOutcome::Replay(events) = state
        .subscribe(parent, None)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .replay
    else {
        panic!("replay");
    };
    let resolved = events
        .iter()
        .filter(|e| e.kind == "permission.resolved" && e.payload["request_id"] == request)
        .collect::<Vec<_>>();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["child_session_id"], child.to_string());
    assert_eq!(resolved[0].payload["resolution"], "cancelled");
    state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn abandoned_mirrored_permission_is_expired_on_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let config = TestServerConfig::unreachable(tmp.path())
        .chat_tools(128_000)
        .config();
    let store = withnav::session::store::HistoryStore::open(&config.history_db).unwrap();
    let parent = ulid::Ulid::generate();
    let child = ulid::Ulid::generate();
    let request = ulid::Ulid::generate();
    store
        .record_session(parent, &ws.display().to_string(), "mock/mock-model", false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    store
        .record_session(child, &ws.display().to_string(), "mock/mock-model", false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    store
        .create_child_link(
            child,
            "writer".into(),
            "write".into(),
            "{}".into(),
            4,
            withnav::bus::event::EventEnvelope::new(
                parent,
                0,
                withnav::bus::event::EventKind::SubagentSpawned,
                json!({}),
            ),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    store
        .append_event_acked(
            parent,
            withnav::bus::event::EventEnvelope::new(
                parent,
                1,
                withnav::bus::event::EventKind::PermissionRequested,
                json!({"request_id":request,"child_session_id":child,"tool_call_id":"pending"}),
            ),
        )
        .await
        .unwrap();
    drop(store);
    let state = AppState::new(config).unwrap();
    state
        .create_or_resume_session(&ws, None, false, Some(parent))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let withnav::bus::ResumeOutcome::Replay(events) = state
        .subscribe(parent, None)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .replay
    else {
        panic!("replay");
    };
    let resolved = events
        .iter()
        .filter(|e| {
            e.kind == "permission.resolved" && e.payload["request_id"] == request.to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["resolution"], "expired");
    state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn recovery_leaves_live_instance_children_and_recovers_stale_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let config = TestServerConfig::unreachable(tmp.path())
        .chat_tools(128_000)
        .config();
    let first = AppState::new(config.clone()).unwrap();
    let parent = ulid::Ulid::generate();
    let child = ulid::Ulid::generate();
    first
        .history()
        .record_session(parent, "/ws", "default", false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    first
        .history()
        .record_session(child, "/ws", "default", false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    first
        .history()
        .create_child_link(
            child,
            "vigie".into(),
            "goal".into(),
            "{}".into(),
            4,
            withnav::bus::event::EventEnvelope::new(
                parent,
                0,
                withnav::bus::event::EventKind::SubagentSpawned,
                json!({}),
            ),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    let mut other_config = config.clone();
    other_config.data_dir = tmp.path().join("another-data-directory");
    #[cfg(unix)]
    let alias = tmp.path().join("database-alias.db");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&config.history_db, &alias).unwrap();
    #[cfg(windows)]
    let alias = config.history_db.clone();
    other_config.history_db = alias;
    let second = AppState::new(other_config).unwrap();
    assert_eq!(
        second
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    assert!(
        second
            .history()
            .pending_completions(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .is_empty()
    );
    drop(first);
    let third = AppState::new(config).unwrap();
    let link = third
        .child_metadata(child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link.status, "stopped");
    assert_eq!(link.terminal_reason.as_deref(), Some("restart"));
    assert_eq!(
        third
            .history()
            .pending_completions(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(third.reserved_child_count(parent), 0);
}
use common::server::TestServerConfig;
use common::sse::{spawn_streaming_provider, text_chunk, tool_call_chunk};

async fn notice_count(state: &AppState, parent: ulid::Ulid, count: usize) {
    tokio::time::timeout(WAIT, async {
        loop {
            if state
                .history()
                .unconsumed_completions(parent)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .len()
                == count
            {
                break;
            }
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
}

fn outputs(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::User { content, .. } => serde_json::from_str::<Value>(content)
                .ok()
                .filter(|v| v["untrusted_subagent_output"] == true),
            _ => None,
        })
        .collect()
}

fn request_outputs(request: &str) -> Vec<Value> {
    let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| {
            serde_json::from_str::<Value>(message["content"].as_str()?)
                .ok()
                .filter(|v| v["untrusted_subagent_output"] == true)
        })
        .collect()
}

#[tokio::test]
async fn finished_child_wakes_idle_parent_and_consumes_once() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let (parent, child) = spawn_linked_child(&state, &ws, "vigie", "inspect").await;
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream.send(text_chunk("a useful answer")).unwrap();
    drop(stream);
    let (request, stream) = tokio::time::timeout(WAIT, provider.requests.recv())
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    let result = request_outputs(&request);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["child_session_id"], child.to_string());
    assert_eq!(result[0]["result"], "a useful answer");
    notice_count(&state, parent, 0)
        .bounded(file!(), line!())
        .await;
    assert_eq!(
        outputs(
            &state
                .history()
                .load(parent)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .unwrap()
                .transcript
        )
        .len(),
        1
    );
    stream.send(text_chunk("processed")).unwrap();
    drop(stream);
    state
        .close_session(parent)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .create_or_resume_session(&ws, None, false, Some(parent))
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), provider.requests.recv())
            .bounded(file!(), line!())
            .await
            .is_err()
    );
    assert_eq!(
        outputs(
            &state
                .history()
                .load(parent)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .unwrap()
                .transcript
        )
        .len(),
        1
    );
    state.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn busy_parent_consumes_several_notices_in_order_at_next_boundary() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .send(common::server::captain_command(
            parent,
            "parent working".into(),
        ))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, parent_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let first = state
        .create_child_session(parent, "vigie", "first")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, first_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let second = state
        .create_child_session(parent, "vigie", "second")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, second_stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    first_stream.send(text_chunk("first answer")).unwrap();
    drop(first_stream);
    notice_count(&state, parent, 1)
        .bounded(file!(), line!())
        .await;
    second_stream
        .send(text_chunk(&"\\\n".repeat(20000)))
        .unwrap();
    drop(second_stream);
    notice_count(&state, parent, 2)
        .bounded(file!(), line!())
        .await;
    for text in ["older navigator wake", "latest navigator wake"] {
        let mut command = common::server::captain_command(parent, text.into());
        command.source_actor_id = "navigator:conversation".into();
        state.send(command).bounded(file!(), line!()).await.unwrap();
    }
    assert!(
        outputs(
            &state
                .history()
                .load(parent)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .unwrap()
                .transcript
        )
        .is_empty()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), provider.requests.recv())
            .bounded(file!(), line!())
            .await
            .is_err()
    );
    parent_stream.send(text_chunk("parent finished")).unwrap();
    drop(parent_stream);
    let (request, stream) = tokio::time::timeout(WAIT, provider.requests.recv())
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    let result = request_outputs(&request);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0]["child_session_id"], first.to_string());
    assert_eq!(result[1]["child_session_id"], second.to_string());
    let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    for m in body["messages"].as_array().unwrap() {
        if m["content"]
            .as_str()
            .is_some_and(|s| s.contains("untrusted_subagent_output"))
        {
            assert!(m["content"].as_str().unwrap().len() <= 16 * 1024);
        }
    }
    notice_count(&state, parent, 0)
        .bounded(file!(), line!())
        .await;
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn unconsumed_notice_survives_child_delete_and_service_recreation() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let config = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .config();
    let state = AppState::new(config.clone()).unwrap();
    let (parent, child) = spawn_linked_child(&state, &ws, "vigie", "inspect").await;
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .close_session(parent)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state.delete(child).bounded(file!(), line!()).await.unwrap();
    drop(stream);
    notice_count(&state, parent, 1)
        .bounded(file!(), line!())
        .await;
    drop(state);
    let state = AppState::new(config).unwrap();
    state
        .create_or_resume_session(&ws, None, false, Some(parent))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (request, stream) = tokio::time::timeout(WAIT, provider.requests.recv())
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    let result = request_outputs(&request);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["child_session_id"], child.to_string());
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn parent_delete_removes_child_tree_and_notices() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let (parent, child) = spawn_linked_child(&state, &ws, "vigie", "inspect").await;
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .delete(parent)
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(!state.session_has_runtime(child));
    assert!(
        !state
            .history()
            .exists(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
    );
    assert!(
        !state
            .history()
            .exists(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()
    );
    assert!(
        state
            .child_metadata(child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        state
            .history()
            .unconsumed_completions(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(state.reserved_child_count(parent), 0);
    drop(stream);
}

#[tokio::test]
async fn recovery_terminalizes_unfinished_child_without_tool_replay() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let config = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .config();
    let history = withnav::session::store::HistoryStore::open(config.history_db.clone()).unwrap();
    let parent = ulid::Ulid::generate();
    let child = ulid::Ulid::generate();
    history
        .record_session(parent, &ws.display().to_string(), "mock/mock-model", false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    history
        .record_session(child, &ws.display().to_string(), "mock/mock-model", false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    history
        .create_child_link(
            child,
            "vigie".into(),
            "inspect".into(),
            "{}".into(),
            4,
            withnav::bus::event::EventEnvelope::new(
                parent,
                0,
                withnav::bus::event::EventKind::SubagentSpawned,
                json!({}),
            ),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    drop(history);
    let state = AppState::new(config).unwrap();
    let link = state
        .child_metadata(child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link.status, "stopped");
    assert_eq!(link.terminal_reason.as_deref(), Some("restart"));
    assert_eq!(state.reserved_child_count(parent), 0);
    state
        .create_or_resume_session(&ws, None, false, Some(parent))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (request, stream) = tokio::time::timeout(WAIT, provider.requests.recv())
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request_outputs(&request)[0]["outcome"], "stopped");
    assert!(!state.session_has_runtime(child));
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn child_permission_is_answered_by_owner_without_parent_interaction() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir_all(ws.join(".withnav/agents")).unwrap();
    std::fs::write(
        ws.join(".withnav/agents/writer.md"),
        "---\nmode: subagent\naccess: full\n---\nWrite.",
    )
    .unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let other = state
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = state
        .create_child_session(parent, "writer", "write")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream
        .send(tool_call_chunk(
            "write-once",
            "bash",
            &json!({"command":"printf x >> approval-count"}),
        ))
        .unwrap();
    drop(stream);
    let request = tokio::time::timeout(WAIT, async {
        loop {
            let replay = state
                .subscribe(parent, None)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .replay;
            if let withnav::bus::ResumeOutcome::Replay(events) = replay
                && let Some(e) = events
                    .into_iter()
                    .find(|e| e.kind == "permission.requested")
            {
                break e;
            }
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    assert_eq!(request.payload["child_session_id"], child.to_string());
    let id = request.payload["request_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(matches!(
        state
            .answer_permission(
                other,
                id,
                withnav::agent::permissions::PermissionDecision::AllowOnce
            )
            .bounded(file!(), line!())
            .await,
        Err(withnav::error::ServerError::ChildReadOnly)
    ));
    assert!(matches!(
        state
            .answer_permission(
                child,
                id,
                withnav::agent::permissions::PermissionDecision::AllowOnce
            )
            .bounded(file!(), line!())
            .await,
        Err(withnav::error::ServerError::ChildReadOnly)
    ));
    state
        .answer_permission(
            parent,
            id,
            withnav::agent::permissions::PermissionDecision::AllowOnce,
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(
        state
            .answer_permission(
                parent,
                id,
                withnav::agent::permissions::PermissionDecision::AllowOnce
            )
            .bounded(file!(), line!())
            .await
            .is_err()
    );
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(ws.join("approval-count")).unwrap(),
        "x"
    );
    stream.send(text_chunk("written")).unwrap();
    drop(stream);
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(ws.join("approval-count")).unwrap(),
        "x"
    );
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn child_permission_and_result_do_not_answer_parent_ask() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir_all(ws.join(".withnav/agents")).unwrap();
    std::fs::write(
        ws.join(".withnav/agents/writer.md"),
        "---\nmode: subagent\naccess: full\n---\nWrite.",
    )
    .unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .state();
    let parent = state
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    state
        .send(common::server::captain_command(parent, "ask me".into()))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream
        .send(tool_call_chunk(
            "parent-ask",
            "ask",
            &json!({"question":"Choose?"}),
        ))
        .unwrap();
    drop(stream);
    tokio::time::timeout(WAIT, async {
        loop {
            if let withnav::bus::ResumeOutcome::Replay(events) = state
                .subscribe(parent, None)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .replay
                && events.iter().any(|e| e.kind == "agent.yield")
            {
                break;
            }
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    let child = state
        .create_child_session(parent, "writer", "write")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream
        .send(tool_call_chunk(
            "child-bash",
            "bash",
            &json!({"command":"true"}),
        ))
        .unwrap();
    drop(stream);
    let request_id = tokio::time::timeout(WAIT, async {
        loop {
            if let withnav::bus::ResumeOutcome::Replay(events) = state
                .subscribe(parent, None)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .replay
                && let Some(e) = events.iter().find(|e| {
                    e.kind == "permission.requested"
                        && e.payload["child_session_id"] == child.to_string()
                })
            {
                break e.payload["request_id"].as_str().unwrap().parse().unwrap();
            }
            tokio::task::yield_now().bounded(file!(), line!()).await;
        }
    })
    .bounded(file!(), line!())
    .await
    .unwrap();
    state
        .answer_permission(
            parent,
            request_id,
            withnav::agent::permissions::PermissionDecision::AllowOnce,
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    stream.send(text_chunk("child finished")).unwrap();
    drop(stream);
    notice_count(&state, parent, 1)
        .bounded(file!(), line!())
        .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), provider.requests.recv())
            .bounded(file!(), line!())
            .await
            .is_err()
    );
    let transcript = state
        .history()
        .load(parent)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap()
        .transcript;
    assert!(!transcript.iter().any(
        |m| matches!(m, ChatMessage::Tool { tool_call_id, .. } if tool_call_id == "parent-ask")
    ));
    state
        .send(common::server::captain_command(parent, "my answer".into()))
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (request, stream) =
        match tokio::time::timeout(common::deadline::WAIT, provider.requests.recv())
            .bounded(file!(), line!())
            .await
        {
            Ok(request) => request.unwrap(),
            Err(_) => panic!(
                "parent answer never reached provider; events={:?}; transcript={:?}; notices={:?}",
                state
                    .subscribe(parent, None)
                    .bounded(file!(), line!())
                    .await,
                state.history().load(parent).bounded(file!(), line!()).await,
                state
                    .history()
                    .unconsumed_completions(parent)
                    .bounded(file!(), line!())
                    .await
            ),
        };
    assert_eq!(request_outputs(&request).len(), 1);
    assert!(request.contains("my answer"));
    state.close_all().bounded(file!(), line!()).await;
    drop(stream);
}

#[tokio::test]
async fn published_notice_is_consumed_after_crash_and_never_consumed_twice() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("history.db");
    let parent = ulid::Ulid::generate();
    let child = ulid::Ulid::generate();
    let store = withnav::session::store::HistoryStore::open(&path).unwrap();
    store
        .record_session(parent, "/ws", "default", false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    store
        .record_session(child, "/ws", "default", false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    store
        .create_child_link(
            child,
            "vigie".into(),
            "goal".into(),
            "{}".into(),
            4,
            withnav::bus::event::EventEnvelope::new(
                parent,
                0,
                withnav::bus::event::EventKind::SubagentSpawned,
                json!({}),
            ),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    store
        .finish_child_link(child, "done".into(), None, "answer".into())
        .bounded(file!(), line!())
        .await
        .unwrap();
    let payload = store
        .pending_completions(parent)
        .bounded(file!(), line!())
        .await
        .unwrap()[0]
        .1
        .clone();
    store
        .deliver_completion(
            child,
            withnav::bus::event::EventEnvelope::new(
                parent,
                1,
                withnav::bus::event::EventKind::SubagentDone,
                payload,
            ),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();
    drop(store);
    let store = withnav::session::store::HistoryStore::open(&path).unwrap();
    assert!(
        store
            .pending_completions(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .unconsumed_completions(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()[0]
            .2,
        "answer"
    );
    let transcript = vec![ChatMessage::User {
        provenance: Default::default(),
        content: "untrusted answer".into(),
        images: Vec::new(),
    }];
    assert!(
        store
            .consume_completions(
                parent,
                vec![child, ulid::Ulid::generate()],
                transcript.clone()
            )
            .bounded(file!(), line!())
            .await
            .is_err()
    );
    assert_eq!(
        store
            .unconsumed_completions(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .load(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .transcript
            .is_empty()
    );
    store
        .consume_completions(parent, vec![child], transcript.clone())
        .bounded(file!(), line!())
        .await
        .unwrap();
    drop(store);
    let store = withnav::session::store::HistoryStore::open(&path).unwrap();
    assert!(
        store
            .unconsumed_completions(parent)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .consume_completions(parent, vec![child], vec![])
            .bounded(file!(), line!())
            .await
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(
            store
                .load(parent)
                .bounded(file!(), line!())
                .await
                .unwrap()
                .unwrap()
                .transcript
        )
        .unwrap(),
        serde_json::to_value(transcript).unwrap()
    );
}
