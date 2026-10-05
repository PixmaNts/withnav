//! `/model <alias>` through the service: between turns, after the running
//! turn, refused while something waits or the conversation does not fit, the
//! stored and replayed `model.changed`, and a resume on the new model.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use navai::bus::event::EventEnvelope;
use navai::provider::openai::ChatMessage;
use navai::server::AppState;

mod common;
use common::server::{collect_until, send_text, wait_for, TestServerConfig};
use common::sse::{
    http_sse, spawn_capturing_provider, spawn_gated_provider, sse_body, text_chunk,
    tool_call_chunk, CapturingProvider,
};

const SUMMARY: &str = "Goal: finish the parser.\nNext: run the tests.";

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
    Env {
        _tmp: tmp,
        root,
        ws,
    }
}

fn answer(text: &str) -> Vec<u8> {
    http_sse(&sse_body(&[&text_chunk(text)]))
}

/// Two providers (`a`, `b`), each scripted: `default` is a/model-a, `twin` the
/// same model under another alias, `other` is b/model-b with variants and a
/// default one, and a window of `window_b`.
fn state(
    env: &Env,
    a: &CapturingProvider,
    b: &CapturingProvider,
    window_a: u64,
    window_b: u64,
) -> AppState {
    state_at(env, &a.url, &b.url, window_a, window_b, false)
}

