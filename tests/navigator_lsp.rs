//! Navigator LSP integration tests: drive the real server over an in-memory
//! duplex stream with real JSON-RPC framed messages (Content-Length headers),
//! a scripted mock commenter, and tokio's paused clock.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use rust_agent::navigator::{self, NavigatorDeps};

mod common;
use common::navigator::{
    spawn_server, LspClient, RemarkAnswer as Answer, ScriptedCommenter as MockCommenter,
    ScriptedFim, ScriptedReflexContext,
};

const URI: &str = "file:///ws/src/main.rs";

fn base_text() -> String {
    ["fn main() {", "    println!(\"hello\");", "}"].join("\n")
}
/// >= 3 changed lines relative to base.
fn big_edit() -> String {
    [
        "fn main() {",
        "    let x = 1;",
        "    let y = 2;",
        "    println!(\"{x}-{y}\");",
        "}",
    ]
    .join("\n")
}

// ---------- tests ----------

/// After a 5-line change and 3 s idle, one navigator/remark arrives.
#[tokio::test(start_paused = true)]
async fn idle_after_big_edit_emits_remark() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let mock = MockCommenter::new(vec![Answer::Remark(1, "careful: x is unused")]);
    let mut client = spawn_server(NavigatorDeps::new(Arc::new(mock.clone()), log_path.clone()));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;

    tokio::time::advance(Duration::from_millis(3001)).await;
    let mut seen = Vec::new();
    let remark = client.expect("navigator/remark", &mut seen).await;
    let params = &remark["params"];
    assert_eq!(params["uri"], URI);
    assert_eq!(params["line"], 1);
    assert_eq!(params["text"], "careful: x is unused");
    assert!(params["id"].as_str().is_some());

    let records = std::fs::read_to_string(log_path).unwrap();
    assert!(records.contains("\"kind\":\"remark\""));
    assert!(records.contains("\"kind\":\"trigger\""));
}

/// A 1-line change never triggers (skip: too_few_changed_lines).
#[tokio::test(start_paused = true)]
async fn one_line_change_never_triggers() {
    let mock = MockCommenter::new(vec![Answer::Remark(0, "should not happen")]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock.clone()),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    // only one line changed
    client
        .did_change(URI, "fn main() {\n    println!(\"hi\");\n}", 2)
        .await;

    tokio::time::advance(Duration::from_millis(10_000)).await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(mock.calls(), 0, "commenter must not be called");
}

/// A diagnostics event no trigger listens to logs nothing; a too-small
/// change logs a light record, without the rendered state.
#[tokio::test(start_paused = true)]
async fn skips_logs_light_or_not_at_all() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let mock = MockCommenter::new(vec![Answer::Silent]);
    let mut client = spawn_server(NavigatorDeps::new(Arc::new(mock), log_path.clone()));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client
        .notify("navigator/diagnostics", json!({"uri": URI, "items": []}))
        .await;
    client
        .did_change(URI, "fn main() {\n    println!(\"hi\");\n}", 2)
        .await;
    client.did_save(URI).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = std::fs::read_to_string(&log_path).unwrap_or_default();
    let triggers: Vec<Value> = records
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["kind"] == "trigger")
        .collect();
    assert_eq!(triggers.len(), 1, "{records}");
    assert_eq!(triggers[0]["event"], "save");
    assert_eq!(triggers[0]["reason"], "too_few_changed_lines");
    assert!(triggers[0].get("template_inputs").is_none());
    assert!(triggers[0].get("rendered_state").is_none());
}

/// Cooldown suppresses a second remark on the same file.
#[tokio::test(start_paused = true)]
async fn cooldown_suppresses_second_remark() {
    let mock = MockCommenter::new(vec![
        Answer::Remark(0, "first"),
        Answer::Remark(0, "second"),
    ]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock.clone()),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    let mut seen = Vec::new();
    let first = client.expect("navigator/remark", &mut seen).await;
    assert_eq!(first["params"]["text"], "first");
    client.expect_status("idle").await;

    // A second big edit within the 45 s cooldown: the idle timer fires but
    // the policy skips, so no second commenter call happens.
    let bigger = format!("{}\n// a\n// b\n// c\n", big_edit());
    client.did_change(URI, &bigger, 3).await;
    tokio::time::advance(Duration::from_millis(5000)).await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(mock.calls(), 1, "cooldown must suppress the second call");

    // After the cooldown elapses, a new edit fires again.
    tokio::time::advance(Duration::from_millis(45_000)).await;
    let bigger2 = format!("{}\n// d\n// e\n// f\n", big_edit());
    client.did_change(URI, &bigger2, 4).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    let second = client.expect("navigator/remark", &mut seen).await;
    assert_eq!(second["params"]["text"], "second");
}

