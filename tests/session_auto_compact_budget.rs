//! The step budget survives automatic compaction. Its own test binary: the
//! step limit is read from the environment when a session starts, and the
//! environment is shared by every test of a process.

use serde_json::json;

mod common;
use common::server::{collect_until, send_text, TestServerConfig};
use common::sse::{http_sse, spawn_responding_provider, sse_body, text_chunk, tool_call_chunk};

const SUMMARY: &str = "Goal: keep reading.\nNext: read again.";

// Given a model that calls a tool at every step, a window so small that the
//       prompt crosses the budget again and again, and an explicit step limit
// Then the turn compacts several times and still takes exactly `max_steps`
//      model steps in ONE turn: no step is given back by a compaction, and the
//      turn then yields on the step budget
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_automatic_compactions_do_not_renew_the_step_budget() {
    std::env::set_var("NAVAI_MAX_STEPS", "6");
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("big.txt"), "lorem ipsum ".repeat(2_000)).unwrap();
    let read = http_sse(&sse_body(&[&tool_call_chunk(
        "r",
        "read",
        &json!({"path": ws.join("big.txt")}),
    )]));
    let summary = http_sse(&sse_body(&[&text_chunk(SUMMARY)]));
    let provider = spawn_responding_provider(40, move |_, request| {
        if request.contains("\"tools\"") {
            read.clone()
        } else {
            summary.clone()
        }
    })
    .await;
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(20_000)
        .state();
    let id = state.create_session(&ws, None, None).await.unwrap();
    let sub = state.subscribe(id, None).await.unwrap();

    send_text(&state, id, "read big.txt until told otherwise").await;
    let events = collect_until(sub, |e| e.kind == "turn.end").await;

    let agent_requests = provider
        .bodies()
        .iter()
        .filter(|b| b.contains("\"tools\""))
        .count();
    let summaries = provider.bodies().len() - agent_requests;
    assert_eq!(agent_requests, 6, "exactly max_steps agent requests");
    assert!(summaries >= 2, "compacted several times: {summaries}");
    let count = |kind: &str| events.iter().filter(|e| e.kind == kind).count();
    assert_eq!(count("turn.start"), 1);
    assert_eq!(count("turn.end"), 1);
    assert_eq!(count("llm.response"), 6);
    assert_eq!(count("context.compacted"), summaries);
    // The turn ends on the step budget: a `budget` yield, not a done or error.
    let yielded = events.iter().find(|e| e.kind == "agent.yield").unwrap();
    assert_eq!(yielded.payload["kind"], "budget", "{yielded:?}");
    let end = events.iter().find(|e| e.kind == "turn.end").unwrap();
    assert_eq!(end.payload["outcome"], "yielded", "{end:?}");
}
