//! Automatic compaction through the service: a turn that outgrows the input
//! budget is compacted at a step boundary and goes on, a conversation that
//! cannot be made to fit ends the turn with a clear error, nothing runs while
//! an `ask` waits, and a `/model` switch to a smaller window compacts first
//! with the model the conversation is on.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use navai::bus::event::EventEnvelope;
use navai::provider::openai::ChatMessage;
use navai::server::AppState;

mod common;
use common::server::{collect_until, send_text, TestServerConfig};
use common::sse::{
    http_sse, spawn_capturing_provider, spawn_responding_provider, sse_body, text_chunk,
    tool_call_chunk, CapturingProvider,
};

const SUMMARY: &str = "Goal: finish the parser.\nNext: run the tests.";
const WINDOW: u64 = 20_000;

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    ws: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("ws");
    // A file whose output the model "reads" over and over: about 6 000 tokens.
    std::fs::write(ws.join("big.txt"), "lorem ipsum ".repeat(2_000)).expect("big");
    Env {
        _tmp: tmp,
        root,
        ws,
    }
}

fn answer(text: &str) -> Vec<u8> {
    http_sse(&sse_body(&[&text_chunk(text)]))
}

fn read_big(env: &Env, id: &str) -> Vec<u8> {
    http_sse(&sse_body(&[&tool_call_chunk(
        id,
        "read",
        &json!({"path": env.ws.join("big.txt")}),
    )]))
}

fn state(env: &Env, provider: &CapturingProvider, window: u64) -> AppState {
    TestServerConfig::mock(&env.root, &provider.url)
        .chat_tools(window)
        .state()
}

