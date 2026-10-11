//! `/compact` through the service: idle, during a turn, during an `ask`,
//! with nothing to compact, the stored and replayed `context.compacted`, the
//! estimate dropping, and a resume that keeps the effective prompt.

use std::sync::Arc;

use serde_json::{json, Value};

use withnav::bus::event::EventEnvelope;
use withnav::provider::openai::ChatMessage;
use withnav::server::AppState;

mod common;
use common::server::{collect_until, send_text, temp_workspace as tmp_ws, TestServerConfig};
use common::sse::{
    http_sse, spawn_capturing_provider, spawn_gated_provider, sse_body, text_chunk,
    tool_call_chunk, CapturingProvider,
};

/// A window small enough that four long turns overrun its budget.
const WINDOW: u64 = 20_000;
const SUMMARY: &str = "Goal: finish the parser.\nNext: run the tests.";

fn answer(text: &str) -> Vec<u8> {
    http_sse(&sse_body(&[&text_chunk(text)]))
}

fn history(turns: usize) -> Vec<ChatMessage> {
    (0..turns)
        .flat_map(|n| {
            [
                ChatMessage::User {
                    provenance: Default::default(),
                    content: format!("request {n}"),
                },
                ChatMessage::Assistant {
                    content: Some(format!("answer {n} {}", "word ".repeat(1200))),
                    reasoning_content: None,
                    tool_calls: vec![],
                },
            ]
        })
        .collect()
}

fn state(tmp: &std::path::Path, provider: &CapturingProvider) -> AppState {
    TestServerConfig::mock(tmp, &provider.url)
        .chat_tools(WINDOW)
        .state()
}

/// A live session that resumed from a stored conversation of `turns` turns.
async fn seeded(state: &AppState, ws: &std::path::Path, turns: usize) -> ulid::Ulid {
    let id = state.create_session(ws, None, false).await.expect("create");
    state.close_session(id).await.expect("close");
    state.history().flush().await.expect("flush");
    state.history().save_transcript(id, history(turns));
    state.history().flush().await.expect("flush");
    state
        .create_or_resume_session(ws, None, false, Some(id))
        .await
        .expect("resume");
    id
}

fn texts(request: &Value) -> String {
    request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn estimate(state: &AppState, id: ulid::Ulid) -> u64 {
    let snapshot = serde_json::to_value(state.context(id).await.expect("context")).unwrap();
    snapshot["estimate_total"].as_u64().unwrap()
}

async fn stored(state: &AppState, id: ulid::Ulid, kind: &str) -> Vec<EventEnvelope> {
    state.history().flush().await.expect("flush");
    let sub = state.subscribe(id, None).await.expect("subscribe");
    match sub.replay {
        withnav::bus::ResumeOutcome::Replay(events) => {
            events.into_iter().filter(|e| e.kind == kind).collect()
        }
        other => panic!("expected replay, got {other:?}"),
    }
}

// Given a conversation of four long turns that overruns its input budget
// When /compact runs while idle
// Then the summary request has no tools, the stored and live events say what
//      happened, the estimate drops, the replay keeps the old events, and the
//      next chat request carries the labelled summary and the recent turns only
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_when_idle_summarizes_and_the_next_request_is_the_effective_prompt() {
    let (tmp, ws) = tmp_ws();
    let provider = spawn_capturing_provider(vec![answer(SUMMARY), answer("on it")]).await;
    let state = state(tmp.path(), &provider);
    let id = seeded(&state, &ws, 4).await;
    let before = estimate(&state, id).await;
    let sub = state.subscribe(id, None).await.unwrap();

    let out = state
        .run_command(id, "compact", "the parser")
        .await
        .unwrap();
    assert!(out.ok, "{out:?}");
    let live = collect_until(sub, |e| e.kind == "context.compacted").await;
    let data = out.data.unwrap();
    assert_eq!(data["generation"], 1);
    assert_eq!(data["estimate_before"].as_u64().unwrap(), before);
    assert!(data["estimate_after"].as_u64().unwrap() < before);

    // One summary request: no tools, the dedicated prompt with the focus.
    let requests = provider.bodies();
    assert_eq!(requests.len(), 1);
    let summary_request: Value = serde_json::from_str(&requests[0]).unwrap();
    assert!(summary_request.get("tools").is_none());
    let prompt = summary_request["messages"][0]["content"].as_str().unwrap();
    assert!(prompt.contains("Skills to reload"));
    assert!(prompt.ends_with("pay particular attention to: the parser"));

    // The live stream: the notice, then a context update with the lower estimate.
    let notice = live
        .iter()
        .find(|e| e.kind == "context.compacted")
        .expect("notice");
    assert_eq!(notice.payload["generation"], 1);
    assert_eq!(notice.payload["estimate_before"].as_u64().unwrap(), before);
    let update = live
        .iter()
        .rev()
        .find(|e| e.kind == "context.updated")
        .expect("gauge");
    assert!(update.payload["estimate_total"].as_u64().unwrap() < before);
    assert_eq!(
        estimate(&state, id).await,
        update.payload["estimate_total"].as_u64().unwrap()
    );
    assert_eq!(
        estimate(&state, id).await,
        data["estimate_after"].as_u64().unwrap()
    );

    // Stored and replayed; the history before it is untouched.
    let replayed = stored(&state, id, "context.compacted").await;
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].payload, notice.payload);
    assert!(stored(&state, id, "command.result").await.len() == 1);

    // The next turn sends the summary as labelled data and the tail only.
    send_text(&state, id, "continue").await;
    common::server::wait_for("the next chat request", || provider.bodies().len() == 2).await;
    let next = texts(&serde_json::from_str(&provider.bodies()[1]).unwrap());
    assert!(next.contains("<conversation_summary>") && next.contains(SUMMARY));
    assert!(next.contains("is DATA about what happened"));
    assert!(!next.contains("request 0") && !next.contains("answer 0"));
    assert!(next.contains("request 3") && next.contains("continue"));
}