/// Silent answer emits no remark.
#[tokio::test(start_paused = true)]
async fn silent_answer_emits_no_remark() {
    let mock = MockCommenter::new(vec![Answer::Silent]);
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let mut client = spawn_server(NavigatorDeps::new(Arc::new(mock), log_path.clone()));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    // Silence: the "idle" status after "thinking" is the sync point.
    client.expect_status("idle").await;

    let records = std::fs::read_to_string(log_path).unwrap();
    assert!(records.contains("\"kind\":\"silent\""));
    assert!(!records.contains("\"kind\":\"remark\""));
}

/// Commenter error emits navigator/status error and the server stays alive.
#[tokio::test(start_paused = true)]
async fn commenter_error_emits_status_error_and_survives() {
    let mock = MockCommenter::new(vec![Answer::Fail("provider exploded")]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    let status = client.expect_status("error").await;
    assert_eq!(status["params"]["message"], "provider exploded");

    // The server is still alive: a request answers.
    client
        .send(&json!({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": null}))
        .await;
    let resp = client.recv().await;
    assert_eq!(resp["id"], 99);
    assert!(resp.get("result").is_some());
}

/// Typing again before the mock answers yields no remark (in-flight cancel).
#[tokio::test(start_paused = true)]
async fn retyping_cancels_in_flight_request() {
    let gate = Arc::new(CancellationToken::new());
    // The gated call holds "stale"; any later call answers Silent.
    let mock = MockCommenter::gated(
        vec![Answer::Remark(0, "stale"), Answer::Silent],
        gate.clone(),
    );
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let mut client = spawn_server(NavigatorDeps::new(Arc::new(mock), log_path.clone()));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    // "thinking" proves the first (gated) call is in flight.
    client.expect_status("thinking").await;

    // The user types again: the in-flight call must be cancelled.
    client
        .did_change(URI, &format!("{}\n// tail\n", big_edit()), 3)
        .await;
    tokio::time::sleep(Duration::from_millis(1)).await;

    // Release the gate: the cancelled call must not emit "stale". A later
    // idle window may legitimately fire a fresh call (Silent).
    gate.cancel();
    tokio::time::advance(Duration::from_millis(10_000)).await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    let msgs = client.collect_for(Duration::from_millis(100)).await;
    for m in &msgs {
        assert_ne!(
            m["params"]["text"].as_str().unwrap_or(""),
            "stale",
            "cancelled answer leaked: {m}"
        );
    }
    let records = std::fs::read_to_string(log_path).unwrap();
    assert!(
        !records.contains("\"text\":\"stale\""),
        "cancelled answer must not be logged"
    );
}

/// Save fires the trigger without waiting for the idle timer.
#[tokio::test(start_paused = true)]
async fn save_triggers_immediately() {
    let mock = MockCommenter::new(vec![Answer::Remark(2, "watch the unwrap")]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    // No time advance: save must trigger on its own.
    client.did_save(URI).await;
    let mut seen = Vec::new();
    let remark = client.expect("navigator/remark", &mut seen).await;
    assert_eq!(remark["params"]["text"], "watch the unwrap");
}

/// Feedback is written to the log.
#[tokio::test(start_paused = true)]
async fn feedback_is_written_to_log() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let mut client = spawn_server(NavigatorDeps::new(Arc::new(mock), log_path.clone()));

    client.initialize(json!({})).await;
    client
        .notify(
            "navigator/feedback",
            json!({"id": "abc123", "verdict": "useful"}),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = std::fs::read_to_string(log_path).unwrap();
    assert!(records.contains("\"kind\":\"feedback\""));
    assert!(records.contains("\"verdict\":\"useful\""));
    assert!(records.contains("\"id\":\"abc123\""));
}

/// didClose clears state and notifies navigator/clear.
#[tokio::test(start_paused = true)]
async fn did_close_clears_remark() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client
        .notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": URI}}),
        )
        .await;
    let mut seen = Vec::new();
    let clear = client.expect("navigator/clear", &mut seen).await;
    assert_eq!(clear["params"]["uri"], URI);
}

/// The pipeline's configured pre-filter, rather than the prototype's LSP
/// minChangedLines setting, controls whether the instinct runs.
#[tokio::test(start_paused = true)]
async fn config_update_changes_idle_threshold() {
    let mock = MockCommenter::new(vec![Answer::Remark(0, "cfg-driven")]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock),
        tmp.path().join("nav.jsonl"),
    ));

    client
        .initialize(json!({"idleMs": 1000, "minChangedLines": 1, "cooldownMs": 60000}))
        .await;
    client.did_open(URI, &base_text(), 1).await;
    // The built-in pipeline still requires at least three changed lines.
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    let mut seen = Vec::new();
    let remark = client.expect("navigator/remark", &mut seen).await;
    assert_eq!(remark["params"]["text"], "cfg-driven");
}

/// Disabled config suppresses everything.
#[tokio::test(start_paused = true)]
async fn disabled_config_suppresses_evaluation() {
    let mock = MockCommenter::new(vec![Answer::Remark(0, "nope")]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock.clone()),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({"enabled": false})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(10_000)).await;
    client.did_save(URI).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(mock.calls(), 0);
}

