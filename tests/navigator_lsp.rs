//! Navigator LSP integration tests: drive the real server over an in-memory
//! duplex stream with real JSON-RPC framed messages (Content-Length headers),
//! a scripted mock commenter, and tokio's paused clock.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use withnav::navigator;

mod common;
use common::navigator::{
    base_text, big_edit, driver_message, feed_with_session, with_min_changed_lines, LspClient, Nav,
    RemarkAnswer as Answer, ScriptedCommenter as MockCommenter, ScriptedFim, ScriptedReflexContext,
    URI,
};

// ---------- tests ----------

#[tokio::test]
async fn failed_legacy_conversion_is_warned_once_after_initialized() {
    let mut client = Nav::start_with(MockCommenter::new(vec![]), json!({}), |mut deps, ws| {
        let source = ws.join(".withnav");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("pipeline.toml"), "unsupported = true").unwrap();
        let host = withnav::flows::host::Host::new(ws.join("config"), ws.join("data"), None);
        let store = host.store(ws.into());
        let errors = store.migration_diagnostics();
        assert_eq!(errors.len(), 1);
        assert!(host
            .diagnostics(ws)
            .iter()
            .any(|error| error.code == "migration"));
        assert!(store
            .list()
            .unwrap()
            .iter()
            .any(|row| row.name == "legacy-pipeline" && row.errors == errors));
        deps.startup_warnings = errors.into_iter().map(|error| error.message).collect();
        deps
    })
    .await;
    let warning = client.expect("window/showMessage", &mut Vec::new()).await;
    assert_eq!(warning["params"]["type"], 2);
    let message = warning["params"]["message"].as_str().unwrap();
    assert!(message.contains("pipeline.toml"), "{message}");
    assert!(message.contains("unsupported"), "{message}");
    assert!(message.contains("original retained"), "{message}");
    client.notify("initialized", json!({})).await;
    let messages = client.collect_for(Duration::from_millis(100)).await;
    assert!(messages
        .iter()
        .all(|message| message["method"] != "window/showMessage"));
}

/// After a 5-line change and 3 s idle, one withnav/remark arrives.
#[tokio::test]
async fn idle_after_big_edit_emits_remark() {
    let mut client = Nav::start(MockCommenter::new(vec![Answer::Remark(
        1,
        "careful: x is unused",
    )]))
    .await;
    client.open_and_edit().await;

    tokio::time::sleep(Duration::from_millis(3001)).await;
    let mut seen = Vec::new();
    let remark = client.expect("withnav/remark", &mut seen).await;
    let params = &remark["params"];
    assert_eq!(params["uri"], URI);
    assert_eq!(params["line"], 1);
    assert_eq!(params["text"], "careful: x is unused");
    assert!(params["id"].as_str().is_some());

    let records = client.log_text();
    assert!(records.contains("\"kind\":\"remark\""));
    assert!(records.contains("\"kind\":\"trigger\""));
}

/// A pipeline that asks for 3 changed lines skips a smaller change
/// (skip: too_few_changed_lines) without calling the metacognition.
#[tokio::test]
async fn a_change_under_the_pipelines_size_limit_never_triggers() {
    let mock = MockCommenter::new(vec![Answer::Remark(0, "should not happen")]);
    let mut client = Nav::start_with(mock.clone(), json!({}), |d, ws| {
        with_min_changed_lines(d, ws, 3)
    })
    .await;
    client.open_base().await;
    // only one line changed
    client
        .did_change(URI, "fn main() {\n    println!(\"hi\");\n}", 2)
        .await;

    tokio::time::sleep(Duration::from_millis(10_000)).await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(mock.calls(), 0, "commenter must not be called");
}

/// A diagnostics event no trigger listens to logs nothing; a too-small
/// change logs a light record, without the rendered state.
#[tokio::test]
async fn skips_logs_light_or_not_at_all() {
    let mut client = Nav::start_with(
        MockCommenter::new(vec![Answer::Silent]),
        json!({}),
        |d, ws| with_min_changed_lines(d, ws, 3),
    )
    .await;
    client.open_base().await;
    client
        .notify("withnav/diagnostics", json!({"uri": URI, "items": []}))
        .await;
    client
        .did_change(URI, "fn main() {\n    println!(\"hi\");\n}", 2)
        .await;
    client.did_save(URI).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = client.log_text();
    let triggers: Vec<Value> = records
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["kind"] == "trigger")
        .collect();
    assert_eq!(triggers.len(), 1, "{records}");
    assert_eq!(triggers[0]["event"], "save");
    assert_eq!(triggers[0]["reason"], "flow_skipped");
    assert!(triggers[0].get("template_inputs").is_none());
    assert!(triggers[0].get("rendered_state").is_none());
}