/// [`state`] over two base URLs; `discovery_a` makes provider `a` an OmniRoute
/// one, whose aliases without variants offer one per reported effort tier.
fn state_at(
    env: &Env,
    a_url: &str,
    b_url: &str,
    window_a: u64,
    window_b: u64,
    discovery_a: bool,
) -> AppState {
    let config = TestServerConfig::mock(&env.root, a_url)
        .chat_tools(window_a)
        .config();
    let entry = |provider: &str, model: &str, window: u64| {
        json!({
            "provider": provider, "model": model, "supports_tools": true,
            "context_window": window, "reasoning_history": "preserve"
        })
    };
    let mut other = entry("b", "model-b", window_b);
    other["variants"] = json!({
        "low": {"reasoning_effort": "low"},
        "high": {"reasoning_effort": "high"}
    });
    other["default_variant"] = json!("high");
    let provider = |url: &str| {
        let mut p = json!({"base_url": url, "api_key_env": "NAVAI_TEST_KEY", "headers": {},
               "connect_timeout_ms": 2000, "request_timeout_ms": 600_000});
        if discovery_a && url == a_url {
            p["catalog_format"] = json!("omniroute");
        }
        p
    };
    std::fs::write(
        config.models_path.as_ref().expect("models path"),
        json!({
            "providers": {"a": provider(a_url), "b": provider(b_url)},
            "models": {
                "default": entry("a", "model-a", window_a),
                "twin": entry("a", "model-a", window_a),
                "other": other,
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

fn history(turns: usize, reasoning: bool) -> Vec<ChatMessage> {
    (0..turns)
        .flat_map(|n| {
            [
                ChatMessage::User {
                    provenance: Default::default(),
                    content: format!("request {n}"),
                },
                ChatMessage::Assistant {
                    content: Some(format!("answer {n} {}", "word ".repeat(1200))),
                    reasoning_content: reasoning.then(|| format!("thinking {n}")),
                    tool_calls: vec![],
                },
            ]
        })
        .collect()
}

/// A live session that resumed from a stored conversation of `turns` turns.
async fn seeded(state: &AppState, ws: &Path, turns: usize, reasoning: bool) -> ulid::Ulid {
    seeded_with(state, ws, history(turns, reasoning)).await
}

/// A live session that resumed from the stored `transcript`.
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

async fn turn_done(state: &AppState, id: ulid::Ulid, text: &str) {
    let sub = state.subscribe(id, None).await.unwrap();
    send_text(state, id, text).await;
    collect_until(sub, |e| e.kind == "task.done" || e.kind == "turn.end").await;
}

// Given an idle conversation on `default`
// When /model other runs
// Then the next request goes to the other provider and model, with its default
//      variant and the whole history; the choice is saved and `model.changed`
//      is stored and replayed
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_switch_sends_the_next_request_to_the_new_model_with_its_default_variant() {
    let env = env();
    let a = spawn_capturing_provider(vec![]).await;
    let b = spawn_capturing_provider(vec![answer("from b")]).await;
    let state = state(&env, &a, &b, 128_000, 128_000);
    let id = seeded(&state, &env.ws, 2, true).await;
    let sub = state.subscribe(id, None).await.unwrap();

    let out = state.run_command(id, "model", "other").await.unwrap();
    assert!(out.ok, "{out:?}");
    let data = out.data.clone().unwrap();
    assert_eq!(data["changed"], true);
    assert_eq!(data["applies"], "now");
    assert_eq!(data["variant"], "high");
    assert_eq!(data["previous"]["alias"], "default");
    assert_eq!(data["new"]["model"], "model-b");

    let live = collect_until(sub, |e| e.kind == "model.changed").await;
    let changed = live.iter().find(|e| e.kind == "model.changed").unwrap();
    assert_eq!(changed.payload["reason"], "switch");
    assert_eq!(changed.payload["operation_id"], json!(out.operation_id));
    assert_eq!(changed.payload["previous"]["alias"], "default");
    assert_eq!(changed.payload["previous"]["provider"], "a");
    assert_eq!(changed.payload["new"]["alias"], "other");
    assert_eq!(changed.payload["new"]["provider"], "b");
    assert_eq!(changed.payload["new"]["model"], "model-b");
    assert_eq!(changed.payload["variant"], "high");
    assert_eq!(changed.payload["window"], 128_000);
    assert_eq!(changed.payload["from_message"], 4);
    assert_eq!(changed.payload["revision"], 2);
    assert!(changed.payload["input_budget"].as_u64().unwrap() > 0);

    // Saved: the row, the summary and the command.
    let meta = state.history().meta(id).await.unwrap().unwrap();
    assert_eq!(meta.model, "other");
    assert_eq!(meta.variant.as_deref(), Some("high"));
    assert!(meta.model_resolution.unwrap().contains("model-b"));
    let summary = state.session_summary(id).await.unwrap();
    assert_eq!(summary.model, "other");
    assert_eq!(summary.variant.as_deref(), Some("high"));
    let model = state.run_command(id, "model", "").await.unwrap();
    assert_eq!(model.data.unwrap()["alias"], "other");
    assert_eq!(stored(&state, id, "model.changed").await.len(), 1);

    turn_done(&state, id, "go on").await;
    assert!(a.bodies().is_empty(), "the old provider is not used again");
    let request = body(&b, 0);
    assert_eq!(request["model"], "model-b");
    assert_eq!(request["reasoning_effort"], "high");
    let text = all_text(&request);
    assert!(text.contains("request 0") && text.contains("answer 1") && text.contains("go on"));
    // Another model never sees the first one's reasoning; the transcript keeps it.
    assert!(!provider_saw_reasoning(&request));
}

fn provider_saw_reasoning(request: &Value) -> bool {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m.get("reasoning_content").is_some())
}

// Given a conversation with reasoning in its history
// When /model moves it to another alias of the same model
// Then the reasoning is still replayed
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_is_kept_for_the_same_resolved_identity() {
    let env = env();
    let a = spawn_capturing_provider(vec![answer("ok")]).await;
    let b = spawn_capturing_provider(vec![]).await;
    let state = state(&env, &a, &b, 128_000, 128_000);
    let id = seeded(&state, &env.ws, 2, true).await;

    let out = state.run_command(id, "model", "twin").await.unwrap();
    assert!(out.ok, "{out:?}");
    turn_done(&state, id, "go on").await;
    assert!(provider_saw_reasoning(&body(&a, 0)));
}

// Given a conversation on `default`
// When /model names the current alias
// Then it is a no-op: nothing is saved or published
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_alias_is_a_no_op() {
    let env = env();
    let a = spawn_capturing_provider(vec![]).await;
    let b = spawn_capturing_provider(vec![]).await;
    let state = state(&env, &a, &b, 128_000, 128_000);
    let id = seeded(&state, &env.ws, 1, false).await;

    let out = state.run_command(id, "model", "default").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(out.data.unwrap()["changed"], false);
    assert!(stored(&state, id, "model.changed").await.is_empty());
    assert_eq!(
        state.history().meta(id).await.unwrap().unwrap().model,
        "default"
    );
}

// Given a turn in flight
// When /model other runs
// Then it is accepted for after the turn, the turn finishes on the old model,
//      and the next one goes to the new
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_during_a_turn_applies_after_it() {
    let env = env();
    let release = Arc::new(tokio::sync::Notify::new());
    let a = spawn_gated_provider(vec![answer("old model")], release.clone()).await;
    let b = spawn_capturing_provider(vec![answer("new model")]).await;
    let state = state(&env, &a, &b, 128_000, 128_000);
    let id = seeded(&state, &env.ws, 1, false).await;
    let sub = state.subscribe(id, None).await.unwrap();

    send_text(&state, id, "first").await;
    wait_for("the turn's request", || a.bodies().len() == 1).await;
    let out = state.run_command(id, "model", "other").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(out.data.as_ref().unwrap()["accepted"], true);
    assert_eq!(out.data.as_ref().unwrap()["applies"], "after_turn");
    assert!(b.bodies().is_empty());
    assert_eq!(state.session_summary(id).await.unwrap().model, "default");

    release.notify_one();
    let events = collect_until(sub, |e| e.kind == "model.changed").await;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let end = kinds.iter().position(|k| *k == "turn.end").unwrap();
    let changed = kinds.iter().position(|k| *k == "model.changed").unwrap();
    assert!(end < changed, "after the turn: {kinds:?}");
    assert_eq!(state.session_summary(id).await.unwrap().model, "other");

    turn_done(&state, id, "second").await;
    assert_eq!(a.bodies().len(), 1);
    assert_eq!(body(&b, 0)["model"], "model-b");
    assert!(all_text(&body(&b, 0)).contains("old model"));
}

// Given an agent waiting on its `ask`
// When /model runs
// Then it is refused and nothing changes
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_is_refused_while_an_ask_waits() {
    let env = env();
    let ask = tool_call_chunk("ask_1", "ask", &json!({"question": "which scope?"}));
    let a = spawn_capturing_provider(vec![http_sse(&sse_body(&[&ask]))]).await;
    let b = spawn_capturing_provider(vec![]).await;
    let state = state(&env, &a, &b, 128_000, 128_000);
    let id = seeded(&state, &env.ws, 1, false).await;
    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "go").await;
    collect_until(sub, |e| {
        e.kind == "agent.state" && e.payload["to"] == "yielded"
    })
    .await;

    let out = state.run_command(id, "model", "other").await.unwrap();
    assert!(!out.ok);
    assert!(out.error.unwrap().contains("waiting for"));
    assert_eq!(state.session_summary(id).await.unwrap().model, "default");
    assert!(stored(&state, id, "model.changed").await.is_empty());
}

// Given a gated tool call waiting for a decision
// When /model runs
// Then it is refused and nothing changes
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_is_refused_while_a_permission_request_waits() {
    let env = env();
    let target = env.ws.join("out.txt");
    let write = tool_call_chunk("w1", "write", &json!({"path": target, "content": "hello"}));
    let a = spawn_capturing_provider(vec![http_sse(&sse_body(&[&write]))]).await;
    let b = spawn_capturing_provider(vec![]).await;
    let state = state(&env, &a, &b, 128_000, 128_000);
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "write it").await;
    collect_until(sub, |e| e.kind == "permission.requested").await;

    let out = state.run_command(id, "model", "other").await.unwrap();
    assert!(!out.ok);
    assert_eq!(state.session_summary(id).await.unwrap().model, "default");
    assert!(stored(&state, id, "model.changed").await.is_empty());
}

// Given a /compact whose summary is still in flight
// When /model runs
// Then it is refused as a compaction is running, and goes through afterwards
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_is_refused_during_a_compaction() {
    let env = env();
    let release = Arc::new(tokio::sync::Notify::new());
    let a = spawn_gated_provider(vec![answer(SUMMARY)], release.clone()).await;
    let b = spawn_capturing_provider(vec![]).await;
    let state = state(&env, &a, &b, 20_000, 128_000);
    let id = seeded(&state, &env.ws, 4, false).await;

    let compacting = {
        let state = state.clone();
        tokio::spawn(async move { state.run_command(id, "compact", "").await })
    };
    wait_for("the summary request", || a.bodies().len() == 1).await;
    let refused = state.run_command(id, "model", "other").await.unwrap();
    assert!(!refused.ok, "{refused:?}");
    assert!(refused.error.unwrap().contains("a compaction is running"));

    release.notify_one();
    assert!(compacting.await.unwrap().unwrap().ok);
    let out = state.run_command(id, "model", "other").await.unwrap();
    assert!(out.ok, "{out:?}");
}

// Given a conversation larger than the target's input budget
// When /model runs
// Then it is refused with the /compact hint and nothing changes
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_target_too_small_for_the_conversation_is_refused_and_nothing_changes() {
    let env = env();
    let a = spawn_capturing_provider(vec![]).await;
    let b = spawn_capturing_provider(vec![]).await;
    let state = state(&env, &a, &b, 128_000, 8_000);
    let id = seeded(&state, &env.ws, 4, false).await;
    let before = serde_json::to_value(state.context(id).await.unwrap()).unwrap();

    let out = state.run_command(id, "model", "other").await.unwrap();
    assert!(!out.ok);
    let error = out.error.unwrap();
    assert!(
        error.contains("does not fit other's window and cannot be compacted to fit it"),
        "{error}"
    );
    assert!(error.contains("larger context window"), "{error}");
    assert!(a.bodies().is_empty(), "no summary was asked for");
    let meta = state.history().meta(id).await.unwrap().unwrap();
    assert_eq!(meta.model, "default");
    assert!(!meta.model_resolution.unwrap().contains("model-b"));
    assert!(stored(&state, id, "model.changed").await.is_empty());
    let after = serde_json::to_value(state.context(id).await.unwrap()).unwrap();
    assert_eq!(after, before);
    assert_eq!(state.session_summary(id).await.unwrap().model, "default");
}

// Given a switch to another alias and a stop
// When the conversation is resumed
// Then it runs on the new model with its variant, and `model.changed` replays
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_after_a_switch_uses_the_new_model() {
    let env = env();
    let a = spawn_capturing_provider(vec![]).await;
    let b = spawn_capturing_provider(vec![answer("still b")]).await;
    let state = state(&env, &a, &b, 128_000, 128_000);
    let id = seeded(&state, &env.ws, 2, false).await;
    let out = state.run_command(id, "model", "other").await.unwrap();
    assert!(out.ok, "{out:?}");
    state.close_session(id).await.unwrap();
    state.history().flush().await.unwrap();

    let (resumed, was_resumed) = state
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap();
    assert!(was_resumed);
    assert_eq!(resumed, id);
    let summary = state.session_summary(id).await.unwrap();
    assert_eq!(summary.model, "other");
    assert_eq!(summary.variant.as_deref(), Some("high"));
    assert_eq!(stored(&state, id, "model.changed").await.len(), 1);

    turn_done(&state, id, "after the restart").await;
    assert!(a.bodies().is_empty());
    let request = body(&b, 0);
    assert_eq!(request["model"], "model-b");
    assert_eq!(request["reasoning_effort"], "high");
    assert!(all_text(&request).contains("answer 1"));

    // A second switch counts on from the stored one.
    let back = state.run_command(id, "model", "default").await.unwrap();
    assert!(back.ok, "{back:?}");
    let events = stored(&state, id, "model.changed").await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].payload["revision"], 2);
    assert_eq!(events[1].payload["revision"], 3);
}