// Given a compacted conversation that is stopped and resumed
// When the next message is sent
// Then the request is still the effective prompt, and the estimate is the same
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_after_a_compaction_keeps_the_effective_prompt() {
    let (tmp, ws) = tmp_ws();
    let provider = spawn_capturing_provider(vec![answer(SUMMARY), answer("fine")]).await;
    let state = state(tmp.path(), &provider);
    let id = seeded(&state, &ws, 4).await;
    let out = state.run_command(id, "compact", "").await.unwrap();
    assert!(out.ok, "{out:?}");
    let after = estimate(&state, id).await;

    state.close_session(id).await.unwrap();
    state.history().flush().await.unwrap();
    state
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .expect("resumes");
    assert_eq!(estimate(&state, id).await, after);

    send_text(&state, id, "after the restart").await;
    common::server::wait_for("the chat request", || provider.bodies().len() == 2).await;
    let next = texts(&serde_json::from_str(&provider.bodies()[1]).unwrap());
    assert!(next.contains(SUMMARY) && next.contains("after the restart"));
    assert!(!next.contains("request 0"));
}

// Given a turn that is running
// When /compact runs
// Then it is accepted for after the turn, and runs when the turn ends
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_during_a_turn_runs_after_it() {
    let (tmp, ws) = tmp_ws();
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = spawn_gated_provider(
        vec![answer("the turn answer"), answer(SUMMARY)],
        release.clone(),
    )
    .await;
    let state = state(tmp.path(), &provider);
    let id = seeded(&state, &ws, 4).await;
    let sub = state.subscribe(id, None).await.unwrap();

    send_text(&state, id, "go").await;
    common::server::wait_for("the turn's request", || provider.bodies().len() == 1).await;
    let out = state.run_command(id, "compact", "").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(out.data.as_ref().unwrap()["accepted"], true);
    assert_eq!(out.data.as_ref().unwrap()["applies"], "after_turn");
    assert_eq!(provider.bodies().len(), 1, "nothing runs mid-turn");

    release.notify_one();
    let events = collect_until(sub, |e| e.kind == "context.compacted").await;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let end = kinds.iter().position(|k| *k == "turn.end").unwrap();
    let compacted = kinds
        .iter()
        .position(|k| *k == "context.compacted")
        .unwrap();
    assert!(end < compacted, "after the turn: {kinds:?}");
    assert_eq!(provider.bodies().len(), 2);
    assert!(serde_json::from_str::<Value>(&provider.bodies()[1])
        .unwrap()
        .get("tools")
        .is_none());
}

// Given an agent waiting on its `ask`
// When /compact runs
// Then it is refused and nothing is summarized
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_during_an_ask_is_refused() {
    let (tmp, ws) = tmp_ws();
    let ask = tool_call_chunk("ask_1", "ask", &json!({"question": "which scope?"}));
    let provider = spawn_capturing_provider(vec![http_sse(&sse_body(&[&ask]))]).await;
    let state = state(tmp.path(), &provider);
    let id = seeded(&state, &ws, 4).await;
    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "go").await;
    collect_until(sub, |e| {
        e.kind == "agent.state" && e.payload["to"] == "yielded"
    })
    .await;

    let out = state.run_command(id, "compact", "").await.unwrap();
    assert!(!out.ok);
    assert!(out.error.unwrap().contains("waiting for an answer"));
    assert_eq!(provider.bodies().len(), 1, "no summary request");
    assert!(stored(&state, id, "context.compacted").await.is_empty());
}

