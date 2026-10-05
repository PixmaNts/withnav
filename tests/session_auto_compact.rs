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
    // The request that started the turn is never only in the summary: it is
    // sent verbatim, as a user message, right after it.
    let messages = next["messages"].as_array().unwrap();
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "user" && m["content"] == "look at big.txt a few times"),
        "the original request is sent verbatim: {text:.400}"
    );
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

/// Two providers: `default` is a/model-a with a 128k window, `small` is
/// b/model-b with `window_b`.
fn two_models(env: &Env, a: &CapturingProvider, b: &CapturingProvider, window_b: u64) -> AppState {
    two_models_both(env, a, b, 128_000, window_b)
}

/// [`two_models`] with the window of `default` too.
fn two_models_both(
    env: &Env,
    a: &CapturingProvider,
    b: &CapturingProvider,
    window_a: u64,
    window_b: u64,
) -> AppState {
    let config = TestServerConfig::mock(&env.root, &a.url)
        .chat_tools(window_a)
        .config();
    let entry = |provider: &str, model: &str, window: u64| {
        json!({"provider": provider, "model": model, "supports_tools": true,
               "context_window": window, "reasoning_history": "preserve"})
    };
    let provider = |url: &str| {
        json!({"base_url": url, "api_key_env": "NAVAI_TEST_KEY", "headers": {},
               "connect_timeout_ms": 2000, "request_timeout_ms": 600_000})
    };
    std::fs::write(
        config.models_path.as_ref().expect("models path"),
        json!({
            "providers": {"a": provider(&a.url), "b": provider(&b.url)},
            "models": {
                "default": entry("a", "model-a", window_a),
                "small": entry("b", "model-b", window_b),
            }
        })
        .to_string(),
    )
    .expect("models.json");
    std::fs::write(
        config.config_dir.join("auth.json"),
        json!({"a": {"api_key": "key-a"}, "b": {"api_key": "key-b"}}).to_string(),
    )
    .expect("auth.json");
    AppState::new(config).expect("state")
}

// Given a long conversation on a large window
// When /model moves it to a window it does not fit
// Then the OLD model writes the summary (the target is never asked to read the
//      conversation), the checkpoint is stored, the switch then goes through,
//      and the next request goes to the new model with the summary and the
//      recent turns; the stored events say `switch` and the switch follows
//      the compaction
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_to_a_smaller_window_compacts_with_the_old_model_then_switches() {
    let env = env();
    let a = spawn_capturing_provider(vec![answer(SUMMARY)]).await;
    let b = spawn_capturing_provider(vec![answer("from the small model")]).await;
    let state = two_models(&env, &a, &b, 20_000);
    let id = seeded(&state, &env.ws, 8).await;
    let sub = state.subscribe(id, None).await.unwrap();

    let out = state.run_command(id, "model", "small").await.unwrap();
    assert!(out.ok, "{out:?}");
    let data = out.data.unwrap();
    assert_eq!(data["changed"], true);
    assert_eq!(data["compacted"]["generation"], 1);
    assert!(
        data["compacted"]["estimate_after"].as_u64().unwrap()
            < data["compacted"]["estimate_before"].as_u64().unwrap()
    );

    // The summary was written by model-a, with no tools; b saw nothing yet.
    assert_eq!(a.bodies().len(), 1);
    let summary_request = body(&a, 0);
    assert_eq!(summary_request["model"], "model-a");
    assert!(summary_request.get("tools").is_none());
    assert!(b.bodies().is_empty());

    let events = collect_until(sub, |e| e.kind == "model.changed").await;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let compacted = kinds
        .iter()
        .position(|k| *k == "context.compacted")
        .unwrap();
    let switched = kinds.iter().position(|k| *k == "model.changed").unwrap();
    assert!(compacted < switched, "{kinds:?}");
    let notice = events
        .iter()
        .find(|e| e.kind == "context.compacted")
        .unwrap();
    assert_eq!(notice.payload["reason"], "switch");
    assert_eq!(stored(&state, id, "context.compacted").await.len(), 1);
    assert_eq!(state.session_summary(id).await.unwrap().model, "small");

    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "go on").await;
    collect_until(sub, |e| e.kind == "turn.end").await;
    assert_eq!(a.bodies().len(), 1, "the old model is not used again");
    let next = body(&b, 0);
    assert_eq!(next["model"], "model-b");
    let text = all_text(&next);
    assert!(text.contains("Goal: finish the parser."));
    assert!(text.contains("go on"));
    assert!(text.contains("answer 7"), "the recent turns stay");
    assert!(!text.contains("answer 0 "), "the old ones were summarized");
}