/// Reasoning, a tool exchange and a reasoning-only assistant message.
fn rich_history() -> Vec<ChatMessage> {
    use navai::provider::openai::{RequestFunction, RequestToolCall};
    vec![
        ChatMessage::User {
            provenance: Default::default(),
            content: "request 0".into(),
        },
        ChatMessage::Assistant {
            content: None,
            reasoning_content: Some("thinking about the call".into()),
            tool_calls: vec![RequestToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: RequestFunction {
                    name: "read".into(),
                    arguments: "{\"path\":\"a.txt\"}".into(),
                },
            }],
        },
        ChatMessage::Tool {
            tool_call_id: "call_1".into(),
            content: "file contents".into(),
        },
        ChatMessage::Assistant {
            content: Some("read it".into()),
            reasoning_content: Some("thinking about the answer".into()),
            tool_calls: vec![],
        },
        ChatMessage::Assistant {
            content: None,
            reasoning_content: Some("only thoughts".into()),
            tool_calls: vec![],
        },
        ChatMessage::User {
            provenance: Default::default(),
            content: "request 1".into(),
        },
    ]
}

fn assert_projection_without_reasoning(request: &Value) {
    let messages = request["messages"].as_array().unwrap();
    assert!(
        messages
            .iter()
            .all(|m| m.get("reasoning_content").is_none()),
        "{request}"
    );
    // The tool exchange is kept as a pair; the reasoning-only message is gone.
    assert!(messages
        .iter()
        .any(|m| m["role"] == "assistant" && m["tool_calls"][0]["id"] == "call_1"));
    assert!(messages
        .iter()
        .any(|m| m["role"] == "tool" && m["tool_call_id"] == "call_1"));
    assert!(
        !messages.iter().any(|m| m["role"] == "assistant"
            && m.get("tool_calls").is_none()
            && m["content"].as_str().is_none_or(str::is_empty)),
        "{request}"
    );
    assert!(all_text(request).contains("read it"));
}

