//! What a live session reports as doing in summaries and message acks:
//! `running` while turn work is in flight, `idle` once it ends, `waiting`
//! while the agent owes the Captain an answer, and `running` for a message
//! accepted mid-turn.

use std::sync::Arc;

use serde_json::{Value, json};

use withnav::server::AppState;

mod common;
use common::server::{
    TestServerConfig, collect_until, post_message_body, send_text, spawn_server,
    temp_workspace as tmp_ws, text_message,
};
use common::sse::{
    http_sse, spawn_capturing_provider, spawn_gated_provider, sse_body, text_response as answer,
};

async fn summary_actor_state(state: &AppState, id: ulid::Ulid) -> String {
    state.session_summary(id).await.unwrap().actor_state
}

// Given a turn whose model request is held back by the mock provider
// When the summary is read mid-turn and again after the turn ends
// Then it reports running during execution and idle once the turn is over
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_turn_is_reported_running_and_an_ended_one_idle() {
    let (tmp, ws) = tmp_ws();
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = spawn_gated_provider(vec![answer("the turn answer")], release.clone()).await;
    let state = TestServerConfig::mock(tmp.path(), &provider.url).state();
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
    let sub = state.subscribe(id, None).await.expect("sub");

    send_text(&state, id, "go").await;
    // The gated provider records the request before holding it, so one
    // captured body means the turn is in flight.
    common::server::wait_for("the turn's request", || provider.bodies().len() == 1).await;
    assert_eq!(
        summary_actor_state(&state, id).await,
        "running",
        "the summary must report a turn in flight"
    );

    release.notify_one();
    collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;
    assert_eq!(summary_actor_state(&state, id).await, "idle");
}

// Given a turn whose model request is held back by the mock provider
// When another message is posted while the first turn runs
// Then the ack reports running, and the summary is idle again only once
//      every queued turn has ended
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_accepted_mid_turn_is_acked_running() {
    let (tmp, ws) = tmp_ws();
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = spawn_gated_provider(
        vec![answer("the first answer"), answer("the second answer")],
        release.clone(),
    )
    .await;
    let state = TestServerConfig::mock(tmp.path(), &provider.url).state();
    let (addr, _task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
    let sub = state.subscribe(id, None).await.expect("sub");

    send_text(&state, id, "go").await;
    common::server::wait_for("the turn's request", || provider.bodies().len() == 1).await;

    let ack: Value = post_message_body(&base, &id.to_string(), &text_message("second"))
        .await
        .json()
        .await
        .expect("ack json");
    assert_eq!(
        ack["actor_state"],
        json!("running"),
        "the ack of a message accepted while a turn runs"
    );

    release.notify_one();
    // The queued message runs its own turn after the held one ends; the
    // first `turn.end` alone does not establish idleness, so wait for the
    // second one on the same subscription.
    let ends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let remaining = ends.clone();
    collect_until(sub, move |e| {
        if e.kind == "turn.end" {
            ends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        remaining.load(std::sync::atomic::Ordering::SeqCst) >= 2
    })
    .await;
    assert_eq!(
        provider.bodies().len(),
        2,
        "the queued message ran its turn"
    );
    assert_eq!(
        summary_actor_state(&state, id).await,
        "idle",
        "after every queued turn"
    );
}

// Given a turn that ends on an `ask` tool call
// When the summary is read while the answer is owed, and after the answer
//      has driven the next turn
// Then it reports waiting while the answer is owed and idle once the
//      answered turn is over
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_ask_is_reported_waiting_until_it_is_answered() {
    let ask = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"ask_1","function":{"name":"ask","arguments":"{\"question\":\"which scope?\"}"}}]},"finish_reason":"tool_calls"}]}"#;
    let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
    let provider = spawn_capturing_provider(vec![
        http_sse(&sse_body(&[ask])),
        http_sse(&sse_body(&[done])),
    ])
    .await;
    let (tmp, ws) = tmp_ws();
    let state = TestServerConfig::mock(tmp.path(), &provider.url).state();
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
    let sub = state.subscribe(id, None).await.expect("sub");

    send_text(&state, id, "go").await;
    collect_until(sub, |e| {
        e.kind == "agent.state" && e.payload["to"] == "yielded"
    })
    .await;
    assert_eq!(
        summary_actor_state(&state, id).await,
        "waiting",
        "the answer is owed to the Captain"
    );

    let sub = state.subscribe(id, None).await.expect("sub");
    send_text(&state, id, "local").await;
    collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;
    assert_eq!(summary_actor_state(&state, id).await, "idle");
}
