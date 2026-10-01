//! The exact JSON of the trigger record for every delivered outcome: key
//! names, and which keys are omitted rather than null. Replay and the
//! tuning tools read these records back, so the shape is a contract.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use tokio_util::sync::CancellationToken;

use rust_agent::navigator;

mod common;
use common::navigator::{
    big_edit, feed_with_session, with_intent_and_feed, Nav, RemarkAnswer as Answer,
    ScriptedCommenter as MockCommenter, ScriptedDecision, URI,
};

/// The one trigger record of a log, with the values that differ from run to
/// run replaced by a type tag so the rest can be compared exactly.
fn only_trigger_shape(nav: &Nav) -> Value {
    let text = nav.log_text();
    let mut triggers: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["kind"] == "trigger")
        .collect();
    assert_eq!(triggers.len(), 1, "exactly one terminal record: {text}");
    let mut record = triggers.remove(0);
    let map = record.as_object_mut().unwrap();
    for key in [
        "ts",
        "trigger_id",
        "run_id",
        "remark_id",
        "session",
        "pipeline_hash",
        "rendered_state",
        "workspace",
    ] {
        if let Some(v) = map.get_mut(key) {
            assert!(v.is_string(), "{key} is a string: {v}");
            *v = json!("<string>");
        }
    }
    for key in ["template_inputs", "questions", "answers"] {
        if let Some(v) = map.get_mut(key) {
            assert!(v.is_object(), "{key} is an object: {v}");
            *v = json!("<object>");
        }
    }
    record
}

async fn edit_and_wait_idle(client: &mut Nav) {
    client.open_and_edit().await;
    // Sleeping (not advancing) lets the server read the edit before the
    // idle timer starts counting.
    tokio::time::sleep(Duration::from_millis(3100)).await;
}

async fn run_idle_trigger(answers: Vec<Answer>) -> Value {
    let mut client = Nav::start(MockCommenter::new(answers)).await;
    edit_and_wait_idle(&mut client).await;
    client.expect_status_in(&["idle", "error"]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    only_trigger_shape(&client)
}

/// Runs one save with an intent on disk (so the built-in wake branch can
/// fire) against `feed`, and returns the trigger record's shape.
async fn run_wake_trigger(
    feed: navigator::ConversationFeed,
    ask_pending_for: Option<ulid::Ulid>,
) -> Value {
    if let Some(session_id) = ask_pending_for {
        feed.observe(&rust_agent::bus::event::EventEnvelope::new(
            session_id,
            2,
            rust_agent::bus::event::EventKind::AgentYield,
            json!({}),
        ))
        .await;
    }
    let mut client = Nav::start_with(MockCommenter::new(vec![]), json!({}), |d, ws| {
        with_intent_and_feed(d, ws, feed)
    })
    .await;
    client.open_and_edit().await;
    client.did_save(URI).await;
    client.expect_status("idle").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    only_trigger_shape(&client)
}

fn inputs(changed_lines: i64, has_conversation: bool, has_intent: bool) -> Value {
    json!({
        "changed_lines": changed_lines,
        "has_conversation": has_conversation,
        "has_errors": false,
        "has_intent": has_intent,
        "since_last_remark_ms": 4611686018427387903i64,
    })
}

/// The record of an idle trigger that ran the whole pipeline to a `remark`
/// branch over `big_edit`; `extra` overrides or adds keys.
fn idle_remark_record(extra: Value) -> Value {
    let mut record = json!({
        "answers": "<object>",
        "branch": 1,
        "event": "idle",
        "kind": "trigger",
        "model": "instinct",
        "pipeline_hash": "<string>",
        "prefilter": {"inputs": inputs(4, false, false), "outcome": "pass"},
        "questions": "<object>",
        "rendered_state": "<string>",
        "requested_action": "remark",
        "run_id": "<string>",
        "template_inputs": "<object>",
        "trigger_id": "<string>",
        "ts": "<string>",
        "uri": URI,
        "version": 2,
    });
    for (key, value) in extra.as_object().unwrap() {
        record[key] = value.clone();
    }
    record
}

/// The record of a save trigger that ran the whole pipeline to a
/// `wake_cognition` branch over `big_edit` with an intent on disk.
fn save_wake_record(has_conversation: bool, extra: Value) -> Value {
    let mut record = json!({
        "answers": "<object>",
        "branch": 0,
        "event": "save",
        "kind": "trigger",
        "model": "instinct",
        "pipeline_hash": "<string>",
        "prefilter": {"inputs": inputs(4, has_conversation, true), "outcome": "pass"},
        "questions": "<object>",
        "rendered_state": "<string>",
        "requested_action": "wake_cognition",
        "run_id": "<string>",
        "template_inputs": "<object>",
        "trigger_id": "<string>",
        "ts": "<string>",
        "uri": URI,
        "version": 2,
        "workspace": "<string>",
    });
    for (key, value) in extra.as_object().unwrap() {
        record[key] = value.clone();
    }
    record
}

// Given a scripted metacognition and instinct
// When one trigger runs to each delivered outcome
// Then the one trigger record has exactly these keys: optional fields are
//      omitted (never null), and the identifiers are strings.
#[tokio::test(start_paused = true)]
async fn the_trigger_record_json_shape_per_delivered_outcome() {
    // remark
    assert_eq!(
        run_idle_trigger(vec![Answer::Remark(1, "careful")]).await,
        idle_remark_record(json!({"delivered": "remark", "remark_id": "<string>"})),
        "remark"
    );
    // silent
    assert_eq!(
        run_idle_trigger(vec![Answer::Silent]).await,
        idle_remark_record(json!({"delivered": "silent"})),
        "silent"
    );
    // malformed
    assert_eq!(
        run_idle_trigger(vec![Answer::Malformed("prose")]).await,
        idle_remark_record(json!({"delivered": "malformed"})),
        "malformed"
    );
    // provider_error (metacognition)
    assert_eq!(
        run_idle_trigger(vec![Answer::Fail("boom")]).await,
        idle_remark_record(json!({"delivered": "provider_error"})),
        "provider_error"
    );
}

#[tokio::test(start_paused = true)]
async fn the_trigger_record_json_shape_when_the_remark_is_superseded() {
    let gate = Arc::new(CancellationToken::new());
    let mock = MockCommenter::gated(vec![Answer::Remark(0, "stale")], gate);
    let mut client = Nav::start(mock).await;
    client.open_and_edit().await;
    client.expect_status("thinking").await;
    client
        .did_change(URI, &format!("{}\n// tail\n", big_edit()), 3)
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        only_trigger_shape(&client),
        idle_remark_record(json!({"delivered": "cancelled"}))
    );
}