/// navigator/intent and navigator/diagnostics are accepted and stored
/// (observable through the log's remark after a trigger).
#[tokio::test(start_paused = true)]
async fn intent_and_diagnostics_are_accepted() {
    let mock = MockCommenter::new(vec![Answer::Silent]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({})).await;
    client
        .notify("navigator/intent", json!({"text": "fix the parser"}))
        .await;
    client
        .notify(
            "navigator/diagnostics",
            json!({
                "uri": URI,
                "items": [{"line": 1, "severity": "error", "message": "mismatched types"}]
            }),
        )
        .await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Server must still respond to a request (custom notifications didn't
    // break it).
    client
        .send(&json!({"jsonrpc": "2.0", "id": 7, "method": "shutdown", "params": null}))
        .await;
    let resp = client.recv().await;
    assert!(resp.get("result").is_some(), "server broken: {resp}");
}

/// navigator/inlineCompletion answers with an item and request id from the
/// reflex service, including the result range.
#[tokio::test(start_paused = true)]
async fn inline_completion_answers_empty_items() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let deps = NavigatorDeps::new(Arc::new(mock), tmp.path().join("nav.jsonl"))
        .with_fim(Arc::new(ScriptedFim::fixed("hello")))
        .with_model_ids(Some("mercury-edit-2".into()), None, None);
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "navigator/inlineCompletion",
            "params": {"uri": URI, "version": 1, "line": 0, "character": 4}
        }))
        .await;
    tokio::time::advance(Duration::from_millis(151)).await;
    let resp = client.recv().await;
    assert_eq!(resp["id"], 21);
    assert_eq!(resp["result"]["items"][0]["text"], "hello");
    assert!(resp["result"]["requestId"].as_str().is_some());
}

/// A request superseded during its debounce never reaches the model, so only
/// the request that did is logged as a reflex call.
#[tokio::test(start_paused = true)]
async fn a_debounced_inline_completion_is_not_logged_as_a_model_call() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let fim = ScriptedFim::fixed("hello");
    let deps = NavigatorDeps::new(Arc::new(MockCommenter::new(vec![])), log_path.clone())
        .with_fim(Arc::new(fim.clone()))
        .with_model_ids(Some("mercury-edit-2".into()), None, None);
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    for id in [51, 52] {
        client
            .send(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "navigator/inlineCompletion",
                "params": {"uri": URI, "version": 1, "line": 0, "character": 4}
            }))
            .await;
        tokio::time::advance(Duration::from_millis(50)).await;
    }
    tokio::time::advance(Duration::from_millis(151)).await;
    let mut answered = 0;
    while answered < 2 {
        if client.recv().await["id"].is_u64() {
            answered += 1;
        }
    }

    assert_eq!(fim.calls(), 1, "only the last request called the model");
    let records = std::fs::read_to_string(&log_path).unwrap();
    let reflex_calls = records
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["kind"] == "call" && v["layer"] == "reflex")
        .count();
    assert_eq!(reflex_calls, 1, "{records}");
}