// Given a conversation with reasoning, a tool exchange and a reasoning-only
//       message, switched to another model and stopped
// When it is resumed
// Then the other model still never receives the reasoning of the first, the
//      tool exchange is intact, and the stored transcript keeps the reasoning
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_keeps_the_reasoning_boundary_of_the_switch() {
    let env = env();
    let a = spawn_capturing_provider(vec![]).await;
    let b = spawn_capturing_provider(vec![answer("one"), answer("two")]).await;
    let state = state(&env, &a, &b, 128_000, 128_000);
    let id = seeded_with(&state, &env.ws, rich_history()).await;
    let out = state.run_command(id, "model", "other").await.unwrap();
    assert!(out.ok, "{out:?}");
    let meta = state.history().meta(id).await.unwrap().unwrap();
    assert_eq!(meta.reasoning_floor, Some(6));

    turn_done(&state, id, "before the stop").await;
    assert_projection_without_reasoning(&body(&b, 0));

    state.close_session(id).await.unwrap();
    state.history().flush().await.unwrap();
    state
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap();
    turn_done(&state, id, "after the stop").await;
    assert!(a.bodies().is_empty());
    assert_projection_without_reasoning(&body(&b, 1));
    assert!(all_text(&body(&b, 1)).contains("before the stop"));

    // The transcript keeps what the first model thought.
    state.close_session(id).await.unwrap();
    state.history().flush().await.unwrap();
    let stored = state.history().load(id).await.unwrap().unwrap().transcript;
    assert!(stored.iter().any(|m| matches!(
        m,
        ChatMessage::Assistant { reasoning_content: Some(r), .. } if r == "only thoughts"
    )));
}

