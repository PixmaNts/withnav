use std::time::Duration;

use serde_json::{Value, json};
use withnav::server::AppState;

mod common;
use common::deadline::Deadline;
use common::server::TestServerConfig;
use common::sse::{spawn_streaming_provider, text_chunk, tool_call_chunk};

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

/// The next provider request whose body satisfies `wanted`. Requests that arrive
/// first and do not match are kept in `pending` for a later call.
async fn request_matching(
    requests: &mut tokio::sync::mpsc::Receiver<(
        String,
        tokio::sync::mpsc::UnboundedSender<String>,
    )>,
    pending: &mut Vec<(String, tokio::sync::mpsc::UnboundedSender<String>)>,
    wanted: impl Fn(&str) -> bool,
) -> (String, tokio::sync::mpsc::UnboundedSender<String>) {
    if let Some(index) = pending.iter().position(|(body, _)| wanted(body)) {
        return pending.remove(index);
    }
    loop {
        let (body, stream) = requests
            .recv()
            .bounded(file!(), line!())
            .await
            .expect("provider closed");
        if wanted(&body) {
            return (body, stream);
        }
        pending.push((body, stream));
    }
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

/// Drives `child` to the step budget boundary by sending `steps` tool calls
/// and awaiting the resulting `agent.yield` with `kind: "budget"`.
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
    tokio::time::timeout(Duration::from_secs(5), async {
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

/// Given: A parent session with a linked child subagent running on a step budget.
/// When: The child exhausts its allocated step budget with tool invocations.
/// Then: The budget park notice is durably enqueued into the parent's event stream
/// with verdict="budget", delivery="next_turn", topups_used=0, topups_remaining=3,
/// and the parent runtime actively wakes to process the notice in its next prompt.
#[tokio::test]
async fn budget_park_enqueues_durable_notice_and_wakes_parent_runtime() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
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
        "park",
        steps,
    )
    .await;

    // Observe child metadata remains running while parked.
    let child_meta = state
        .child_metadata(child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child_meta.status, "running");

    // The budget park wakes the parent: the delivery loop turns unconsumed
    // completions into a prompt turn carrying the counts and evidence.
    let (notice, parent_turn) = request_matching(&mut provider.requests, &mut pending, |body| {
        body.contains("used its 50 steps")
    })
    .bounded(file!(), line!())
    .await;
    assert!(notice.contains("top-ups used 0/3"), "body was: {notice}");
    assert!(notice.contains("inspect"), "body was: {notice}");
    assert!(
        notice.contains("continue, stop, or start a new subagent?"),
        "body was: {notice}"
    );

    // Assert durable enqueue on the parent event stream.
    let replay = match state
        .subscribe(parent, None)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .replay
    {
        withnav::bus::ResumeOutcome::Replay(events) => events,
        other => panic!("expected parent event replay, got {other:?}"),
    };
    let budget_event = replay
        .iter()
        .find(|e| e.kind == "subagent.watchdog" && e.payload["verdict"] == "budget")
        .expect("parent stream must record the durable budget watchdog event");
    assert_eq!(budget_event.payload["notice_kind"], "budget");
    assert_eq!(budget_event.payload["child_session_id"], child.to_string());
    assert_eq!(budget_event.payload["verdict"], "budget");
    assert_eq!(budget_event.payload["delivery"], "next_turn");
    assert_eq!(budget_event.payload["topups_used"], 0);
    assert_eq!(budget_event.payload["topups_remaining"], 3);
    assert_eq!(budget_event.payload["agent"], "vigie");
    assert_eq!(budget_event.payload["evidence"]["step_count"], steps as u64);
    assert_eq!(budget_event.payload["evidence"]["goal"], "inspect");

    parent_turn
        .send(text_chunk("parent acknowledged the budget park"))
        .unwrap();
    drop(parent_turn);

    state.close_all().bounded(file!(), line!()).await;
}

/// Given: Child subagents parked at their step budget with initial budget notices delivered.
/// When: A parent either renews the budget (unparking the child and incrementing topups_used)
/// or terminalizes the child with an owner-stop.
/// Then: In both cases, the parent stream records exactly one budget notice per child; once
/// the child leaves the parked state or is stopped, stale park conditions do not emit duplicate notices.
#[tokio::test]
async fn stale_park_suppression_prevents_duplicate_notices_after_topup_and_stop() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128_000)
        .allow_everything()
        .state();
    let steps = withnav::agent::run_loop::max_steps();
    let mut pending = Vec::new();

    // Case 1: Parent continues a parked child via topup; child completes successfully.
    let (parent, child_1) = spawn_linked_child(&state, &ws, "vigie", "inspect").await;
    let (_, first_1) = request_matching(
        &mut provider.requests,
        &mut pending,
        is_child_turn("inspect"),
    )
    .await;

    exhaust_child_budget(
        &mut provider,
        &mut pending,
        &state,
        child_1,
        first_1,
        "stale-park",
        steps,
    )
    .await;

    let (notice_1, parent_turn_1) =
        request_matching(&mut provider.requests, &mut pending, |body| {
            body.contains("used its 50 steps")
        })
        .bounded(file!(), line!())
        .await;
    assert!(
        notice_1.contains("top-ups used 0/3"),
        "body was: {notice_1}"
    );
    parent_turn_1.send(text_chunk("noted first park")).unwrap();
    drop(parent_turn_1);

    // Parent tops up child_1. This releases the park (is_budget_parked becomes false)
    // and increments budget_topups_used to 1.
    state
        .service()
        .decide_subagent(
            parent,
            child_1,
            false,
            Some("Continue inspection to finish"),
        )
        .bounded(file!(), line!())
        .await
        .unwrap();

    // While unparked and running, decision continuation is rejected because the child is not parked.
    assert!(matches!(
        state
            .service()
            .decide_subagent(parent, child_1, false, None)
            .bounded(file!(), line!())
            .await,
        Err(withnav::error::ServerError::MalformedBody(_))
    ));

    // The child runs its renewed turn and completes without exhausting budget.
    let (_, child_stream_1) = request_matching(
        &mut provider.requests,
        &mut pending,
        is_child_turn("inspect"),
    )
    .await;
    child_stream_1
        .send(text_chunk("child completed inspection"))
        .unwrap();
    drop(child_stream_1);

    // The parent receives the natural completion wake (outcome="done").
    let (_, parent_complete_turn) = tokio::time::timeout(
        Duration::from_secs(5),
        request_matching(&mut provider.requests, &mut pending, is_completion_wake),
    )
    .bounded(file!(), line!())
    .await
    .unwrap();
    parent_complete_turn
        .send(text_chunk("acknowledged completion"))
        .unwrap();
    drop(parent_complete_turn);

    assert_eq!(
        state
            .child_metadata(child_1)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "done"
    );

    // Case 2: Parent stops a parked child; the child terminalizes as stopped.
    let child_2 = state
        .create_child_session(parent, "vigie", "inspect second")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, first_2) = request_matching(&mut provider.requests, &mut pending, |body| {
        let v: Value =
            serde_json::from_str(body.split_once("\r\n\r\n").unwrap().1).unwrap_or(Value::Null);
        v["messages"].as_array().is_some_and(|msgs| {
            msgs.iter()
                .any(|m| m["content"].as_str() == Some("inspect second"))
        })
    })
    .await;

    // Exhaust child_2's budget.
    {
        let mut stream = Some(first_2);
        for index in 0..steps {
            let tx = stream.take().unwrap();
            let mut chunk: Value = serde_json::from_str(&tool_call_chunk(
                &format!("stop-park-{index}"),
                "glob",
                &json!({"pattern":"*"}),
            ))
            .unwrap();
            chunk["choices"][0]["delta"]["content"] = json!("Observable second progress.");
            tx.send(chunk.to_string()).unwrap();
            drop(tx);
            if index + 1 < steps {
                stream = Some(
                    request_matching(&mut provider.requests, &mut pending, |body| {
                        let v: Value = serde_json::from_str(body.split_once("\r\n\r\n").unwrap().1)
                            .unwrap_or(Value::Null);
                        v["messages"].as_array().is_some_and(|msgs| {
                            msgs.iter()
                                .any(|m| m["content"].as_str() == Some("inspect second"))
                        })
                    })
                    .await
                    .1,
                );
            }
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let withnav::bus::ResumeOutcome::Replay(events) = state
                    .subscribe(child_2, None)
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

    let (notice_2, parent_turn_2) =
        request_matching(&mut provider.requests, &mut pending, |body| {
            body.contains("inspect second") && body.contains("used its 50 steps")
        })
        .bounded(file!(), line!())
        .await;
    assert!(
        notice_2.contains("top-ups used 0/3"),
        "body was: {notice_2}"
    );
    parent_turn_2.send(text_chunk("noted second park")).unwrap();
    drop(parent_turn_2);

    // Owner stops the budget-parked child.
    state
        .service()
        .decide_subagent(parent, child_2, true, None)
        .bounded(file!(), line!())
        .await
        .unwrap();

    let meta_2 = state
        .child_metadata(child_2)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(meta_2.status, "stopped");

    // Parent wakes on the owner-stop notice.
    let (wake_stop, parent_stop_turn) = tokio::time::timeout(
        Duration::from_secs(5),
        request_matching(&mut provider.requests, &mut pending, |body| {
            body.contains("stopped by owner at its step budget")
        }),
    )
    .bounded(file!(), line!())
    .await
    .unwrap();
    assert!(wake_stop.contains("Observable second progress."));
    parent_stop_turn
        .send(text_chunk("acknowledged stop"))
        .unwrap();
    drop(parent_stop_turn);

    // Verify parent event stream has exactly one budget notice per child; stale park states
    // were not re-emitted once the child moved on or was stopped.
    let replay = match state
        .subscribe(parent, None)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .replay
    {
        withnav::bus::ResumeOutcome::Replay(events) => events,
        other => panic!("expected parent event replay, got {other:?}"),
    };
    let budget_events_1: Vec<_> = replay
        .iter()
        .filter(|e| {
            e.kind == "subagent.watchdog"
                && e.payload["verdict"] == "budget"
                && e.payload["child_session_id"] == child_1.to_string()
        })
        .collect();
    assert_eq!(
        budget_events_1.len(),
        1,
        "parent stream must retain exactly one budget notice for child 1; stale park states do not produce duplicate notices"
    );
    assert_eq!(budget_events_1[0].payload["topups_used"], 0);

    let budget_events_2: Vec<_> = replay
        .iter()
        .filter(|e| {
            e.kind == "subagent.watchdog"
                && e.payload["verdict"] == "budget"
                && e.payload["child_session_id"] == child_2.to_string()
        })
        .collect();
    assert_eq!(
        budget_events_2.len(),
        1,
        "parent stream must retain exactly one budget notice for child 2 after owner stop"
    );
    assert_eq!(budget_events_2[0].payload["topups_used"], 0);

    state.close_all().bounded(file!(), line!()).await;
}