// Given a conversation whose recent turns alone are bigger than the target
// When /model moves it there
// Then no summary is asked for, the switch is refused with the way out, and
//      the conversation stays on the old model, uncompacted
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_that_cannot_be_made_to_fit_is_refused_without_compacting() {
    let env = env();
    let a = spawn_capturing_provider(vec![]).await;
    let b = spawn_capturing_provider(vec![]).await;
    let state = two_models(&env, &a, &b, 8_000);
    let id = seeded(&state, &env.ws, 8).await;

    let out = state.run_command(id, "model", "small").await.unwrap();
    assert!(!out.ok, "{out:?}");
    let error = out.error.unwrap();
    assert!(error.contains("cannot be compacted to fit it"), "{error}");
    assert!(error.contains("larger context window"), "{error}");
    assert!(a.bodies().is_empty() && b.bodies().is_empty());
    assert_eq!(state.session_summary(id).await.unwrap().model, "default");
    assert!(stored(&state, id, "context.compacted").await.is_empty());
    assert!(stored(&state, id, "model.changed").await.is_empty());
    let (_, _, checkpoint) = state.history().load_for_resume(id).await.unwrap().unwrap();
    assert!(checkpoint.is_none());
}

// Given a switch whose summary the old model cannot write
// Then the switch fails, nothing changed, and the conversation is unchanged
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_whose_summary_fails_changes_nothing() {
    let env = env();
    let a = spawn_responding_provider(1, |_, _| {
        b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n".to_vec()
    })
    .await;
    let b = spawn_capturing_provider(vec![]).await;
    let state = two_models(&env, &a, &b, 20_000);
    let id = seeded(&state, &env.ws, 8).await;

    let out = state.run_command(id, "model", "small").await.unwrap();
    assert!(!out.ok, "{out:?}");
    assert!(out
        .error
        .unwrap()
        .contains("compaction before the switch failed"));
    assert_eq!(state.session_summary(id).await.unwrap().model, "default");
    assert!(stored(&state, id, "model.changed").await.is_empty());
    assert!(stored(&state, id, "context.compacted").await.is_empty());
}

// Given a manual /compact
// Then the stored event says `manual`; an event stored without the field
//      still reads
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reason_is_stored_and_an_old_event_without_it_still_reads() {
    let env = env();
    let provider = spawn_capturing_provider(vec![answer(SUMMARY)]).await;
    let state = state(&env, &provider, WINDOW);
    let id = seeded(&state, &env.ws, 4).await;
    let out = state.run_command(id, "compact", "").await.unwrap();
    assert!(out.ok, "{out:?}");
    let events = stored(&state, id, "context.compacted").await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload["reason"], "manual");

    let old = json!({"generation": 1, "through_message": 3, "pruned": 0,
                     "estimate_before": 100, "estimate_after": 50});
    let parsed: navai::bus::event::ContextCompacted = serde_json::from_value(old).unwrap();
    assert_eq!(parsed.reason, None);
}