/// Cooldown suppresses a second remark on the same file.
#[tokio::test]
async fn cooldown_suppresses_second_remark() {
    let mock = MockCommenter::new(vec![
        Answer::Remark(0, "first"),
        Answer::Remark(0, "second"),
    ]);
    let mut client = Nav::start(mock.clone()).await;
    client.open_and_edit().await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    let mut seen = Vec::new();
    let first = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(first["params"]["text"], "first");
    client.expect_status("idle").await;

    // A second big edit within the 45 s cooldown: the idle timer fires but
    // the policy skips, so no second commenter call happens.
    let bigger = format!("{}\n// a\n// b\n// c\n", big_edit());
    client.did_change(URI, &bigger, 3).await;
    tokio::time::sleep(Duration::from_millis(5000)).await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(mock.calls(), 1, "cooldown must suppress the second call");

    // After the cooldown elapses, a new edit fires again.
    tokio::time::sleep(Duration::from_millis(45_000)).await;
    let bigger2 = format!("{}\n// d\n// e\n// f\n", big_edit());
    client.did_change(URI, &bigger2, 4).await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    let second = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(second["params"]["text"], "second");
}

/// A one-line comment that calls on the navigator reaches it: the instinct
/// runs and the remark is delivered, where an ordinary one-line edit is
/// skipped (`one_line_change_never_triggers`).
#[tokio::test]
async fn a_one_line_call_for_help_in_a_comment_reaches_the_metacognition() {
    let mock = MockCommenter::new(vec![Answer::Remark(2, "what part is stuck?")]);
    let mut client = Nav::start(mock.clone()).await;
    client.open_base().await;
    client
        .did_change(
            URI,
            "fn main() {\n    println!(\"hello\");\n    // J'ai besoin d'aide\n}",
            2,
        )
        .await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    let mut seen = Vec::new();
    let remark = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(remark["params"]["text"], "what part is stuck?");
    assert_eq!(mock.calls(), 1);
    let records = client.log_text();
    assert!(records.contains("\"called_for_help\":true"), "{records}");
}

/// Silent answer emits no remark.
#[tokio::test]
async fn silent_answer_emits_no_remark() {
    let mut client = Nav::start(MockCommenter::new(vec![Answer::Silent])).await;
    client.open_and_edit().await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    // Silence: the "idle" status after "thinking" is the sync point.
    client.expect_status("idle").await;

    let records = client.log_text();
    assert!(records.contains("\"kind\":\"silent\""));
    assert!(!records.contains("\"kind\":\"remark\""));
}