/// navigator/reflexShown is accepted and logged as a reflex_shown record.
#[tokio::test(start_paused = true)]
async fn reflex_shown_is_logged() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let mut client = spawn_server(NavigatorDeps::new(Arc::new(mock), log_path.clone()));

    client.initialize(json!({})).await;
    client
        .notify(
            "navigator/reflexShown",
            json!({"requestId": "req1", "shownAtMs": 1759000000123u64}),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = std::fs::read_to_string(log_path).unwrap();
    assert!(records.contains("\"kind\":\"reflex_shown\""));
    assert!(records.contains("\"request_id\":\"req1\""));
    assert!(records.contains("\"shown_at_ms\":1759000000123"));
}

/// Trigger records carry all pipeline-era replay fields as well as the
/// requested and delivered outcome.
#[tokio::test(start_paused = true)]
async fn trigger_records_carry_replay_fields() {
    let mock = MockCommenter::new(vec![Answer::Remark(1, "careful")]);
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let mut client = spawn_server(NavigatorDeps::new(Arc::new(mock), log_path.clone()));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    let mut seen = Vec::new();
    let _ = client.expect("navigator/remark", &mut seen).await;

    let records = std::fs::read_to_string(log_path).unwrap();
    let trigger: Value = records
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .find(|v: &Value| v["kind"] == "trigger")
        .expect("a trigger record");
    assert!(trigger["trigger_id"].as_str().is_some());
    assert!(trigger["run_id"].as_str().is_some());
    assert_eq!(trigger["uri"], URI);
    assert_eq!(trigger["version"], 2);
    assert_eq!(trigger["event"], "idle");
    assert_eq!(trigger["prefilter"]["outcome"], "pass");
    assert_eq!(trigger["prefilter"]["inputs"]["changed_lines"], 4);
    assert_eq!(trigger["requested_action"], "remark");
    assert_eq!(trigger["delivered"], "remark");
    assert!(trigger["remark_id"].as_str().is_some());
    assert!(trigger["pipeline_hash"].as_str().is_some());
    assert!(trigger["template_inputs"].is_object());
    assert!(trigger["rendered_state"].as_str().is_some());
    assert!(trigger["questions"].is_object());
    assert!(trigger["answers"].is_object());
    assert_eq!(trigger["branch"], 1);
}

/// With `server_info` in the deps, `initialized` delivers
/// `navigator/serverInfo` with exactly those values: the extension learns
/// the cognition chat server's port and bearer token.
#[tokio::test(start_paused = true)]
async fn server_info_arrives_after_initialized_with_the_configured_values() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let deps = NavigatorDeps::new(Arc::new(mock), tmp.path().join("nav.jsonl")).with_server_info(
        navigator::ServerInfoParams {
            port: 45678,
            token: "tok-123".to_string(),
        },
    );
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    let mut seen = Vec::new();
    let info = client.expect("navigator/serverInfo", &mut seen).await;
    assert_eq!(info["params"]["port"], 45678);
    assert_eq!(info["params"]["token"], "tok-123");
}

// ---------- wake_cognition delivery ----------

/// A feed whose active conversation is a scripted poster: records every
/// text posted and answers Ok. Active means one driver message was
/// observed for a registered session.
async fn feed_with_active_session() -> (
    navigator::ConversationFeed,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    let (feed, posted, _session_id) = feed_with_session().await;
    (feed, posted)
}