// Given a conversation with a lot of reasoning from the first model, switched
//       to another model (which is never sent that reasoning)
// When a long turn later crosses the budget
// Then compaction plans against the prompt as it is really sent, succeeds, and
//      the turn goes on; the stripped reasoning is not counted against it
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switched_session_with_large_old_reasoning_compacts_and_continues() {
    let env = env();
    let a = spawn_capturing_provider(vec![]).await;
    let (r0, r1) = (read_big(&env, "r0"), read_big(&env, "r1"));
    let b = spawn_responding_provider(8, move |i, request| match i {
        0 => r0.clone(),
        1 => r1.clone(),
        _ if is_summary(request) => answer(SUMMARY),
        _ => answer("all done"),
    })
    .await;
    let state = two_models_both(&env, &a, &b, 128_000, 20_000);
    // Ten small turns whose reasoning alone is far more than the target budget
    // (about 11k tokens), but which the target never receives.
    let heavy: Vec<ChatMessage> = (0..10)
        .flat_map(|n| {
            [
                ChatMessage::User {
                    provenance: Default::default(),
                    content: format!("request {n}"),
                },
                ChatMessage::Assistant {
                    content: Some(format!("answer {n}")),
                    reasoning_content: Some("think ".repeat(2_500)),
                    tool_calls: vec![],
                },
            ]
        })
        .collect();
    let id = state.create_session(&env.ws, None, None).await.unwrap();
    state.close_session(id).await.unwrap();
    state.history().flush().await.unwrap();
    state.history().save_transcript(id, heavy);
    state.history().flush().await.unwrap();
    state
        .create_or_resume_session(&env.ws, None, None, Some(id))
        .await
        .unwrap();
    let out = state.run_command(id, "model", "small").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert!(
        out.data.as_ref().unwrap()["compacted"].is_null(),
        "the stripped history already fits: no compaction was needed to switch"
    );

    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "now read big.txt twice").await;
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    let end = events.iter().find(|e| e.kind == "turn.end").unwrap();
    assert_eq!(end.payload["outcome"], "done", "{end:?}");
    let compacted: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "context.compacted")
        .collect();
    assert_eq!(compacted.len(), 1);
    assert_eq!(compacted[0].payload["reason"], "automatic");
    // Measured on the real prompt: the 25k tokens of old reasoning are not in it.
    let before = compacted[0].payload["estimate_before"].as_u64().unwrap();
    assert!(before < 20_000, "{before}");
    assert!(a.bodies().is_empty());
    // And none of it was ever sent.
    for body in b.bodies() {
        assert!(!body.contains("think think think"), "reasoning was sent");
    }
}

/// A conversation over the budget whose next request is held by an automatic
/// summary, parked at its provider call. Returns the state, the session, the
/// provider and the two gates.
async fn parked_summary() -> (
    Env,
    AppState,
    ulid::Ulid,
    CapturingProvider,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let env = env();
    let reached = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = common::sse::spawn_holding_provider(
        6,
        0,
        reached.clone(),
        release.clone(),
        |_, request| {
            if is_summary(request) {
                answer(SUMMARY)
            } else {
                answer("done")
            }
        },
    )
    .await;
    let state = state(&env, &provider, WINDOW);
    let id = seeded(&state, &env.ws, 7).await;
    send_text(&state, id, "go").await;
    reached.notified().await;
    assert!(is_summary(&provider.bodies()[0]), "parked in the summary");
    (env, state, id, provider, release)
}

// Given an automatic summary in flight
// When the session is stopped
// Then nothing is committed (no checkpoint, no context.compacted), the
//      compacting mark is cleared, and the stop completes
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_during_an_automatic_summary_commits_nothing_and_clears_the_mark() {
    let (_env, state, id, _provider, release) = parked_summary().await;
    // The mark is up while the summary runs: an agent switch says so.
    let marked = state.run_command(id, "agent", "plan").await.unwrap();
    assert!(!marked.ok);
    assert!(marked.error.unwrap().contains("a compaction is running"));

    let stopping = {
        let state = state.clone();
        tokio::spawn(async move { state.close_session(id).await })
    };
    // The summary answer arrives after the stop was asked.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    release.notify_one();
    stopping.await.unwrap().expect("the stop completes");

    assert!(stored(&state, id, "context.compacted").await.is_empty());
    assert!(stored(&state, id, "model.changed").await.is_empty());
    let (_, _, checkpoint) = state.history().load_for_resume(id).await.unwrap().unwrap();
    assert!(checkpoint.is_none(), "nothing was committed");
}