#[tokio::test(start_paused = true)]
async fn the_trigger_record_json_shape_when_the_instinct_fails() {
    let mut client = Nav::start_as_is(MockCommenter::new(vec![]), |d, _| {
        d.with_decision(Arc::new(ScriptedDecision::failing("bad")))
    })
    .await;
    edit_and_wait_idle(&mut client).await;
    assert_eq!(
        only_trigger_shape(&client),
        json!({
            "delivered": "provider_error",
            "event": "idle",
            "kind": "trigger",
            "prefilter": {"inputs": inputs(4, false, false), "outcome": "pass"},
            "reason": "instinct",
            "run_id": "<string>",
            "trigger_id": "<string>",
            "ts": "<string>",
            "uri": URI,
            "version": 2,
        })
    );
}

#[tokio::test(start_paused = true)]
async fn the_trigger_record_json_shape_without_an_instinct() {
    let mut client = Nav::start_as_is(MockCommenter::new(vec![]), |d, _| d).await;
    edit_and_wait_idle(&mut client).await;
    assert_eq!(
        only_trigger_shape(&client),
        json!({
            "delivered": "none",
            "event": "idle",
            "kind": "trigger",
            "prefilter": {
                "inputs": inputs(4, false, false),
                "outcome": "skip",
                "reason": "instinct_unavailable",
            },
            "reason": "instinct_unavailable",
            "run_id": "<string>",
            "trigger_id": "<string>",
            "ts": "<string>",
            "uri": URI,
            "version": 2,
        })
    );
}

#[tokio::test(start_paused = true)]
async fn the_trigger_record_json_shape_for_a_pre_filter_skip() {
    let mut client = Nav::start(MockCommenter::new(vec![])).await;
    client.open_base().await;
    client
        .did_change(URI, "fn main() {\n    println!(\"hi\");\n}", 2)
        .await;
    client.did_save(URI).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // A skip keeps the instinct's identity but not the heavy inputs, and
    // carries no requested action.
    assert_eq!(
        only_trigger_shape(&client),
        json!({
            "delivered": "none",
            "event": "save",
            "kind": "trigger",
            "model": "instinct",
            "pipeline_hash": "<string>",
            "prefilter": {
                "inputs": inputs(2, false, false),
                "outcome": "skip",
                "reason": "too_few_changed_lines",
            },
            "questions": "<object>",
            "reason": "too_few_changed_lines",
            "run_id": "<string>",
            "trigger_id": "<string>",
            "ts": "<string>",
            "uri": URI,
            "version": 2,
        })
    );
}

#[tokio::test(start_paused = true)]
async fn the_trigger_record_json_shape_for_a_wake() {
    let (feed, _posted, _session) = feed_with_session(Ok(())).await;
    assert_eq!(
        run_wake_trigger(feed, None).await,
        save_wake_record(true, json!({"delivered": "wake", "session": "<string>"}))
    );
}

#[tokio::test(start_paused = true)]
async fn the_trigger_record_json_shape_for_a_queued_wake() {
    let (feed, _posted, session) = feed_with_session(Ok(())).await;
    assert_eq!(
        run_wake_trigger(feed, Some(session)).await,
        save_wake_record(
            true,
            json!({"delivered": "wake_queued", "session": "<string>"})
        )
    );
}

#[tokio::test(start_paused = true)]
async fn the_trigger_record_json_shape_when_no_cognition_can_be_woken() {
    let unavailable = json!({"delivered": "none", "reason": "cognition_unavailable"});
    // No session at all.
    assert_eq!(
        run_wake_trigger(navigator::ConversationFeed::new(), None).await,
        save_wake_record(false, unavailable.clone()),
        "no session"
    );
    // A session that refuses the post.
    let (feed, _posted, _session) = feed_with_session(Err("inbox full".into())).await;
    assert_eq!(
        run_wake_trigger(feed, None).await,
        save_wake_record(true, unavailable),
        "refused post"
    );
}

#[tokio::test(start_paused = true)]
async fn a_save_without_any_change_writes_no_trigger_record() {
    let mut client = Nav::start(MockCommenter::new(vec![])).await;
    client.open_base().await;
    client.did_save(URI).await;
    client.expect_status("idle").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let text = client.log_text();
    assert!(!text.contains("\"kind\":\"trigger\""), "{text}");
}