/// [`feed_with_active_session`], also returning the session's id so a test
/// can feed it more events.
async fn feed_with_session() -> (
    navigator::ConversationFeed,
    Arc<std::sync::Mutex<Vec<String>>>,
    ulid::Ulid,
) {
    use navigator::conversation::SessionPost;
    let feed = navigator::ConversationFeed::new();
    let posted = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = posted.clone();
    let post: SessionPost = Arc::new(move |text| {
        sink.lock().unwrap().push(text);
        Box::pin(async { Ok::<(), String>(()) })
    });
    let session_id = ulid::Ulid::new();
    feed.register_session(session_id, post);
    feed.observe(&rust_agent::bus::event::EventEnvelope::new(
        session_id,
        1,
        rust_agent::bus::event::EventKind::MessageReceived,
        serde_json::json!({
            "command_id": ulid::Ulid::new().to_string(),
            "source": "user:http",
            "target": "agent:main",
            "mode": "next-turn",
            "content": "add an auth remove command",
        }),
    ))
    .await;
    (feed, posted, session_id)
}

/// With a feed whose active session exists and an instinct that requests
/// `wake_cognition`, the branch posts ONCE through the feed and the
/// trigger record is delivered `wake` with the session id.
#[tokio::test(start_paused = true)]
async fn wake_cognition_posts_once_and_logs_wake() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let (feed, posted) = feed_with_active_session().await;
    // The intent record must exist for the built-in wake branch.
    let ws = tmp.path();
    std::fs::create_dir_all(ws.join(".navigator")).unwrap();
    std::fs::write(
        ws.join(".navigator/intent.md"),
        "## Intent\nadd an auth remove command",
    )
    .unwrap();

    let deps = NavigatorDeps::new(Arc::new(mock), ws.join("nav.jsonl"))
        .with_conversation(feed)
        .with_dirs(Some(ws.to_path_buf()), None);
    // spawn_server wires its ScriptedDecision, whose noul answers (0.9)
    // fire the built-in drift branch when an intent exists.
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    client.did_save(URI).await;
    // The wake path ends with the idle status.
    client.expect_status("idle").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let posted = posted.lock().unwrap().clone();
    assert_eq!(posted.len(), 1, "exactly one wake posted");
    let message = &posted[0];
    assert!(message.contains(URI), "names the file: {message}");
    assert!(
        message.contains("auth remove command"),
        "carries the intent: {message}"
    );

    let records = std::fs::read_to_string(ws.join("nav.jsonl")).unwrap();
    let trigger: Value = records
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .find(|v: &Value| v["kind"] == "trigger")
        .expect("a trigger record");
    assert_eq!(trigger["requested_action"], "wake_cognition");
    assert_eq!(trigger["delivered"], "wake");
    assert!(trigger["session"].as_str().is_some(), "session id logged");
}

/// Without a feed (or with no active conversation), the wake branch keeps
/// today's behaviour: nothing posted, delivered `none`, reason
/// `cognition_unavailable`.
#[tokio::test(start_paused = true)]
async fn wake_cognition_without_a_feed_logs_cognition_unavailable() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    std::fs::create_dir_all(ws.join(".navigator")).unwrap();
    std::fs::write(
        ws.join(".navigator/intent.md"),
        "## Intent\nadd an auth remove command",
    )
    .unwrap();
    let feed = navigator::ConversationFeed::new(); // registered nothing: no active session
    let deps = NavigatorDeps::new(Arc::new(mock), ws.join("nav.jsonl"))
        .with_conversation(feed)
        .with_dirs(Some(ws.to_path_buf()), None);
    // spawn_server wires its ScriptedDecision, whose noul answers (0.9)
    // fire the built-in drift branch when an intent exists.
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    client.did_save(URI).await;
    client.expect_status("idle").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = std::fs::read_to_string(ws.join("nav.jsonl")).unwrap();
    let trigger: Value = records
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .find(|v: &Value| v["kind"] == "trigger")
        .expect("a trigger record");
    assert_eq!(trigger["delivered"], "none");
    assert_eq!(trigger["reason"], "cognition_unavailable");
}

/// With a `ConversationFeed` holding a digest (an active session with
/// driver messages), the logged trigger's `template_inputs.conversation`
/// carries that digest: the instinct and the metacognition see what the
/// driver and the cognition agreed.
#[tokio::test(start_paused = true)]
async fn trigger_template_inputs_carry_the_conversation_digest() {
    let mock = MockCommenter::new(vec![Answer::Silent]);
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let (feed, _posted) = feed_with_active_session().await;
    let deps = NavigatorDeps::new(Arc::new(mock), log_path.clone()).with_conversation(feed);
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    client.expect_status("idle").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = std::fs::read_to_string(log_path).unwrap();
    let trigger: Value = records
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .find(|v: &Value| v["kind"] == "trigger")
        .expect("a trigger record");
    let conversation = trigger["template_inputs"]["conversation"]
        .as_str()
        .expect("conversation template input");
    assert!(
        conversation.contains("driver: add an auth remove command"),
        "the digest: {conversation}"
    );
}