// Given an automatic summary in flight (the turn is running)
// When /agent, /variant and /model are run, and then /compact
// Then the first three are refused (a compaction is running) and change
//      nothing; /compact is accepted for after the turn and runs then, as a
//      manual compaction under its own operation id; the mark is cleared when
//      it is over, so the refused commands go through
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controls_during_an_automatic_summary_are_refused_or_wait_their_turn() {
    let (_env, state, id, provider, release) = parked_summary().await;

    for (command, arguments) in [("agent", "plan"), ("variant", "off"), ("model", "default")] {
        let refused = state.run_command(id, command, arguments).await.unwrap();
        assert!(!refused.ok, "{command}: {refused:?}");
        assert!(
            refused.error.unwrap().contains("a compaction is running"),
            "{command}"
        );
    }
    let compact = state.run_command(id, "compact", "").await.unwrap();
    assert!(compact.ok, "{compact:?}");
    assert_eq!(compact.data.as_ref().unwrap()["applies"], "after_turn");

    let sub = state.subscribe(id, None).await.unwrap();
    release.notify_one();
    // The turn, then the manual compaction (a second summary).
    collect_until(sub, |e| {
        e.kind == "command.result"
            && e.payload["operation_id"] == compact.operation_id.to_string()
            && e.payload["data"]["applies"] == "after_turn"
            && e.payload["data"]["generation"].is_number()
    })
    .await;

    let results: Vec<_> = stored(&state, id, "command.result")
        .await
        .into_iter()
        .filter(|e| e.payload["operation_id"] == compact.operation_id.to_string())
        .collect();
    assert_eq!(results.len(), 2, "accepted, then done: {results:?}");
    assert!(results.iter().all(|e| e.payload["ok"] == true));
    let reasons: Vec<String> = stored(&state, id, "context.compacted")
        .await
        .iter()
        .map(|e| e.payload["reason"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(reasons, ["automatic", "manual"]);
    assert_eq!(provider.bodies().len(), 3, "summary, turn, manual summary");
    assert!(stored(&state, id, "model.changed").await.is_empty());

    // The mark is gone: the refused commands go through.
    let again = state.run_command(id, "agent", "plan").await.unwrap();
    assert!(again.ok, "{again:?}");
}

// A failed destination selection must not install its staged compaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_switch_rolls_back_the_staged_checkpoint_and_selection() {
    let env = env();
    let a = spawn_capturing_provider(vec![answer(SUMMARY), answer("still the old model")]).await;
    let b = spawn_capturing_provider(vec![]).await;
    let state = two_models(&env, &a, &b, 20_000);
    let id = seeded(&state, &env.ws, 8).await;
    state.history().flush().await.unwrap();
    let before = state.history().load_for_resume(id).await.unwrap().unwrap();
    let before_gauge = serde_json::to_value(state.context(id).await.unwrap()).unwrap();
    // Fail the second strict event insert, after context.compacted was inserted.
    let head = state.session_summary(id).await.unwrap().current_seq + 1;
    let conn = rusqlite::Connection::open(env.root.join("history.db")).unwrap();
    for seq in head + 1..=head + 1 {
        conn.execute(
            "INSERT INTO events (session_id, seq, kind, envelope) VALUES (?1, ?2, 'x', '{}')",
            rusqlite::params![id.to_string(), seq as i64],
        )
        .unwrap();
    }

    let out = state.run_command(id, "model", "small").await.unwrap();
    assert!(!out.ok, "{out:?}");
    assert!(out.error.unwrap().contains("could not be saved"));
    assert_eq!(state.session_summary(id).await.unwrap().model, "default");
    assert!(stored(&state, id, "model.changed").await.is_empty());
    // Neither the staged checkpoint nor its notice survives the failed transaction.
    let notices = stored(&state, id, "context.compacted").await;
    assert!(notices.is_empty());
    let meta = state.history().meta(id).await.unwrap().unwrap();
    assert_eq!(meta.model, "default");
    assert!(!meta.model_resolution.unwrap().contains("model-b"));
    let (_, _, checkpoint) = state.history().load_for_resume(id).await.unwrap().unwrap();
    assert!(checkpoint.is_none());
    let after = state.history().load_for_resume(id).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&after.1).unwrap(),
        serde_json::to_value(&before.1).unwrap()
    );
    assert_eq!(after.2, before.2);
    assert_eq!(
        serde_json::to_value(state.context(id).await.unwrap()).unwrap(),
        before_gauge
    );

    // The same actor must still send the original history after the refusal.
    assert_eq!(state.session_summary(id).await.unwrap().model, "default");
    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "go on").await;
    collect_until(sub, |e| e.kind == "turn.end").await;
    assert!(b.bodies().is_empty());
    let request = body(&a, 1);
    assert_eq!(request["model"], "model-a");
    let text = all_text(&request);
    assert!(
        !text.contains("Goal: finish the parser."),
        "no staged checkpoint installed"
    );
    assert!(text.contains("go on"));
    assert!(
        text.contains("answer 0 "),
        "the original transcript is retained"
    );
}