fn history(turns: usize) -> Vec<ChatMessage> {
    (0..turns)
        .flat_map(|n| {
            [
                ChatMessage::User {
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

async fn seeded(state: &AppState, ws: &Path, turns: usize) -> ulid::Ulid {
    let id = state.create_session(ws, None, None).await.expect("create");
    state.close_session(id).await.expect("close");
    state.history().flush().await.expect("flush");
    state.history().save_transcript(id, history(turns));
    state.history().flush().await.expect("flush");
    state
        .create_or_resume_session(ws, None, None, Some(id))
        .await
        .expect("resume");
    id
}

async fn stored(state: &AppState, id: ulid::Ulid, kind: &str) -> Vec<EventEnvelope> {
    state.history().flush().await.expect("flush");
    let sub = state.subscribe(id, None).await.expect("subscribe");
    match sub.replay {
        navai::bus::ResumeOutcome::Replay(events) => {
            events.into_iter().filter(|e| e.kind == kind).collect()
        }
        other => panic!("expected replay, got {other:?}"),
    }
}

fn body(provider: &CapturingProvider, index: usize) -> Value {
    serde_json::from_str(&provider.bodies()[index]).expect("request body")
}

fn all_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A request is a summary request when it carries no tools.
fn is_summary(request: &str) -> bool {
    !request.contains("\"tools\"")
}

// Given a conversation of two short turns on a 20k window
// When one turn reads a 6k-token file again and again
// Then the prompt crosses the input budget mid-turn: ONE automatic compaction
//      runs at a step boundary, the same turn carries on and finishes, the
//      request after it carries the summary and the recent messages, the
//      steps already taken are not given back, and the stored event says why
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_heavy_turn_that_crosses_the_budget_is_compacted_and_goes_on() {
    let env = env();
    // Requests: 1-2 read, 3 would be over budget -> summary (no tools), then
    // the third read goes on, and a final answer.
    let (a, b, c) = (
        read_big(&env, "r0"),
        read_big(&env, "r1"),
        read_big(&env, "r2"),
    );
    let provider = spawn_responding_provider(8, move |i, request| match i {
        0 => a.clone(),
        1 => b.clone(),
        _ if is_summary(request) => answer(SUMMARY),
        2 => c.clone(),
        _ => answer("all done"),
    })
    .await;
    let state = state(&env, &provider, WINDOW);
    let id = state.create_session(&env.ws, None, None).await.unwrap();
    let sub = state.subscribe(id, None).await.unwrap();

    send_text(&state, id, "look at big.txt a few times").await;
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();

    // One turn: one turn.start, one turn.end, done, and the compaction inside.
    assert_eq!(kinds.iter().filter(|k| **k == "turn.start").count(), 1);
    let end = events.iter().find(|e| e.kind == "turn.end").unwrap();
    assert_eq!(end.payload["outcome"], "done", "{end:?}");
    let compacted: Vec<&EventEnvelope> = events
        .iter()
        .filter(|e| e.kind == "context.compacted")
        .collect();
    assert_eq!(compacted.len(), 1, "{kinds:?}");
    let turn_end = kinds.iter().position(|k| *k == "turn.end").unwrap();
    let at = kinds
        .iter()
        .position(|k| *k == "context.compacted")
        .unwrap();
    assert!(at < turn_end);
    assert_eq!(compacted[0].payload["reason"], "automatic");
    let after = compacted[0].payload["estimate_after"].as_u64().unwrap();
    let before = compacted[0].payload["estimate_before"].as_u64().unwrap();
    assert!(after < before);
    // Below ~70% of the budget (20k - 5k reserve - 4k margin = 11k).
    assert!(after <= 11_000 / 10 * 7 + 1, "{after}");

    // The provider saw: read, read, summary request, then the turn on.
    let bodies = provider.bodies();
    assert_eq!(bodies.len(), 4, "{}", bodies.len());
    assert!(is_summary(&bodies[2]));
    let next = body(&provider, 3);
    let text = all_text(&next);
    assert!(text.contains("Goal: finish the parser."), "the summary");
    // The latest exchange (the second read) stays verbatim; the first read's
    // output went into the summarized part.
    let tool_outputs: Vec<&Value> = next["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(tool_outputs.len(), 1, "{text:.300}");
    assert!(tool_outputs[0]["content"]
        .as_str()
        .unwrap()
        .contains("lorem ipsum"));
    assert!(next["tools"].is_array());
    // Step budget not reset: the steps counted are the turn's, the same task.
    assert_eq!(
        events.iter().filter(|e| e.kind == "llm.response").count(),
        3,
        "two reads and the answer: no step lost or repeated, the summary is not a step"
    );
    assert_eq!(stored(&state, id, "context.compacted").await.len(), 1);
}

// Given a window whose recent messages alone do not fit
// When a request would go over the budget
// Then the turn ends with a capacity error naming the way out, and no
//      summary is asked for
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tail_that_cannot_fit_ends_the_turn_with_a_clear_error() {
    let env = env();
    // The user message alone is bigger than the whole budget.
    let provider = spawn_capturing_provider(vec![]).await;
    let state = state(&env, &provider, 10_000);
    let id = state.create_session(&env.ws, None, None).await.unwrap();
    let sub = state.subscribe(id, None).await.unwrap();

    send_text(&state, id, &"huge ".repeat(8_000)).await;
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    let end = events.iter().find(|e| e.kind == "turn.end").unwrap();
    assert_eq!(end.payload["outcome"], "error", "{end:?}");
    assert_eq!(end.payload["error"]["code"], "compaction");
    let message = end.payload["error"]["message"].as_str().unwrap();
    assert!(message.contains("/compact cannot help either"), "{message}");
    assert!(message.contains("larger context window"), "{message}");
    assert!(provider.bodies().is_empty(), "nothing was sent");
    assert!(stored(&state, id, "context.compacted").await.is_empty());
}

// Given a history over the budget and an agent whose turn ends by asking
// Then the compaction ran before the request that asked, and none runs while
//      the ask waits
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_is_compacted_while_an_ask_waits() {
    let env = env();
    let ask = tool_call_chunk("ask_1", "ask", &json!({"question": "which file?"}));
    let ask = http_sse(&sse_body(&[&ask]));
    let provider = spawn_responding_provider(4, move |_, request| {
        if is_summary(request) {
            answer(SUMMARY)
        } else {
            ask.clone()
        }
    })
    .await;
    let state = state(&env, &provider, WINDOW);
    let id = seeded(&state, &env.ws, 7).await;
    let sub = state.subscribe(id, None).await.unwrap();

    send_text(&state, id, "go").await;
    collect_until(sub, |e| {
        e.kind == "agent.state" && e.payload["to"] == "yielded"
    })
    .await;
    assert_eq!(stored(&state, id, "context.compacted").await.len(), 1);
    let bodies = provider.bodies();
    assert_eq!(bodies.len(), 2, "the summary, then the agent's own request");
    assert!(is_summary(&bodies[0]));
    assert!(bodies[1].contains("\"ask\""));

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        provider.bodies().len(),
        2,
        "nothing else was asked of the model"
    );
    assert_eq!(stored(&state, id, "context.compacted").await.len(), 1);
}

// Given a history over the budget and a gated call that needs a decision
// Then the compaction ran before the request, and none runs while the
//      permission request waits
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_is_compacted_while_a_permission_request_waits() {
    let env = env();
    let write = tool_call_chunk(
        "w1",
        "write",
        &json!({"path": env.ws.join("out.txt"), "content": "hi"}),
    );
    let write = http_sse(&sse_body(&[&write]));
    let provider = spawn_responding_provider(4, move |_, request| {
        if is_summary(request) {
            answer(SUMMARY)
        } else {
            write.clone()
        }
    })
    .await;
    let state = state(&env, &provider, WINDOW);
    let id = seeded(&state, &env.ws, 7).await;
    let sub = state.subscribe(id, None).await.unwrap();

    send_text(&state, id, "write it").await;
    collect_until(sub, |e| e.kind == "permission.requested").await;
    assert_eq!(stored(&state, id, "context.compacted").await.len(), 1);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(provider.bodies().len(), 2);
    assert_eq!(stored(&state, id, "context.compacted").await.len(), 1);
    assert!(
        !env.ws.join("out.txt").exists(),
        "the held call did not run"
    );
}