/// Without `server_info` in the deps, NO `navigator/serverInfo` ever leaves
/// the server (the chat stays unannounced when its server failed to bind).
#[tokio::test(start_paused = true)]
async fn without_server_info_no_notification_is_sent() {
    let mock = MockCommenter::new(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let mut client = spawn_server(NavigatorDeps::new(
        Arc::new(mock),
        tmp.path().join("nav.jsonl"),
    ));

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_change(URI, &big_edit(), 2).await;
    tokio::time::advance(Duration::from_millis(3001)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let msgs = client.collect_for(Duration::from_millis(100)).await;
    assert!(
        msgs.iter()
            .all(|m| m.get("method").and_then(|m| m.as_str()) != Some("navigator/serverInfo")),
        "no serverInfo without server_info deps: {msgs:?}"
    );
}

impl LspClient {
    /// One inline completion at the document start; returns once answered.
    async fn complete(&mut self, id: u64, version: i32) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "navigator/inlineCompletion",
            "params": {"uri": URI, "version": version, "line": 0, "character": 4}
        }))
        .await;
        tokio::time::advance(Duration::from_millis(151)).await;
        // Skip notifications (idle-timer status) until the response.
        loop {
            let resp = self.recv().await;
            if resp["id"] == id {
                return;
            }
        }
    }
}

fn driver_message(
    session_id: ulid::Ulid,
    seq: u64,
    content: &str,
) -> rust_agent::bus::event::EventEnvelope {
    rust_agent::bus::event::EventEnvelope::new(
        session_id,
        seq,
        rust_agent::bus::event::EventKind::MessageReceived,
        json!({
            "command_id": ulid::Ulid::new().to_string(),
            "source": "user:http",
            "target": "agent:main",
            "mode": "next-turn",
            "content": content,
        }),
    )
}

/// The first completion serves the intent summary; once the refresh has
/// completed the next one carries the refreshed text. Inside a minute with
/// unchanged inputs the model is not called again; a changed conversation
/// digest after a minute refreshes again; a changed digest inside a minute
/// does not.
#[tokio::test(start_paused = true)]
async fn reflex_context_is_refreshed_by_the_metacognition() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let feed = navigator::ConversationFeed::new();
    let session_id = ulid::Ulid::new();
    feed.register_session(
        session_id,
        Arc::new(|_| Box::pin(async { Ok::<(), String>(()) })),
    );
    feed.observe(&driver_message(session_id, 1, "use anyhow for errors"))
        .await;

    let model = ScriptedReflexContext::new(vec![Ok("REFRESHED-ONE"), Ok("REFRESHED-TWO")]);
    let fim = ScriptedFim::fixed("x");
    let deps = NavigatorDeps::new(Arc::new(MockCommenter::new(vec![])), log_path.clone())
        .with_fim(Arc::new(fim.clone()))
        .with_reflex_context(Arc::new(model.clone()))
        .with_model_ids(None, None, Some("fast-model".into()))
        .with_conversation(feed.clone());
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    client
        .notify("navigator/intent", json!({"text": "fix the parser"}))
        .await;
    client.did_open(URI, &base_text(), 1).await;

    // 1. Intent summary first; the refresh starts in the background.
    client.complete(21, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(model.calls(), 1);
    {
        let prompts = fim.prompts();
        assert!(prompts[0].contains("fix the parser"), "{}", prompts[0]);
        assert!(!prompts[0].contains("REFRESHED-ONE"));
    }
    {
        let requests = model.requests();
        assert_eq!(requests[0].intent, "fix the parser");
        assert!(requests[0].conversation.contains("use anyhow for errors"));
        assert_eq!(requests[0].file, URI);
        assert_eq!(requests[0].language_id, "rust");
        assert!(requests[0].excerpt.contains("   1 | fn main() {"));
    }

    // 2. The refreshed text is served; same inputs inside a minute: no call.
    client.complete(22, 1).await;
    assert!(fim.prompts()[1].contains("REFRESHED-ONE"));
    assert_eq!(model.calls(), 1);

    // 3. A changed digest inside a minute does not refresh.
    feed.observe(&driver_message(session_id, 2, "and add a --force flag"))
        .await;
    client.complete(23, 1).await;
    assert_eq!(model.calls(), 1);

    // 4. After a minute, with the digest changed, it refreshes.
    tokio::time::advance(Duration::from_secs(61)).await;
    client.complete(24, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(model.calls(), 2);
    assert!(model.requests()[1]
        .conversation
        .contains("and add a --force flag"));
    client.complete(25, 1).await;
    assert!(fim.prompts()[4].contains("REFRESHED-TWO"));

    // 5. Unchanged inputs after another minute: still no call.
    tokio::time::advance(Duration::from_secs(61)).await;
    client.complete(26, 1).await;
    assert_eq!(model.calls(), 2);

    // The refresh calls are logged against the metacognition.
    let records = std::fs::read_to_string(log_path).unwrap();
    let calls: Vec<Value> = records
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|v: &Value| v["kind"] == "call" && v["purpose"] == "reflex_context")
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["layer"], "metacognition");
    assert_eq!(calls[0]["model"], "fast-model");
}