// Given a store that cannot take the `model.changed` event of the switch
// When /model runs
// Then it is refused, the row is unchanged, nothing is announced, the actor
//      keeps talking to the old model, and a later switch goes through
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_whose_event_cannot_be_stored_changes_nothing() {
    let env = env();
    let a = spawn_capturing_provider(vec![answer("still a")]).await;
    let b = spawn_capturing_provider(vec![]).await;
    let state = state(&env, &a, &b, 128_000, 128_000);
    let id = seeded(&state, &env.ws, 2, true).await;
    state.history().flush().await.unwrap();
    let head = state.session_summary(id).await.unwrap().current_seq + 1;

    // A row already sits where the event would be written.
    let conn = rusqlite::Connection::open(env.root.join("history.db")).unwrap();
    conn.execute(
        "INSERT INTO events (session_id, seq, kind, envelope) VALUES (?1, ?2, 'x', '{}')",
        rusqlite::params![id.to_string(), head as i64],
    )
    .unwrap();

    let out = state.run_command(id, "model", "other").await.unwrap();
    assert!(!out.ok, "{out:?}");
    assert!(out.error.unwrap().contains("could not be saved"));
    let meta = state.history().meta(id).await.unwrap().unwrap();
    assert_eq!(meta.model, "default");
    assert_eq!(meta.variant, None);
    assert_eq!(meta.reasoning_floor, None);
    assert!(!meta.model_resolution.unwrap().contains("model-b"));
    assert_eq!(state.session_summary(id).await.unwrap().model, "default");
    assert!(stored(&state, id, "model.changed").await.is_empty());

    turn_done(&state, id, "go on").await;
    assert!(b.bodies().is_empty());
    let request = body(&a, 0);
    assert_eq!(request["model"], "model-a");
    assert!(request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m.get("reasoning_content").is_some()));

    let retry = state.run_command(id, "model", "other").await.unwrap();
    assert!(retry.ok, "{retry:?}");
    assert_eq!(stored(&state, id, "model.changed").await.len(), 1);
}