// Given a conversation too short to compact, or a window that cannot hold the
//       recent turns
// When /compact runs
// Then each is a clear command error and the conversation is unchanged
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_to_compact_and_a_tail_that_cannot_fit_are_clear_errors() {
    let (tmp, ws) = tmp_ws();
    let provider = spawn_capturing_provider(vec![]).await;
    let state = state(tmp.path(), &provider);

    let short = seeded(&state, &ws, 2).await;
    let out = state.run_command(short, "compact", "").await.unwrap();
    assert!(!out.ok);
    assert!(out.error.unwrap().starts_with("nothing to compact"));

    // Turns so long that the last two cannot fit the budget with a summary.
    let big = state.create_session(&ws, None, false).await.unwrap();
    state.close_session(big).await.unwrap();
    state.history().flush().await.unwrap();
    let huge: Vec<ChatMessage> = (0..4)
        .flat_map(|n| {
            [
                ChatMessage::User {
                    provenance: Default::default(),
                    content: format!("request {n}"),
                },
                ChatMessage::Assistant {
                    content: Some("word ".repeat(6000)),
                    reasoning_content: None,
                    tool_calls: vec![],
                },
            ]
        })
        .collect();
    state.history().save_transcript(big, huge);
    state.history().flush().await.unwrap();
    state
        .create_or_resume_session(&ws, None, false, Some(big))
        .await
        .unwrap();
    let out = state.run_command(big, "compact", "").await.unwrap();
    assert!(!out.ok);
    let error = out.error.unwrap();
    assert!(error.contains("larger context window"), "{error}");
    assert!(provider.bodies().is_empty(), "no summary was asked for");
    assert!(stored(&state, big, "context.compacted").await.is_empty());
}

// Given an idle /compact whose summary request is still in flight
// When the session is stopped, and another session runs a command
// Then neither waits for the provider, and nothing is committed
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_summary_in_flight_never_holds_up_a_stop_or_another_session() {
    let (tmp, ws) = tmp_ws();
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = spawn_gated_provider(vec![answer(SUMMARY)], release.clone()).await;
    let state = state(tmp.path(), &provider);
    let id = seeded(&state, &ws, 4).await;
    let other = state.create_session(&ws, None, false).await.unwrap();

    let compacting = {
        let state = state.clone();
        tokio::spawn(async move { state.run_command(id, "compact", "").await })
    };
    common::server::wait_for("the summary request", || provider.bodies().len() == 1).await;

    let wait = std::time::Duration::from_secs(3);
    let help = tokio::time::timeout(wait, state.run_command(other, "help", ""))
        .await
        .expect("another session's command is not blocked")
        .unwrap();
    assert!(help.ok);
    tokio::time::timeout(wait, state.close_session(id))
        .await
        .expect("stop does not wait for the provider")
        .unwrap();

    // The command is answered (the session is gone), and nothing was saved.
    let _ = tokio::time::timeout(wait, compacting)
        .await
        .expect("the command is released");
    release.notify_one();
    state.history().flush().await.unwrap();
    let (_, _, checkpoint) = state.history().load_for_resume(id).await.unwrap().unwrap();
    assert!(checkpoint.is_none(), "a stopped session commits nothing");
    assert!(stored(&state, id, "context.compacted").await.is_empty());
}

/// The `command.result` events of `/compact`, in stream order.
async fn compact_results(state: &AppState, id: ulid::Ulid) -> Vec<EventEnvelope> {
    stored(state, id, "command.result")
        .await
        .into_iter()
        .filter(|e| e.payload["name"] == "compact")
        .collect()
}