/// A failed refresh keeps the previous context (here: the intent summary),
/// logs the error, and is retried only after the minute.
#[tokio::test(start_paused = true)]
async fn reflex_context_refresh_failure_keeps_the_previous_context() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let model = ScriptedReflexContext::new(vec![Err("boom"), Ok("RECOVERED")]);
    let fim = ScriptedFim::fixed("x");
    let deps = NavigatorDeps::new(Arc::new(MockCommenter::new(vec![])), log_path.clone())
        .with_fim(Arc::new(fim.clone()))
        .with_reflex_context(Arc::new(model.clone()));
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    client
        .notify("navigator/intent", json!({"text": "fix the parser"}))
        .await;
    client.did_open(URI, &base_text(), 1).await;

    client.complete(31, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.complete(32, 1).await;
    {
        let prompts = fim.prompts();
        assert!(prompts[1].contains("fix the parser"), "{}", prompts[1]);
    }
    assert_eq!(model.calls(), 1, "no retry inside the minute");
    let records = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        records.contains("reflex context refresh") && records.contains("boom"),
        "the error is logged: {records}"
    );

    // After the minute the inputs are still "never computed": retried.
    tokio::time::advance(Duration::from_secs(61)).await;
    client.complete(33, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.complete(34, 1).await;
    assert_eq!(model.calls(), 2);
    assert!(fim.prompts()[3].contains("RECOVERED"));
}

// ---------- conversation trigger ----------

fn session_event(
    session_id: ulid::Ulid,
    kind: rust_agent::bus::event::EventKind,
    payload: Value,
) -> rust_agent::bus::event::EventEnvelope {
    rust_agent::bus::event::EventEnvelope::new(session_id, 0, kind, payload)
}

fn message_event(session_id: ulid::Ulid, source: &str) -> rust_agent::bus::event::EventEnvelope {
    session_event(
        session_id,
        rust_agent::bus::event::EventKind::MessageReceived,
        json!({
            "command_id": ulid::Ulid::new().to_string(),
            "source": source,
            "target": "agent:main",
            "mode": "next-turn",
            "content": "keep the parser, drop the cache",
        }),
    )
}

fn turn_end_event(session_id: ulid::Ulid) -> rust_agent::bus::event::EventEnvelope {
    session_event(
        session_id,
        rust_agent::bus::event::EventKind::TurnEnd,
        json!({
            "turn_id": ulid::Ulid::new().to_string(),
            "actor_id": "agent:main",
            "outcome": "done",
            "duration_ms": 1,
        }),
    )
}

fn trigger_records(log_path: &std::path::Path, event: &str) -> Vec<Value> {
    std::fs::read_to_string(log_path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["kind"] == "trigger" && v["event"] == event)
        .collect()
}