/// Commenter error emits withnav/status error and the server stays alive.
#[tokio::test]
async fn commenter_error_emits_status_error_and_survives() {
    let mut client = Nav::start(MockCommenter::new(vec![Answer::Fail("provider exploded")])).await;
    client.open_and_edit().await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    let status = client.expect_status("error").await;
    assert_eq!(status["params"]["message"], "provider exploded");

    // The server is still alive: a request answers.
    client
        .send(&json!({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": null}))
        .await;
    let mut resp = client.recv().await;
    while resp["id"] != 99 {
        resp = client.recv().await;
    }
    assert_eq!(resp["id"], 99);
    assert!(resp.get("result").is_some());
}

/// The remark request outlives typing: it is delivered after further
/// `didChange` events, on the line's place in the current text.
#[tokio::test]
async fn typing_does_not_cancel_a_remark_and_its_line_follows_the_edits() {
    let gate = Arc::new(CancellationToken::new());
    let mock = MockCommenter::gated(vec![Answer::Remark(1, "x is unused")], gate.clone());
    let mut client = Nav::start(mock).await;
    client.open_and_edit().await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    // "thinking" proves the instinct answered and the remark is in flight.
    client.expect_status("thinking").await;

    // Typing above the remark's line (the file grows by two lines), twice.
    let above = format!("// one\n// two\n{}", big_edit());
    client
        .did_change(URI, &format!("// one\n{}", big_edit()), 3)
        .await;
    client.did_change(URI, &above, 4).await;
    tokio::time::sleep(Duration::from_millis(1)).await;

    gate.cancel();
    let mut seen = Vec::new();
    let remark = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(remark["params"]["text"], "x is unused");
    assert_eq!(
        remark["params"]["line"], 3,
        "line 1 of the judged text is line 3 after two lines were added above"
    );
}

/// A remark about a line that was edited meanwhile has nothing left to
/// talk about: it is dropped, and logged as such.
#[tokio::test]
async fn a_remark_on_a_line_edited_meanwhile_is_dropped() {
    let gate = Arc::new(CancellationToken::new());
    let mock = MockCommenter::gated(vec![Answer::Remark(1, "x is unused")], gate.clone());
    let mut client = Nav::start(mock).await;
    client.open_and_edit().await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    client.expect_status("thinking").await;

    // Line 2 of the file (index 1) is rewritten.
    client
        .did_change(URI, &big_edit().replace("let x = 1;", "let x = 10;"), 3)
        .await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    gate.cancel();
    let msgs = client.collect_for(Duration::from_millis(100)).await;
    assert!(
        msgs.iter().all(|m| m["method"] != "withnav/remark"),
        "a remark about a rewritten line leaked: {msgs:?}"
    );
    let records = client.log_text();
    assert!(
        records.contains("\"reason\":\"stale_version\""),
        "{records}"
    );
    assert!(!records.contains("\"kind\":\"remark\""));
}

/// A newer trigger for the same document supersedes the remark still being
/// written: only the newer one is delivered.
#[tokio::test]
async fn a_newer_trigger_supersedes_the_remark_in_flight() {
    let gate = Arc::new(CancellationToken::new());
    let mock = MockCommenter::gated(
        vec![Answer::Remark(0, "old"), Answer::Remark(0, "new")],
        gate.clone(),
    );
    let mut client = Nav::start(mock.clone()).await;
    client.open_and_edit().await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    client.expect_status("thinking").await;

    // Typing, then another idle: a second remark request starts.
    client
        .did_change(URI, &format!("{}\n// tail\n", big_edit()), 3)
        .await;
    // Let the server register the new idle timer before the clock moves.
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        mock.calls(),
        2,
        "the newer trigger asked for its own remark"
    );

    gate.cancel();
    let mut seen = Vec::new();
    let remark = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(remark["params"]["text"], "new");
    let rest = client.collect_for(Duration::from_millis(100)).await;
    assert!(
        rest.iter().all(|m| m["method"] != "withnav/remark"),
        "the superseded remark leaked: {rest:?}"
    );
    assert!(client.log_text().contains("\"delivered\":\"cancelled\""));
}

/// Closing the document still cancels the remark in flight.
#[tokio::test]
async fn closing_the_document_cancels_the_remark_in_flight() {
    let gate = Arc::new(CancellationToken::new());
    let mock = MockCommenter::gated(vec![Answer::Remark(0, "late")], gate.clone());
    let mut client = Nav::start(mock).await;
    client.open_and_edit().await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    client.expect_status("thinking").await;

    client
        .notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": URI}}),
        )
        .await;
    let mut seen = Vec::new();
    client.expect("withnav/clear", &mut seen).await;
    gate.cancel();
    let msgs = client.collect_for(Duration::from_millis(100)).await;
    assert!(
        msgs.iter().all(|m| m["method"] != "withnav/remark"),
        "a remark was delivered for a closed document: {msgs:?}"
    );
    let records = client.log_text();
    assert!(records.contains("\"delivered\":\"cancelled\""), "{records}");
    assert!(!records.contains("\"text\":\"late\""));
}

/// Save fires the trigger without waiting for the idle timer.
#[tokio::test]
async fn save_triggers_immediately() {
    let mut client = Nav::start(MockCommenter::new(vec![Answer::Remark(
        2,
        "watch the unwrap",
    )]))
    .await;
    client.open_and_edit().await;
    // No time advance: save must trigger on its own.
    client.did_save(URI).await;
    let mut seen = Vec::new();
    let remark = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(remark["params"]["text"], "watch the unwrap");
}

/// Feedback is written to the log.
#[tokio::test]
async fn feedback_is_written_to_log() {
    let mut client = Nav::start(MockCommenter::new(vec![])).await;
    client
        .notify(
            "withnav/feedback",
            json!({"id": "abc123", "verdict": "useful"}),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = client.log_text();
    assert!(records.contains("\"kind\":\"feedback\""));
    assert!(records.contains("\"verdict\":\"useful\""));
    assert!(records.contains("\"id\":\"abc123\""));
}

/// didClose clears state and notifies withnav/clear.
#[tokio::test]
async fn did_close_clears_remark() {
    let mut client = Nav::start(MockCommenter::new(vec![])).await;
    client.open_base().await;
    client
        .notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": URI}}),
        )
        .await;
    let mut seen = Vec::new();
    let clear = client.expect("withnav/clear", &mut seen).await;
    assert_eq!(clear["params"]["uri"], URI);
}

/// A `workspace/didChangeConfiguration` with a new `idleMs` moves the idle
/// timer boundary for the next edit. The legacy keys the pipeline now owns
/// (`minChangedLines`, `cooldownMs`) are still accepted, and ignored: the
/// pipeline's own pre-filter keeps deciding whether the instinct runs.
#[tokio::test]
async fn config_update_changes_idle_threshold() {
    let mock = MockCommenter::new(vec![Answer::Remark(0, "cfg-driven")]);
    let mut client = Nav::start_with(
        mock.clone(),
        json!({"idleMs": 5000, "minChangedLines": 1, "cooldownMs": 60000}),
        |d, _| d,
    )
    .await;
    client.open_and_edit().await;

    // Past the default 3 s, still short of the configured 5 s.
    tokio::time::sleep(Duration::from_millis(3001)).await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(mock.calls(), 0, "the initial idleMs, not the default");

    // A new idle delay applies to the next edit, which restarts the timer.
    client
        .notify(
            "workspace/didChangeConfiguration",
            json!({"settings": {"idleMs": 1000, "minChangedLines": 99, "cooldownMs": 1}}),
        )
        .await;
    client
        .did_change(URI, &format!("{}\n// tail\n", big_edit()), 3)
        .await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(mock.calls(), 0, "still inside the new 1 s delay");

    tokio::time::sleep(Duration::from_millis(200)).await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(mock.calls(), 1, "fired at the new 1 s delay");
    let mut seen = Vec::new();
    let remark = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(remark["params"]["text"], "cfg-driven");
}

/// Disabled config suppresses everything.
#[tokio::test]
async fn disabled_config_suppresses_evaluation() {
    let mock = MockCommenter::new(vec![Answer::Remark(0, "nope")]);
    let mut client = Nav::start_with(mock.clone(), json!({"enabled": false}), |d, _| d).await;
    client.open_and_edit().await;
    tokio::time::sleep(Duration::from_millis(10_000)).await;
    client.did_save(URI).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(mock.calls(), 0);
}

/// withnav/intent and withnav/diagnostics are accepted and stored
/// (observable through the log's remark after a trigger).
#[tokio::test]
async fn intent_and_diagnostics_are_accepted() {
    let mut client = Nav::start(MockCommenter::new(vec![Answer::Silent])).await;
    client
        .notify("navigator/intent", json!({"text": "legacy parser intent"}))
        .await;
    client
        .notify("withnav/intent", json!({"text": "fix the parser"}))
        .await;
    client
        .notify(
            "withnav/diagnostics",
            json!({
                "uri": URI,
                "items": [{"line": 1, "severity": "error", "message": "mismatched types"}]
            }),
        )
        .await;
    client.open_and_edit().await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Server must still respond to a request (custom notifications didn't
    // break it).
    client
        .send(&json!({"jsonrpc": "2.0", "id": 7, "method": "shutdown", "params": null}))
        .await;
    let mut resp = client.recv().await;
    while resp["id"] != 7 {
        resp = client.recv().await;
    }
    assert!(resp.get("result").is_some(), "server broken: {resp}");
}

/// withnav/inlineCompletion answers with an item and request id from the
/// reflex service, including the result range.
#[tokio::test]
async fn inline_completion_answers_empty_items() {
    let mut client = Nav::start_with(MockCommenter::new(vec![]), json!({}), |d, _| {
        d.with_fim(Arc::new(ScriptedFim::fixed("hello")))
            .with_model_ids(Some("mercury-edit-2".into()), None, None)
    })
    .await;
    client.open_base().await;
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "withnav/inlineCompletion",
            "params": {"uri": URI, "version": 1, "line": 0, "character": 4}
        }))
        .await;
    tokio::time::sleep(Duration::from_millis(151)).await;
    let resp = client.recv().await;
    assert_eq!(resp["id"], 21);
    assert_eq!(resp["result"]["items"][0]["text"], "hello");
    assert!(resp["result"]["requestId"].as_str().is_some());
}