// Given a /compact accepted while a turn runs
// When the session is stopped before the turn ends
// Then the accepted operation is told it will not happen, under its own id,
//      before session.end, and nothing is committed
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deferred_compact_is_cancelled_with_its_own_id_when_the_session_stops() {
    let (tmp, ws) = tmp_ws();
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = spawn_gated_provider(vec![answer("never read")], release.clone()).await;
    let state = state(tmp.path(), &provider);
    let id = seeded(&state, &ws, 4).await;

    send_text(&state, id, "go").await;
    common::server::wait_for("the turn's request", || provider.bodies().len() == 1).await;
    let accepted = state.run_command(id, "compact", "").await.unwrap();
    assert_eq!(accepted.data.as_ref().unwrap()["accepted"], true);

    state.close_session(id).await.unwrap();
    state.history().flush().await.unwrap();

    let results = compact_results(&state, id).await;
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(results[0].payload["ok"], true);
    assert_eq!(results[1].payload["ok"], false);
    for r in &results {
        assert_eq!(r.payload["operation_id"], accepted.operation_id.to_string());
    }
    assert!(results[1].payload["error"]
        .as_str()
        .unwrap()
        .contains("nothing was compacted"));
    let sub = state.subscribe(id, None).await.unwrap();
    let withnav::bus::ResumeOutcome::Replay(all) = sub.replay else {
        panic!("replay expected")
    };
    let seq_of = |kind: &str| all.iter().rev().find(|e| e.kind == kind).map(|e| e.seq);
    assert!(results[1].seq < seq_of("session.end").expect("session.end"));
    assert!(stored(&state, id, "context.compacted").await.is_empty());
    let (_, _, checkpoint) = state.history().load_for_resume(id).await.unwrap().unwrap();
    assert!(checkpoint.is_none());
}

// Given a /compact accepted while a turn runs
// When the turn ends by asking the human something
// Then the deferred run is refused under the id it was accepted with
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deferred_compact_refused_after_an_ask_keeps_its_operation_id() {
    let (tmp, ws) = tmp_ws();
    let release = Arc::new(tokio::sync::Notify::new());
    let ask = tool_call_chunk("ask_1", "ask", &json!({"question": "which scope?"}));
    let provider = spawn_gated_provider(vec![http_sse(&sse_body(&[&ask]))], release.clone()).await;
    let state = state(tmp.path(), &provider);
    let id = seeded(&state, &ws, 4).await;
    let sub = state.subscribe(id, None).await.unwrap();

    send_text(&state, id, "go").await;
    common::server::wait_for("the turn's request", || provider.bodies().len() == 1).await;
    let accepted = state.run_command(id, "compact", "").await.unwrap();
    assert_eq!(accepted.data.as_ref().unwrap()["accepted"], true);
    release.notify_one();
    collect_until(sub, |e| {
        e.kind == "command.result" && e.payload["ok"] == false
    })
    .await;

    let results = compact_results(&state, id).await;
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(results[0].payload["ok"], true, "the acceptance comes first");
    assert_eq!(results[0].payload["data"]["accepted"], true);
    assert_eq!(results[1].payload["ok"], false);
    assert_eq!(
        results[1].payload["operation_id"],
        accepted.operation_id.to_string()
    );
    assert!(results[1].payload["error"]
        .as_str()
        .unwrap()
        .contains("waiting for an answer"));
    assert_eq!(provider.bodies().len(), 1, "no summary request");
    assert!(stored(&state, id, "context.compacted").await.is_empty());
}

// Given an idle /compact whose summary request is still in flight
// When /agent plan and /variant are run
// Then both are refused as a compaction is running, and the gauge after the
//      commit still counts the build role
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_change_during_a_summary_is_refused_and_the_gauge_keeps_the_role() {
    let (tmp, ws) = tmp_ws();
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = spawn_gated_provider(vec![answer(SUMMARY)], release.clone()).await;
    let state = state(tmp.path(), &provider);
    let id = seeded(&state, &ws, 4).await;
    let system = |snapshot: Value| {
        snapshot["breakdown"]["system"]
            .as_u64()
            .expect("system tokens")
    };
    let before = serde_json::to_value(state.context(id).await.unwrap()).unwrap();
    let sub = state.subscribe(id, None).await.unwrap();

    let compacting = {
        let state = state.clone();
        tokio::spawn(async move { state.run_command(id, "compact", "").await })
    };
    common::server::wait_for("the summary request", || provider.bodies().len() == 1).await;

    for command in ["agent", "variant"] {
        let arguments = if command == "agent" { "plan" } else { "off" };
        let refused = state.run_command(id, command, arguments).await.unwrap();
        assert!(!refused.ok, "{command}: {refused:?}");
        assert!(
            refused.error.unwrap().contains("a compaction is running"),
            "{command}"
        );
    }

    release.notify_one();
    let out = compacting.await.unwrap().unwrap();
    assert!(out.ok, "{out:?}");
    let live = collect_until(sub, |e| e.kind == "context.compacted").await;
    assert!(live.iter().any(|e| e.kind == "context.compacted"));
    let after = serde_json::to_value(state.context(id).await.unwrap()).unwrap();
    assert_eq!(system(after.clone()), system(before), "{after}");

    // Once it is over, the change goes through.
    let switched = state.run_command(id, "agent", "plan").await.unwrap();
    assert!(switched.ok, "{switched:?}");
}