/// A driver turn ending evaluates the pipeline with event `conversation` on
/// the focused document, and the built-in remarks on a contradiction. A
/// second driver turn within the cooldown is skipped.
#[tokio::test(start_paused = true)]
async fn driver_turn_end_triggers_one_conversation_evaluation_and_the_cooldown_applies() {
    let mock = MockCommenter::new(vec![Answer::Remark(2, "the cache is back")]);
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let (feed, _posted, session_id) = feed_with_session().await;
    let deps = NavigatorDeps::new(Arc::new(mock.clone()), log_path.clone())
        .with_conversation(feed.clone());
    let mut client = spawn_server(deps);

    client.initialize(json!({})).await;
    // Unchanged document: min_changed_lines would skip it for a save.
    client.did_open(URI, &base_text(), 1).await;
    // Let the server take the notification in before the turn ends.
    tokio::time::sleep(Duration::from_millis(50)).await;

    feed.observe(&turn_end_event(session_id)).await;
    let mut seen = Vec::new();
    let remark = client.expect("navigator/remark", &mut seen).await;
    assert_eq!(remark["params"]["uri"], URI);
    assert_eq!(remark["params"]["text"], "the cache is back");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = trigger_records(&log_path, "conversation");
    assert_eq!(records.len(), 1, "one conversation evaluation: {records:?}");
    assert_eq!(records[0]["prefilter"]["outcome"], "pass");
    assert_eq!(records[0]["requested_action"], "remark");
    assert_eq!(records[0]["delivered"], "remark");
    let state = records[0]["rendered_state"].as_str().unwrap();
    assert!(
        state.contains("driver: add an auth remove command"),
        "the conversation digest is in the state: {state}"
    );
    assert!(
        state.contains("   1| fn main() {"),
        "numbered code: {state}"
    );
    assert_eq!(mock.calls(), 1);

    // The cooldown: another driver turn right after is skipped.
    feed.observe(&message_event(session_id, "user:http")).await;
    feed.observe(&turn_end_event(session_id)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let records = trigger_records(&log_path, "conversation");
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[1]["prefilter"]["outcome"], "skip");
    assert_eq!(records[1]["prefilter"]["reason"], "cooldown");
    assert_eq!(mock.calls(), 1, "no second remark");
}

/// The turn a navigator wake provokes never triggers the pipeline, and
/// neither does a turn end with no focused document.
#[tokio::test(start_paused = true)]
async fn navigator_caused_turn_end_and_no_focused_document_trigger_nothing() {
    let mock = MockCommenter::new(vec![Answer::Remark(1, "unexpected")]);
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let (feed, _posted, session_id) = feed_with_session().await;
    let deps = NavigatorDeps::new(Arc::new(mock.clone()), log_path.clone())
        .with_conversation(feed.clone());
    let mut client = spawn_server(deps);
    client.initialize(json!({})).await;

    // Driver turn, but no document was ever opened: nothing to judge.
    feed.observe(&turn_end_event(session_id)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(trigger_records(&log_path, "conversation").is_empty());

    // A wake's turn, with a focused document: still nothing.
    client.did_open(URI, &base_text(), 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    feed.observe(&message_event(session_id, navigator::NAVIGATOR_ACTOR_ID))
        .await;
    feed.observe(&turn_end_event(session_id)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(trigger_records(&log_path, "conversation").is_empty());
    assert_eq!(mock.calls(), 0);
}

/// The focused document is the last one opened, changed or saved.
#[tokio::test(start_paused = true)]
async fn conversation_trigger_targets_the_last_focused_document() {
    let mock = MockCommenter::new(vec![Answer::Silent]);
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("nav.jsonl");
    let (feed, _posted, session_id) = feed_with_session().await;
    let deps = NavigatorDeps::new(Arc::new(mock), log_path.clone()).with_conversation(feed.clone());
    let mut client = spawn_server(deps);
    let other = "file:///ws/src/other.rs";

    client.initialize(json!({})).await;
    client.did_open(URI, &base_text(), 1).await;
    client.did_open(other, &base_text(), 1).await;
    client.did_change(URI, &base_text(), 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    feed.observe(&turn_end_event(session_id)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let records = trigger_records(&log_path, "conversation");
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["uri"], URI, "the document changed last");
}
