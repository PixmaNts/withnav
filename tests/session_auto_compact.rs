//! Automatic compaction through the service: a turn that outgrows the input
//! budget is compacted at a step boundary and goes on, a conversation that
//! cannot be made to fit ends the turn with a clear error, nothing runs while
//! an `ask` waits, and a `/model` switch to a smaller window compacts first
//! with the model the conversation is on.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use withnav::bus::event::EventEnvelope;
use withnav::provider::openai::ChatMessage;
use withnav::server::AppState;

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
                    content: Some(format!("answer {n} {}", "word ".repeat(1400))),
                    reasoning_content: None,
                    tool_calls: vec![],
                },
            ]
        })
        .collect()
}

async fn seeded(state: &AppState, ws: &Path, turns: usize) -> ulid::Ulid {
    seeded_with(state, ws, history(turns)).await
}

async fn seeded_with(state: &AppState, ws: &Path, transcript: Vec<ChatMessage>) -> ulid::Ulid {
    let id = state.create_session(ws, None, false).await.expect("create");
    state.close_session(id).await.expect("close");
    state.history().flush().await.expect("flush");
    state.history().save_transcript(id, transcript);
    state.history().flush().await.expect("flush");
    state
        .create_or_resume_session(ws, None, false, Some(id))
        .await
        .expect("resume");
    id
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
    let id = state.create_session(&env.ws, None, false).await.unwrap();
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
    let role_tokens = withnav::agent::definitions::Agents::builtin()
        .get("build")
        .unwrap()
        .prompt
        .len()
        .div_ceil(4) as u64;
    // Expanded ask guidance and its choice schema increase the fixed prompt overhead.
    assert!(after <= 11_000 / 10 * 7 + 1 + role_tokens + 250, "{after}");

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
    let id = state.create_session(&env.ws, None, false).await.unwrap();
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
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    assert_eq!(events.last().unwrap().payload["outcome"], "yielded");
    assert_eq!(stored(&state, id, "context.compacted").await.len(), 1);
    let bodies = provider.bodies();
    assert_eq!(
        bodies.len(),
        3,
        "rolling summary, then the agent's own request"
    );
    assert!(is_summary(&bodies[0]));
    assert!(bodies[2].contains("\"ask\""));

    assert_eq!(
        provider.bodies().len(),
        3,
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
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    assert!(events.iter().any(|e| e.kind == "permission.requested"));
    assert_eq!(events.last().unwrap().payload["outcome"], "yielded");
    assert_eq!(stored(&state, id, "context.compacted").await.len(), 1);
    assert_eq!(provider.bodies().len(), 3);
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
        json!({"base_url": url, "api_key_env": "WITHNAV_TEST_KEY", "headers": {},
               "connect_timeout_ms": 2000, "request_timeout_ms": 600_000})
    };
    std::fs::write(
        config.models_path.as_ref().expect("models path"),
        json!({
            "providers": {"a": provider(&a.url), "b": provider(&b.url)},
            "models": {
                "a/model-a": entry("a", "model-a", window_a),
                "b/model-b": entry("b", "model-b", window_b),
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
    std::fs::write(config.config_dir.join("config.json"),json!({"agents":{"build":{"model":"a/model-a"},"plan":{"model":"a/model-a"}},"permissions":{"scratch_dirs":[]}}).to_string()).unwrap();
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

    let out = state.run_command(id, "model", "b/model-b").await.unwrap();
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
    assert_eq!(state.session_summary(id).await.unwrap().model, "b/model-b");

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

    let out = state.run_command(id, "model", "b/model-b").await.unwrap();
    assert!(!out.ok, "{out:?}");
    let error = out.error.unwrap();
    assert!(error.contains("cannot be compacted to fit it"), "{error}");
    assert!(error.contains("larger context window"), "{error}");
    assert!(a.bodies().is_empty() && b.bodies().is_empty());
    assert_eq!(state.session_summary(id).await.unwrap().model, "a/model-a");
    assert!(stored(&state, id, "context.compacted").await.is_empty());
    assert!(stored(&state, id, "model.changed").await.is_empty());
    let (_, _, checkpoint) = state.history().load_for_resume(id).await.unwrap().unwrap();
    assert!(checkpoint.is_none());
}

const SERVER_ERROR: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metacognition_provider_failure_falls_back_to_session() {
    let env = env();
    let a = spawn_responding_provider(20, |_, _| answer(SUMMARY)).await;
    let b = spawn_responding_provider(20, |_, _| SERVER_ERROR.to_vec()).await;
    let state = two_models(&env, &a, &b, 1_000_000);
    std::fs::write(
        env.root.join("cfg/config.json"),
        json!({
            "agents":{"build":{"model":"a/model-a"}},
            "layers":{"metacognition":"b/model-b"},
            "permissions":{"scratch_dirs":[]}
        })
        .to_string(),
    )
    .unwrap();
    let id = seeded(&state, &env.ws, 8).await;
    let out = state.run_command(id, "compact", "").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(b.bodies().len(), 2);
    assert!(!a.bodies().is_empty());
    assert_eq!(
        stored(&state, id, "context.compacted").await[0].payload["written_by"]["model"],
        "model-a"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metacognition_writes_manual_rolling_summary_and_session_can_opt_out() {
    for session in [false, true] {
        let env = env();
        let a = spawn_responding_provider(20, |_, _| answer(SUMMARY)).await;
        let b = spawn_responding_provider(20, |_, _| answer(SUMMARY)).await;
        let state = two_models(&env, &a, &b, 1_000_000);
        std::fs::write(env.root.join("cfg/config.json"), json!({
            "agents":{"build":{"model":"a/model-a"}},
            "layers":{"metacognition":"b/model-b"},
            "compaction":{"writer":if session {"session"} else {"metacognition"}, "chunk_tokens":4000},
            "permissions":{"scratch_dirs":[]}
        }).to_string()).unwrap();
        let id = seeded(&state, &env.ws, 8).await;
        let out = state.run_command(id, "compact", "").await.unwrap();
        assert!(out.ok, "{out:?}");
        let chosen = if session { &a } else { &b };
        let other = if session { &b } else { &a };
        assert!(other.bodies().is_empty());
        assert!(chosen.bodies().len() > 1);
        assert!(chosen.requests()[0].to_lowercase().contains(if session {
            "authorization: bearer key-a"
        } else {
            "authorization: bearer key-b"
        }));
        let requests = chosen.bodies();
        for (index, request) in requests.iter().enumerate() {
            let request: Value = serde_json::from_str(request).unwrap();
            assert_eq!(
                request["model"],
                if session { "model-a" } else { "model-b" }
            );
            assert_eq!(
                request["messages"].as_array().unwrap().len(),
                if index == 0 { 2 } else { 3 }
            );
            if index > 0 {
                assert!(request["messages"][1]["content"]
                    .as_str()
                    .unwrap()
                    .contains(SUMMARY.split('\n').next().unwrap()));
            }
        }
        let notices = stored(&state, id, "context.compacted").await;
        assert_eq!(notices[0].payload["passes"], requests.len());
        assert_eq!(
            notices[0].payload["written_by"]["layer"],
            if session {
                "cognition"
            } else {
                "metacognition"
            }
        );
    }
}

// Given a switch whose summary neither the old model nor the destination can write
// Then the switch fails, nothing changed, and the conversation is unchanged
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_whose_summary_fails_changes_nothing() {
    let env = env();
    let a = spawn_responding_provider(1, |_, _| SERVER_ERROR.to_vec()).await;
    let b = spawn_responding_provider(1, |_, _| SERVER_ERROR.to_vec()).await;
    let state = two_models(&env, &a, &b, 20_000);
    let id = seeded(&state, &env.ws, 8).await;

    let out = state.run_command(id, "model", "b/model-b").await.unwrap();
    assert!(!out.ok, "{out:?}");
    assert!(out
        .error
        .unwrap()
        .contains("compaction before the switch failed"));
    assert_eq!(state.session_summary(id).await.unwrap().model, "a/model-a");
    assert!(stored(&state, id, "model.changed").await.is_empty());
    assert!(stored(&state, id, "context.compacted").await.is_empty());
}

// Given a conversation on a model that is failing (its provider answers 500)
// When /model moves it to a smaller window the conversation does not fit
// Then the destination writes the summary, the switch goes through, and the
//      next request goes to the destination with that summary
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_off_a_broken_model_lets_the_destination_write_the_summary() {
    let env = env();
    let a = spawn_responding_provider(1, |_, _| SERVER_ERROR.to_vec()).await;
    let b = spawn_capturing_provider(vec![answer(SUMMARY), answer("from the small model")]).await;
    let state = two_models(&env, &a, &b, 20_000);
    let id = seeded(&state, &env.ws, 8).await;

    let out = state.run_command(id, "model", "b/model-b").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(out.data.unwrap()["compacted"]["generation"], 1);
    assert_eq!(a.bodies().len(), 1, "the broken model was tried first");
    let summary_request = body(&b, 0);
    assert_eq!(summary_request["model"], "model-b");
    assert!(summary_request.get("tools").is_none());
    assert_eq!(state.session_summary(id).await.unwrap().model, "b/model-b");
    assert_eq!(stored(&state, id, "context.compacted").await.len(), 1);

    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "go on").await;
    collect_until(sub, |e| e.kind == "turn.end").await;
    assert_eq!(a.bodies().len(), 1, "the broken model is not used again");
    assert!(all_text(&body(&b, 1)).contains("Goal: finish the parser."));
}

/// Rewrites `models.json` after [`two_models`] wrote it.
fn edit_models(env: &Env, edit: impl FnOnce(&mut Value)) {
    let path = env.root.join("models.json");
    let mut models: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    edit(&mut models);
    std::fs::write(path, models.to_string()).unwrap();
}

// Given a broken current model and a destination with its own key, header,
//       variant and output limit
// When the destination writes the summary instead
// Then that request carries the destination's credentials, header, variant
//      and output limit, and none of the current model's
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fallback_summary_is_sent_with_the_destinations_own_settings() {
    let env = env();
    let a = spawn_responding_provider(1, |_, _| SERVER_ERROR.to_vec()).await;
    let b = spawn_responding_provider(30, |_, _| answer(SUMMARY)).await;
    let state = two_models(&env, &a, &b, 16_000);
    edit_models(&env, |models| {
        models["providers"]["a"]["headers"] = json!({"x-gateway": "a"});
        models["providers"]["b"]["headers"] = json!({"x-gateway": "b"});
        let entry = &mut models["models"]["b/model-b"];
        entry["variants"] = json!({"high": {"reasoning_effort": "high"}});
        entry["default_variant"] = json!("high");
        entry["max_output_tokens"] = json!(777);
    });
    let id = seeded(&state, &env.ws, 8).await;

    let out = state.run_command(id, "model", "b/model-b").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(out.data.unwrap()["compacted"]["generation"], 1);

    let (tried, retried) = (
        a.requests()[0].to_lowercase(),
        b.requests()[0].to_lowercase(),
    );
    assert!(tried.contains("authorization: bearer key-a"), "{tried}");
    assert!(tried.contains("x-gateway: a"), "{tried}");
    assert!(retried.contains("authorization: bearer key-b"), "{retried}");
    assert!(retried.contains("x-gateway: b"), "{retried}");
    assert!(!retried.contains("key-a") && !retried.contains("x-gateway: a"));
    let first = body(&a, 0);
    assert!(first.get("reasoning_effort").is_none(), "{first}");
    assert_ne!(first["max_tokens"], 777);
    let summary = body(&b, 0);
    assert_eq!(summary["model"], "model-b");
    assert_eq!(summary["reasoning_effort"], "high");
    assert_eq!(summary["max_tokens"], 777);
    assert!(summary.get("tools").is_none());
}

/// Seeds a turn that reads a 20 kB file, then answers at length.
fn read_turn(n: usize) -> Vec<ChatMessage> {
    use withnav::provider::openai::{RequestFunction, RequestToolCall};
    vec![
        ChatMessage::User {
            provenance: Default::default(),
            content: format!("request {n}"),
        },
        ChatMessage::Assistant {
            content: None,
            reasoning_content: None,
            tool_calls: vec![RequestToolCall {
                id: format!("call_{n}"),
                kind: "function".into(),
                function: RequestFunction {
                    name: "read".into(),
                    arguments: "{\"path\":\"big.txt\"}".into(),
                },
            }],
        },
        ChatMessage::Tool {
            tool_call_id: format!("call_{n}"),
            content: "lorem ".repeat(3_400),
        },
        ChatMessage::Assistant {
            content: Some(format!("answer {n} {}", "word ".repeat(1_000))),
            reasoning_content: None,
            tool_calls: vec![],
        },
    ]
}

fn short_turn(n: usize) -> Vec<ChatMessage> {
    vec![
        ChatMessage::User {
            provenance: Default::default(),
            content: format!("request {n}"),
        },
        ChatMessage::Assistant {
            content: Some(format!("answer {n}")),
            reasoning_content: None,
            tool_calls: vec![],
        },
    ]
}

// Given a broken current model, a destination with a small window, and tool
//       outputs in the part to summarize that together outgrow that window
// When the destination writes the summary
// Then its request has those outputs cut down to markers (the current model's
//      request, on its big window, had none) and the switch goes through
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fallback_summary_is_cut_down_to_the_destinations_window() {
    let env = env();
    let a = spawn_responding_provider(1, |_, _| SERVER_ERROR.to_vec()).await;
    let b = spawn_responding_provider(30, |_, _| answer(SUMMARY)).await;
    let state = two_models(&env, &a, &b, 20_000);
    let transcript: Vec<ChatMessage> = (0..12)
        .flat_map(read_turn)
        .chain((12..14).flat_map(short_turn))
        .collect();
    let id = seeded_with(&state, &env.ws, transcript).await;

    let out = state.run_command(id, "model", "b/model-b").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(a.bodies().len(), 1);
    assert!(
        !a.bodies()[0].contains("output elided to save context"),
        "the current model's window holds the whole part"
    );
    let summary = &b.bodies()[0];
    assert!(is_summary(summary));
    assert!(
        summary.contains("output elided to save context"),
        "the destination reads the part through its own window"
    );
    assert_eq!(state.session_summary(id).await.unwrap().model, "b/model-b");
    assert_eq!(stored(&state, id, "context.compacted").await.len(), 1);
}

// Given a broken current model and a part to summarize that is too big for the
//       destination's window and has nothing that can be cut (no tool output)
// When /model moves the conversation there
// Then the switch is refused with the way out, the destination is never
//      asked, and nothing changed
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fallback_summary_without_elidable_outputs_rolls_through_the_destinations_window() {
    let env = env();
    let a = spawn_responding_provider(1, |_, _| SERVER_ERROR.to_vec()).await;
    let b = spawn_responding_provider(30, |_, _| answer(SUMMARY)).await;
    let state = two_models(&env, &a, &b, 20_000);
    let id = seeded(&state, &env.ws, 16).await;

    let out = state.run_command(id, "model", "b/model-b").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(a.bodies().len(), 1, "the current model was tried");
    assert!(b.bodies().len() > 1, "the destination uses rolling chunks");
    assert_eq!(state.session_summary(id).await.unwrap().model, "b/model-b");
    assert_eq!(stored(&state, id, "context.compacted").await.len(), 1);
    let (_, _, checkpoint) = state.history().load_for_resume(id).await.unwrap().unwrap();
    assert!(checkpoint.is_some());
}

/// Asserts that a switch stopped mid-way left no selection, notice or
/// checkpoint behind.
async fn assert_nothing_installed(state: &AppState, id: ulid::Ulid) {
    assert!(stored(state, id, "context.compacted").await.is_empty());
    assert!(stored(state, id, "model.changed").await.is_empty());
    let (meta, _, checkpoint) = state.history().load_for_resume(id).await.unwrap().unwrap();
    assert_eq!(meta.model, "a/model-a");
    assert!(checkpoint.is_none(), "no checkpoint was installed");
}

/// Runs `/model b/model-b` and stops the session while the provider that
/// `held` is parked on its answer; then lets that answer through.
async fn stop_a_switch_held_at(
    state: &AppState,
    id: ulid::Ulid,
    reached: std::sync::Arc<tokio::sync::Notify>,
    release: std::sync::Arc<tokio::sync::Notify>,
) {
    let switching = {
        let state = state.clone();
        tokio::spawn(async move { state.run_command(id, "model", "b/model-b").await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), reached.notified())
        .await
        .expect("switch reaches the held provider");
    tokio::time::timeout(std::time::Duration::from_secs(15), state.close_session(id))
        .await
        .expect("stop does not wait for the held provider response")
        .expect("the stop completes");
    release.notify_one();
    if let Ok(out) = switching.await.unwrap() {
        assert!(!out.ok, "{out:?}");
    }
}

// Given a switch whose current model fails, held at that failure
// When the session is stopped before the destination is asked
// Then the stop wins: no selection, notice or checkpoint is installed
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_between_the_two_summary_attempts_installs_nothing() {
    let env = env();
    let (reached, release) = (
        std::sync::Arc::new(tokio::sync::Notify::new()),
        std::sync::Arc::new(tokio::sync::Notify::new()),
    );
    let a = common::sse::spawn_holding_provider(1, 0, reached.clone(), release.clone(), |_, _| {
        SERVER_ERROR.to_vec()
    })
    .await;
    let b = spawn_capturing_provider(vec![answer(SUMMARY)]).await;
    let state = two_models(&env, &a, &b, 20_000);
    let id = seeded(&state, &env.ws, 8).await;

    stop_a_switch_held_at(&state, id, reached, release).await;
    assert_nothing_installed(&state, id).await;
}

// Given a switch whose destination is writing the summary
// When the session is stopped before that summary arrives
// Then the stop wins: no selection, notice or checkpoint is installed
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_during_the_fallback_summary_installs_nothing() {
    let env = env();
    let (reached, release) = (
        std::sync::Arc::new(tokio::sync::Notify::new()),
        std::sync::Arc::new(tokio::sync::Notify::new()),
    );
    let a = spawn_responding_provider(1, |_, _| SERVER_ERROR.to_vec()).await;
    let b = common::sse::spawn_holding_provider(1, 0, reached.clone(), release.clone(), |_, _| {
        answer(SUMMARY)
    })
    .await;
    let state = two_models(&env, &a, &b, 20_000);
    let id = seeded(&state, &env.ws, 8).await;

    stop_a_switch_held_at(&state, id, reached, release).await;
    assert_eq!(a.bodies().len(), 1, "the current model was tried first");
    assert_nothing_installed(&state, id).await;
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
    let parsed: withnav::bus::event::ContextCompacted = serde_json::from_value(old).unwrap();
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
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    state.close_session(id).await.unwrap();
    state.history().flush().await.unwrap();
    state.history().save_transcript(id, heavy);
    state.history().flush().await.unwrap();
    state
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap();
    let out = state.run_command(id, "model", "b/model-b").await.unwrap();
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
    assert_eq!(
        provider.bodies().len(),
        4,
        "rolling summary, turn, manual summary"
    );
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
    // Occupy the later model.changed seq, after context.compacted was inserted.
    let head = state.session_summary(id).await.unwrap().current_seq + 1;
    let conn = rusqlite::Connection::open(env.root.join("history.db")).unwrap();
    for seq in head + 1..=head + 1 {
        conn.execute(
            "INSERT INTO events (session_id, seq, kind, envelope) VALUES (?1, ?2, 'x', '{}')",
            rusqlite::params![id.to_string(), seq as i64],
        )
        .unwrap();
    }

    let out = state.run_command(id, "model", "b/model-b").await.unwrap();
    assert!(!out.ok, "{out:?}");
    assert!(out.error.unwrap().contains("could not be saved"));
    assert_eq!(state.session_summary(id).await.unwrap().model, "a/model-a");
    assert!(stored(&state, id, "model.changed").await.is_empty());
    // Neither the staged checkpoint nor its notice survives the failed transaction.
    let notices = stored(&state, id, "context.compacted").await;
    assert!(notices.is_empty());
    let meta = state.history().meta(id).await.unwrap().unwrap();
    assert_eq!(meta.model, "a/model-a");
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
    assert_eq!(state.session_summary(id).await.unwrap().model, "a/model-a");
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

// A successful SQLite commit is the linearization point, even when stop wins
// before its callback can install the selection or publish the batch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_after_selection_commit_completes_the_new_identity_before_teardown() {
    let env = env();
    let a = spawn_capturing_provider(vec![answer(SUMMARY)]).await;
    let b = spawn_capturing_provider(vec![answer("new selection still works")]).await;
    let state = two_models(&env, &a, &b, 20_000);
    let path = env.root.join("models.json");
    let mut models: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    models["models"]["b/model-b"]["variants"] = json!({"high":{"reasoning_effort":"high"}});
    models["models"]["b/model-b"]["default_variant"] = json!("high");
    std::fs::write(path, models.to_string()).unwrap();
    std::fs::write(
        env.root.join("cfg/config.json"),
        json!({"agents":{"build":{"model":"a/model-a"},"scribe":{"model":"b/model-b"}}})
            .to_string(),
    )
    .unwrap();
    // This role is not build/plan: its distinct prompt proves the actor swap.
    std::fs::create_dir_all(env.root.join("cfg/agents")).unwrap();
    std::fs::write(
        env.root.join("cfg/agents/scribe.md"),
        "---\nmode: primary\nmodel: b/model-b\n---\nPOSTCOMMIT SCRIBE ROLE\n",
    )
    .unwrap();
    let id = seeded(&state, &env.ws, 8).await;
    // Seed an inactive agent override; /agent must install it with its provenance.
    state
        .history()
        .update_agent_override(
            id,
            "scribe",
            withnav::session::store::AgentOverride {
                model: Some("b/model-b".into()),
                variant: withnav::session::store::AgentVariantOverride::Named("high".into()),
            },
        )
        .await
        .unwrap();
    let sub = state.subscribe(id, None).await.unwrap();
    let barrier = state.history().arm_selection_commit_barrier();
    let mut switching = tokio::spawn({
        let state = state.clone();
        async move { state.run_command(id, "agent", "scribe").await }
    });
    tokio::select! {
        _ = barrier.reached.notified() => {},
        out = &mut switching => panic!("switch returned before postcommit boundary: {out:?}"),
    }
    // Independent SQLite reads prove the write completed before cancellation.
    let committed = state.history().load_for_resume(id).await.unwrap().unwrap();
    assert_eq!(committed.0.primary_agent.as_deref(), Some("scribe"));
    assert_eq!(committed.0.model, "b/model-b");
    assert_eq!(committed.0.variant.as_deref(), Some("high"));
    assert!(committed.2.is_some());
    let loaded = state.history().load(id).await.unwrap().unwrap();
    let operation = loaded
        .envelopes
        .iter()
        .rev()
        .find(|e| e.kind == "model.changed")
        .unwrap()
        .payload["operation_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let (installed, observed) = tokio::sync::oneshot::channel();
    withnav::bus::runtime::observe_selection_install(operation, move |actor| {
        installed
            .send((
                actor.role.as_ref().unwrap().id.to_string(),
                actor.model.clone(),
                serde_json::to_value(&actor.variant).unwrap(),
                actor.compaction.clone(),
                actor.reasoning_floor,
            ))
            .unwrap();
    });
    assert_eq!(state.context(id).await.unwrap().model, "a/model-a");
    let stopping = tokio::spawn({
        let state = state.clone();
        async move { state.close_session(id).await }
    });
    barrier.cancelled().await; // no scheduling assumption or sleep
    barrier.release.notify_one();
    let out = switching.await.unwrap().unwrap();
    assert!(out.ok, "{out:?}");
    let (role, model, variant, checkpoint, floor) = observed.await.unwrap();
    assert_eq!(role, "scribe");
    assert_eq!(model, "model-b");
    assert_eq!(variant["reasoning_effort"], "high");
    assert_eq!(checkpoint, committed.2);
    assert_eq!(Some(floor as u64), committed.0.reasoning_floor);
    stopping.await.unwrap().unwrap();
    let events = collect_until(sub, |e| e.kind == "session.end").await;
    let changed = events.iter().find(|e| e.kind == "model.changed").unwrap();
    assert_eq!(changed.payload["new"]["agent"], "scribe");
    assert_eq!(changed.payload["new"]["model"], "b/model-b");
    assert_eq!(changed.payload["new"]["gateway"], "b");
    assert_eq!(changed.payload["new"]["upstream_id"], "model-b");
    assert_eq!(changed.payload["variant"], "high");
    assert_eq!(changed.payload["new"]["source"], "override");
    assert_eq!(changed.payload["new"]["variant_source"], "override");
    let compacted = events
        .iter()
        .find(|e| e.kind == "context.compacted")
        .unwrap();
    let role = events
        .iter()
        .find(|e| e.kind == "session.agent_changed")
        .unwrap();
    assert_eq!(role.payload["agent"], "scribe");
    assert!(compacted.seq < changed.seq && changed.seq < role.seq);
    let result = events
        .iter()
        .find(|e| {
            e.kind == "command.result" && e.payload["operation_id"] == out.operation_id.to_string()
        })
        .unwrap();
    assert_eq!(result.payload["ok"], true);
    let gauge = events
        .iter()
        .find(|e| e.kind == "context.updated" && e.seq > changed.seq)
        .unwrap();
    assert_eq!(gauge.payload["model"], "b/model-b");
    let end = events.last().unwrap();
    assert!(changed.seq < gauge.seq && gauge.seq < result.seq && result.seq < end.seq);
    let after = state.history().load_for_resume(id).await.unwrap().unwrap();
    assert_eq!(after.0.agent_overrides, committed.0.agent_overrides);
    assert_eq!(after.0.provenance, committed.0.provenance);
    assert_eq!(after.2, committed.2);
    state
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap();
    let live = state.session_summary(id).await.unwrap();
    assert_eq!(live.agent, "scribe");
    assert_eq!(live.model, "b/model-b");
    assert_eq!(live.variant.as_deref(), Some("high"));
    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "continue after stop").await;
    collect_until(sub, |e| e.kind == "turn.end").await;
    let request = body(&b, 0);
    assert_eq!(request["model"], "model-b");
    assert_eq!(request["reasoning_effort"], "high");
    let text = all_text(&request);
    assert!(text.contains("POSTCOMMIT SCRIBE ROLE"));
    assert!(text.contains(SUMMARY));
    assert_eq!(a.bodies().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_switch_preserves_an_existing_checkpoint_and_the_same_live_runtime() {
    let env = env();
    let a = spawn_capturing_provider(vec![
        answer(SUMMARY),
        answer("NEW STAGED SUMMARY"),
        answer("old runtime still works"),
    ])
    .await;
    let b = spawn_capturing_provider(vec![]).await;
    let state = two_models(&env, &a, &b, 20_000);
    let id = seeded(&state, &env.ws, 8).await;
    assert!(state.run_command(id, "compact", "").await.unwrap().ok);
    state.close_session(id).await.unwrap();
    let (_, mut transcript, checkpoint) =
        state.history().load_for_resume(id).await.unwrap().unwrap();
    let checkpoint = checkpoint.expect("valid prior compaction");
    transcript.extend(history(5));
    state
        .history()
        .save_checkpoint(id, transcript, &checkpoint)
        .await
        .unwrap();
    state
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap();
    let before = state.history().load_for_resume(id).await.unwrap().unwrap();
    withnav::agent::compaction::validate(before.2.as_ref().unwrap(), &before.1).unwrap();
    let live = state.session_summary(id).await.unwrap();
    let gauge = serde_json::to_value(state.context(id).await.unwrap()).unwrap();
    let notices = stored(&state, id, "context.compacted").await;
    let head = live.current_seq + 1;
    let conn = rusqlite::Connection::open(env.root.join("history.db")).unwrap();
    conn.execute(
        "INSERT INTO events (session_id, seq, kind, envelope) VALUES (?1, ?2, 'x', '{}')",
        rusqlite::params![id.to_string(), (head + 1) as i64],
    )
    .unwrap();
    let out = state.run_command(id, "model", "b/model-b").await.unwrap();
    assert!(!out.ok, "{out:?}");
    assert!(out.error.unwrap().contains("could not be saved"));
    assert_eq!(
        a.bodies().len(),
        2,
        "the second compaction was actually staged"
    );
    assert!(is_summary(&a.bodies()[1]));
    let after = state.history().load_for_resume(id).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&after.1).unwrap(),
        serde_json::to_value(&before.1).unwrap()
    );
    assert_eq!(after.2, before.2);
    withnav::agent::compaction::validate(after.2.as_ref().unwrap(), &after.1).unwrap();
    assert_eq!(after.0.model, before.0.model);
    assert_eq!(after.0.primary_agent, before.0.primary_agent);
    assert_eq!(after.0.variant, before.0.variant);
    assert_eq!(after.0.variant_override, before.0.variant_override);
    assert_eq!(after.0.model_resolution, before.0.model_resolution);
    assert_eq!(after.0.reasoning_floor, before.0.reasoning_floor);
    assert_eq!(after.0.agent_overrides, before.0.agent_overrides);
    assert_eq!(after.0.provenance, before.0.provenance);
    let current = state.session_summary(id).await.unwrap();
    assert_eq!(
        (current.agent, current.model, current.variant),
        (live.agent, live.model, live.variant)
    );
    assert_eq!(
        serde_json::to_value(state.context(id).await.unwrap()).unwrap(),
        gauge
    );
    assert_eq!(
        serde_json::to_value(stored(&state, id, "context.compacted").await).unwrap(),
        serde_json::to_value(notices).unwrap()
    );
    assert!(stored(&state, id, "model.changed").await.is_empty());
    // No close/resume here: exercise the actor whose staged switch failed.
    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "same runtime continues").await;
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    assert_eq!(
        events
            .iter()
            .rev()
            .find(|e| e.kind == "turn.end")
            .unwrap()
            .payload["outcome"],
        "done"
    );
    assert!(b.bodies().is_empty());
    let request = body(&a, 2);
    assert_eq!(request["model"], "model-a");
    let text = all_text(&request);
    assert!(text.contains(SUMMARY));
    assert!(!text.contains("NEW STAGED SUMMARY"));
    assert!(text.contains("same runtime continues"));
}