/// The `command.result` events, in stream order, as (name, ok, operation id).
async fn results(state: &AppState, id: ulid::Ulid) -> Vec<(String, bool, String)> {
    stored(state, id, "command.result")
        .await
        .into_iter()
        .filter(|e| e.payload["name"] != "watchdog")
        .map(|e| {
            (
                e.payload["name"].as_str().unwrap().to_string(),
                e.payload["ok"].as_bool().unwrap(),
                e.payload["operation_id"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

// Given a turn in flight
// When /model other and then /compact are accepted
// Then they run in that order after the turn: the summary is written by the
//      new model, not the old one
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_controls_run_in_the_order_they_were_accepted() {
    let env = env();
    let release = Arc::new(tokio::sync::Notify::new());
    let a = spawn_gated_provider(vec![answer("turn answer")], release.clone()).await;
    let b = spawn_capturing_provider(vec![answer(SUMMARY)]).await;
    let state = state(&env, &a, &b, 20_000, 20_000);
    let id = seeded(&state, &env.ws, 4, false).await;
    let sub = state.subscribe(id, None).await.unwrap();

    send_text(&state, id, "go").await;
    wait_for("the turn's request", || a.bodies().len() == 1).await;
    let switch = state.run_command(id, "model", "other").await.unwrap();
    let compact = state.run_command(id, "compact", "").await.unwrap();
    assert_eq!(switch.data.as_ref().unwrap()["applies"], "after_turn");
    assert_eq!(compact.data.as_ref().unwrap()["applies"], "after_turn");

    release.notify_one();
    collect_until(sub, |e| e.kind == "context.compacted").await;
    assert_eq!(a.bodies().len(), 1, "the old model wrote no summary");
    let summary = body(&b, 0);
    assert_eq!(summary["model"], "model-b");
    assert!(summary.get("tools").is_none());
    assert_eq!(state.session_summary(id).await.unwrap().model, "other");

    let done: Vec<(String, bool, String)> = results(&state, id)
        .await
        .into_iter()
        .filter(|(_, _, op)| {
            *op == switch.operation_id.to_string() || *op == compact.operation_id.to_string()
        })
        .collect();
    let names: Vec<&str> = done.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(names, ["model", "compact", "model", "compact"], "{done:?}");
    assert!(done.iter().all(|(_, ok, _)| *ok));
}

// Given a /compact and a /model accepted while a turn runs
// When the session is stopped before the turn ends
// Then each is told it will not happen, under its own id and in the order it
//      was accepted, before session.end
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_controls_are_cancelled_in_order_before_session_end() {
    let env = env();
    let release = Arc::new(tokio::sync::Notify::new());
    let a = spawn_gated_provider(vec![answer("never read")], release.clone()).await;
    let b = spawn_capturing_provider(vec![]).await;
    let state = state(&env, &a, &b, 20_000, 20_000);
    let id = seeded(&state, &env.ws, 4, false).await;

    send_text(&state, id, "go").await;
    wait_for("the turn's request", || a.bodies().len() == 1).await;
    let compact = state.run_command(id, "compact", "").await.unwrap();
    let switch = state.run_command(id, "model", "other").await.unwrap();

    state.close_session(id).await.unwrap();
    state.history().flush().await.unwrap();

    let all = results(&state, id).await;
    let cancelled: Vec<&(String, bool, String)> = all.iter().filter(|(_, ok, _)| !*ok).collect();
    assert_eq!(cancelled.len(), 2, "{all:?}");
    assert_eq!(
        (cancelled[0].0.as_str(), cancelled[0].2.as_str()),
        ("compact", compact.operation_id.to_string().as_str())
    );
    assert_eq!(
        (cancelled[1].0.as_str(), cancelled[1].2.as_str()),
        ("model", switch.operation_id.to_string().as_str())
    );
    let events = stored(&state, id, "command.result").await;
    let end = stored(&state, id, "session.end").await;
    let last_result = events.iter().map(|e| e.seq).max().unwrap();
    assert!(last_result < end.last().expect("session.end").seq);
    assert!(stored(&state, id, "model.changed").await.is_empty());
    assert_eq!(
        state.history().meta(id).await.unwrap().unwrap().model,
        "default"
    );
    assert!(b.bodies().is_empty());
}

// Given a history whose reasoning alone overruns the target's budget
// When /model moves it to another model, which never sees that reasoning
// Then the switch is judged on what the target would receive: it goes through,
//      and the gauge afterwards counts the stripped history
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fit_check_measures_the_projection_the_target_will_receive() {
    let env = env();
    let a = spawn_capturing_provider(vec![]).await;
    let b = spawn_capturing_provider(vec![]).await;
    let state = state(&env, &a, &b, 128_000, 20_000);
    let heavy: Vec<ChatMessage> = (0..4)
        .flat_map(|n| {
            [
                ChatMessage::User {
                    provenance: Default::default(),
                    content: format!("request {n}"),
                },
                ChatMessage::Assistant {
                    content: Some(format!("answer {n}")),
                    reasoning_content: Some("think ".repeat(2000)),
                    tool_calls: vec![],
                },
            ]
        })
        .collect();
    let id = seeded_with(&state, &env.ws, heavy).await;
    let before = serde_json::to_value(state.context(id).await.unwrap()).unwrap();
    let budget_b = 20_000 - 5_000 - 4_096;
    assert!(
        before["estimate_total"].as_u64().unwrap() > budget_b,
        "the source projection must not fit: {before}"
    );

    let out = state.run_command(id, "model", "other").await.unwrap();
    assert!(out.ok, "{out:?}");
    let after = serde_json::to_value(state.context(id).await.unwrap()).unwrap();
    assert!(
        after["estimate_total"].as_u64().unwrap() <= budget_b,
        "{after}"
    );
    assert_eq!(after["alias"], "other");
}

// Given a session whose variant offer was unknown, a turn in flight with a
//       /model accepted for after it, and a /variant waiting for the
//       provider's model list
// When the turn ends and the switch lands before the list arrives
// Then the variant command is refused, and neither the old offer nor the old
//      variant is installed on the new model
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_variant_read_against_the_old_model_is_not_installed_after_a_switch() {
    let env = env();
    let server = wiremock::MockServer::start().await;
    let b = spawn_capturing_provider(vec![]).await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/models"))
        .respond_with(wiremock::ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let state = state_at(
        &env,
        &format!("{}/v1", server.uri()),
        &b.url,
        128_000,
        128_000,
        true,
    );
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    // The provider comes back: the model list is slow, and so is the turn,
    // but less. A failed lookup is not repeated for a few seconds.
    server.reset().await;
    let delayed = |ms: u64| std::time::Duration::from_millis(ms);
    wiremock::Mock::given(wiremock::matchers::path("/v1/models"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_delay(delayed(1500))
                .set_body_json(json!({"data": [{
                    "id": "model-a",
                    "context_length": 128_000,
                    "capabilities": {"effort_tiers": ["low", "high"]}
                }]})),
        )
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_delay(delayed(400))
                .set_body_raw(
                    sse_body(&[&text_chunk("turn answer")]).into_bytes(),
                    "text/event-stream",
                ),
        )
        .mount(&server)
        .await;
    tokio::time::sleep(delayed(5300)).await;

    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "go").await;
    wait_for_async("the turn's request", || async {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .any(|r| r.url.path() == "/v1/chat/completions")
    })
    .await;
    let accepted = state.run_command(id, "model", "other").await.unwrap();
    assert_eq!(accepted.data.as_ref().unwrap()["applies"], "after_turn");
    let refused = state.run_command(id, "variant", "high").await.unwrap();
    // (the command above returns only once the list arrived: the switch,
    // deferred behind the 400 ms turn, has landed by then)
    collect_until(sub, |e| e.kind == "model.changed").await;
    assert_eq!(state.session_summary(id).await.unwrap().model, "other");

    assert!(!refused.ok, "{refused:?}");
    let error = refused.error.unwrap();
    assert!(error.contains("switched"), "{error}");
    let meta = state.history().meta(id).await.unwrap().unwrap();
    assert_eq!(meta.model, "other");
    // The new model keeps its own default variant (none), not a choice read
    // against the old model's offer.
    assert_eq!(meta.variant.as_deref(), Some("high"));
    assert_eq!(
        meta.variant_override.as_deref(),
        Some(r#"{"reasoning_effort":"high"}"#)
    );
    let shown = state.run_command(id, "variant", "").await.unwrap();
    assert_eq!(shown.data.unwrap()["offered"], json!(["high", "low"]));
}

async fn wait_for_async<F, Fut>(what: &str, cond: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..500 {
        if cond().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}