/// A request superseded during its debounce never reaches the model, so only
/// the request that did is logged as a reflex call.
#[tokio::test]
async fn a_debounced_inline_completion_is_not_logged_as_a_model_call() {
    let fim = ScriptedFim::fixed("hello");
    let mut client = Nav::start_with(MockCommenter::new(vec![]), json!({}), |d, _| {
        d.with_fim(Arc::new(fim.clone()))
            .with_model_ids(Some("mercury-edit-2".into()), None, None)
    })
    .await;
    client.open_base().await;
    for id in [51, 52] {
        client
            .send(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "withnav/inlineCompletion",
                "params": {"uri": URI, "version": 1, "line": 0, "character": 4}
            }))
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(151)).await;
    let mut answered = 0;
    while answered < 2 {
        if client.recv().await["id"].is_u64() {
            answered += 1;
        }
    }

    assert_eq!(fim.calls(), 1, "only the last request called the model");
    let records = client.log_text();
    let reflex_calls = records
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["kind"] == "call" && v["layer"] == "reflex")
        .count();
    assert_eq!(reflex_calls, 1, "{records}");
}

/// withnav/reflexShown is accepted and logged as a reflex_shown record.
#[tokio::test]
async fn reflex_shown_is_logged() {
    let mut client = Nav::start(MockCommenter::new(vec![])).await;
    client
        .notify(
            "withnav/reflexShown",
            json!({"requestId": "req1", "shownAtMs": 1759000000123u64}),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = client.log_text();
    assert!(records.contains("\"kind\":\"reflex_shown\""));
    assert!(records.contains("\"request_id\":\"req1\""));
    assert!(records.contains("\"shown_at_ms\":1759000000123"));
}

/// With `server_info` in the deps, `initialized` delivers
/// `withnav/serverInfo` with exactly those values: the extension learns
/// the cognition chat server's port and bearer token.
#[tokio::test]
async fn server_info_arrives_after_initialized_with_the_configured_values() {
    let mut client = Nav::start_with(MockCommenter::new(vec![]), json!({}), |d, _| {
        d.with_server_info(navigator::ServerInfoParams {
            port: 45678,
            token: "tok-123".to_string(),
        })
    })
    .await;
    let mut seen = Vec::new();
    let info = client.expect("withnav/serverInfo", &mut seen).await;
    assert_eq!(info["params"]["port"], 45678);
    assert_eq!(info["params"]["token"], "tok-123");
}

/// Without `server_info` in the deps, NO `withnav/serverInfo` ever leaves
/// the server (the chat stays unannounced when its server failed to bind).
#[tokio::test]
async fn without_server_info_no_notification_is_sent() {
    let mut client = Nav::start(MockCommenter::new(vec![])).await;
    client.open_and_edit().await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let msgs = client.collect_for(Duration::from_millis(100)).await;
    assert!(
        msgs.iter()
            .all(|m| m.get("method").and_then(|m| m.as_str()) != Some("withnav/serverInfo")),
        "no serverInfo without server_info deps: {msgs:?}"
    );
}

impl LspClient {
    /// One inline completion at the document start; returns once answered.
    async fn complete(&mut self, id: u64, version: i32) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "withnav/inlineCompletion",
            "params": {"uri": URI, "version": version, "line": 0, "character": 4}
        }))
        .await;
        tokio::time::sleep(Duration::from_millis(151)).await;
        // Skip notifications (idle-timer status) until the response.
        loop {
            let resp = self.recv().await;
            if resp["id"] == id {
                return;
            }
        }
    }
}

/// The first completion serves the intent summary; once the refresh has
/// completed the next one carries the refreshed text. Inside a minute with
/// unchanged inputs the model is not called again; a changed conversation
/// digest after a minute refreshes again; a changed digest inside a minute
/// does not.
#[tokio::test(start_paused = true)]
async fn reflex_context_is_refreshed_by_the_metacognition() {
    let (feed, _posted, session_id) = feed_with_session(Ok(())).await;
    // The conversation holds the session's first driver message already.
    feed.observe(&driver_message(session_id, 2, "use anyhow for errors"))
        .await;

    let model = ScriptedReflexContext::new(vec![Ok("REFRESHED-ONE"), Ok("REFRESHED-TWO")]);
    let fim = ScriptedFim::fixed("x");
    let mut client = Nav::start_with(MockCommenter::new(vec![]), json!({}), |d, _| {
        d.with_fim(Arc::new(fim.clone()))
            .with_reflex_context(Arc::new(model.clone()))
            .with_model_ids(None, None, Some("fast-model".into()))
            .with_conversation(feed.clone())
    })
    .await;
    client
        .notify("navigator/intent", json!({"text": "legacy parser intent"}))
        .await;
    client
        .notify("withnav/intent", json!({"text": "fix the parser"}))
        .await;
    client.open_base().await;

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
    feed.observe(&driver_message(session_id, 3, "and add a --force flag"))
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
    let records = client.log_text();
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
    let model = ScriptedReflexContext::new(vec![Err("boom"), Ok("RECOVERED")]);
    let fim = ScriptedFim::fixed("x");
    let mut client = Nav::start_with(MockCommenter::new(vec![]), json!({}), |d, _| {
        d.with_fim(Arc::new(fim.clone()))
            .with_reflex_context(Arc::new(model.clone()))
    })
    .await;
    client
        .notify("navigator/intent", json!({"text": "legacy parser intent"}))
        .await;
    client
        .notify("withnav/intent", json!({"text": "fix the parser"}))
        .await;
    client.open_base().await;

    client.complete(31, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.complete(32, 1).await;
    {
        let prompts = fim.prompts();
        assert!(prompts[1].contains("fix the parser"), "{}", prompts[1]);
    }
    assert_eq!(model.calls(), 1, "no retry inside the minute");
    let records = client.log_text();
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

/// An empty answer keeps the previous context and is logged once per run,
/// not on every refresh; the next real answer still lands.
#[tokio::test(start_paused = true)]
async fn reflex_context_empty_answers_are_logged_once() {
    let model = ScriptedReflexContext::new(vec![Ok(""), Ok("  "), Ok("FRESH")]);
    let fim = ScriptedFim::fixed("x");
    let mut client = Nav::start_with(MockCommenter::new(vec![]), json!({}), |d, _| {
        d.with_fim(Arc::new(fim.clone()))
            .with_reflex_context(Arc::new(model.clone()))
    })
    .await;
    client
        .notify("navigator/intent", json!({"text": "legacy parser intent"}))
        .await;
    client
        .notify("withnav/intent", json!({"text": "fix the parser"}))
        .await;
    client.open_base().await;

    for step in 0..3u64 {
        client.complete(31 + step, 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::advance(Duration::from_secs(61)).await;
    }
    client.complete(40, 1).await;
    assert_eq!(model.calls(), 3);
    let records = client.log_text();
    assert_eq!(
        records.matches("empty answer").count(),
        1,
        "logged once: {records}"
    );
    assert!(fim.prompts().last().unwrap().contains("FRESH"));
}

// ---------- conversation trigger ----------

fn session_event(
    session_id: ulid::Ulid,
    kind: withnav::bus::event::EventKind,
    payload: Value,
) -> withnav::bus::event::EventEnvelope {
    withnav::bus::event::EventEnvelope::new(session_id, 0, kind, payload)
}

fn message_event(session_id: ulid::Ulid, source: &str) -> withnav::bus::event::EventEnvelope {
    session_event(
        session_id,
        withnav::bus::event::EventKind::MessageReceived,
        json!({
            "command_id": ulid::Ulid::new().to_string(),
            "source": source,
            "target": "agent:main",
            "mode": "next-turn",
            "content": "keep the parser, drop the cache",
        }),
    )
}

fn turn_end_event(session_id: ulid::Ulid) -> withnav::bus::event::EventEnvelope {
    session_event(
        session_id,
        withnav::bus::event::EventKind::TurnEnd,
        json!({
            "turn_id": ulid::Ulid::new().to_string(),
            "actor_id": "agent:main",
            "outcome": "done",
            "duration_ms": 1,
        }),
    )
}

fn trigger_records(client: &Nav, event: &str) -> Vec<Value> {
    client
        .log_text()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["kind"] == "trigger" && v["event"] == event)
        .collect()
}

/// A driver turn ending evaluates the pipeline with event `conversation` on
/// the focused document, and the built-in remarks on a contradiction. A
/// second driver turn within the cooldown is skipped.
#[tokio::test]
async fn driver_turn_end_triggers_one_conversation_evaluation_and_the_cooldown_applies() {
    let mock = MockCommenter::new(vec![Answer::Remark(2, "the cache is back")]);
    let (feed, _posted, session_id) = feed_with_session(Ok(())).await;
    let mut client = Nav::start_with(mock.clone(), json!({}), |d, _| {
        d.with_conversation(feed.clone())
    })
    .await;
    // Unchanged document: min_changed_lines would skip it for a save.
    client.open_base().await;
    // Let the server take the notification in before the turn ends.
    tokio::time::sleep(Duration::from_millis(50)).await;

    feed.observe(&turn_end_event(session_id)).await;
    let mut seen = Vec::new();
    let remark = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(remark["params"]["uri"], URI);
    assert_eq!(remark["params"]["text"], "the cache is back");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = trigger_records(&client, "conversation");
    assert_eq!(records.len(), 1, "one conversation evaluation: {records:?}");
    assert_eq!(records[0]["prefilter"]["outcome"], "pass");
    assert_eq!(records[0]["requested_action"], "remark");
    assert_eq!(records[0]["delivered"], "remark");
    assert!(records[0].get("rendered_state").is_none());
    assert_eq!(mock.calls(), 1);

    // The cooldown: another driver turn right after is skipped.
    feed.observe(&message_event(session_id, "user:http")).await;
    feed.observe(&turn_end_event(session_id)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let records = trigger_records(&client, "conversation");
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[1]["reason"], "flow_skipped");
    assert_eq!(mock.calls(), 1, "no second remark");
}

/// The turn a navigator wake provokes never triggers the pipeline, and
/// neither does a turn end with no focused document.
#[tokio::test]
async fn navigator_caused_turn_end_and_no_focused_document_trigger_nothing() {
    let mock = MockCommenter::new(vec![Answer::Remark(1, "unexpected")]);
    let (feed, _posted, session_id) = feed_with_session(Ok(())).await;
    let mut client = Nav::start_with(mock.clone(), json!({}), |d, _| {
        d.with_conversation(feed.clone())
    })
    .await;

    // Driver turn, but no document was ever opened: nothing to judge.
    feed.observe(&turn_end_event(session_id)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(trigger_records(&client, "conversation").is_empty());

    // A wake's turn, with a focused document: still nothing.
    client.open_base().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    feed.observe(&message_event(session_id, navigator::NAVIGATOR_ACTOR_ID))
        .await;
    feed.observe(&turn_end_event(session_id)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(trigger_records(&client, "conversation").is_empty());
    assert_eq!(mock.calls(), 0);
}

/// The focused document is the last one opened, changed or saved.
#[tokio::test]
async fn conversation_trigger_targets_the_last_focused_document() {
    let mock = MockCommenter::new(vec![Answer::Silent]);
    let (feed, _posted, session_id) = feed_with_session(Ok(())).await;
    let mut client =
        Nav::start_with(mock, json!({}), |d, _| d.with_conversation(feed.clone())).await;
    let other = "file:///ws/src/other.rs";

    client.open_base().await;
    client.did_open(other, &base_text(), 1).await;
    client.did_change(URI, &base_text(), 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    feed.observe(&turn_end_event(session_id)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let records = trigger_records(&client, "conversation");
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["uri"], URI, "the document changed last");
}

// ---------- a call for help is answered once ----------

const HELP_COMMENT: &str = "    // need help here :)";

/// [`base_text`] with the call for help on a line of its own, plus `extra`
/// lines after it.
fn with_help(extra: &[&str]) -> String {
    let mut lines = vec![
        "fn main() {".to_string(),
        HELP_COMMENT.to_string(),
        "    println!(\"hello\");".to_string(),
    ];
    lines.extend(extra.iter().map(|l| l.to_string()));
    lines.push("}".to_string());
    lines.join("\n")
}

fn delivered(client: &Nav) -> Vec<(String, Option<String>)> {
    client
        .log_text()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["kind"] == "trigger")
        .map(|v| {
            (
                v["delivered"].as_str().unwrap().to_string(),
                v["reason"].as_str().map(str::to_string),
            )
        })
        .collect()
}

/// The same comment, triggered again while its remark is being written,
/// leaves that remark alone: it is skipped, not superseded.
#[tokio::test]
async fn the_same_call_for_help_does_not_cancel_its_own_remark() {
    let gate = Arc::new(CancellationToken::new());
    let mock = MockCommenter::gated(vec![Answer::Remark(1, "the cache")], gate.clone());
    let mut client = Nav::start(mock.clone()).await;
    client.open_base().await;
    client.did_change(URI, &with_help(&[]), 2).await;
    client.did_save(URI).await;
    client.expect_status("thinking").await;

    // The comment moves down a line and the driver saves again.
    client
        .did_change(URI, &with_help(&["    // tail"]), 3)
        .await;
    client.did_save(URI).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(mock.calls(), 1, "no second request for the same comment");

    gate.cancel();
    let mut seen = Vec::new();
    let remark = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(remark["params"]["text"], "the cache");
    let outcomes = delivered(&client);
    assert!(
        !outcomes.iter().any(|(d, _)| d == "cancelled"),
        "{outcomes:?}"
    );
}

/// After the remark, the comment does not fire again unless it was removed
/// and written anew.
#[tokio::test]
async fn an_answered_call_for_help_fires_again_only_once_rewritten() {
    let mock = MockCommenter::new(vec![
        Answer::Remark(1, "first answer"),
        Answer::Remark(1, "second answer"),
    ]);
    let mut client = Nav::start(mock.clone()).await;
    client.open_base().await;
    client.did_change(URI, &with_help(&[]), 2).await;
    client.did_save(URI).await;
    let mut seen = Vec::new();
    let first = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(first["params"]["text"], "first answer");
    client.expect_status("idle").await;

    // The same comment written a second time elsewhere is the same call.
    let copied = with_help(&["    // Need  help here :)"]);
    client.did_change(URI, &copied, 3).await;
    client.did_save(URI).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(mock.calls(), 1, "an answered call is not asked again");

    // Removing every copy forgets it; writing it again asks again.
    client.did_change(URI, &base_text(), 4).await;
    client.did_change(URI, &with_help(&[]), 5).await;
    client.did_save(URI).await;
    let second = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(second["params"]["text"], "second answer");
    assert_eq!(mock.calls(), 2);
}

/// A trigger that is not a call for help still supersedes the remark in
/// flight, and help remarks get three tool rounds, ordinary ones one.
#[tokio::test]
async fn an_ordinary_trigger_still_supersedes_a_help_remark_and_tool_rounds_differ() {
    let gate = Arc::new(CancellationToken::new());
    let mock = MockCommenter::gated(
        vec![Answer::Remark(1, "help"), Answer::Remark(0, "ordinary")],
        gate.clone(),
    );
    let mut client = Nav::start(mock.clone()).await;
    client.open_base().await;
    client.did_change(URI, &with_help(&[]), 2).await;
    client.did_save(URI).await;
    client.expect_status("thinking").await;

    // The comment is rewritten away and plain lines are added: not a call
    // for help, so it supersedes the help remark.
    let plain = [
        "fn main() {",
        "    let x = 1;",
        "    let y = 2;",
        "    println!(\"{x}-{y}\");",
        "}",
    ]
    .join("\n");
    client.did_change(URI, &plain, 3).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::sleep(Duration::from_millis(3001)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(mock.calls(), 2, "the ordinary trigger asked for its own");
    assert_eq!(mock.tool_rounds(), vec![3, 1]);

    gate.cancel();
    let mut seen = Vec::new();
    let remark = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(remark["params"]["text"], "ordinary");
    assert!(delivered(&client).iter().any(|(d, _)| d == "cancelled"));
}
